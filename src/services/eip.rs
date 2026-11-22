//! EtherNet/IP (CIP explicit messaging) for Allen-Bradley Logix controllers.
//!
//! Client: reads and writes Logix tags by symbolic name (`Program:Main.Level`) with the
//! Read Tag (0x4C) and Write Tag (0x4D) services, routed through the backplane to `slot`
//! with Unconnected Send. Server: a simulated Logix controller answering the same
//! requests (direct, via Unconnected Send, or batched in a Multiple Service Packet) from
//! the tag values, plus ListIdentity so browse tools find it.
//!
//! Not covered: connected (Forward Open) messaging, arrays beyond one element,
//! UDTs other than STRING, and tag browsing.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::manager::Ctx;
use crate::model::{DataType, Mode, ServiceDef, TagDef};
use crate::tags::{Origin, Quality, TagStore, coerce_tag};

// ------------------------------------------------------------------ CIP types

pub const BOOL: u16 = 0xC1;
pub const SINT: u16 = 0xC2;
pub const INT: u16 = 0xC3;
pub const DINT: u16 = 0xC4;
pub const LINT: u16 = 0xC5;
pub const USINT: u16 = 0xC6;
pub const UINT: u16 = 0xC7;
pub const UDINT: u16 = 0xC8;
pub const ULINT: u16 = 0xC9;
pub const REAL: u16 = 0xCA;
pub const LREAL: u16 = 0xCB;
/// Structures: the type is 0x02A0 followed by a structure handle.
pub const STRUCT: u16 = 0x02A0;
/// The structure handle of Logix's built-in STRING (LEN: DINT, DATA: SINT[82]).
pub const STRING_HANDLE: u16 = 0x0FCE;
const STRING_MAX: usize = 82;

/// A CIP type as it travels on the wire: the code, plus a structure handle for structs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CipType {
    pub code: u16,
    pub handle: Option<u16>,
}

pub fn cip_type(dt: DataType) -> CipType {
    let code = match dt {
        DataType::Bool => BOOL,
        DataType::Int16 => INT,
        DataType::Uint16 => UINT,
        DataType::Int32 => DINT,
        DataType::Uint32 => UDINT,
        DataType::Int64 => LINT,
        DataType::Uint64 => ULINT,
        DataType::Float32 => REAL,
        DataType::Float64 => LREAL,
        DataType::String => {
            return CipType { code: STRUCT, handle: Some(STRING_HANDLE) };
        }
    };
    CipType { code, handle: None }
}

/// Encode a value as `ty`.
pub fn encode_value(ty: CipType, v: &Value) -> Result<Vec<u8>, String> {
    let f = || match v {
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::Number(n) => n.as_f64().ok_or("not a number".to_string()),
        Value::String(s) => s.trim().parse::<f64>().map_err(|_| format!("`{s}` is not a number")),
        _ => Err("expected a number".to_string()),
    };
    let i = || -> Result<i128, String> {
        match v {
            Value::Number(n) if n.is_i64() => Ok(n.as_i64().unwrap() as i128),
            Value::Number(n) if n.is_u64() => Ok(n.as_u64().unwrap() as i128),
            _ => Ok(f()?.round() as i128),
        }
    };
    Ok(match ty.code {
        BOOL => vec![if f()? != 0.0 { 1 } else { 0 }],
        SINT => (i()? as i8).to_le_bytes().to_vec(),
        USINT => (i()? as u8).to_le_bytes().to_vec(),
        INT => (i()? as i16).to_le_bytes().to_vec(),
        UINT => (i()? as u16).to_le_bytes().to_vec(),
        DINT => (i()? as i32).to_le_bytes().to_vec(),
        UDINT => (i()? as u32).to_le_bytes().to_vec(),
        LINT => (i()? as i64).to_le_bytes().to_vec(),
        ULINT => (i()? as u64).to_le_bytes().to_vec(),
        REAL => (f()? as f32).to_le_bytes().to_vec(),
        LREAL => f()?.to_le_bytes().to_vec(),
        STRUCT if ty.handle == Some(STRING_HANDLE) => {
            let s = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let b = s.as_bytes();
            if b.len() > STRING_MAX {
                return Err(format!("Logix strings hold {STRING_MAX} characters"));
            }
            let mut out = (b.len() as i32).to_le_bytes().to_vec();
            out.extend_from_slice(b);
            out.resize(4 + STRING_MAX, 0);
            out
        }
        c => return Err(format!("type 0x{c:04X} isn't supported")),
    })
}

