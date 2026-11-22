//! Live tag values for every service, plus a broadcast of every change.

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::model::{DataType, TagDef};

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Good,
    Bad,
    Uncertain,
}

#[derive(Serialize, Clone, Debug)]
pub struct TagState {
    pub value: Value,
    pub quality: Quality,
    pub ts: DateTime<Utc>,
}

/// Who changed a value. Services ignore their own echoes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The web UI or REST API.
    User,
    /// The service itself (a poll result or a protocol write).
    Service,
}

#[derive(Clone, Debug)]
pub struct TagEvent {
    pub service: String,
    pub tag: String,
    pub state: TagState,
    pub origin: Origin,
}

#[derive(Clone)]
pub struct TagStore {
    map: std::sync::Arc<DashMap<(String, String), TagState>>,
    tx: broadcast::Sender<TagEvent>,
}

impl Default for TagStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TagStore {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(4096);
        Self {
            map: Default::default(),
            tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TagEvent> {
        self.tx.subscribe()
    }

    pub fn get(&self, service: &str, tag: &str) -> Option<TagState> {
        self.map
            .get(&(service.to_string(), tag.to_string()))
            .map(|s| s.clone())
    }

    pub fn value(&self, service: &str, tag: &str) -> Option<Value> {
        self.get(service, tag).map(|s| s.value)
    }

    pub fn service_tags(&self, service: &str) -> Vec<(String, TagState)> {
        let mut out: Vec<_> = self
            .map
            .iter()
            .filter(|e| e.key().0 == service)
            .map(|e| (e.key().1.clone(), e.value().clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn set(&self, service: &str, tag: &str, value: Value, quality: Quality, origin: Origin) {
        let state = TagState {
            value,
            quality,
            ts: Utc::now(),
        };
        self.map
            .insert((service.to_string(), tag.to_string()), state.clone());
        let _ = self.tx.send(TagEvent {
            service: service.to_string(),
            tag: tag.to_string(),
            state,
            origin,
        });
    }

    /// Mark a tag bad without losing its last value.
    pub fn set_bad(&self, service: &str, tag: &str) {
        let last = self.value(service, tag).unwrap_or(Value::Null);
        self.set(service, tag, last, Quality::Bad, Origin::Service);
    }

    pub fn remove_service(&self, service: &str) {
        self.map.retain(|k, _| k.0 != service);
    }

    /// Seed a server-mode service's tags with their initial values (keeps values already present).
    pub fn seed(&self, service: &str, tags: &[TagDef]) {
        self.map.retain(|k, _| k.0 != service || tags.iter().any(|t| t.name == k.1));
        for t in tags {
            if self.get(service, &t.name).is_none() {
                let v = t
                    .initial
                    .clone()
                    .map(|v| coerce_tag(t, &v).unwrap_or(v))
                    .unwrap_or_else(|| zero(t.datatype));
                self.set(service, &t.name, v, Quality::Good, Origin::Service);
            }
        }
    }
}

pub fn zero(dt: DataType) -> Value {
    match dt {
        DataType::Bool => Value::Bool(false),
        DataType::String => Value::String(String::new()),
        DataType::Float32 | DataType::Float64 => serde_json::json!(0.0),
        _ => serde_json::json!(0),
    }
}

/// Convert a user-supplied value to the tag's data type.
pub fn coerce(dt: DataType, v: &Value) -> Result<Value, String> {
    let num = || -> Result<f64, String> {
        match v {
            Value::Number(n) => n.as_f64().ok_or_else(|| "not a number".into()),
            Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
            Value::String(s) => s
                .trim()
                .parse::<f64>()
                .map_err(|_| format!("`{s}` is not a number")),
            _ => Err("expected a number".into()),
        }
    };
    let int_in = |lo: f64, hi: f64| -> Result<Value, String> {
        let n = num()?.round();
        if n < lo || n > hi {
            return Err(format!("{n} is out of range {lo}..={hi}"));
        }
        Ok(if lo < 0.0 {
            serde_json::json!(n as i64)
        } else {
            serde_json::json!(n as u64)
        })
    };
    match dt {
        DataType::Bool => match v {
            Value::Bool(b) => Ok(Value::Bool(*b)),
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "on" | "yes" => Ok(Value::Bool(true)),
                "false" | "0" | "off" | "no" => Ok(Value::Bool(false)),
                _ => Err(format!("`{s}` is not a bool")),
            },
            _ => Ok(Value::Bool(num()? != 0.0)),
        },
        DataType::Int16 => int_in(i16::MIN as f64, i16::MAX as f64),
        DataType::Uint16 => int_in(0.0, u16::MAX as f64),
        DataType::Int32 => int_in(i32::MIN as f64, i32::MAX as f64),
        DataType::Uint32 => int_in(0.0, u32::MAX as f64),
        DataType::Int64 => int_in(i64::MIN as f64, i64::MAX as f64),
        DataType::Uint64 => int_in(0.0, u64::MAX as f64),
        DataType::Float32 | DataType::Float64 => Ok(serde_json::json!(num()?)),
        DataType::String => Ok(Value::String(match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })),
    }
}

/// Like [`coerce`], but a scaled tag keeps its engineering value as a float.
pub fn coerce_tag(t: &TagDef, v: &Value) -> Result<Value, String> {
    if t.is_scaled() && t.datatype.is_numeric() && t.datatype != DataType::Bool {
        coerce(DataType::Float64, v)
    } else {
        coerce(t.datatype, v)
    }
}

pub fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}
