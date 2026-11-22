//! Sparkplug B for the MQTT client: the protobuf payload (hand-rolled, only the fields
//! Cowpoke uses), topics, and the alias table an edge node announces in its births.
//!
//! Cowpoke acts as a host application for one edge node, or one device under it:
//! it reads metrics from NBIRTH/NDATA (or DBIRTH/DDATA), marks tags bad on a death
//! certificate, asks for a rebirth on connect, and writes with NCMD (or DCMD).

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Value, json};

// Sparkplug B data types.
pub const INT8: u32 = 1;
pub const INT16: u32 = 2;
pub const INT32: u32 = 3;
pub const INT64: u32 = 4;
pub const UINT8: u32 = 5;
pub const UINT16: u32 = 6;
pub const UINT32: u32 = 7;
pub const UINT64: u32 = 8;
pub const FLOAT: u32 = 9;
pub const DOUBLE: u32 = 10;
pub const BOOLEAN: u32 = 11;
pub const STRING: u32 = 12;
pub const DATETIME: u32 = 13;
pub const TEXT: u32 = 14;

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub group_id: String,
    pub edge_node: String,
    /// Read a device under the edge node (DBIRTH/DDATA/DCMD) instead of the node itself.
    #[serde(default)]
    pub device: Option<String>,
    /// Client: send `Node Control/Rebirth` on connect, so the node republishes its births
    /// (and aliases). Ignored in server mode.
    #[serde(default = "yes")]
    pub rebirth: bool,
    /// Server: also be the edge node on this outside broker (with an NDEATH will),
    /// not only on Cowpoke's own.
    #[serde(default)]
    pub broker: Option<Remote>,
}

/// An outside MQTT broker the edge node connects to.
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub tls: bool,
    /// PEM CA certificate for a broker with its own CA.
    #[serde(default)]
    pub ca: Option<String>,
    #[serde(default = "keep_alive")]
    pub keep_alive_s: u64,
}

fn keep_alive() -> u64 {
    30
}

impl Remote {
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(if self.tls { 8883 } else { 1883 })
    }

    pub fn url(&self) -> String {
        format!("{}{}:{}", if self.tls { "mqtts://" } else { "mqtt://" }, self.host, self.port())
    }

    pub fn options(&self, default_id: String) -> rumqttc::MqttOptions {
        let mut o = rumqttc::MqttOptions::new(self.client_id.clone().unwrap_or(default_id), self.host.clone(), self.port());
        o.set_keep_alive(std::time::Duration::from_secs(self.keep_alive_s.max(5)));
        o.set_max_packet_size(1 << 20, 1 << 20);
        if let Some(u) = &self.username {
            o.set_credentials(u.clone(), self.password.clone().unwrap_or_default());
        }
        if self.tls {
            o.set_transport(match &self.ca {
                Some(ca) => rumqttc::Transport::tls(ca.as_bytes().to_vec(), None, None),
                None => rumqttc::Transport::tls_with_default_config(),
            });
        }
        o
    }
}

fn yes() -> bool {
    true
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        for (what, v) in [("group_id", Some(&self.group_id)), ("edge_node", Some(&self.edge_node)), ("device", self.device.as_ref())] {
            if let Some(v) = v {
                if v.is_empty() || v.contains(['/', '+', '#']) {
                    return Err(format!("sparkplug.{what}: can't be empty or hold /, + or #"));
                }
            }
        }
        if let Some(b) = &self.broker {
            if b.host.trim().is_empty() {
                return Err("sparkplug.broker: give the broker's host".into());
            }
        }
        Ok(())
    }

    pub fn topic(&self, kind: &str, device: bool) -> String {
        let mut t = format!("spBv1.0/{}/{kind}/{}", self.group_id, self.edge_node);
        if device {
            if let Some(d) = &self.device {
                t.push('/');
                t.push_str(d);
            }
        }
        t
    }

    /// Topics to subscribe to: the node's messages, plus the device's.
    pub fn subscriptions(&self) -> Vec<String> {
        let mut v = vec![format!("spBv1.0/{}/+/{}", self.group_id, self.edge_node)];
        if let Some(d) = &self.device {
            v.push(format!("spBv1.0/{}/+/{}/{d}", self.group_id, self.edge_node));
        }
        v
    }

    pub fn ncmd(&self) -> String {
        self.topic("NCMD", false)
    }

    /// Where writes go: DCMD for a device, NCMD for the node.
    pub fn cmd(&self) -> String {
        if self.device.is_some() { self.topic("DCMD", true) } else { self.ncmd() }
    }
}