/// Decode one element of `ty` from `b`.
pub fn decode_value(ty: CipType, b: &[u8]) -> Result<Value, String> {
    fn arr<const N: usize>(b: &[u8]) -> Result<[u8; N], String> {
        b.get(..N)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| "short data".to_string())
    }
    Ok(match ty.code {
        BOOL => json!(arr::<1>(b)?[0] != 0),
        SINT => json!(i8::from_le_bytes(arr(b)?)),
        USINT => json!(u8::from_le_bytes(arr(b)?)),
        INT => json!(i16::from_le_bytes(arr(b)?)),
        UINT => json!(u16::from_le_bytes(arr(b)?)),
        DINT => json!(i32::from_le_bytes(arr(b)?)),
        UDINT => json!(u32::from_le_bytes(arr(b)?)),
        LINT => json!(i64::from_le_bytes(arr(b)?)),
        ULINT => json!(u64::from_le_bytes(arr(b)?)),
        REAL => json!(f32::from_le_bytes(arr(b)?) as f64),
        LREAL => json!(f64::from_le_bytes(arr(b)?)),
        STRUCT if ty.handle == Some(STRING_HANDLE) => {
            let n = i32::from_le_bytes(arr(b)?).clamp(0, STRING_MAX as i32) as usize;
            let data = b.get(4..4 + n).ok_or("short string")?;
            json!(String::from_utf8_lossy(data))
        }
        c => return Err(format!("type 0x{c:04X} isn't supported")),
    })
}

// ------------------------------------------------------------------ tag paths

/// Encode a symbolic tag name as a CIP path: `Program:Main.Tank[3].Level`.
pub fn tag_path(name: &str) -> Result<Vec<u8>, String> {
    let mut out = vec![];
    if name.trim().is_empty() {
        return Err("empty tag name".into());
    }
    for seg in name.split('.') {
        let (sym, idx) = match seg.find('[') {
            Some(i) => {
                let inner = seg[i..]
                    .strip_prefix('[')
                    .and_then(|s| s.strip_suffix(']'))
                    .ok_or_else(|| format!("bad index in `{seg}`"))?;
                let idx: Vec<u32> = inner
                    .split(',')
                    .map(|n| n.trim().parse::<u32>().map_err(|_| format!("bad index in `{seg}`")))
                    .collect::<Result<_, _>>()?;
                (&seg[..i], idx)
            }
            None => (seg, vec![]),
        };
        if sym.is_empty() || sym.len() > 255 {
            return Err(format!("bad tag name `{name}`"));
        }
        if !sym.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':') {
            return Err(format!("`{sym}`: tag names use letters, digits, _ and Program:"));
        }
        out.push(0x91);
        out.push(sym.len() as u8);
        out.extend_from_slice(sym.as_bytes());
        if sym.len() % 2 == 1 {
            out.push(0);
        }
        for n in idx {
            if n <= 0xFF {
                out.extend_from_slice(&[0x28, n as u8]);
            } else if n <= 0xFFFF {
                out.extend_from_slice(&[0x29, 0]);
                out.extend_from_slice(&(n as u16).to_le_bytes());
            } else {
                out.extend_from_slice(&[0x2A, 0]);
                out.extend_from_slice(&n.to_le_bytes());
            }
        }
    }
    Ok(out)
}

