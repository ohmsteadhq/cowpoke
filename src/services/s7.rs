//! Siemens S7 (S7comm over ISO-on-TCP, port 102): a client for S7-300/400/1200/1500
//! PLCs (variable reads and writes, the "PUT/GET" services) and a simulated PLC
//! answering the same requests from the tag values.
//!
//! Tags take `s7_address` in Siemens notation: `DB1.DBX0.0`, `DB1.DBB2`, `DB1.DBW4`,
//! `DB1.DBD8`, `MW10`, `MD20`, `M5.3`, `I0.1` (or `E0.1`), `QW4` (or `AW4`).
//! Values are big-endian, as in the PLC. Strings are S7 `STRING`s: a max-length byte,
//! a length byte, then `length` characters.
//!
//! S7-1200/1500: the data blocks must have "optimized block access" off, and the CPU
//! must allow PUT/GET from remote partners. S7comm-plus is not spoken.

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

// ------------------------------------------------------------------ addresses

pub const AREA_I: u8 = 0x81;
pub const AREA_Q: u8 = 0x82;
pub const AREA_M: u8 = 0x83;
pub const AREA_DB: u8 = 0x84;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addr {
    pub area: u8,
    pub db: u16,
    pub byte: u32,
    pub bit: Option<u8>,
    /// Width letter from the address: X, B, W or D.
    pub width: char,
}

/// Parse Siemens notation (English or German mnemonics, any case).
pub fn parse_addr(s: &str) -> Result<Addr, String> {
    let up = s.trim().to_ascii_uppercase();
    let bad = || format!("`{s}` isn't an S7 address (like DB1.DBD0, DB1.DBX0.1, MW10, I0.1, QB4)");
    let (area, db, rest) = if let Some(r) = up.strip_prefix("DB") {
        let (n, r) = r.split_once('.').ok_or_else(bad)?;
        let db: u16 = n.parse().map_err(|_| bad())?;
        if db == 0 {
            return Err("data block numbers start at 1".into());
        }
        let r = r.strip_prefix("DB").ok_or_else(bad)?;
        (AREA_DB, db, r)
    } else {
        let (area, r) = match up.chars().next().ok_or_else(bad)? {
            'I' | 'E' => (AREA_I, &up[1..]),
            'Q' | 'A' => (AREA_Q, &up[1..]),
            'M' => (AREA_M, &up[1..]),
            _ => return Err(bad()),
        };
        (area, 0, r)
    };
    let (width, num) = match rest.chars().next().ok_or_else(bad)? {
        c @ ('X' | 'B' | 'W' | 'D') => (c, &rest[1..]),
        c if c.is_ascii_digit() => ('X', rest), // I0.1, M5.3
        _ => return Err(bad()),
    };
    let (byte, bit) = match num.split_once('.') {
        Some((b, bit)) => {
            let bit: u8 = bit.parse().map_err(|_| bad())?;
            if bit > 7 {
                return Err(format!("`{s}`: bits go 0 to 7"));
            }
            (b.parse::<u32>().map_err(|_| bad())?, Some(bit))
        }
        None => (num.parse::<u32>().map_err(|_| bad())?, None),
    };
    if width == 'X' && bit.is_none() {
        return Err(format!("`{s}`: a bit address needs .bit (like {s}.0)"));
    }
    if width != 'X' && bit.is_some() {
        return Err(format!("`{s}`: only X (bit) addresses take .bit"));
    }
    if byte >= 1 << 16 {
        return Err(format!("`{s}`: byte offset too large"));
    }
    Ok(Addr { area, db, byte, bit, width })
}

/// Bytes a tag occupies.
pub fn size_of(t: &TagDef) -> usize {
    match t.datatype {
        DataType::Bool => 1,
        DataType::Int16 | DataType::Uint16 => 2,
        DataType::Int32 | DataType::Uint32 | DataType::Float32 => 4,
        DataType::Int64 | DataType::Uint64 | DataType::Float64 => 8,
        DataType::String => 2 + t.length.unwrap_or(16).clamp(1, 254) as usize,
    }
}