/// What a message means for the tags.
#[derive(Debug, PartialEq)]
pub enum Message {
    /// A birth or data message carrying metrics for our node or device.
    Metrics { birth: bool },
    /// Our node or device went away.
    Death,
    Other,
}

pub fn classify(cfg: &Config, topic: &str) -> Message {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() < 4 || parts[0] != "spBv1.0" || parts[1] != cfg.group_id || parts[3] != cfg.edge_node {
        return Message::Other;
    }
    let dev = parts.get(4).copied();
    let ours = match (&cfg.device, dev) {
        (None, None) => true,
        (Some(d), Some(x)) => d == x,
        _ => false,
    };
    match parts[2] {
        // A node's death ends its devices too.
        "NDEATH" => Message::Death,
        "DDEATH" if ours => Message::Death,
        "NBIRTH" | "DBIRTH" if ours => Message::Metrics { birth: true },
        "NDATA" | "DDATA" if ours => Message::Metrics { birth: false },
        _ => Message::Other,
    }
}

// ------------------------------------------------------------------ protobuf

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Metric {
    pub name: Option<String>,
    pub alias: Option<u64>,
    pub datatype: Option<u32>,
    pub is_null: bool,
    pub value: Option<Value>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Payload {
    pub timestamp: Option<u64>,
    pub seq: Option<u64>,
    pub metrics: Vec<Metric>,
}

fn varint(b: &[u8], i: &mut usize) -> Result<u64, String> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*i).ok_or("truncated varint")?;
        *i += 1;
        v |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err("varint too long".into())
}

/// Iterate (field, wire type, varint value or byte slice) over a protobuf message.
enum Field<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

fn fields(b: &[u8]) -> Result<Vec<(u32, Field<'_>)>, String> {
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let key = varint(b, &mut i)?;
        let (num, wt) = ((key >> 3) as u32, key & 7);
        let f = match wt {
            0 => Field::Varint(varint(b, &mut i)?),
            1 => {
                let s = b.get(i..i + 8).ok_or("truncated fixed64")?;
                i += 8;
                Field::Fixed64(u64::from_le_bytes(s.try_into().unwrap()))
            }
            2 => {
                let n = varint(b, &mut i)? as usize;
                let s = b.get(i..i + n).ok_or("truncated field")?;
                i += n;
                Field::Bytes(s)
            }
            5 => {
                let s = b.get(i..i + 4).ok_or("truncated fixed32")?;
                i += 4;
                Field::Fixed32(u32::from_le_bytes(s.try_into().unwrap()))
            }
            w => return Err(format!("unsupported wire type {w}")),
        };
        out.push((num, f));
    }
    Ok(out)
}

fn decode_metric(b: &[u8]) -> Result<Metric, String> {
    let mut m = Metric::default();
    let mut raw: Option<Value> = None;
    let mut int_raw: Option<u64> = None;
    for (n, f) in fields(b)? {
        match (n, f) {
            (1, Field::Bytes(s)) => m.name = Some(String::from_utf8_lossy(s).into_owned()),
            (2, Field::Varint(v)) => m.alias = Some(v),
            (4, Field::Varint(v)) => m.datatype = Some(v as u32),
            (7, Field::Varint(v)) => m.is_null = v != 0,
            (10, Field::Varint(v)) | (11, Field::Varint(v)) => int_raw = Some(v),
            (12, Field::Fixed32(v)) => raw = Some(json!(f32::from_bits(v) as f64)),
            (13, Field::Fixed64(v)) => raw = Some(json!(f64::from_bits(v))),
            (14, Field::Varint(v)) => raw = Some(json!(v != 0)),
            (15, Field::Bytes(s)) => raw = Some(json!(String::from_utf8_lossy(s))),
            _ => {}
        }
    }
    if let Some(v) = int_raw {
        // Signed types are two's complement in the unsigned field.
        raw = Some(match m.datatype {
            Some(INT8) => json!(v as u8 as i8),
            Some(INT16) => json!(v as u16 as i16),
            Some(INT32) => json!(v as u32 as i32),
            Some(INT64) => json!(v as i64),
            Some(BOOLEAN) => json!(v != 0),
            _ => json!(v),
        });
    }
    if !m.is_null {
        m.value = raw;
    }
    Ok(m)
}