/// Turn a symbolic CIP path back into a tag name (the inverse of [`tag_path`]).
pub fn path_name(mut p: &[u8]) -> Option<String> {
    let mut name = String::new();
    let mut idx: Vec<u32> = vec![];
    let flush = |name: &mut String, idx: &mut Vec<u32>| {
        if !idx.is_empty() {
            name.push('[');
            name.push_str(&idx.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(","));
            name.push(']');
            idx.clear();
        }
    };
    while !p.is_empty() {
        match p[0] {
            0x91 => {
                flush(&mut name, &mut idx);
                let n = *p.get(1)? as usize;
                let sym = std::str::from_utf8(p.get(2..2 + n)?).ok()?;
                if !name.is_empty() {
                    name.push('.');
                }
                name.push_str(sym);
                p = &p[(2 + n + n % 2).min(p.len())..];
            }
            0x28 => {
                idx.push(*p.get(1)? as u32);
                p = &p[2..];
            }
            0x29 => {
                idx.push(u16::from_le_bytes(p.get(2..4)?.try_into().ok()?) as u32);
                p = &p[4..];
            }
            0x2A => {
                idx.push(u32::from_le_bytes(p.get(2..6)?.try_into().ok()?));
                p = &p[6..];
            }
            _ => return None,
        }
    }
    flush(&mut name, &mut idx);
    (!name.is_empty()).then_some(name)
}

// ------------------------------------------------------------------ encapsulation

const REGISTER_SESSION: u16 = 0x65;
const UNREGISTER_SESSION: u16 = 0x66;
const SEND_RR_DATA: u16 = 0x6F;
const LIST_IDENTITY: u16 = 0x63;
const LIST_SERVICES: u16 = 0x04;

const READ_TAG: u8 = 0x4C;
const WRITE_TAG: u8 = 0x4D;
const UNCONNECTED_SEND: u8 = 0x52;
const MULTIPLE_SERVICE: u8 = 0x0A;

/// The Connection Manager object (class 6, instance 1).
const CONNECTION_MANAGER: [u8; 4] = [0x20, 0x06, 0x24, 0x01];
/// The Message Router (class 2, instance 1).
const MESSAGE_ROUTER: [u8; 4] = [0x20, 0x02, 0x24, 0x01];

struct Encap {
    command: u16,
    session: u32,
    status: u32,
    context: [u8; 8],
    data: Vec<u8>,
}

impl Encap {
    fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 + self.data.len());
        out.extend_from_slice(&self.command.to_le_bytes());
        out.extend_from_slice(&(self.data.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.session.to_le_bytes());
        out.extend_from_slice(&self.status.to_le_bytes());
        out.extend_from_slice(&self.context);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    async fn read(sock: &mut TcpStream) -> std::io::Result<Self> {
        let mut h = [0u8; 24];
        sock.read_exact(&mut h).await?;
        let len = u16::from_le_bytes([h[2], h[3]]) as usize;
        let mut data = vec![0u8; len];
        sock.read_exact(&mut data).await?;
        Ok(Self {
            command: u16::from_le_bytes([h[0], h[1]]),
            session: u32::from_le_bytes(h[4..8].try_into().unwrap()),
            status: u32::from_le_bytes(h[8..12].try_into().unwrap()),
            context: h[12..20].try_into().unwrap(),
            data,
        })
    }
}

/// SendRRData payload around an unconnected message.
fn rr_data(msg: &[u8]) -> Vec<u8> {
    let mut d = vec![0, 0, 0, 0, 0x0A, 0x00]; // interface handle, timeout
    d.extend_from_slice(&2u16.to_le_bytes()); // item count
    d.extend_from_slice(&[0, 0, 0, 0]); // null address item
    d.extend_from_slice(&0x00B2u16.to_le_bytes());
    d.extend_from_slice(&(msg.len() as u16).to_le_bytes());
    d.extend_from_slice(msg);
    d
}

/// The unconnected data item of a SendRRData payload.
fn rr_message(d: &[u8]) -> Option<&[u8]> {
    let mut p = d.get(6..)?;
    let count = u16::from_le_bytes(p.get(..2)?.try_into().ok()?);
    p = &p[2..];
    for _ in 0..count {
        let ty = u16::from_le_bytes(p.get(..2)?.try_into().ok()?);
        let len = u16::from_le_bytes(p.get(2..4)?.try_into().ok()?) as usize;
        let body = p.get(4..4 + len)?;
        if ty == 0x00B2 {
            return Some(body);
        }
        p = &p[4 + len..];
    }
    None
}