/// Check a tag's address against its datatype.
pub fn check_tag(t: &TagDef) -> Result<Addr, String> {
    let s = t.s7_address.as_deref().ok_or("S7 tags need `s7_address`")?;
    let a = parse_addr(s)?;
    let n = size_of(t);
    let ok = match (t.datatype, a.width) {
        (DataType::Bool, w) => w == 'X',
        (_, 'X') => false,
        (DataType::String, w) => w == 'B',
        (_, 'B') => n == 1 || n == 8,
        (_, 'W') => n == 2,
        (_, 'D') => n == 4 || n == 8,
        _ => false,
    };
    if !ok {
        let want = match n {
            _ if t.datatype == DataType::Bool => "an X (bit) address",
            2 => "a W (word) address",
            4 => "a D (double word) address",
            _ => "a B (byte) address",
        };
        return Err(format!("`{s}`: a {:?} tag needs {want}", t.datatype).to_lowercase());
    }
    Ok(a)
}

pub fn encode(t: &TagDef, v: &Value) -> Result<Vec<u8>, String> {
    let f = || -> Result<f64, String> {
        match v {
            Value::Number(n) => n.as_f64().ok_or("not a number".into()),
            Value::Bool(b) => Ok(*b as u8 as f64),
            Value::String(s) => s.trim().parse().map_err(|_| format!("`{s}` is not a number")),
            _ => Err("expected a number".into()),
        }
    };
    let raw = |x: f64| t.to_raw(x);
    let i = || -> Result<i128, String> {
        Ok(match v {
            Value::Number(n) if n.is_i64() && !t.is_scaled() => n.as_i64().unwrap() as i128,
            Value::Number(n) if n.is_u64() && !t.is_scaled() => n.as_u64().unwrap() as i128,
            _ => raw(f()?).round() as i128,
        })
    };
    Ok(match t.datatype {
        DataType::Bool => vec![(f()? != 0.0) as u8],
        DataType::Int16 => (i()? as i16).to_be_bytes().to_vec(),
        DataType::Uint16 => (i()? as u16).to_be_bytes().to_vec(),
        DataType::Int32 => (i()? as i32).to_be_bytes().to_vec(),
        DataType::Uint32 => (i()? as u32).to_be_bytes().to_vec(),
        DataType::Int64 => (i()? as i64).to_be_bytes().to_vec(),
        DataType::Uint64 => (i()? as u64).to_be_bytes().to_vec(),
        DataType::Float32 => (raw(f()?) as f32).to_be_bytes().to_vec(),
        DataType::Float64 => raw(f()?).to_be_bytes().to_vec(),
        DataType::String => {
            let s = match v {
                Value::String(s) => s.clone(),
                o => o.to_string(),
            };
            let max = size_of(t) - 2;
            let b: Vec<u8> = s.bytes().take(max).collect();
            let mut out = vec![max as u8, b.len() as u8];
            out.extend_from_slice(&b);
            out.resize(max + 2, 0);
            out
        }
    })
}

pub fn decode(t: &TagDef, b: &[u8]) -> Result<Value, String> {
    fn arr<const N: usize>(b: &[u8]) -> Result<[u8; N], String> {
        b.get(..N).and_then(|s| s.try_into().ok()).ok_or_else(|| "short data".to_string())
    }
    let eng = |x: f64| json!(t.to_eng(x));
    let v = match t.datatype {
        DataType::Bool => json!(arr::<1>(b)?[0] != 0),
        DataType::Int16 => json!(i16::from_be_bytes(arr(b)?)),
        DataType::Uint16 => json!(u16::from_be_bytes(arr(b)?)),
        DataType::Int32 => json!(i32::from_be_bytes(arr(b)?)),
        DataType::Uint32 => json!(u32::from_be_bytes(arr(b)?)),
        DataType::Int64 => json!(i64::from_be_bytes(arr(b)?)),
        DataType::Uint64 => json!(u64::from_be_bytes(arr(b)?)),
        DataType::Float32 => json!(f32::from_be_bytes(arr(b)?) as f64),
        DataType::Float64 => json!(f64::from_be_bytes(arr(b)?)),
        DataType::String => {
            let n = (*b.get(1).ok_or("short string")? as usize).min(b.len().saturating_sub(2));
            json!(String::from_utf8_lossy(&b[2..2 + n]))
        }
    };
    if t.is_scaled() && t.datatype.is_numeric() && t.datatype != DataType::Bool {
        if let Some(x) = v.as_f64() {
            return Ok(eng(x));
        }
    }
    Ok(v)
}

