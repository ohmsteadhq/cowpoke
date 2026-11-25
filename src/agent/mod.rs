//! Edge agents: a Cowpoke on the plant network that dials out to a hub (hosted or
//! self-hosted Cowpoke) and runs the services the hub assigns to it.
//!
//! The link is one WebSocket at `GET /api/agents/connect`, authorized with the agent's
//! token (`Authorization: Bearer …`). Messages are JSON, tagged by `type`.

pub mod edge;
pub mod hub;

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manager::Status;
use crate::model::ServiceDef;
use crate::tags::Quality;

/// Messages from the hub to an agent.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToAgent {
    /// Every service assigned to this agent. Sent on connect and after every change.
    Config { services: Vec<ServiceDef> },
    /// A user wrote a tag on the hub.
    Write {
        req_id: u64,
        service: String,
        tag: String,
        value: Value,
    },
    Request { req_id: u64, op: Op },
    Ping,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// The trust list of an OPC UA service running on the agent.
    CertList { service: String },
    /// `trust`, `reject` or `delete` one certificate there.
    CertAction {
        service: String,
        file: String,
        action: String,
    },
}

/// Messages from an agent to the hub.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToHub {
    Hello {
        version: String,
        hostname: String,
        /// Services already running (from the cached configuration): a worker keeps
        /// them when the hub restarts or fails over, instead of them starting elsewhere.
        #[serde(default)]
        running: Vec<String>,
    },
    /// Tag values, coalesced (and a full snapshot right after connecting).
    Values { batch: Vec<ValueMsg> },
    /// Status of each assigned service.
    Status { statuses: HashMap<String, Status> },
    /// Historized points recorded while the link was down: (service, tag, ms since epoch, value).
    Backfill { points: Vec<(String, String, i64, f64)> },
    Reply {
        req_id: u64,
        result: Result<Value, String>,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ValueMsg {
    pub service: String,
    pub tag: String,
    pub value: Value,
    pub quality: Quality,
    pub ts: DateTime<Utc>,
}

/// SHA-256 of a token, hex. Agent tokens are 32 random bytes, so a plain hash is enough.
pub fn token_hash(token: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(token.as_bytes()))
}
