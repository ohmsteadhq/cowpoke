//! Modbus TCP: a server (simulated device answering as one or more unit ids)
//! and a client (polls a real device or gateway).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::{Ready, ready};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;
use tokio_modbus::prelude::*;
use tokio_modbus::server::tcp::{Server, accept_tcp_connection};

use crate::codec::{WordOrder, decode_registers, encode_registers};
use crate::manager::Ctx;
use crate::model::{ModbusTable, TagDef};
use crate::tags::{Origin, Quality, TagStore};

// ------------------------------------------------------------------ server

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerSettings {
    #[serde(default = "crate::services::any_host")]
    bind: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default)]
    word_order: WordOrder,
    /// Unit ids this server answers as. Default: every unit a tag uses.
    #[serde(default)]
    units: Option<Vec<u8>>,
    /// Reject reads and writes of registers no tag covers (default: read as 0, writes ignored).
    #[serde(default)]
    strict: bool,
}

fn default_port() -> u16 {
    502
}

type Loc = (u8, ModbusTable);

struct Image {
    /// Register (or bit) values per unit and table.
    words: HashMap<Loc, HashMap<u16, u16>>,
}

struct Shared {
    service: String,
    tags: Vec<TagDef>,
    order: WordOrder,
    units: HashSet<u8>,
    strict: bool,
    store: TagStore,
    image: Mutex<Image>,
}

impl Shared {
    fn span(t: &TagDef) -> (u16, u16) {
        let start = t.register.unwrap_or(0);
        (start, t.datatype.registers(t.length))
    }

    fn put_tag(&self, img: &mut Image, t: &TagDef, v: &Value) {
        let Ok(words) = encode_registers(t, v, self.order) else {
            return;
        };
        let (start, _) = Self::span(t);
        let m = img.words.entry((t.unit(), t.table())).or_default();
        for (i, w) in words.into_iter().enumerate() {
            m.insert(start.wrapping_add(i as u16), w);
        }
    }

    fn tag_covering(&self, loc: Loc, addr: u16) -> Option<&TagDef> {
        self.tags.iter().find(|t| {
            let (s, n) = Self::span(t);
            (t.unit(), t.table()) == loc && addr >= s && (addr as u32) < s as u32 + n as u32
        })
    }

    fn read(&self, loc: Loc, addr: u16, cnt: u16) -> Result<Vec<u16>, ExceptionCode> {
        if cnt == 0 || addr as u32 + cnt as u32 > 0x1_0000 {
            return Err(ExceptionCode::IllegalDataAddress);
        }
        let img = self.image.lock();
        let m = img.words.get(&loc);
        (addr..addr + cnt)
            .map(|a| match m.and_then(|m| m.get(&a)) {
                Some(w) => Ok(*w),
                None if self.strict => Err(ExceptionCode::IllegalDataAddress),
                None => Ok(0),
            })
            .collect()
    }

    fn write(&self, loc: Loc, addr: u16, vals: &[u16]) -> Result<(), ExceptionCode> {
        if addr as u32 + vals.len() as u32 > 0x1_0000 {
            return Err(ExceptionCode::IllegalDataAddress);
        }
        let mut touched: Vec<&TagDef> = vec![];
        for i in 0..vals.len() as u16 {
            match self.tag_covering(loc, addr + i) {
                Some(t) if !t.writable => return Err(ExceptionCode::IllegalDataAddress),
                Some(t) => {
                    if !touched.iter().any(|x| x.name == t.name) {
                        touched.push(t)
                    }
                }
                None if self.strict => return Err(ExceptionCode::IllegalDataAddress),
                None => {}
            }
        }
        let mut img = self.image.lock();
        let m = img.words.entry(loc).or_default();
        for (i, v) in vals.iter().enumerate() {
            m.insert(addr + i as u16, *v);
        }
        let updates: Vec<(String, Value)> = touched
            .iter()
            .map(|t| {
                let (s, n) = Self::span(t);
                let words: Vec<u16> = (s..s + n).map(|a| *m.get(&a).unwrap_or(&0)).collect();
                (t.name.clone(), decode_registers(t, &words, self.order))
            })
            .collect();
        drop(img);
        for (name, v) in updates {
            self.store.set(&self.service, &name, v, Quality::Good, Origin::Service);
        }
        Ok(())
    }

