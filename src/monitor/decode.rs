//! Protocol decoders for passive monitoring: each turns one direction of a TCP stream
//! into observations (reads, writes, connects, program and control actions, errors).
//! They frame PDUs by the protocol's own length field and consume what they decode.

use serde::Serialize;

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Proto {
    Modbus,
    Enip,
    S7,
    Opcua,
    Mqtt,
}

impl Proto {
    pub fn default_port(self) -> u16 {
        match self {
            Proto::Modbus => 502,
            Proto::Enip => 44818,
            Proto::S7 => 102,
            Proto::Opcua => 4840,
            Proto::Mqtt => 1883,
        }
    }
    pub const ALL: [Proto; 5] = [Proto::Modbus, Proto::Enip, Proto::S7, Proto::Opcua, Proto::Mqtt];
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Connect,
    Login,
    Read,
    Write,
    /// Program or configuration transfer: downloads, uploads, address-space changes.
    Program,
    /// Run/stop, reset, method calls.
    Control,
    /// Discovery and diagnostics: identity, device info, system lists.
    Diagnostic,
    /// The device answered with an error or refused the request.
    Error,
    Other,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Obs {
    pub action: Action,
    /// What, in plain words: "write holding registers 100-103 (unit 1)".
    pub what: String,
}

fn obs(action: Action, what: impl Into<String>) -> Obs {
    Obs { action, what: what.into() }
}

fn be16(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(i..i + 2)?.try_into().ok()?))
}
fn le16(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?))
}
fn le32(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

/// Decode as many whole PDUs as `buf` holds. Returns observations and bytes consumed;
/// `None` means the stream doesn't look like this protocol (drop it).
#[cfg(test)]
pub fn decode(proto: Proto, to_server: bool, buf: &[u8]) -> Option<(Vec<Obs>, usize)> {
    decode_counted(proto, to_server, buf).map(|(o, used, _)| (o, used))
}

/// The id an answer echoes from its request, where the protocol has one: the Modbus
/// transaction id, the S7 PDU reference, the EtherNet/IP sender context.
fn pdu_id(proto: Proto, p: &[u8]) -> Option<u64> {
    match proto {
        Proto::Modbus => be16(p, 0).map(u64::from),
        Proto::Enip => p.get(12..20).map(|c| u64::from_le_bytes(c.try_into().unwrap())),
        Proto::S7 => {
            let cl = *p.get(4)? as usize;
            let s7 = p.get(5 + cl..)?;
            (s7.first() == Some(&0x32)).then(|| be16(s7, 4)).flatten().map(u64::from)
        }
        _ => None,
    }
}

/// Like [`decode`], also giving each whole PDU's request id (for response timing).
pub fn decode_counted(proto: Proto, to_server: bool, buf: &[u8]) -> Option<(Vec<Obs>, usize, Vec<Option<u64>>)> {
    let mut out = vec![];
    let mut at = 0;
    let mut pdus = vec![];
    while at < buf.len() {
        let rest = &buf[at..];
        let step = match proto {
            Proto::Modbus => modbus(to_server, rest, &mut out)?,
            Proto::Enip => enip(to_server, rest, &mut out)?,
            Proto::S7 => s7(to_server, rest, &mut out)?,
            Proto::Opcua => opcua(to_server, rest, &mut out)?,
            Proto::Mqtt => mqtt(to_server, rest, &mut out)?,
        };
        if step == 0 {
            break; // incomplete
        }
        pdus.push(pdu_id(proto, rest));
        at += step;
    }
    Some((out, at, pdus))
}

impl Proto {
    /// Answers every request, in order: responses can be timed against requests.
    pub fn request_response(self) -> bool {
        matches!(self, Proto::Modbus | Proto::Enip | Proto::S7)
    }
}

// ------------------------------------------------------------------ Modbus TCP

fn modbus(to_server: bool, b: &[u8], out: &mut Vec<Obs>) -> Option<usize> {
    if b.len() < 8 {
        return Some(0);
    }
    if be16(b, 2)? != 0 {
        return None; // protocol id is always 0
    }
    let len = be16(b, 4)? as usize;
    if !(2..=254).contains(&len) {
        return None;
    }
    if b.len() < 6 + len {
        return Some(0);
    }
    let unit = b[6];
    let fc = b[7];
    let d = &b[8..6 + len];
    let range = |a: u16, n: u16| if n <= 1 { format!("{a}") } else { format!("{a}-{}", a as u32 + n as u32 - 1) };
    if !to_server {
        if fc & 0x80 != 0 {
            let code = d.first().copied().unwrap_or(0);
            let why = match code {
                1 => "illegal function",
                2 => "illegal data address",
                3 => "illegal data value",
                4 => "device failure",
                6 => "device busy",
                0x0B => "gateway target didn't respond",
                _ => "exception",
            };
            out.push(obs(Action::Error, format!("exception {code:02X} ({why}) to function {} (unit {unit})", fc & 0x7F)));
        }
        return Some(6 + len);
    }
    let a = be16(d, 0).unwrap_or(0);
    let n = be16(d, 2).unwrap_or(0);
    out.push(match fc {
        1 => obs(Action::Read, format!("read coils {} (unit {unit})", range(a, n))),
        2 => obs(Action::Read, format!("read discrete inputs {} (unit {unit})", range(a, n))),
        3 => obs(Action::Read, format!("read holding registers {} (unit {unit})", range(a, n))),
        4 => obs(Action::Read, format!("read input registers {} (unit {unit})", range(a, n))),
        5 => obs(Action::Write, format!("write coil {a} (unit {unit})")),
        6 => obs(Action::Write, format!("write holding register {a} = {n} (unit {unit})")),
        15 => obs(Action::Write, format!("write coils {} (unit {unit})", range(a, n))),
        16 => obs(Action::Write, format!("write holding registers {} (unit {unit})", range(a, n))),
        22 => obs(Action::Write, format!("mask write register {a} (unit {unit})")),
        23 => obs(Action::Write, format!("read/write registers, writing {} (unit {unit})",
            range(be16(d, 4).unwrap_or(0), be16(d, 6).unwrap_or(0)))),
        8 => obs(Action::Diagnostic, format!("diagnostics sub-function {} (unit {unit})", a)),
        17 => obs(Action::Diagnostic, format!("report server id (unit {unit})")),
        43 => obs(Action::Diagnostic, format!("read device identification (unit {unit})")),
        // Schneider UMAS rides on function 90: program transfer and run/stop.
        90 => match d.get(1) {
            Some(0x30..=0x32) => obs(Action::Program, format!("UMAS program download (unit {unit})")),
            Some(0x33..=0x35) => obs(Action::Program, format!("UMAS program upload (unit {unit})")),
            Some(0x40) => obs(Action::Control, format!("UMAS start PLC (unit {unit})")),
            Some(0x41) => obs(Action::Control, format!("UMAS stop PLC (unit {unit})")),
            Some(0x10) => obs(Action::Login, format!("UMAS take reservation (unit {unit})")),
            _ => obs(Action::Other, format!("UMAS function (unit {unit})")),
        },
        f => obs(Action::Other, format!("function {f} (unit {unit})")),
    });
    Some(6 + len)
}

// ------------------------------------------------------------------ EtherNet/IP + CIP

fn cip_path(p: &[u8]) -> String {
    // Logical class/instance/attribute or a symbolic tag name.
    if let Some(name) = crate::services::eip::path_name(p) {
        return name;
    }
    let mut parts = vec![];
    let mut i = 0;
    while i + 1 < p.len() {
        let (kind, v, step) = match p[i] {
            0x20 => ("class", p[i + 1] as u32, 2),
            0x21 => ("class", le16(p, i + 2).unwrap_or(0) as u32, 4),
            0x24 => ("instance", p[i + 1] as u32, 2),
            0x25 => ("instance", le16(p, i + 2).unwrap_or(0) as u32, 4),
            0x30 => ("attribute", p[i + 1] as u32, 2),
            _ => break,
        };
        parts.push(format!("{kind} 0x{v:X}"));
        i += step;
    }
    parts.join(" ")
}

fn path_class(p: &[u8]) -> Option<u32> {
    match p.first()? {
        0x20 => Some(*p.get(1)? as u32),
        0x21 => le16(p, 2).map(|v| v as u32),
        _ => None,
    }
}

/// One CIP request (Message Router format).
fn cip_request(m: &[u8], out: &mut Vec<Obs>, depth: u8) {
    let (Some(&service), Some(&words)) = (m.first(), m.get(1)) else { return };
    let plen = words as usize * 2;
    let Some(path) = m.get(2..2 + plen) else { return };
    let data = &m[2 + plen..];
    let class = path_class(path);
    let target = cip_path(path);
    out.push(match (service, class) {
        (0x52, Some(6)) => {
            // Unconnected Send: unwrap the embedded request.
            if let (Some(n), true) = (le16(data, 2), depth < 3) {
                if let Some(inner) = data.get(4..4 + n as usize) {
                    cip_request(inner, out, depth + 1);
                }
            }
            return;
        }
        (0x0A, Some(2)) => {
            let count = le16(data, 0).unwrap_or(0) as usize;
            let offs: Vec<usize> = (0..count).filter_map(|i| le16(data, 2 + 2 * i).map(|v| v as usize)).collect();
            for (i, &o) in offs.iter().enumerate() {
                let end = offs.get(i + 1).copied().unwrap_or(data.len());
                if let (Some(r), true) = (data.get(o..end), depth < 3) {
                    cip_request(r, out, depth + 1);
                }
            }
            return;
        }
        (0x54 | 0x5B, Some(6)) => obs(Action::Connect, "Forward Open (connected messaging)"),
        (0x4E, Some(6)) => obs(Action::Connect, "Forward Close"),
        (0x4C, _) => obs(Action::Read, format!("read tag {target}")),
        (0x52, _) => obs(Action::Read, format!("read tag {target} (fragmented)")),
        (0x4D, _) => obs(Action::Write, format!("write tag {target}")),
        (0x53, _) => obs(Action::Write, format!("write tag {target} (fragmented)")),
        (0x4E, _) => obs(Action::Write, format!("read-modify-write tag {target}")),
        (0x55, _) => obs(Action::Diagnostic, "list tags (browse)"),
        (0x01 | 0x0E, Some(1)) => obs(Action::Diagnostic, format!("read identity ({target})")),
        (0x01 | 0x0E | 0x03, _) => obs(Action::Read, format!("get attribute ({target})")),
        (0x02 | 0x04 | 0x10, _) => obs(Action::Write, format!("set attribute ({target})")),
        (0x05, _) => obs(Action::Control, format!("reset ({target})")),
        (0x06, _) => obs(Action::Control, format!("start ({target})")),
        (0x07, _) => obs(Action::Control, format!("stop ({target})")),
        // The File object (class 0x37) carries firmware and program files.
        (_, Some(0x37)) => obs(Action::Program, format!("file object service 0x{service:02X} ({target})")),
        (s, _) => obs(Action::Other, format!("CIP service 0x{s:02X} ({target})")),
    });
}

fn cip_reply(m: &[u8], out: &mut Vec<Obs>) {
    let (Some(&service), Some(&status)) = (m.first(), m.get(2)) else { return };
    if service & 0x80 == 0 || status == 0 || status == 0x06 {
        return;
    }
    if service & 0x7F == 0x0A && status == 0x1E {
        return; // batch with a failure inside: the inner replies aren't parsed
    }
    let why = match status {
        0x04 => "malformed path",
        0x05 => "no such object or tag",
        0x08 => "service not supported",
        0x0F => "permission denied",
        0x13 | 0x15 => "wrong amount of data",
        0xFF => "vendor-specific error",
        _ => "error",
    };
    out.push(obs(Action::Error, format!("CIP status 0x{status:02X} ({why}) to service 0x{:02X}", service & 0x7F)));
}

fn enip(to_server: bool, b: &[u8], out: &mut Vec<Obs>) -> Option<usize> {
    if b.len() < 24 {
        return Some(0);
    }
    let cmd = le16(b, 0)?;
    if !matches!(cmd, 0x0001 | 0x0004 | 0x0063 | 0x0064 | 0x0065 | 0x0066 | 0x006F | 0x0070) {
        return None;
    }
    let len = le16(b, 2)? as usize;
    if b.len() < 24 + len {
        return Some(0);
    }
    let d = &b[24..24 + len];
    let status = le32(b, 8)?;
    match cmd {
        0x0065 if to_server => out.push(obs(Action::Connect, "register session")),
        0x0063 | 0x0004 if to_server => out.push(obs(Action::Diagnostic, "list identity / services (discovery)")),
        0x006F | 0x0070 => {
            // CPF items after interface handle (4) and timeout (2).
            let mut p = d.get(6..).unwrap_or_default();
            let count = le16(p, 0).unwrap_or(0);
            p = p.get(2..).unwrap_or_default();
            for _ in 0..count {
                let (Some(ty), Some(n)) = (le16(p, 0), le16(p, 2)) else { break };
                let body = p.get(4..4 + n as usize).unwrap_or_default();
                let msg = match ty {
                    0x00B2 => Some(body),
                    0x00B1 => body.get(2..), // connected: 2-byte sequence count first
                    _ => None,
                };
                if let Some(m) = msg {
                    if to_server { cip_request(m, out, 0) } else { cip_reply(m, out) }
                }
                p = p.get(4 + n as usize..).unwrap_or_default();
            }
        }
        _ => {}
    }
    if !to_server && status != 0 {
        out.push(obs(Action::Error, format!("encapsulation status 0x{status:X}")));
    }
    Some(24 + len)
}

// ------------------------------------------------------------------ S7comm

fn s7_area(area: u8, db: u16, addr: u32, size: u8, count: u16) -> String {
    let byte = addr >> 3;
    let bit = addr & 7;
    let (prefix, letter) = match area {
        0x81 => ("I".to_string(), ""),
        0x82 => ("Q".to_string(), ""),
        0x83 => ("M".to_string(), ""),
        0x84 => (format!("DB{db}.DB"), ""),
        0x1C => ("C".to_string(), ""),
        0x1D => ("T".to_string(), ""),
        a => (format!("area 0x{a:02X} "), ""),
    };
    let _ = letter;
    let unit = match size {
        1 => format!("X{byte}.{bit}"),
        2 => format!("B{byte}"),
        4 | 5 => format!("W{byte}"),
        6 | 7 | 8 => format!("D{byte}"),
        _ => format!("B{byte}"),
    };
    let n = if count > 1 { format!(" ×{count}") } else { String::new() };
    format!("{prefix}{unit}{n}")
}

fn s7_items(param: &[u8]) -> String {
    let count = *param.get(1).unwrap_or(&0) as usize;
    let mut items = vec![];
    for i in 0..count.min(8) {
        let it = param.get(2 + i * 12..2 + i * 12 + 12).unwrap_or_default();
        if it.len() == 12 && it[0] == 0x12 && it[2] == 0x10 {
            let size = it[3];
            let n = be16(it, 4).unwrap_or(1);
            let db = be16(it, 6).unwrap_or(0);
            let addr = u32::from_be_bytes([0, it[9], it[10], it[11]]);
            items.push(s7_area(it[8], db, addr, size, n));
        }
    }
    if count > 8 {
        items.push(format!("and {} more", count - 8));
    }
    items.join(", ")
}

fn s7(to_server: bool, b: &[u8], out: &mut Vec<Obs>) -> Option<usize> {
    if b.len() < 4 {
        return Some(0);
    }
    if b[0] != 3 || b[1] != 0 {
        return None; // TPKT version 3
    }
    let len = be16(b, 2)? as usize;
    if len < 7 {
        return None;
    }
    if b.len() < len {
        return Some(0);
    }
    let cotp = &b[4..len];
    let cl = *cotp.first()? as usize;
    let pdu = *cotp.get(1)?;
    match pdu {
        0xE0 if to_server => {
            // Connection request: the destination TSAP names rack and slot.
            let mut tsap = String::new();
            let mut i = 7;
            while i + 1 < cl + 1 {
                let (code, n) = (cotp[i], cotp[i + 1] as usize);
                if code == 0xC2 && n == 2 {
                    tsap = format!(" to rack {} slot {}", cotp[i + 3] >> 5, cotp[i + 3] & 0x1F);
                }
                i += 2 + n;
            }
            out.push(obs(Action::Connect, format!("COTP connect{tsap}")));
        }
        0xF0 => {
            let s7 = cotp.get(cl + 1..).unwrap_or_default();
            match s7.first() {
                Some(0x32) => s7_pdu(to_server, s7, out),
                Some(0x72) if to_server => out.push(obs(Action::Other, "S7comm-plus message (S7-1200/1500)")),
                _ => {}
            }
        }
        _ => {}
    }
    Some(len)
}

fn s7_pdu(to_server: bool, s: &[u8], out: &mut Vec<Obs>) {
    let rosctr = s.get(1).copied().unwrap_or(0);
    let hdr = if rosctr == 2 || rosctr == 3 { 12 } else { 10 };
    let plen = be16(s, 6).unwrap_or(0) as usize;
    let param = s.get(hdr..hdr + plen).unwrap_or_default();
    if !to_server {
        if (rosctr == 2 || rosctr == 3) && s.len() >= 12 && (s[10] != 0 || s[11] != 0) {
            out.push(obs(Action::Error, format!("S7 error class 0x{:02X} code 0x{:02X}", s[10], s[11])));
        }
        return;
    }
    match rosctr {
        1 => out.push(match param.first() {
            Some(0xF0) => obs(Action::Connect, "setup communication"),
            Some(0x04) => obs(Action::Read, format!("read {}", s7_items(param))),
            Some(0x05) => obs(Action::Write, format!("write {}", s7_items(param))),
            Some(0x1A) => obs(Action::Program, "request download (block)"),
            Some(0x1B) => obs(Action::Program, "download block"),
            Some(0x1C) => obs(Action::Program, "download ended"),
            Some(0x1D) => obs(Action::Program, "start upload (block)"),
            Some(0x1E) => obs(Action::Program, "upload block"),
            Some(0x1F) => obs(Action::Program, "end upload"),
            Some(0x28) => {
                let txt = String::from_utf8_lossy(param).to_string();
                if txt.contains("P_PROGRAM") {
                    obs(Action::Control, "PLC control: run (P_PROGRAM)")
                } else if txt.contains("_INSE") || txt.contains("_DELE") {
                    obs(Action::Program, "PLC control: insert or delete block")
                } else {
                    obs(Action::Control, "PLC control")
                }
            }
            Some(0x29) => obs(Action::Control, "PLC stop"),
            Some(f) => obs(Action::Other, format!("S7 function 0x{f:02X}")),
            None => return,
        }),
        7 => {
            // Userdata: method, type/group, subfunction at param[5..7].
            let group = param.get(5).map(|g| g & 0x0F).unwrap_or(0);
            let sub = param.get(6).copied().unwrap_or(0);
            out.push(match group {
                4 => obs(Action::Diagnostic, format!("CPU function (SZL read, sub 0x{sub:02X})")),
                3 => obs(Action::Diagnostic, "block functions (list blocks)"),
                5 if sub == 1 => obs(Action::Login, "PLC password"),
                5 => obs(Action::Login, "security function"),
                1 => obs(Action::Control, "programmer commands (force/monitor)"),
                7 => obs(Action::Write, "set clock"),
                _ => obs(Action::Other, format!("userdata group {group}")),
            });
        }
        _ => {}
    }
}

// ------------------------------------------------------------------ OPC UA binary

fn opcua_service(id: u32) -> Option<(Action, &'static str)> {
    Some(match id {
        428 => (Action::Diagnostic, "GetEndpoints"),
        422 => (Action::Diagnostic, "FindServers"),
        461 => (Action::Connect, "CreateSession"),
        467 => (Action::Login, "ActivateSession"),
        473 => (Action::Connect, "CloseSession"),
        527 => (Action::Diagnostic, "Browse"),
        631 => (Action::Read, "Read"),
        673 => (Action::Write, "Write"),
        712 => (Action::Control, "Call (method)"),
        488 => (Action::Program, "AddNodes"),
        500 => (Action::Program, "DeleteNodes"),
        494 => (Action::Program, "AddReferences"),
        506 => (Action::Program, "DeleteReferences"),
        751 => (Action::Read, "CreateMonitoredItems"),
        787 => (Action::Read, "CreateSubscription"),
        826 => return None, // Publish: keep-alive traffic, too noisy to report
        664 => (Action::Read, "HistoryRead"),
        700 => (Action::Write, "HistoryUpdate"),
        554 => (Action::Diagnostic, "TranslateBrowsePathsToNodeIds"),
        _ => (Action::Other, "service"),
    })
}

fn opcua(to_server: bool, b: &[u8], out: &mut Vec<Obs>) -> Option<usize> {
    if b.len() < 8 {
        return Some(0);
    }
    let ty = &b[..3];
    if !matches!(ty, b"HEL" | b"ACK" | b"OPN" | b"MSG" | b"CLO" | b"ERR" | b"RHE") {
        return None;
    }
    let size = le32(b, 4)? as usize;
    if !(8..=16 << 20).contains(&size) {
        return None;
    }
    if b.len() < size {
        return Some(0);
    }
    let m = &b[..size];
    let string = |at: usize| -> Option<String> {
        let n = le32(m, at)? as i32;
        if n <= 0 {
            return Some(String::new());
        }
        Some(String::from_utf8_lossy(m.get(at + 4..at + 4 + n as usize)?).into_owned())
    };
    match ty {
        b"HEL" => out.push(obs(Action::Connect, format!("hello {}", string(28).unwrap_or_default()))),
        b"OPN" if to_server => {
            let policy = string(12).unwrap_or_default();
            let short = policy.rsplit('#').next().unwrap_or(&policy).to_string();
            out.push(obs(Action::Connect, format!("open secure channel ({short})")));
        }
        b"ERR" => out.push(obs(Action::Error, format!("error 0x{:08X}", le32(m, 8).unwrap_or(0)))),
        b"MSG" if to_server && &m[3..4] != b"C" => {
            // Symmetric header: channel id, token id, sequence, request id = 24 bytes in.
            // Only readable when the channel isn't encrypted.
            if let (Some(0x01), Some(id)) = (m.get(24), le16(m, 26)) {
                if m.get(25) == Some(&0) {
                    if let Some((a, name)) = opcua_service(id as u32) {
                        out.push(obs(a, if a == Action::Other { format!("service {id}") } else { name.to_string() }));
                    }
                }
            }
        }
        b"MSG" if !to_server => {
            // ServiceFault responses.
            if m.get(24) == Some(&0x01) && le16(m, 26) == Some(397) {
                out.push(obs(Action::Error, "service fault"));
            }
        }
        _ => {}
    }
    Some(size)
}

// ------------------------------------------------------------------ MQTT

fn mqtt(to_server: bool, b: &[u8], out: &mut Vec<Obs>) -> Option<usize> {
    if b.len() < 2 {
        return Some(0);
    }
    let kind = b[0] >> 4;
    if kind == 0 || kind == 15 {
        return None;
    }
    let mut len = 0usize;
    let mut i = 1;
    loop {
        let Some(&byte) = b.get(i) else { return Some(0) };
        len |= ((byte & 0x7F) as usize) << (7 * (i - 1));
        i += 1;
        if byte & 0x80 == 0 {
            break;
        }
        if i > 4 {
            return None;
        }
    }
    if b.len() < i + len {
        return Some(0);
    }
    let p = &b[i..i + len];
    let s = |at: usize| -> Option<(String, usize)> {
        let n = be16(p, at)? as usize;
        Some((String::from_utf8_lossy(p.get(at + 2..at + 2 + n)?).into_owned(), at + 2 + n))
    };
    match kind {
        1 if to_server => {
            // CONNECT: protocol name, level, flags, keep-alive, client id.
            let (_name, at) = s(0)?;
            let flags = *p.get(at + 1)?;
            let (client, _) = s(at + 4).unwrap_or_default();
            let who = if flags & 0x80 != 0 { " with username" } else { "" };
            out.push(obs(if flags & 0x80 != 0 { Action::Login } else { Action::Connect },
                format!("connect as client \"{client}\"{who}")));
        }
        2 if !to_server => {
            let code = *p.get(1).unwrap_or(&0);
            if code != 0 {
                let why = match code {
                    4 => "bad username or password",
                    5 => "not authorized",
                    2 => "client id rejected",
                    _ => "refused",
                };
                out.push(obs(Action::Error, format!("connection refused: {why}")));
            }
        }
        3 if to_server => {
            let (topic, _) = s(0).unwrap_or_default();
            let a = if topic.contains("/NCMD/") || topic.contains("/DCMD/") || topic.ends_with("/set") {
                Action::Control
            } else {
                Action::Write
            };
            out.push(obs(a, format!("publish {topic}")));
        }
        8 if to_server => {
            let mut at = 2;
            let mut topics = vec![];
            while let Some((t, next)) = s(at) {
                topics.push(t);
                at = next + 1;
            }
            out.push(obs(Action::Read, format!("subscribe {}", topics.join(", "))));
        }
        _ => {}
    }
    Some(i + len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(p: Proto, to_server: bool, b: &[u8]) -> Vec<Obs> {
        let (o, used) = decode(p, to_server, b).expect("recognized");
        assert_eq!(used, b.len(), "{p:?} consumed everything");
        o
    }

    #[test]
    fn modbus_requests_and_exceptions() {
        // Write multiple registers 100..103 on unit 1.
        let w = [0, 1, 0, 0, 0, 0x0F, 1, 16, 0, 100, 0, 4, 8, 0, 1, 0, 2, 0, 3, 0, 4];
        assert_eq!(one(Proto::Modbus, true, &w)[0].what, "write holding registers 100-103 (unit 1)");
        let e = [0, 1, 0, 0, 0, 3, 1, 0x83, 2];
        let o = one(Proto::Modbus, false, &e);
        assert_eq!(o[0].action, Action::Error);
        assert!(o[0].what.contains("illegal data address"));
        // Two PDUs in one buffer, plus half of a third.
        let r = [0, 2, 0, 0, 0, 6, 1, 3, 0, 0, 0, 2];
        let mut buf = [r, r].concat();
        buf.extend_from_slice(&r[..5]);
        let (o, used) = decode(Proto::Modbus, true, &buf).unwrap();
        assert_eq!((o.len(), used), (2, 24));
        assert!(decode(Proto::Modbus, true, b"GET / HTTP/1.1\r\n").is_none());
    }

    #[test]
    fn s7_read_write_and_stop() {
        // TPKT + COTP DT + S7 job: write var, 1 item, DB1.DBW10 (word, 1 element).
        let mut s7 = vec![0x32, 1, 0, 0, 0, 1, 0, 14, 0, 6];
        s7.extend_from_slice(&[0x05, 1, 0x12, 0x0A, 0x10, 4, 0, 1, 0, 1, 0x84, 0, 0, 10 << 3]);
        s7.extend_from_slice(&[0, 4, 0, 16, 0x12, 0x34]);
        let mut f = vec![3, 0, 0, 0, 2, 0xF0, 0x80];
        f.extend_from_slice(&s7);
        let n = f.len() as u16;
        f[2..4].copy_from_slice(&n.to_be_bytes());
        let o = one(Proto::S7, true, &f);
        assert_eq!(o[0], obs(Action::Write, "write DB1.DBW10"));
        // PLC stop.
        let mut f = vec![3, 0, 0, 0, 2, 0xF0, 0x80, 0x32, 1, 0, 0, 0, 2, 0, 1, 0, 0, 0x29];
        let n = f.len() as u16;
        f[2..4].copy_from_slice(&n.to_be_bytes());
        assert_eq!(one(Proto::S7, true, &f)[0].action, Action::Control);
    }

    #[test]
    fn opcua_write_and_mqtt_connect() {
        // MSG, final chunk, symmetric header, then a WriteRequest type id (ns 0, 673).
        let mut m = b"MSGF".to_vec();
        m.extend_from_slice(&[0; 4]);
        m.extend_from_slice(&[0; 16]);
        m.extend_from_slice(&[0x01, 0x00]);
        m.extend_from_slice(&673u16.to_le_bytes());
        m.extend_from_slice(&[0; 6]);
        let n = m.len() as u32;
        m[4..8].copy_from_slice(&n.to_le_bytes());
        assert_eq!(one(Proto::Opcua, true, &m)[0], obs(Action::Write, "Write"));

        // MQTT CONNECT with a username, client id "hmi".
        let body = [0, 4, b'M', b'Q', b'T', b'T', 4, 0xC2, 0, 60, 0, 3, b'h', b'm', b'i', 0, 1, b'u', 0, 1, b'p'];
        let mut c = vec![0x10, body.len() as u8];
        c.extend_from_slice(&body);
        assert_eq!(one(Proto::Mqtt, true, &c)[0], obs(Action::Login, "connect as client \"hmi\" with username"));
        // CONNACK refused: bad credentials.
        assert_eq!(one(Proto::Mqtt, false, &[0x20, 2, 0, 4])[0].action, Action::Error);
    }
}