/// A Message Router request: service, path (in words) and data.
fn mr_request(service: u8, path: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = vec![service, (path.len() / 2) as u8];
    m.extend_from_slice(path);
    m.extend_from_slice(data);
    m
}

// ------------------------------------------------------------------ client

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientSettings {
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    /// The controller's backplane slot (ControlLogix: the CPU's slot; CompactLogix: 0).
    #[serde(default)]
    slot: u8,
    /// Send requests straight to the Message Router instead of routing to `slot`
    /// (Micro800, and devices without a backplane).
    #[serde(default)]
    direct: bool,
    #[serde(default = "default_poll")]
    poll_ms: u64,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

fn default_port() -> u16 {
    44818
}
fn default_poll() -> u64 {
    1000
}
fn default_timeout() -> u64 {
    3000
}

struct Plc {
    sock: TcpStream,
    session: u32,
    slot: u8,
    direct: bool,
    timeout: Duration,
}

/// A CIP error reply.
#[derive(Debug)]
struct CipError(u8, u16);

impl std::fmt::Display for CipError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let what = match self.0 {
            0x04 => "the tag name is malformed",
            0x05 => "no such tag",
            0x06 => "the reply is too big",
            0x08 => "service not supported",
            0x0F => "permission denied",
            0x13 | 0x15 => "wrong amount of data",
            0xFF if self.1 == 0x2107 => "the data type doesn't match the tag",
            _ => "CIP error",
        };
        write!(f, "{what} (status 0x{:02X}", self.0)?;
        if self.1 != 0 {
            write!(f, ", 0x{:04X}", self.1)?;
        }
        write!(f, ")")
    }
}

impl std::error::Error for CipError {}

impl Plc {
    async fn connect(host: &str, port: u16, slot: u8, direct: bool, timeout: Duration) -> Result<Self> {
        let sock = tokio::time::timeout(timeout, TcpStream::connect((host, port)))
            .await
            .map_err(|_| anyhow!("timed out connecting to {host}:{port}"))??;
        sock.set_nodelay(true)?;
        let mut plc = Self { sock, session: 0, slot, direct, timeout };
        let reply = plc
            .exchange(Encap { command: REGISTER_SESSION, session: 0, status: 0, context: [0; 8], data: vec![1, 0, 0, 0] })
            .await?;
        plc.session = reply.session;
        Ok(plc)
    }

    async fn exchange(&mut self, e: Encap) -> Result<Encap> {
        let cmd = e.command;
        tokio::time::timeout(self.timeout, async {
            self.sock.write_all(&e.bytes()).await?;
            loop {
                let r = Encap::read(&mut self.sock).await?;
                if r.command == cmd {
                    if r.status != 0 {
                        bail!("encapsulation error 0x{:X}", r.status);
                    }
                    return Ok(r);
                }
            }
        })
        .await
        .map_err(|_| anyhow!("timed out"))?
    }

    /// Send a Message Router request, routed to the slot unless direct; return the reply data.
    async fn request(&mut self, service: u8, path: &[u8], data: &[u8]) -> Result<Vec<u8>> {
        let req = mr_request(service, path, data);
        let msg = if self.direct {
            req
        } else {
            let mut d = vec![0x0A, 0x0E]; // priority/tick, timeout ticks
            d.extend_from_slice(&(req.len() as u16).to_le_bytes());
            d.extend_from_slice(&req);
            if req.len() % 2 == 1 {
                d.push(0);
            }
            d.extend_from_slice(&[1, 0, 1, self.slot]); // route: 1 word, backplane port 1, slot
            mr_request(UNCONNECTED_SEND, &CONNECTION_MANAGER, &d)
        };
        let r = self
            .exchange(Encap { command: SEND_RR_DATA, session: self.session, status: 0, context: [0; 8], data: rr_data(&msg) })
            .await?;
        let m = rr_message(&r.data).ok_or_else(|| anyhow!("malformed reply"))?;
        if m.len() < 4 || m[0] != service | 0x80 {
            bail!("unexpected reply");
        }
        let ext_words = m[3] as usize;
        let ext = if ext_words > 0 && m.len() >= 6 { u16::from_le_bytes([m[4], m[5]]) } else { 0 };
        if m[2] != 0 && m[2] != 0x06 {
            return Err(CipError(m[2], ext).into());
        }
        Ok(m.get(4 + 2 * ext_words..).unwrap_or_default().to_vec())
    }