// ------------------------------------------------------------------ framing

fn tpkt(payload: &[u8]) -> Vec<u8> {
    let mut out = vec![3, 0, 0, 0];
    out[2..4].copy_from_slice(&((payload.len() + 4) as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// A COTP data frame around an S7 PDU.
fn dt(s7: &[u8]) -> Vec<u8> {
    let mut p = vec![2, 0xF0, 0x80];
    p.extend_from_slice(s7);
    tpkt(&p)
}

async fn read_tpkt(sock: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut h = [0u8; 4];
    sock.read_exact(&mut h).await?;
    if h[0] != 3 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "not ISO-on-TCP"));
    }
    let n = u16::from_be_bytes([h[2], h[3]]) as usize;
    if n < 7 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "short TPKT"));
    }
    let mut b = vec![0u8; n - 4];
    sock.read_exact(&mut b).await?;
    Ok(b)
}

/// The S7 PDU inside a COTP data frame.
fn s7_of(cotp: &[u8]) -> Option<&[u8]> {
    if cotp.get(1) != Some(&0xF0) {
        return None;
    }
    cotp.get(*cotp.first()? as usize + 1..)
}

/// An S7 header. Ack-data (rosctr 3) carries an error class and code.
fn s7_header(rosctr: u8, pdu_ref: u16, param: &[u8], data: &[u8]) -> Vec<u8> {
    let mut h = vec![0x32, rosctr, 0, 0];
    h.extend_from_slice(&pdu_ref.to_be_bytes());
    h.extend_from_slice(&(param.len() as u16).to_be_bytes());
    h.extend_from_slice(&(data.len() as u16).to_be_bytes());
    if rosctr == 2 || rosctr == 3 {
        h.extend_from_slice(&[0, 0]);
    }
    h.extend_from_slice(param);
    h.extend_from_slice(data);
    h
}

fn item_spec(a: &Addr, len: usize, bit: bool) -> [u8; 12] {
    let start = a.byte * 8 + a.bit.unwrap_or(0) as u32;
    let (ts, n) = if bit { (1u8, 1u16) } else { (2u8, len as u16) };
    let db = a.db.to_be_bytes();
    let n = n.to_be_bytes();
    [0x12, 0x0A, 0x10, ts, n[0], n[1], db[0], db[1], a.area, (start >> 16) as u8, (start >> 8) as u8, start as u8]
}

/// S7 data item return codes.
fn return_code_text(c: u8) -> &'static str {
    match c {
        0x01 => "hardware fault",
        0x03 => "access to the object not allowed",
        0x05 => "address out of range",
        0x06 => "data type not supported",
        0x07 => "data type inconsistent",
        0x0A => "object doesn't exist",
        _ => "error",
    }
}

// ------------------------------------------------------------------ client

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientSettings {
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default)]
    rack: u8,
    /// S7-300: 2. S7-1200/1500: 1 (or 0). S7-400: the CPU's slot.
    #[serde(default = "default_slot")]
    slot: u8,
    /// 1 PG, 2 OP, 3 basic. Some CPUs limit PG connections.
    #[serde(default = "pg")]
    connection_type: u8,
    #[serde(default = "default_poll")]
    poll_ms: u64,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

fn default_port() -> u16 {
    102
}
fn default_slot() -> u8 {
    1
}
fn pg() -> u8 {
    1
}
fn default_poll() -> u64 {
    1000
}
fn default_timeout() -> u64 {
    3000
}

struct Plc {
    sock: TcpStream,
    pdu: usize,
    pdu_ref: u16,
    timeout: Duration,
}

