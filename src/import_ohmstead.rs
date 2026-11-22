//! Import tags from an Ohmstead project's PLC I/O.
//!
//! Ohmstead exports the I/O drawn in a project as JSON (`format: "ohmstead-io"`, version 1): one point per wired
//! digital or analog channel, named by its field device. This turns those points into tags on one service, with
//! simple, fixed rules (see the README, "Import from Ohmstead"):
//!
//! - Modbus: digital in → discrete input, digital out → coil, analog in → input register, analog out → holding
//!   register (float32). Addresses count up per table from `start`.
//! - OPC UA: bool / float32; client tags get `ns=<ns>;s=<prefix><name>`.
//! - IO-Link: one port per device, points stacked by bit offset on it.
//! - Anything else: name, type, description and units only.
//!
//! Re-import matches tags by name: an existing tag keeps its address, history, simulation and scale.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::{DataType, Kind, Mode, ModbusTable, ProcessData, ServiceDef, TagDef};

pub const DEFAULT_URL: &str = "https://mcp.ohmstead.io/mcp";

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct IoRange {
    pub min: f64,
    pub max: f64,
    #[serde(default)]
    pub units: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct IoPoint {
    pub name: String,
    #[serde(default)]
    pub device: String,
    #[serde(default)]
    pub device_terminal: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub module: String,
    #[serde(default)]
    pub terminal: String,
    /// "digital" or "analog".
    pub kind: String,
    /// "in" or "out", seen from the PLC.
    pub direction: String,
    #[serde(default)]
    pub range: Option<IoRange>,
    #[serde(default)]
    pub subsystem: Option<String>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct IoExport {
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub version: Option<u32>,
    #[serde(default)]
    pub points: Vec<IoPoint>,
}

/// Mapping options. Every field has a default.
#[derive(Deserialize, Clone, Debug, Default)]
pub struct Options {
    /// Modbus: first address in each table for new tags. Default 0.
    pub start: Option<u16>,
    /// Modbus: unit id for new tags. Default 1.
    pub unit: Option<u8>,
    /// OPC UA client: namespace index of new node ids. Default 2.
    pub ns: Option<u16>,
    /// OPC UA client: text before the tag name in new node ids (`PLC1.`). Default none.
    pub prefix: Option<String>,
    /// IO-Link: first port for new devices. Default 1.
    pub port_start: Option<u8>,
}

/// What an import would do (or did).
#[derive(Serialize, Debug, Default)]
pub struct Preview {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub unchanged: Vec<String>,
    /// Tags on the service that the import doesn't name. They're kept.
    pub not_in_import: Vec<String>,
    pub skipped: Vec<Skipped>,
    /// One row per imported point: the tag it became and its address, for the preview table.
    pub rows: Vec<Value>,
}

#[derive(Serialize, Debug)]
pub struct Skipped {
    pub name: String,
    pub reason: String,
}

/// Read pasted text: the ohmstead-io JSON, a bare array of points, or the MCP tool reply wrapping either.
pub fn parse(text: &str) -> Result<IoExport, String> {
    let v: Value = serde_json::from_str(text.trim()).map_err(|e| format!("not JSON: {e}"))?;
    from_value(v)
}

pub fn from_value(v: Value) -> Result<IoExport, String> {
    // A JSON-RPC tools/call reply, or its `result`.
    let v = match v.get("result").cloned().unwrap_or(v) {
        r if r.get("content").is_some() => {
            if r.get("isError").and_then(Value::as_bool) == Some(true) {
                return Err(format!("Ohmstead said: {}", tool_text(&r).unwrap_or_default()));
            }
            let t = tool_text(&r).ok_or("the Ohmstead reply has no text")?;
            serde_json::from_str(&t).map_err(|_| format!("Ohmstead said: {t}"))?
        }
        r => r,
    };
    let ex: IoExport = if v.is_array() {
        IoExport { points: serde_json::from_value(v).map_err(|e| e.to_string())?, ..Default::default() }
    } else {
        serde_json::from_value(v).map_err(|e| e.to_string())?
    };
    if let Some(f) = &ex.format {
        if f != "ohmstead-io" {
            return Err(format!("format `{f}` isn't an Ohmstead I/O export (ohmstead-io)"));
        }
    }
    if ex.version.unwrap_or(1) > 1 {
        return Err(format!("ohmstead-io version {} is newer than this Cowpoke reads (1)", ex.version.unwrap_or(0)));
    }
    Ok(ex)
}

fn tool_text(r: &Value) -> Option<String> {
    r.get("content")?
        .as_array()?
        .iter()
        .find_map(|c| c.get("text").and_then(Value::as_str))
        .map(str::to_string)
}

/// Fetch a project's I/O from an Ohmstead MCP endpoint (Cloud or self-hosted) with a personal access token.
pub async fn fetch(url: &str, token: &str, project_id: &str) -> Result<IoExport, String> {
    let url = if url.trim().is_empty() { DEFAULT_URL } else { url.trim() };
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "export_io_points", "arguments": { "project_id": project_id } }
    });
    let res = reqwest::Client::new()
        .post(url)
        .bearer_auth(token.trim())
        .header("accept", "application/json, text/event-stream")
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("couldn't reach {url}: {e}"))?;
    let status = res.status();
    let text = res.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("{url} answered {status}: {}", text.chars().take(300).collect::<String>()));
    }
    // A server that streams anyway: take the last `data:` line.
    let text = if text.trim_start().starts_with('{') {
        text
    } else {
        text.lines().filter_map(|l| l.strip_prefix("data:")).last().unwrap_or("").trim().to_string()
    };
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("unexpected reply from {url}: {e}"))?;
    if let Some(e) = v.get("error") {
        return Err(format!("Ohmstead: {}", e.get("message").and_then(Value::as_str).unwrap_or("error")));
    }
    from_value(v)
}