    fn handle(&self, req: SlaveRequest<'static>) -> Result<Response, ExceptionCode> {
        let unit = req.slave;
        if !self.units.is_empty() && !self.units.contains(&unit) {
            return Err(ExceptionCode::GatewayTargetDevice);
        }
        let bits = |v: Vec<u16>| v.into_iter().map(|w| w != 0).collect::<Vec<bool>>();
        use ModbusTable::*;
        match req.request {
            Request::ReadCoils(a, n) => Ok(Response::ReadCoils(bits(self.read((unit, Coil), a, n)?))),
            Request::ReadDiscreteInputs(a, n) => Ok(Response::ReadDiscreteInputs(bits(
                self.read((unit, DiscreteInput), a, n)?,
            ))),
            Request::ReadHoldingRegisters(a, n) => Ok(Response::ReadHoldingRegisters(
                self.read((unit, HoldingRegister), a, n)?,
            )),
            Request::ReadInputRegisters(a, n) => Ok(Response::ReadInputRegisters(
                self.read((unit, InputRegister), a, n)?,
            )),
            Request::WriteSingleCoil(a, v) => {
                self.write((unit, Coil), a, &[u16::from(v)])?;
                Ok(Response::WriteSingleCoil(a, v))
            }
            Request::WriteMultipleCoils(a, v) => {
                let w: Vec<u16> = v.iter().map(|b| u16::from(*b)).collect();
                self.write((unit, Coil), a, &w)?;
                Ok(Response::WriteMultipleCoils(a, v.len() as u16))
            }
            Request::WriteSingleRegister(a, v) => {
                self.write((unit, HoldingRegister), a, &[v])?;
                Ok(Response::WriteSingleRegister(a, v))
            }
            Request::WriteMultipleRegisters(a, v) => {
                self.write((unit, HoldingRegister), a, &v)?;
                Ok(Response::WriteMultipleRegisters(a, v.len() as u16))
            }
            Request::MaskWriteRegister(a, and, or) => {
                let cur = self.read((unit, HoldingRegister), a, 1)?[0];
                self.write((unit, HoldingRegister), a, &[(cur & and) | (or & !and)])?;
                Ok(Response::MaskWriteRegister(a, and, or))
            }
            Request::ReadWriteMultipleRegisters(ra, rn, wa, wv) => {
                self.write((unit, HoldingRegister), wa, &wv)?;
                Ok(Response::ReadWriteMultipleRegisters(
                    self.read((unit, HoldingRegister), ra, rn)?,
                ))
            }
            _ => Err(ExceptionCode::IllegalFunction),
        }
    }
}

#[derive(Clone)]
struct MbService(Arc<Shared>);

impl tokio_modbus::server::Service for MbService {
    type Request = SlaveRequest<'static>;
    type Response = Response;
    type Exception = ExceptionCode;
    type Future = Ready<Result<Response, ExceptionCode>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        ready(self.0.handle(req))
    }
}

pub async fn server(ctx: &mut Ctx) -> Result<()> {
    let s: ServerSettings = ctx.settings()?;
    let units: HashSet<u8> = match &s.units {
        Some(u) => u.iter().copied().collect(),
        None => ctx.def.tags.iter().map(|t| t.unit()).collect(),
    };
    let shared = Arc::new(Shared {
        service: ctx.def.id.clone(),
        tags: ctx.def.tags.clone(),
        order: s.word_order,
        units,
        strict: s.strict,
        store: ctx.tags.clone(),
        image: Mutex::new(Image {
            words: HashMap::new(),
        }),
    });
    // Subscribe before filling the image so no change is missed in between.
    let mut events = ctx.tags.subscribe();
    {
        let mut img = shared.image.lock();
        for t in &shared.tags {
            if let Some(v) = ctx.tags.value(&ctx.def.id, &t.name) {
                shared.put_tag(&mut img, t, &v);
            }
        }
    }

    let listener = crate::services::listen(&s.bind, s.port).await?;
    let local = listener.local_addr()?;
    let mut units: Vec<u8> = shared.units.iter().copied().collect();
    units.sort();
    ctx.status.running(format!(
        "listening on {local}, unit {}",
        if units.is_empty() {
            "any".to_string()
        } else {
            units.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(", ")
        }
    ));

    let server = Server::new(listener);
    let svc = MbService(shared.clone());
    let on_connected = move |stream, addr: SocketAddr| {
        let svc = svc.clone();
        async move { accept_tcp_connection(stream, addr, move |_| Ok(Some(svc.clone()))) }
    };
    let on_error = |e: std::io::Error| tracing::debug!("modbus connection: {e}");

    let id = ctx.def.id.clone();
    let follow = {
        let shared = shared.clone();
        async move {
            loop {
                match events.recv().await {
                    Ok(e) if e.service == id && e.origin != Origin::Service => {
                        if let Some(t) = shared.tags.iter().find(|t| t.name == e.tag) {
                            let mut img = shared.image.lock();
                            shared.put_tag(&mut img, t, &e.state.value);
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                }
            }
        }
    };

    tokio::select! {
        r = server.serve(&on_connected, on_error) => r.context("modbus server stopped")?,
        _ = follow => {}
        _ = crate::services::idle_until_cancelled(ctx) => {}
    }
    Ok(())
}

// ------------------------------------------------------------------ client

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientSettings {
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default = "default_poll")]
    poll_ms: u64,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
    #[serde(default)]
    word_order: WordOrder,
}