impl Plc {
    async fn connect(s: &ClientSettings) -> Result<Self> {
        let timeout = Duration::from_millis(s.timeout_ms.max(100));
        let sock = tokio::time::timeout(timeout, TcpStream::connect((s.host.as_str(), s.port)))
            .await
            .map_err(|_| anyhow!("timed out connecting"))??;
        sock.set_nodelay(true)?;
        let mut plc = Self { sock, pdu: 240, pdu_ref: 0, timeout };
        // COTP connection request: our TSAP 0x0100, theirs (type, rack/slot).
        let cr = [
            17, 0xE0, 0, 0, 0, 1, 0, // length, CR, dst ref, src ref, class 0
            0xC1, 2, 1, 0, // calling TSAP
            0xC2, 2, s.connection_type, (s.rack << 5) | (s.slot & 0x1F), // called TSAP
            0xC0, 1, 0x0A, // TPDU size 1024
        ];
        plc.sock.write_all(&tpkt(&cr)).await?;
        let cc = tokio::time::timeout(timeout, read_tpkt(&mut plc.sock)).await.map_err(|_| anyhow!("timed out"))??;
        if cc.get(1) != Some(&0xD0) {
            bail!("the PLC refused the connection (rack {}, slot {}): check rack and slot", s.rack, s.slot);
        }
        // Setup communication: ask for 480-byte PDUs.
        let param = [0xF0, 0, 0, 1, 0, 1, 0x01, 0xE0];
        let r = plc.exchange(1, &param, &[]).await?;
        let p = r.param;
        if p.len() >= 8 {
            plc.pdu = u16::from_be_bytes([p[6], p[7]]).clamp(240, 960) as usize;
        }
        Ok(plc)
    }

    async fn exchange(&mut self, rosctr: u8, param: &[u8], data: &[u8]) -> Result<Reply> {
        self.pdu_ref = self.pdu_ref.wrapping_add(1);
        let req = dt(&s7_header(rosctr, self.pdu_ref, param, data));
        let timeout = self.timeout;
        tokio::time::timeout(timeout, async {
            self.sock.write_all(&req).await?;
            loop {
                let cotp = read_tpkt(&mut self.sock).await?;
                let Some(s7) = s7_of(&cotp) else { continue };
                if s7.first() != Some(&0x32) || s7.len() < 12 {
                    bail!("malformed S7 reply");
                }
                let (class, code) = (s7[10], s7[11]);
                if class != 0 || code != 0 {
                    bail!("the PLC rejected the request (error class 0x{class:02X}, code 0x{code:02X})");
                }
                let pl = u16::from_be_bytes([s7[6], s7[7]]) as usize;
                let dl = u16::from_be_bytes([s7[8], s7[9]]) as usize;
                let param = s7.get(12..12 + pl).ok_or_else(|| anyhow!("short reply"))?.to_vec();
                let data = s7.get(12 + pl..12 + pl + dl).ok_or_else(|| anyhow!("short reply"))?.to_vec();
                return Ok(Reply { param, data });
            }
        })
        .await
        .map_err(|_| anyhow!("timed out"))?
    }

    /// Read several items in one request; per item, its bytes or an error.
    async fn read(&mut self, items: &[(Addr, usize, bool)]) -> Result<Vec<Result<Vec<u8>, String>>> {
        let mut param = vec![0x04, items.len() as u8];
        for (a, n, bit) in items {
            param.extend_from_slice(&item_spec(a, *n, *bit));
        }
        let r = self.exchange(1, &param, &[]).await?;
        let mut out = vec![];
        let mut d = &r.data[..];
        for _ in items {
            if d.len() < 4 {
                bail!("short read reply");
            }
            let (rc, ts) = (d[0], d[1]);
            let len = u16::from_be_bytes([d[2], d[3]]) as usize;
            // Transport sizes 3, 4 and 5 count bits; 7 and 9 count bytes.
            let n = if matches!(ts, 3 | 4 | 5) { len.div_ceil(8) } else { len };
            if rc != 0xFF {
                out.push(Err(format!("{} (0x{rc:02X})", return_code_text(rc))));
                d = &d[4.min(d.len())..];
                continue;
            }
            let bytes = d.get(4..4 + n).ok_or_else(|| anyhow!("short read reply"))?.to_vec();
            out.push(Ok(bytes));
            let step = 4 + n + (n % 2);
            d = d.get(step.min(d.len())..).unwrap_or_default();
        }
        Ok(out)
    }