/// Tag names can't hold `/`.
fn tag_name(p: &IoPoint) -> String {
    p.name.trim().replace('/', ".")
}

fn datatype(p: &IoPoint) -> DataType {
    if p.kind == "analog" { DataType::Float32 } else { DataType::Bool }
}

fn is_out(p: &IoPoint) -> bool {
    p.direction == "out"
}

fn modbus_table(p: &IoPoint) -> ModbusTable {
    match (p.kind == "analog", is_out(p)) {
        (false, false) => ModbusTable::DiscreteInput,
        (false, true) => ModbusTable::Coil,
        (true, false) => ModbusTable::InputRegister,
        (true, true) => ModbusTable::HoldingRegister,
    }
}

fn describe(p: &IoPoint) -> String {
    let mut s = String::new();
    if !p.description.is_empty() {
        s.push_str(&p.description);
        s.push_str(": ");
    }
    s.push_str(&format!("{} {} → {}:{}", p.device, p.device_terminal, p.module, p.terminal));
    if let Some(sub) = &p.subsystem {
        s.push_str(&format!(" ({sub})"));
    }
    s
}

fn units(p: &IoPoint) -> Option<String> {
    p.range.as_ref().and_then(|r| r.units.clone()).filter(|u| !u.is_empty())
}

/// Merge the points into `def`'s tags. Returns what changed; `def` is updated in place.
pub fn apply(def: &mut ServiceDef, ex: &IoExport, opt: &Options) -> Preview {
    let mut pv = Preview::default();
    let imported: HashSet<String> = ex.points.iter().map(tag_name).collect();
    pv.not_in_import = def.tags.iter().filter(|t| !imported.contains(&t.name)).map(|t| t.name.clone()).collect();

    // Next free Modbus address per (unit, table): after the highest one in use, at least `start`.
    let start = opt.start.unwrap_or(0);
    let mut next_reg: HashMap<(u8, ModbusTable), u32> = HashMap::new();
    for t in &def.tags {
        if let Some(r) = t.register {
            let end = r as u32 + t.datatype.registers(t.length) as u32;
            let e = next_reg.entry((t.unit(), t.table())).or_insert(start as u32);
            *e = (*e).max(end);
        }
    }
    // IO-Link: the port each device is on, and the next free bit per (port, pd).
    let mut device_port: HashMap<String, u8> = HashMap::new();
    let mut used_ports: HashSet<u8> = def.tags.iter().filter_map(|t| t.port).collect();
    let mut next_bit: HashMap<(u8, bool), u16> = HashMap::new();
    for t in &def.tags {
        if let (Some(port), Some(off)) = (t.port, t.bit_offset) {
            let len = t.bit_length.unwrap_or(t.datatype.bits());
            let e = next_bit.entry((port, t.pd() == ProcessData::Out)).or_insert(0);
            *e = (*e).max(off + len);
        }
    }
    for p in &ex.points {
        if let Some(port) = def.tag(&tag_name(p)).and_then(|t| t.port) {
            device_port.entry(p.device.clone()).or_insert(port);
        }
    }
    let max_port: u8 = def.settings.get("ports").and_then(Value::as_u64).map(|n| n.min(255) as u8).unwrap_or(8);
    let mut next_port = opt.port_start.unwrap_or(1).max(1);

    let mut seen = HashSet::new();
    for p in &ex.points {
        let name = tag_name(p);
        if name.is_empty() {
            pv.skipped.push(Skipped { name: p.device.clone(), reason: "no name".into() });
            continue;
        }
        if p.kind != "digital" && p.kind != "analog" {
            pv.skipped.push(Skipped { name, reason: format!("kind `{}` isn't digital or analog", p.kind) });
            continue;
        }
        if !seen.insert(name.clone()) {
            pv.skipped.push(Skipped { name, reason: "appears twice in the import".into() });
            continue;
        }
        let existing = def.tags.iter().position(|t| t.name == name);
        let mut t = existing.map(|i| def.tags[i].clone()).unwrap_or_else(|| TagDef { name: name.clone(), ..Default::default() });
        let before = serde_json::to_value(&t).unwrap_or_default();
        let old_dt = t.datatype;
        // IO-Link analog values come as raw 16-bit words.
        let dt = if def.kind == Kind::Iolink && p.kind == "analog" { DataType::Uint16 } else { datatype(p) };
        t.datatype = dt;
        t.description = Some(describe(p));
        t.units = units(p).or(t.units.take());
        t.writable = is_out(p) || def.mode == Mode::Server;

        let mut address = String::new();
        match def.kind {
            Kind::Modbus => {
                let table = modbus_table(p);
                let keep = t.register.is_some() && t.table == Some(table) && old_dt.registers(t.length) == dt.registers(None);
                if !keep {
                    let unit = t.unit.unwrap_or(opt.unit.unwrap_or(1));
                    let next = next_reg.entry((unit, table)).or_insert(start as u32);
                    if *next + dt.registers(None) as u32 > 65536 {
                        pv.skipped.push(Skipped { name, reason: "no Modbus addresses left in that table".into() });
                        continue;
                    }
                    t.unit = if unit == 1 { None } else { Some(unit) };
                    t.table = Some(table);
                    t.register = Some(*next as u16);
                    *next += dt.registers(None) as u32;
                }
                address = format!("{} {}", serde_json::to_value(table).unwrap().as_str().unwrap_or(""), t.register.unwrap_or(0));
            }
            Kind::Opcua => {
                if def.mode == Mode::Client {
                    if t.node_id.is_none() {
                        t.node_id = Some(format!("ns={};s={}{}", opt.ns.unwrap_or(2), opt.prefix.clone().unwrap_or_default(), name));
                    }
                    address = t.node_id.clone().unwrap_or_default();
                } else {
                    address = t.node_id.clone().unwrap_or_else(|| format!("s={name}"));
                }
            }
            Kind::Iolink => {
                let port = match device_port.get(&p.device) {
                    Some(&port) => port,
                    None => {
                        while used_ports.contains(&next_port) && next_port <= max_port {
                            next_port += 1;
                        }
                        if next_port > max_port {
                            pv.skipped.push(Skipped { name, reason: format!("no free IO-Link port (the master has {max_port})") });
                            continue;
                        }
                        used_ports.insert(next_port);
                        device_port.insert(p.device.clone(), next_port);
                        next_port
                    }
                };
                let pd = if is_out(p) { ProcessData::Out } else { ProcessData::In };
                let len = dt.bits();
                let keep = t.port == Some(port) && t.pd == Some(pd) && t.bit_offset.is_some() && t.bit_length.unwrap_or(old_dt.bits()) == len;
                if !keep {
                    let next = next_bit.entry((port, pd == ProcessData::Out)).or_insert(0);
                    t.port = Some(port);
                    t.pd = Some(pd);
                    t.bit_offset = Some(*next);
                    t.bit_length = None;
                    *next += len;
                }
                address = format!("port {} {} bit {}", port, if pd == ProcessData::Out { "out" } else { "in" }, t.bit_offset.unwrap_or(0));
            }
            _ => {}
        }

        let after = serde_json::to_value(&t).unwrap_or_default();
        pv.rows.push(json!({
            "name": name, "device": p.device, "channel": format!("{}:{}", p.module, p.terminal),
            "kind": p.kind, "direction": p.direction, "datatype": t.datatype, "address": address, "units": t.units,
            "status": match existing { None => "added", Some(_) if before == after => "unchanged", _ => "updated" },
        }));
        match existing {
            None => {
                pv.added.push(name);
                def.tags.push(t);
            }
            Some(i) => {
                if before == after {
                    pv.unchanged.push(name);
                } else {
                    pv.updated.push(name);
                }
                def.tags[i] = t;
            }
        }
    }
    pv
}