fn default_poll() -> u64 {
    1000
}
fn default_timeout() -> u64 {
    3000
}

/// A contiguous read covering several tags.
struct Block {
    unit: u8,
    table: ModbusTable,
    start: u16,
    count: u16,
    tags: Vec<usize>,
}

fn plan_blocks(tags: &[TagDef]) -> Vec<Block> {
    let mut groups: BTreeMap<(u8, u8), Vec<usize>> = BTreeMap::new();
    for (i, t) in tags.iter().enumerate() {
        groups.entry((t.unit(), t.table() as u8)).or_default().push(i);
    }
    let mut blocks = vec![];
    for (_, mut idx) in groups {
        idx.sort_by_key(|&i| tags[i].register.unwrap_or(0));
        let table = tags[idx[0]].table();
        let max = if matches!(table, ModbusTable::Coil | ModbusTable::DiscreteInput) {
            2000
        } else {
            125
        };
        let mut cur: Option<Block> = None;
        for i in idx {
            let t = &tags[i];
            let s = t.register.unwrap_or(0);
            let end = s as u32 + t.datatype.registers(t.length) as u32;
            if let Some(b) = cur.as_mut() {
                let b_end = b.start as u32 + b.count as u32;
                if s as u32 <= b_end + 8 && end - b.start as u32 <= max {
                    b.count = (end.max(b_end) - b.start as u32) as u16;
                    b.tags.push(i);
                    continue;
                }
                blocks.push(cur.take().unwrap());
            }
            cur = Some(Block {
                unit: t.unit(),
                table,
                start: s,
                count: (end - s as u32).min(max) as u16,
                tags: vec![i],
            });
        }
        blocks.extend(cur);
    }
    blocks
}

pub async fn client(ctx: &mut Ctx) -> Result<()> {
    let s: ClientSettings = ctx.settings()?;
    let addr = tokio::net::lookup_host((s.host.as_str(), s.port))
        .await
        .with_context(|| format!("can't resolve {}", s.host))?
        .next()
        .ok_or_else(|| anyhow!("can't resolve {}", s.host))?;
    let timeout = Duration::from_millis(s.timeout_ms);
    let mut mb = tokio::time::timeout(timeout, tcp::connect_slave(addr, Slave(1)))
        .await
        .map_err(|_| anyhow!("timed out connecting to {addr}"))?
        .with_context(|| format!("can't connect to {addr}"))?;
    ctx.status.running(format!("connected to {addr}"));

    let tags = ctx.def.tags.clone();
    let blocks = plan_blocks(&tags);
    let id = ctx.def.id.clone();
    let mut tick = tokio::time::interval(Duration::from_millis(s.poll_ms.max(50)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ctx.cancel.cancelled() => {
                let _ = mb.disconnect().await;
                return Ok(());
            }
            _ = tick.tick() => {
                for b in &blocks {
                    mb.set_slave(Slave(b.unit));
                    let res = tokio::time::timeout(timeout, read_block(&mut mb, b)).await
                        .map_err(|_| anyhow!("timed out reading unit {} at {}", b.unit, b.start))?
                        .with_context(|| format!("reading unit {} at {}", b.unit, b.start))?;
                    match res {
                        Ok(words) => for &i in &b.tags {
                            let t = &tags[i];
                            let off = (t.register.unwrap_or(0) - b.start) as usize;
                            let n = t.datatype.registers(t.length) as usize;
                            if let Some(w) = words.get(off..off + n) {
                                ctx.tags.set(&id, &t.name, decode_registers(t, w, s.word_order), Quality::Good, Origin::Service);
                            }
                        },
                        Err(exc) => {
                            tracing::debug!(service = %id, "unit {} {:?} at {}: {exc}", b.unit, b.table, b.start);
                            for &i in &b.tags { ctx.tags.set_bad(&id, &tags[i].name); }
                        }
                    }
                }
            }
            Some(w) = ctx.writes.recv() => {
                let r = write_tag(&mut mb, &tags, &w.tag, &w.value, s.word_order, timeout).await;
                if r.is_ok() {
                    ctx.tags.set(&id, &w.tag, w.value.clone(), Quality::Good, Origin::Service);
                }
                let _ = w.reply.send(r.as_ref().map(|_| ()).map_err(|e| e.to_string()));
                // A transport error means the connection is gone: reconnect.
                if let Err(WriteErr::Io(e)) = r {
                    bail!("{e}");
                }
            }
        }
    }
}

