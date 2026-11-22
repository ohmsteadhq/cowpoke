//! The configuration model: services, tags and their settings.
//!
//! Everything here round-trips through `cowpoke.yaml`, the REST API and the web UI.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Modbus,
    Opcua,
    Api,
    Iolink,
    Mqtt,
    /// EtherNet/IP (CIP explicit messaging to Logix controllers).
    Eip,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Cowpoke connects out to a real device or endpoint.
    Client,
    /// Cowpoke acts as the device or endpoint.
    Server,
}

/// The whole configuration file.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Config {
    #[serde(default)]
    pub services: Vec<ServiceDef>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ServiceDef {
    /// Unique, URL-safe id (`a-z 0-9 - _`).
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub kind: Kind,
    pub mode: Mode,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Run on this edge agent instead of on the hub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Kind- and mode-specific settings. Each service parses its own.
    #[serde(default = "empty_object")]
    pub settings: Value,
    #[serde(default)]
    pub tags: Vec<TagDef>,
}

fn yes() -> bool {
    true
}
fn empty_object() -> Value {
    Value::Object(Default::default())
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    Bool,
    Int16,
    #[default]
    Uint16,
    Int32,
    Uint32,
    Int64,
    Uint64,
    Float32,
    Float64,
    String,
}

impl DataType {
    pub fn is_numeric(self) -> bool {
        !matches!(self, DataType::String)
    }
    /// Modbus registers this type spans (strings use the tag's `length`).
    pub fn registers(self, length: Option<u16>) -> u16 {
        match self {
            DataType::Bool | DataType::Int16 | DataType::Uint16 => 1,
            DataType::Int32 | DataType::Uint32 | DataType::Float32 => 2,
            DataType::Int64 | DataType::Uint64 | DataType::Float64 => 4,
            DataType::String => length.unwrap_or(10).max(1),
        }
    }
    /// Bits this type spans in IO-Link process data, when the tag doesn't say.
    pub fn bits(self) -> u16 {
        match self {
            DataType::Bool => 1,
            DataType::Int16 | DataType::Uint16 => 16,
            DataType::Int32 | DataType::Uint32 | DataType::Float32 => 32,
            DataType::Int64 | DataType::Uint64 | DataType::Float64 => 64,
            DataType::String => 8 * 16,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum ModbusTable {
    Coil,
    DiscreteInput,
    InputRegister,
    #[default]
    HoldingRegister,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProcessData {
    /// Device → master (sensor values).
    #[default]
    In,
    /// Master → device (actuator outputs).
    Out,
}

/// One tag. The address fields that apply depend on the service kind;
/// the others are ignored.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct TagDef {
    pub name: String,
    #[serde(default)]
    pub datatype: DataType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Value the tag starts with (server mode) before anything writes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial: Option<Value>,
    #[serde(default = "yes")]
    pub writable: bool,
    /// Raw value × scale + offset = engineering value (Modbus and IO-Link).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,

    // --- Modbus ---
    /// Modbus unit id (slave id). Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<ModbusTable>,
    /// Zero-based protocol address (40001 is holding register 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub register: Option<u16>,
    /// String length in registers (2 characters each).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u16>,

    // --- OPC UA ---
    /// Client: the server's node id (`ns=2;s=Tank.Level`). Server: optional; defaults to `ns=<cowpoke>;s=<tag name>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,

    // --- API ---
    /// JSON pointer into the response body (`/data/temperature`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,

    // --- MQTT ---
    /// Client: the topic (or filter) the value arrives on; `pointer` picks it out of JSON.
    /// Server: where it is published; defaults to `<prefix>/<tag name>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// Sparkplug B client: the metric name. Defaults to the tag name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,

    // --- EtherNet/IP ---
    /// Logix tag name (`Program:Main.Level`, `Tank[3].Temp`). Server: defaults to the tag name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cip_tag: Option<String>,

    // --- IO-Link ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pd: Option<ProcessData>,
    /// IODD bit offset: counted from the least significant bit of the last byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_offset: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_length: Option<u16>,

    // --- Simulation (server mode) ---
    /// Generate values for this tag: a wave, noise, a counter or a sequence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulate: Option<crate::generate::Generator>,

    // --- Historian ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<HistoryCfg>,
}

impl TagDef {
    pub fn unit(&self) -> u8 {
        self.unit.unwrap_or(1)
    }
    pub fn table(&self) -> ModbusTable {
        self.table.unwrap_or_default()
    }
    pub fn pd(&self) -> ProcessData {
        self.pd.unwrap_or_default()
    }
    pub fn to_eng(&self, raw: f64) -> f64 {
        raw * self.scale.unwrap_or(1.0) + self.offset.unwrap_or(0.0)
    }
    pub fn to_raw(&self, eng: f64) -> f64 {
        let s = self.scale.unwrap_or(1.0);
        let s = if s == 0.0 { 1.0 } else { s };
        (eng - self.offset.unwrap_or(0.0)) / s
    }
    pub fn is_scaled(&self) -> bool {
        self.scale.is_some() || self.offset.is_some()
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum HistoryMode {
    /// Record on change, when the value moves more than `deadband`.
    #[default]
    Deadband,
    /// Record the current value every `interval_ms`.
    Interval,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HistoryCfg {
    #[serde(default)]
    pub mode: HistoryMode,
    #[serde(default)]
    pub deadband: f64,
    #[serde(default = "default_interval")]
    pub interval_ms: u64,
    #[serde(default = "default_retention")]
    pub retention_days: u32,
}

fn default_interval() -> u64 {
    1000
}
fn default_retention() -> u32 {
    30
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

impl ServiceDef {
    pub fn name_or_id(&self) -> String {
        if self.name.trim().is_empty() {
            self.id.clone()
        } else {
            self.name.clone()
        }
    }

    pub fn tag(&self, name: &str) -> Option<&TagDef> {
        self.tags.iter().find(|t| t.name == name)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() {
            return Err("give the service an id".into());
        }
        if !valid_id(&self.id) {
            return Err(format!(
                "service id `{}`: use 1-64 of a-z, 0-9, - and _",
                self.id
            ));
        }
        if let Some(a) = &self.agent {
            if !valid_id(a) {
                return Err(format!("agent `{a}`: use 1-64 of a-z, 0-9, - and _"));
            }
        }
        let mut seen = std::collections::HashSet::new();
        for t in &self.tags {
            if let Some(g) = &t.simulate {
                if self.mode == Mode::Client {
                    return Err(format!(
                        "tag `{}`: simulate is for server-mode services; a client reads its values from the device",
                        t.name
                    ));
                }
                g.validate(t)?;
            }
            if t.name.trim().is_empty() {
                return Err("every tag needs a name".into());
            }
            if t.name.contains('/') {
                return Err(format!("tag `{}`: names can't contain /", t.name));
            }
            if !seen.insert(t.name.as_str()) {
                return Err(format!("tag `{}` appears twice", t.name));
            }
            match self.kind {
                Kind::Modbus => {
                    if t.register.is_none() {
                        return Err(format!("tag `{}`: Modbus tags need `register`", t.name));
                    }
                    if matches!(t.table(), ModbusTable::Coil | ModbusTable::DiscreteInput)
                        && t.datatype != DataType::Bool
                    {
                        return Err(format!(
                            "tag `{}`: coils and discrete inputs are bool",
                            t.name
                        ));
                    }
                }
                Kind::Opcua if self.mode == Mode::Client => {
                    if t.node_id.is_none() {
                        return Err(format!("tag `{}`: OPC UA client tags need `node_id`", t.name));
                    }
                }
                Kind::Mqtt => match (&t.topic, self.mode) {
                    (None, Mode::Client) if self.settings.get("sparkplug").is_none_or(|v| v.is_null()) => {
                        return Err(format!("tag `{}`: MQTT client tags need `topic` (or the service needs `sparkplug`)", t.name));
                    }
                    (Some(topic), Mode::Server) if topic.contains(['+', '#']) || topic.is_empty() => {
                        return Err(format!("tag `{}`: a published topic can't be empty or hold + or #", t.name));
                    }
                    _ => {}
                },
                Kind::Eip => {
                    if self.mode == Mode::Client && t.cip_tag.is_none() {
                        return Err(format!("tag `{}`: EtherNet/IP client tags need `cip_tag`", t.name));
                    }
                    let name = t.cip_tag.as_deref().unwrap_or(&t.name);
                    crate::services::eip::tag_path(name).map_err(|e| format!("tag `{}`: {e}", t.name))?;
                }
                Kind::Iolink => {
                    if t.port.is_none() {
                        return Err(format!("tag `{}`: IO-Link tags need `port`", t.name));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}