pub fn decode(b: &[u8]) -> Result<Payload, String> {
    let mut p = Payload::default();
    for (n, f) in fields(b)? {
        match (n, f) {
            (1, Field::Varint(v)) => p.timestamp = Some(v),
            (2, Field::Bytes(s)) => p.metrics.push(decode_metric(s)?),
            (3, Field::Varint(v)) => p.seq = Some(v),
            _ => {}
        }
    }
    Ok(p)
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_key(out: &mut Vec<u8>, field: u32, wt: u8) {
    put_varint(out, ((field as u64) << 3) | wt as u64);
}

fn put_bytes(out: &mut Vec<u8>, field: u32, b: &[u8]) {
    put_key(out, field, 2);
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

/// Encode a metric's value as `datatype`.
pub fn encode_metric(name: Option<&str>, alias: Option<u64>, datatype: u32, v: &Value, ts: u64) -> Result<Vec<u8>, String> {
    let mut m = vec![];
    if let Some(n) = name {
        put_bytes(&mut m, 1, n.as_bytes());
    }
    if let Some(a) = alias {
        put_key(&mut m, 2, 0);
        put_varint(&mut m, a);
    }
    put_key(&mut m, 3, 0);
    put_varint(&mut m, ts);
    put_key(&mut m, 4, 0);
    put_varint(&mut m, datatype as u64);
    let num = || match v {
        Value::Number(n) => n.as_f64().ok_or("not a number".to_string()),
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => s.trim().parse().map_err(|_| format!("`{s}` is not a number")),
        _ => Err("expected a number".to_string()),
    };
    let int = || -> Result<i128, String> {
        match v {
            Value::Number(n) if n.is_i64() => Ok(n.as_i64().unwrap() as i128),
            Value::Number(n) if n.is_u64() => Ok(n.as_u64().unwrap() as i128),
            _ => Ok(num()?.round() as i128),
        }
    };
    match datatype {
        INT8 | INT16 | INT32 => {
            put_key(&mut m, 10, 0);
            put_varint(&mut m, int()? as i32 as u32 as u64);
        }
        UINT8 | UINT16 | UINT32 => {
            put_key(&mut m, 10, 0);
            put_varint(&mut m, int()? as u32 as u64);
        }
        INT64 => {
            put_key(&mut m, 11, 0);
            put_varint(&mut m, int()? as i64 as u64);
        }
        UINT64 | DATETIME => {
            put_key(&mut m, 11, 0);
            put_varint(&mut m, int()? as u64);
        }
        FLOAT => {
            put_key(&mut m, 12, 5);
            m.extend_from_slice(&(num()? as f32).to_bits().to_le_bytes());
        }
        DOUBLE => {
            put_key(&mut m, 13, 1);
            m.extend_from_slice(&num()?.to_bits().to_le_bytes());
        }
        BOOLEAN => {
            put_key(&mut m, 14, 0);
            put_varint(&mut m, (num()? != 0.0) as u64);
        }
        STRING | TEXT => {
            let s = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            put_bytes(&mut m, 15, s.as_bytes());
        }
        d => return Err(format!("Sparkplug datatype {d} isn't supported")),
    }
    Ok(m)
}

/// Encode a payload from already-encoded metrics.
pub fn encode(ts: u64, seq: Option<u64>, metrics: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![];
    put_key(&mut out, 1, 0);
    put_varint(&mut out, ts);
    for m in metrics {
        put_bytes(&mut out, 2, m);
    }
    if let Some(s) = seq {
        put_key(&mut out, 3, 0);
        put_varint(&mut out, s);
    }
    out
}

/// The Sparkplug type for a Cowpoke datatype, for writes to metrics without a birth.
pub fn datatype_for(dt: crate::model::DataType) -> u32 {
    use crate::model::DataType as D;
    match dt {
        D::Bool => BOOLEAN,
        D::Int16 => INT16,
        D::Uint16 => UINT16,
        D::Int32 => INT32,
        D::Uint32 => UINT32,
        D::Int64 => INT64,
        D::Uint64 => UINT64,
        D::Float32 => FLOAT,
        D::Float64 => DOUBLE,
        D::String => STRING,
    }
}

pub fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// What the births told us: alias → name, and each name's datatype and alias.
#[derive(Default)]
pub struct Births {
    names: HashMap<u64, String>,
    pub metrics: HashMap<String, (u32, Option<u64>)>,
}

impl Births {
    pub fn clear(&mut self) {
        self.names.clear();
        self.metrics.clear();
    }

    /// Resolve each metric's name (learning aliases from births); returns (name, value or None for null).
    pub fn resolve(&mut self, p: &Payload, birth: bool) -> Vec<(String, Option<Value>)> {
        if birth {
            self.clear();
        }
        let mut out = vec![];
        for m in &p.metrics {
            let name = match (&m.name, m.alias) {
                (Some(n), a) => {
                    if birth {
                        if let Some(a) = a {
                            self.names.insert(a, n.clone());
                        }
                        if let Some(dt) = m.datatype {
                            self.metrics.insert(n.clone(), (dt, a));
                        }
                    }
                    n.clone()
                }
                (None, Some(a)) => match self.names.get(&a) {
                    Some(n) => n.clone(),
                    None => continue,
                },
                (None, None) => continue,
            };
            out.push((name, m.value.clone()));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let ts = 1_700_000_000_000;
        let ms = vec![
            encode_metric(Some("Level"), Some(1), FLOAT, &json!(42.5), ts).unwrap(),
            encode_metric(Some("Temp"), Some(2), INT16, &json!(-12), ts).unwrap(),
            encode_metric(Some("Big"), None, INT64, &json!(-9_000_000_000i64), ts).unwrap(),
            encode_metric(Some("Run"), None, BOOLEAN, &json!(true), ts).unwrap(),
            encode_metric(Some("Name"), None, STRING, &json!("Cow1"), ts).unwrap(),
            encode_metric(Some("Pi"), None, DOUBLE, &json!(3.25), ts).unwrap(),
        ];
        let p = decode(&encode(ts, Some(0), &ms)).unwrap();
        assert_eq!(p.seq, Some(0));
        let mut b = Births::default();
        let got = b.resolve(&p, true);
        assert_eq!(
            got,
            vec![
                ("Level".into(), Some(json!(42.5))),
                ("Temp".into(), Some(json!(-12))),
                ("Big".into(), Some(json!(-9_000_000_000i64))),
                ("Run".into(), Some(json!(true))),
                ("Name".into(), Some(json!("Cow1"))),
                ("Pi".into(), Some(json!(3.25))),
            ]
        );
        // Data by alias only.
        let d = encode(ts, Some(1), &[encode_metric(None, Some(2), INT16, &json!(7), ts).unwrap()]);
        assert_eq!(b.resolve(&decode(&d).unwrap(), false), vec![("Temp".into(), Some(json!(7)))]);
    }

    #[test]
    fn topics() {
        let c = Config { group_id: "G".into(), edge_node: "N".into(), device: Some("D".into()), rebirth: true, broker: None };
        assert_eq!(classify(&c, "spBv1.0/G/DDATA/N/D"), Message::Metrics { birth: false });
        assert_eq!(classify(&c, "spBv1.0/G/DBIRTH/N/D"), Message::Metrics { birth: true });
        assert_eq!(classify(&c, "spBv1.0/G/NDATA/N"), Message::Other);
        assert_eq!(classify(&c, "spBv1.0/G/NDEATH/N"), Message::Death);
        assert_eq!(classify(&c, "spBv1.0/G/DDATA/N/X"), Message::Other);
        assert_eq!(c.cmd(), "spBv1.0/G/DCMD/N/D");
        assert_eq!(c.ncmd(), "spBv1.0/G/NCMD/N");
    }
}