    async fn read_tag(&mut self, name: &str) -> Result<(CipType, Value)> {
        let path = tag_path(name).map_err(|e| anyhow!(e))?;
        let d = self.request(READ_TAG, &path, &1u16.to_le_bytes()).await?;
        if d.len() < 2 {
            bail!("short reply");
        }
        let code = u16::from_le_bytes([d[0], d[1]]);
        let (ty, rest) = if code == STRUCT {
            let h = d.get(2..4).ok_or_else(|| anyhow!("short reply"))?;
            (CipType { code, handle: Some(u16::from_le_bytes([h[0], h[1]])) }, &d[4..])
        } else {
            (CipType { code, handle: None }, &d[2..])
        };
        Ok((ty, decode_value(ty, rest).map_err(|e| anyhow!(e))?))
    }

    async fn write_tag(&mut self, name: &str, ty: CipType, v: &Value) -> Result<()> {
        let path = tag_path(name).map_err(|e| anyhow!(e))?;
        let mut d = ty.code.to_le_bytes().to_vec();
        if let Some(h) = ty.handle {
            d.extend_from_slice(&h.to_le_bytes());
        }
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&encode_value(ty, v).map_err(|e| anyhow!(e))?);
        self.request(WRITE_TAG, &path, &d).await?;
        Ok(())
    }
}

fn cip_name(t: &TagDef) -> &str {
    t.cip_tag.as_deref().unwrap_or(&t.name)
}