async fn read_block(
    mb: &mut client::Context,
    b: &Block,
) -> tokio_modbus::Result<Vec<u16>> {
    let bits = |r: tokio_modbus::Result<Vec<bool>>| {
        r.map(|x| x.map(|v| v.into_iter().map(u16::from).collect()))
    };
    match b.table {
        ModbusTable::HoldingRegister => mb.read_holding_registers(b.start, b.count).await,
        ModbusTable::InputRegister => mb.read_input_registers(b.start, b.count).await,
        ModbusTable::Coil => bits(mb.read_coils(b.start, b.count).await),
        ModbusTable::DiscreteInput => bits(mb.read_discrete_inputs(b.start, b.count).await),
    }
}

#[derive(Debug, thiserror::Error)]
enum WriteErr {
    #[error("{0}")]
    Rejected(String),
    #[error("{0}")]
    Io(String),
}

async fn write_tag(
    mb: &mut client::Context,
    tags: &[TagDef],
    name: &str,
    v: &Value,
    order: WordOrder,
    timeout: Duration,
) -> Result<(), WriteErr> {
    let t = tags
        .iter()
        .find(|t| t.name == name)
        .ok_or_else(|| WriteErr::Rejected(format!("no tag `{name}`")))?;
    let a = t.register.unwrap_or(0);
    mb.set_slave(Slave(t.unit()));
    let fut = async {
        match t.table() {
            ModbusTable::Coil => mb.write_single_coil(a, crate::codec::truthy(v)).await,
            ModbusTable::HoldingRegister => {
                let w = encode_registers(t, v, order).map_err(|e| {
                    tokio_modbus::Error::Transport(std::io::Error::other(e))
                })?;
                if w.len() == 1 {
                    mb.write_single_register(a, w[0]).await
                } else {
                    mb.write_multiple_registers(a, &w).await
                }
            }
            _ => Ok(Err(ExceptionCode::IllegalFunction)),
        }
    };
    match tokio::time::timeout(timeout, fut).await {
        Err(_) => Err(WriteErr::Io("timed out writing".into())),
        Ok(Err(e)) => Err(WriteErr::Io(e.to_string())),
        Ok(Ok(Err(ExceptionCode::IllegalFunction)))
            if matches!(t.table(), ModbusTable::InputRegister | ModbusTable::DiscreteInput) =>
        {
            Err(WriteErr::Rejected("input registers and discrete inputs are read-only".into()))
        }
        Ok(Ok(Err(exc))) => Err(WriteErr::Rejected(format!("device refused: {exc}"))),
        Ok(Ok(Ok(()))) => Ok(()),
    }
}

/// Tags whose registers overlap on the same unit and table, as readable warnings.
pub fn overlaps(tags: &[TagDef]) -> Vec<String> {
    let mut out = vec![];
    for (i, a) in tags.iter().enumerate() {
        let Some(ar) = a.register else { continue };
        let a_end = ar as u32 + a.datatype.registers(a.length) as u32;
        for b in &tags[i + 1..] {
            let Some(br) = b.register else { continue };
            if (a.unit(), a.table()) != (b.unit(), b.table()) {
                continue;
            }
            let b_end = br as u32 + b.datatype.registers(b.length) as u32;
            if (ar as u32) < b_end && (br as u32) < a_end {
                out.push(format!(
                    "`{}` ({}..{}) and `{}` ({}..{}) share registers on unit {}",
                    a.name, ar, a_end - 1, b.name, br, b_end - 1, a.unit()
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DataType;

    fn t(name: &str, reg: u16, dt: DataType) -> TagDef {
        TagDef {
            name: name.into(),
            datatype: dt,
            register: Some(reg),
            ..Default::default()
        }
    }

    #[test]
    fn overlap_warnings() {
        let tags = vec![
            t("a", 0, DataType::Float32),
            t("b", 1, DataType::Uint16),
            t("c", 2, DataType::Uint16),
        ];
        let w = overlaps(&tags);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("`a`") && w[0].contains("`b`"));
    }

    #[test]
    fn blocks_merge_nearby_registers() {
        let tags = vec![
            t("a", 0, DataType::Uint16),
            t("b", 2, DataType::Float32),
            t("c", 300, DataType::Uint16),
        ];
        let b = plan_blocks(&tags);
        assert_eq!(b.len(), 2);
        assert_eq!((b[0].start, b[0].count), (0, 4));
        assert_eq!((b[1].start, b[1].count), (300, 1));
    }
}

/// Check settings without running the service (used by the form and `/api/services/check`).
pub fn check_settings(mode: crate::model::Mode, v: &Value) -> Result<(), String> {
    let r = match mode {
        crate::model::Mode::Server => serde_json::from_value::<ServerSettings>(v.clone()).map(|_| ()),
        crate::model::Mode::Client => serde_json::from_value::<ClientSettings>(v.clone()).map(|_| ()),
    };
    r.map_err(|e| format!("settings: {e}"))
}