    async fn write(&mut self, a: &Addr, bytes: &[u8], bit: bool) -> Result<()> {
        let mut param = vec![0x05, 1];
        param.extend_from_slice(&item_spec(a, bytes.len(), bit));
        let (ts, len) = if bit { (3u8, 1u16) } else { (4u8, (bytes.len() * 8) as u16) };
        let mut data = vec![0, ts];
        data.extend_from_slice(&len.to_be_bytes());
        data.extend_from_slice(bytes);
        let r = self.exchange(1, &param, &data).await?;
        match r.data.first() {
            Some(0xFF) => Ok(()),
            Some(&rc) => bail!("{} (0x{rc:02X})", return_code_text(rc)),
            None => bail!("short write reply"),
        }
    }
}

struct Reply {
    param: Vec<u8>,
    data: Vec<u8>,
}

pub async fn client(ctx: &mut Ctx) -> Result<()> {
    let s: ClientSettings = ctx.settings()?;
    let id = ctx.def.id.clone();
    let tags: Vec<(TagDef, Addr)> = ctx
        .def
        .tags
        .iter()
        .map(|t| check_tag(t).map(|a| (t.clone(), a)).map_err(|e| anyhow!("tag `{}`: {e}", t.name)))
        .collect::<Result<_>>()?;
    let where_ = format!("{}:{}, rack {} slot {}", s.host, s.port, s.rack, s.slot);
    let mut plc: Option<Plc> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(s.poll_ms.max(50)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ctx.status.degraded(format!("connecting to {where_}"));
    loop {
        tokio::select! {
            _ = ctx.cancel.cancelled() => return Ok(()),
            _ = tick.tick() => {
                if plc.is_none() {
                    match Plc::connect(&s).await {
                        Ok(p) => {
                            ctx.status.running(format!("connected to {where_}, PDU {} bytes", p.pdu));
                            plc = Some(p);
                        }
                        Err(e) => {
                            for (t, _) in &tags { ctx.tags.set_bad(&id, &t.name); }
                            ctx.status.degraded(format!("{where_}: {e:#}"));
                            continue;
                        }
                    }
                }
                let p = plc.as_mut().unwrap();
                // Batch items while the request and reply fit the PDU.
                let mut failed: Option<String> = None;
                let mut i = 0;
                while i < tags.len() {
                    let (mut req, mut rep, mut j) = (19usize, 14usize, i);
                    while j < tags.len() && j - i < 19 {
                        let n = size_of(&tags[j].0);
                        if req + 12 > p.pdu || rep + 4 + n + 1 > p.pdu { break; }
                        req += 12;
                        rep += 4 + n + 1;
                        j += 1;
                    }
                    let j = j.max(i + 1);
                    let items: Vec<(Addr, usize, bool)> = tags[i..j].iter()
                        .map(|(t, a)| (*a, size_of(t), t.datatype == DataType::Bool)).collect();
                    match p.read(&items).await {
                        Ok(res) => {
                            for ((t, a), r) in tags[i..j].iter().zip(res) {
                                match r.and_then(|b| decode(t, &b)).and_then(|v| coerce_tag(t, &v)) {
                                    Ok(v) => ctx.tags.set(&id, &t.name, v, Quality::Good, Origin::Service),
                                    Err(e) => {
                                        ctx.tags.set_bad(&id, &t.name);
                                        failed.get_or_insert_with(|| format!("{}: {e}", t.s7_address.as_deref().unwrap_or("?")));
                                        let _ = a;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            for (t, _) in &tags { ctx.tags.set_bad(&id, &t.name); }
                            failed = Some(format!("{e:#}"));
                            plc = None;
                            break;
                        }
                    }
                    i = j;
                }
                match failed {
                    Some(f) => ctx.status.degraded(format!("{where_}: {f}")),
                    None => {
                        let pdu = plc.as_ref().map(|p| p.pdu).unwrap_or(0);
                        ctx.status.running(format!("connected to {where_}, PDU {pdu} bytes"));
                    }
                }
            }
            Some(w) = ctx.writes.recv() => {
                let r = async {
                    let (t, a) = tags.iter().find(|(t, _)| t.name == w.tag).ok_or_else(|| format!("no tag `{}`", w.tag))?;
                    let p = plc.as_mut().ok_or("not connected to the PLC")?;
                    let bytes = encode(t, &w.value)?;
                    p.write(a, &bytes, t.datatype == DataType::Bool).await.map_err(|e| format!("{e:#}"))
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
    /// Largest PDU offered in setup communication (S7-300: 240, S7-1500: 960).
    #[serde(default = "default_pdu")]
    pdu_size: u16,
}

fn default_pdu() -> u16 {
    480
}

struct Sim {
    def: ServiceDef,
    store: TagStore,
    tags: Vec<(TagDef, Addr)>,
    pdu: u16,
    /// Bytes no tag covers keep what was last written to them, like PLC memory.
    memory: Mutex<HashMap<(u8, u16), Vec<u8>>>,
}

impl Sim {
    /// Memory of one area as the tags currently say, over the given span.
    fn image(&self, area: u8, db: u16, start: usize, len: usize) -> Vec<u8> {
        let mut mem = self.memory.lock();
        let m = mem.entry((area, db)).or_default();
        if m.len() < start + len {
            m.resize(start + len, 0);
        }
        for (t, a) in &self.tags {
            if a.area != area || a.db != db {
                continue;
            }
            let v = self.store.value(&self.def.id, &t.name).unwrap_or_else(|| crate::tags::zero(t.datatype));
            let Ok(b) = encode(t, &v) else { continue };
            let at = a.byte as usize;
            if m.len() < at + b.len() {
                m.resize(at + b.len(), 0);
            }
            if let Some(bit) = a.bit {
                if b[0] != 0 { m[at] |= 1 << bit } else { m[at] &= !(1 << bit) }
            } else {
                m[at..at + b.len()].copy_from_slice(&b);
            }
        }
        m[start..start + len].to_vec()
    }

    fn known(&self, area: u8, db: u16) -> bool {
        area != AREA_DB || self.tags.iter().any(|(_, a)| a.area == AREA_DB && a.db == db)
    }

    /// Write bytes (or one bit) and update the tags they cover.
    fn apply(&self, area: u8, db: u16, start_bit: u32, bytes: &[u8], bit: bool) -> u8 {
        let start = (start_bit / 8) as usize;
        let span = if bit { 1 } else { bytes.len() };
        let mut cur = self.image(area, db, start, span);
        if bit {
            let b = (start_bit % 8) as u8;
            if bytes.first().copied().unwrap_or(0) != 0 { cur[0] |= 1 << b } else { cur[0] &= !(1 << b) }
        } else {
            cur.copy_from_slice(bytes);
        }
        // Writing to a read-only tag is refused, like a write-protected area.
        for (t, a) in &self.tags {
            if a.area == area && a.db == db {
                let at = a.byte as usize;
                let overlaps = at < start + span && start < at + size_of(t);
                if overlaps && !t.writable {
                    return 0x03;
                }
            }
        }
        {
            let mut mem = self.memory.lock();
            let m = mem.entry((area, db)).or_default();
            m[start..start + span].copy_from_slice(&cur);
        }
        for (t, a) in &self.tags {
            if a.area != area || a.db != db {
                continue;
            }
            let at = a.byte as usize;
            let n = size_of(t);
            if at >= start + span || start >= at + n {
                continue;
            }
            let m = self.memory.lock().get(&(area, db)).cloned().unwrap_or_default();
            let raw: Vec<u8> = match a.bit {
                Some(b) => vec![(m.get(at).copied().unwrap_or(0) >> b) & 1],
                None => m.get(at..at + n).map(|s| s.to_vec()).unwrap_or_default(),
            };
            if let Ok(v) = decode(t, &raw).and_then(|v| coerce_tag(t, &v)) {
                if self.store.value(&self.def.id, &t.name).as_ref() != Some(&v) {
                    self.store.set(&self.def.id, &t.name, v, Quality::Good, Origin::Service);
                }
            }
        }
        0xFF
    }

    fn handle(&self, s7: &[u8]) -> Option<Vec<u8>> {
        if s7.first() != Some(&0x32) || s7.len() < 10 {
            return None;
        }
        let rosctr = s7[1];
        let pdu_ref = u16::from_be_bytes([s7[4], s7[5]]);
        let pl = u16::from_be_bytes([s7[6], s7[7]]) as usize;
        let dl = u16::from_be_bytes([s7[8], s7[9]]) as usize;
        let param = s7.get(10..10 + pl)?;
        let data = s7.get(10 + pl..10 + pl + dl).unwrap_or_default();
        if rosctr == 7 {
            // Userdata (SZL reads, block lists, clock): not simulated. Reply with "not available".
            let mut p = param.to_vec();
            if p.len() >= 8 {
                p[4] = 0x12;
                p[5] = (p[5] & 0x0F) | 0x80;
            }
            return Some(s7_header(7, pdu_ref, &p, &[0x0A, 0, 0, 0]));
        }
        if rosctr != 1 {
            return None;
        }
        match param.first()? {
            0xF0 => {
                let mut p = param.to_vec();
                if p.len() >= 8 {
                    let want = u16::from_be_bytes([p[6], p[7]]).min(self.pdu);
                    p[6..8].copy_from_slice(&want.to_be_bytes());
                }
                Some(s7_header(3, pdu_ref, &p, &[]))
            }
            0x04 => {
                let count = *param.get(1)? as usize;
                let mut out = vec![];
                for i in 0..count {
                    let it = param.get(2 + i * 12..14 + i * 12)?;
                    let (ts, n) = (it[3], u16::from_be_bytes([it[4], it[5]]) as usize);
                    let db = u16::from_be_bytes([it[6], it[7]]);
                    let area = it[8];
                    let start_bit = u32::from_be_bytes([0, it[9], it[10], it[11]]);
                    let last = i + 1 == count;
                    if !self.known(area, db) || ![AREA_I, AREA_Q, AREA_M, AREA_DB].contains(&area) {
                        out.extend_from_slice(&[0x0A, 0, 0, 0]);
                        continue;
                    }
                    if ts == 1 {
                        let byte = self.image(area, db, (start_bit / 8) as usize, 1)[0];
                        out.extend_from_slice(&[0xFF, 3, 0, 1, (byte >> (start_bit % 8)) & 1]);
                        if !last { out.push(0); }
                    } else {
                        let bytes = self.image(area, db, (start_bit / 8) as usize, n);
                        out.extend_from_slice(&[0xFF, 4]);
                        out.extend_from_slice(&((n * 8) as u16).to_be_bytes());
                        out.extend_from_slice(&bytes);
                        if n % 2 == 1 && !last { out.push(0); }
                    }
                }
                Some(s7_header(3, pdu_ref, &[0x04, count as u8], &out))
            }
            0x05 => {
                let count = *param.get(1)? as usize;
                let mut codes = vec![];
                let mut d = data;
                for i in 0..count {
                    let it = param.get(2 + i * 12..14 + i * 12)?;
                    let db = u16::from_be_bytes([it[6], it[7]]);
                    let area = it[8];
                    let start_bit = u32::from_be_bytes([0, it[9], it[10], it[11]]);
                    let ts = *d.get(1)?;
                    let len = u16::from_be_bytes([*d.get(2)?, *d.get(3)?]) as usize;
                    let n = if matches!(ts, 3 | 4 | 5) { len.div_ceil(8) } else { len };
                    let bytes = d.get(4..4 + n)?;
                    let code = if !self.known(area, db) || ![AREA_I, AREA_Q, AREA_M, AREA_DB].contains(&area) {
                        0x0A
                    } else {
                        self.apply(area, db, start_bit, bytes, ts == 3)
                    };
                    codes.push(code);
                    d = d.get((4 + n + n % 2).min(d.len())..).unwrap_or_default();
                }
                Some(s7_header(3, pdu_ref, &[0x05, count as u8], &codes))
            }
            // Program transfer, run/stop: refused, like a CPU protected by a password.
            _ => {
                let mut r = s7_header(3, pdu_ref, &param[..1], &[]);
                r[10] = 0xD6; // error class: access denied (PI service / protection)
                r[11] = 0x02;
                Some(r)
            }
        }
    }

    async fn serve(self: Arc<Self>, mut sock: TcpStream) -> Result<()> {
        loop {
            let cotp = match read_tpkt(&mut sock).await {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            match cotp.get(1) {
                Some(0xE0) => {
                    // Connection confirm: echo the parameters, swap the references.
                    let mut cc = cotp.clone();
                    cc[1] = 0xD0;
                    cc[2..4].copy_from_slice(&cotp[4..6]);
                    cc[4..6].copy_from_slice(&[0, 1]);
                    sock.write_all(&tpkt(&cc)).await?;
                }
                Some(0xF0) => {
                    if let Some(reply) = s7_of(&cotp).and_then(|s7| self.handle(s7)) {
                        sock.write_all(&dt(&reply)).await?;
                    }
                }
                Some(0x80) => return Ok(()), // disconnect request
                _ => {}
            }
        }
    }
}

pub async fn server(ctx: &mut Ctx) -> Result<()> {
    let s: ServerSettings = ctx.settings()?;
    let tags: Vec<(TagDef, Addr)> = ctx
        .def
        .tags
        .iter()
        .map(|t| check_tag(t).map(|a| (t.clone(), a)).map_err(|e| anyhow!("tag `{}`: {e}", t.name)))
        .collect::<Result<_>>()?;
    let sim = Arc::new(Sim {
        def: ctx.def.clone(),
        store: ctx.tags.clone(),
        tags,
        pdu: s.pdu_size.clamp(240, 960),
        memory: Mutex::new(HashMap::new()),
    });
    let listener = crate::services::listen(&s.bind, s.port).await?;
    ctx.status.running(format!("S7 PLC simulator on {}", listener.local_addr()?));
    let accept = async move {
        loop {
            let (sock, addr) = listener.accept().await?;
            let _ = sock.set_nodelay(true);
            let sim = sim.clone();
            tokio::spawn(async move {
                if let Err(e) = sim.serve(sock).await {
                    tracing::debug!("s7 client {addr}: {e:#}");
                }
            });
        }
        #[allow(unreachable_code)]
        Ok::<(), std::io::Error>(())
    };
    tokio::select! {
        r = accept => r.context("S7 server stopped")?,
        _ = crate::services::idle_until_cancelled(ctx) => {}
    }
    Ok(())
}

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
    fn addresses() {
        let a = parse_addr("DB10.DBX4.3").unwrap();
        assert_eq!((a.area, a.db, a.byte, a.bit, a.width), (AREA_DB, 10, 4, Some(3), 'X'));
        assert_eq!(parse_addr("mw20").unwrap().width, 'W');
        assert_eq!(parse_addr("E0.1").unwrap().area, AREA_I);
        assert_eq!(parse_addr("AW4").unwrap().area, AREA_Q);
        assert!(parse_addr("DB1.DBX4").is_err());
        assert!(parse_addr("MW4.1").is_err());
        assert!(parse_addr("DB0.DBW0").is_err());
        let t = TagDef { name: "x".into(), datatype: DataType::Float32, s7_address: Some("DB1.DBW0".into()), ..Default::default() };
        assert!(check_tag(&t).is_err());
    }

    #[test]
    fn values_round_trip() {
        for (dt, v) in [(DataType::Float32, json!(1.5)), (DataType::Int16, json!(-7)), (DataType::Uint64, json!(123456789012u64)),
                        (DataType::Bool, json!(true)), (DataType::String, json!("Cow1"))] {
            let t = TagDef { name: "x".into(), datatype: dt, length: Some(10), ..Default::default() };
            assert_eq!(decode(&t, &encode(&t, &v).unwrap()).unwrap(), v, "{dt:?}");
        }
    }
}