pub async fn client(ctx: &mut Ctx) -> Result<()> {
    let s: ClientSettings = ctx.settings()?;
    let id = ctx.def.id.clone();
    let tags = ctx.def.tags.clone();
    let timeout = Duration::from_millis(s.timeout_ms.max(100));
    let where_ = if s.direct { format!("{}:{}", s.host, s.port) } else { format!("{}:{}, slot {}", s.host, s.port, s.slot) };
    // The type each tag answered with, so writes use the controller's own type (SINT, …).
    let mut types: HashMap<String, CipType> = HashMap::new();
    let mut plc: Option<Plc> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(s.poll_ms.max(50)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ctx.status.degraded(format!("connecting to {where_}"));

    loop {
        tokio::select! {
            _ = ctx.cancel.cancelled() => return Ok(()),
            _ = tick.tick() => {
                if plc.is_none() {
                    match Plc::connect(&s.host, s.port, s.slot, s.direct, timeout).await {
                        Ok(p) => {
                            plc = Some(p);
                            ctx.status.running(format!("connected to {where_}"));
                        }
                        Err(e) => {
                            for t in &tags { ctx.tags.set_bad(&id, &t.name); }
                            ctx.status.degraded(format!("{where_}: {e:#}"));
                            continue;
                        }
                    }
                }
                let p = plc.as_mut().unwrap();
                let mut failed: Option<String> = None;
                for t in &tags {
                    match p.read_tag(cip_name(t)).await {
                        Ok((ty, v)) => {
                            types.insert(t.name.clone(), ty);
                            match coerce_tag(t, &v) {
                                Ok(v) => ctx.tags.set(&id, &t.name, v, Quality::Good, Origin::Service),
                                Err(_) => ctx.tags.set_bad(&id, &t.name),
                            }
                        }
                        Err(e) => {
                            ctx.tags.set_bad(&id, &t.name);
                            if e.downcast_ref::<CipError>().is_some() {
                                failed.get_or_insert_with(|| format!("{}: {e}", cip_name(t)));
                            } else {
                                // Socket trouble: reconnect on the next tick.
                                failed = Some(format!("{e:#}"));
                                plc = None;
                                break;
                            }
                        }
                    }
                }
                match failed {
                    Some(f) => ctx.status.degraded(format!("{where_}: {f}")),
                    None => ctx.status.running(format!("connected to {where_}")),
                }
            }
            Some(w) = ctx.writes.recv() => {
                let r = async {
                    let t = ctx.def.tag(&w.tag).ok_or_else(|| format!("no tag `{}`", w.tag))?;
                    let p = plc.as_mut().ok_or("not connected to the controller")?;
                    let ty = types.get(&t.name).copied().unwrap_or_else(|| cip_type(t.datatype));
                    p.write_tag(cip_name(t), ty, &w.value).await.map_err(|e| format!("{e:#}"))
                }.await;
                if r.is_ok() {
                    ctx.tags.set(&id, &w.tag, w.value.clone(), Quality::Good, Origin::Service);
                }
                let _ = w.reply.send(r);
            }
        }
    }
}

// ------------------------------------------------------------------ server

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerSettings {
    #[serde(default = "crate::services::any_host")]
    bind: String,
    #[serde(default = "default_port")]
    port: u16,
    /// Product name in ListIdentity.
    #[serde(default = "default_product")]
    product_name: String,
}

fn default_product() -> String {
    "Cowpoke Logix simulator".into()
}

struct Sim {
    def: ServiceDef,
    store: TagStore,
    /// Logix tag name (case-insensitive, like Logix) → Cowpoke tag.
    by_name: HashMap<String, usize>,
    product: String,
    next_session: Mutex<u32>,
}

/// A CIP reply: service | 0x80, status and data.
fn mr_reply(service: u8, status: u8, ext: Option<u16>, data: &[u8]) -> Vec<u8> {
    let mut r = vec![service | 0x80, 0, status];
    match ext {
        Some(x) => {
            r.push(1);
            r.extend_from_slice(&x.to_le_bytes());
        }
        None => r.push(0),
    }
    r.extend_from_slice(data);
    r
}

impl Sim {
    fn tag(&self, path: &[u8]) -> Option<&TagDef> {
        let name = path_name(path)?;
        self.by_name.get(&name.to_ascii_lowercase()).map(|&i| &self.def.tags[i])
    }

    /// Answer one Message Router request.
    fn handle(&self, m: &[u8]) -> Vec<u8> {
        let Some((&service, rest)) = m.split_first() else {
            return mr_reply(0, 0x04, None, &[]);
        };
        let Some((&words, rest)) = rest.split_first() else {
            return mr_reply(service, 0x04, None, &[]);
        };
        let plen = words as usize * 2;
        if rest.len() < plen {
            return mr_reply(service, 0x04, None, &[]);
        }
        let (path, data) = rest.split_at(plen);
        match service {
            READ_TAG => {
                let Some(t) = self.tag(path) else { return mr_reply(service, 0x05, Some(0), &[]) };
                let ty = cip_type(t.datatype);
                let v = self.store.value(&self.def.id, &t.name).unwrap_or(Value::Null);
                let mut out = ty.code.to_le_bytes().to_vec();
                if let Some(h) = ty.handle {
                    out.extend_from_slice(&h.to_le_bytes());
                }
                match encode_value(ty, &v) {
                    Ok(b) => out.extend_from_slice(&b),
                    Err(_) => out.extend_from_slice(&encode_value(ty, &crate::tags::zero(t.datatype)).unwrap_or_default()),
                }
                mr_reply(service, 0, None, &out)
            }
            WRITE_TAG => {
                let Some(t) = self.tag(path) else { return mr_reply(service, 0x05, Some(0), &[]) };
                if !t.writable {
                    return mr_reply(service, 0x0F, None, &[]);
                }
                let Some(code) = data.get(..2).map(|b| u16::from_le_bytes([b[0], b[1]])) else {
                    return mr_reply(service, 0x13, None, &[]);
                };
                let (ty, rest) = if code == STRUCT {
                    let Some(h) = data.get(2..4) else { return mr_reply(service, 0x13, None, &[]) };
                    (CipType { code, handle: Some(u16::from_le_bytes([h[0], h[1]])) }, &data[4..])
                } else {
                    (CipType { code, handle: None }, &data[2..])
                };
                // Like a controller: the type must match the tag's.
                let want = cip_type(t.datatype);
                let compatible = ty == want
                    || (t.datatype != DataType::String && t.datatype != DataType::Bool && ty.handle.is_none() && ty.code != BOOL);
                if !compatible {
                    return mr_reply(service, 0xFF, Some(0x2107), &[]);
                }
                let Some(elem) = rest.get(2..) else { return mr_reply(service, 0x13, None, &[]) };
                let v = match decode_value(ty, elem).and_then(|v| coerce_tag(t, &v)) {
                    Ok(v) => v,
                    Err(_) => return mr_reply(service, 0x13, None, &[]),
                };
                self.store.set(&self.def.id, &t.name, v, Quality::Good, Origin::Service);
                mr_reply(service, 0, None, &[])
            }
            UNCONNECTED_SEND if path == CONNECTION_MANAGER => {
                // priority, ticks, size, embedded request (route path ignored: we are every slot).
                let Some(n) = data.get(2..4).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize) else {
                    return mr_reply(service, 0x13, None, &[]);
                };
                match data.get(4..4 + n) {
                    Some(inner) => self.handle(inner),
                    None => mr_reply(service, 0x13, None, &[]),
                }
            }
            MULTIPLE_SERVICE if path == MESSAGE_ROUTER => {
                let count = data.get(..2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize).unwrap_or(0);
                let offs: Vec<usize> = (0..count)
                    .filter_map(|i| data.get(2 + 2 * i..4 + 2 * i).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize))
                    .collect();
                if offs.len() != count {
                    return mr_reply(service, 0x13, None, &[]);
                }
                let replies: Vec<Vec<u8>> = (0..count)
                    .map(|i| {
                        let end = offs.get(i + 1).copied().unwrap_or(data.len());
                        match data.get(offs[i]..end) {
                            Some(req) => self.handle(req),
                            None => mr_reply(0, 0x13, None, &[]),
                        }
                    })
                    .collect();
                let mut out = (count as u16).to_le_bytes().to_vec();
                let mut off = 2 + 2 * count;
                for r in &replies {
                    out.extend_from_slice(&(off as u16).to_le_bytes());
                    off += r.len();
                }
                let any_err = replies.iter().any(|r| r.get(2).is_some_and(|&s| s != 0));
                for r in replies {
                    out.extend(r);
                }
                mr_reply(service, if any_err { 0x1E } else { 0 }, None, &out)
            }
            _ => mr_reply(service, 0x08, None, &[]),
        }
    }

    fn identity(&self, local: std::net::SocketAddr) -> Vec<u8> {
        let mut item = vec![1, 0]; // encapsulation protocol version
        // Socket address, big-endian: family, port, address, zero.
        item.extend_from_slice(&2u16.to_be_bytes());
        item.extend_from_slice(&local.port().to_be_bytes());
        match local.ip() {
            std::net::IpAddr::V4(ip) => item.extend_from_slice(&ip.octets()),
            std::net::IpAddr::V6(ip) => item.extend_from_slice(&ip.to_ipv4_mapped().map(|i| i.octets()).unwrap_or([0; 4])),
        }
        item.extend_from_slice(&[0; 8]);
        item.extend_from_slice(&1u16.to_le_bytes()); // vendor: Rockwell
        item.extend_from_slice(&0x0Eu16.to_le_bytes()); // device type: PLC
        item.extend_from_slice(&0x0055u16.to_le_bytes()); // product code
        item.extend_from_slice(&[33, 11]); // revision
        item.extend_from_slice(&0u16.to_le_bytes()); // status
        item.extend_from_slice(&0x434F5750u32.to_le_bytes()); // serial: "COWP"
        let name = &self.product.as_bytes()[..self.product.len().min(32)];
        item.push(name.len() as u8);
        item.extend_from_slice(name);
        item.push(3); // state: operational
        let mut d = 1u16.to_le_bytes().to_vec();
        d.extend_from_slice(&0x000Cu16.to_le_bytes());
        d.extend_from_slice(&(item.len() as u16).to_le_bytes());
        d.extend(item);
        d
    }

    async fn serve(self: Arc<Self>, mut sock: TcpStream) -> Result<()> {
        let local = sock.local_addr()?;
        let mut session = 0u32;
        loop {
            let e = match Encap::read(&mut sock).await {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            let mut r = Encap { command: e.command, session: e.session, status: 0, context: e.context, data: vec![] };
            match e.command {
                REGISTER_SESSION => {
                    let mut n = self.next_session.lock();
                    *n = n.wrapping_add(1).max(1);
                    session = *n;
                    r.session = session;
                    r.data = vec![1, 0, 0, 0];
                }
                UNREGISTER_SESSION => return Ok(()),
                LIST_IDENTITY => r.data = self.identity(local),
                LIST_SERVICES => {
                    let mut d = 1u16.to_le_bytes().to_vec();
                    d.extend_from_slice(&0x0100u16.to_le_bytes());
                    d.extend_from_slice(&20u16.to_le_bytes());
                    d.extend_from_slice(&1u16.to_le_bytes());
                    d.extend_from_slice(&0x0020u16.to_le_bytes()); // supports CIP over TCP
                    let mut name = *b"Communications\0\0";
                    name[15] = 0;
                    d.extend_from_slice(&name);
                    r.data = d;
                }
                SEND_RR_DATA if session != 0 && e.session == session => match rr_message(&e.data) {
                    Some(m) => r.data = rr_data(&self.handle(m)),
                    None => r.status = 0x03,
                },
                SEND_RR_DATA => r.status = 0x64, // invalid session handle
                _ => r.status = 0x01, // unsupported command
            }
            sock.write_all(&r.bytes()).await?;
        }
    }
}

pub async fn server(ctx: &mut Ctx) -> Result<()> {
    let s: ServerSettings = ctx.settings()?;
    let by_name = ctx
        .def
        .tags
        .iter()
        .enumerate()
        .map(|(i, t)| (cip_name(t).to_ascii_lowercase(), i))
        .collect();
    let sim = Arc::new(Sim {
        def: ctx.def.clone(),
        store: ctx.tags.clone(),
        by_name,
        product: s.product_name.clone(),
        next_session: Mutex::new(0),
    });
    let listener = crate::services::listen(&s.bind, s.port).await?;
    ctx.status.running(format!("Logix simulator on {}", listener.local_addr()?));
    let accept = async move {
        loop {
            let (sock, addr) = listener.accept().await?;
            let _ = sock.set_nodelay(true);
            let sim = sim.clone();
            tokio::spawn(async move {
                if let Err(e) = sim.serve(sock).await {
                    tracing::debug!("eip client {addr}: {e:#}");
                }
            });
        }
        #[allow(unreachable_code)]
        Ok::<(), std::io::Error>(())
    };
    tokio::select! {
        r = accept => r.context("EtherNet/IP server stopped")?,
        _ = crate::services::idle_until_cancelled(ctx) => {}
    }
    Ok(())
}

/// Check settings without running the service.
pub fn check_settings(mode: Mode, v: &Value) -> Result<(), String> {
    let r = match mode {
        Mode::Server => serde_json::from_value::<ServerSettings>(v.clone()).map(|_| ()),
        Mode::Client => serde_json::from_value::<ClientSettings>(v.clone()).map(|_| ()),
    };
    r.map_err(|e| format!("settings: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_round_trip() {
        for n in ["Level", "Program:Main.Level", "Tank[3].Temp", "Grid[1,2]", "Big[70000]"] {
            let p = tag_path(n).unwrap();
            assert_eq!(p.len() % 2, 0, "{n}");
            assert_eq!(path_name(&p).as_deref(), Some(n));
        }
        assert!(tag_path("bad name").is_err());
    }

    #[test]
    fn values_round_trip() {
        for (dt, v) in [
            (DataType::Bool, json!(true)),
            (DataType::Int16, json!(-12)),
            (DataType::Int32, json!(123456)),
            (DataType::Int64, json!(-9_000_000_000i64)),
            (DataType::Float32, json!(1.5)),
            (DataType::Float64, json!(0.1)),
            (DataType::String, json!("Cow1")),
        ] {
            let ty = cip_type(dt);
            assert_eq!(decode_value(ty, &encode_value(ty, &v).unwrap()).unwrap(), v, "{dt:?}");
        }
        let sint = CipType { code: SINT, handle: None };
        assert_eq!(decode_value(sint, &encode_value(sint, &json!(-5)).unwrap()).unwrap(), json!(-5));
    }
}
