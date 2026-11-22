//! The hub's side of edge agents: the agent registry (`agents.json`), the WebSocket
//! each agent keeps open, and relaying writes, values, statuses and backfill.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::{Op, ToAgent, ToHub, token_hash};
use crate::manager::State;
use crate::model::{Config, valid_id};
use crate::tags::Origin;
use crate::web::App;

#[derive(Serialize, Deserialize, Clone, Debug)]
struct AgentRec {
    id: String,
    name: String,
    token_sha256: String,
    created: DateTime<Utc>,
}

struct Conn {
    conn_id: u64,
    tx: mpsc::UnboundedSender<ToAgent>,
    cancel: CancellationToken,
    version: String,
    hostname: String,
    since: DateTime<Utc>,
}

type Pending = DashMap<u64, oneshot::Sender<Result<Value, String>>>;

pub struct Hub {
    path: PathBuf,
    recs: Mutex<Vec<AgentRec>>,
    conns: DashMap<String, Conn>,
    last_seen: DashMap<String, DateTime<Utc>>,
    pending: Pending,
    next_id: AtomicU64,
}

impl Hub {
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        let recs = if path.exists() {
            serde_json::from_str(&std::fs::read_to_string(path)?)?
        } else {
            vec![]
        };
        Ok(Arc::new(Self {
            path: path.to_path_buf(),
            recs: Mutex::new(recs),
            conns: DashMap::new(),
            last_seen: DashMap::new(),
            pending: DashMap::new(),
            next_id: AtomicU64::new(1),
        }))
    }

    fn save(&self, recs: &[AgentRec]) -> Result<(), String> {
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(recs).unwrap_or_default())
            .and_then(|_| std::fs::rename(&tmp, &self.path))
            .map_err(|e| e.to_string())
    }

    pub fn exists(&self, id: &str) -> bool {
        self.recs.lock().iter().any(|r| r.id == id)
    }

    /// The agents, as the API lists them.
    pub fn list(&self, cfg: &Config) -> Vec<Value> {
        self.recs
            .lock()
            .iter()
            .map(|r| {
                let c = self.conns.get(&r.id);
                json!({
                    "id": r.id,
                    "name": if r.name.is_empty() { r.id.clone() } else { r.name.clone() },
                    "online": c.is_some(),
                    "version": c.as_ref().map(|c| c.version.clone()),
                    "hostname": c.as_ref().map(|c| c.hostname.clone()),
                    "connected_since": c.as_ref().map(|c| c.since),
                    "last_seen": self.last_seen.get(&r.id).map(|t| *t),
                    "created": r.created,
                    "services": cfg.services.iter().filter(|s| s.agent.as_deref() == Some(r.id.as_str())).count(),
                })
            })
            .collect()
    }

    /// Register an agent. Returns its token, which is never stored or shown again.
    pub fn create(&self, id: &str, name: &str) -> Result<String, String> {
        if !valid_id(id) {
            return Err("agent id: use 1-64 of a-z, 0-9, - and _".into());
        }
        let mut recs = self.recs.lock();
        if recs.iter().any(|r| r.id == id) {
            return Err(format!("an agent with id `{id}` already exists"));
        }
        let token = crate::auth::random_token(32);
        recs.push(AgentRec {
            id: id.to_string(),
            name: name.trim().to_string(),
            token_sha256: token_hash(&token),
            created: Utc::now(),
        });
        self.save(&recs)?;
        Ok(token)
    }

    /// Issue a new token; the old one stops working and a live connection is dropped.
    pub fn rotate(&self, id: &str) -> Result<String, String> {
        let token = crate::auth::random_token(32);
        {
            let mut recs = self.recs.lock();
            let r = recs.iter_mut().find(|r| r.id == id).ok_or("no such agent")?;
            r.token_sha256 = token_hash(&token);
            self.save(&recs)?;
        }
        self.kick(id);
        Ok(token)
    }

    pub fn delete(&self, id: &str, cfg: &Config) -> Result<(), String> {
        let used: Vec<&str> = cfg
            .services
            .iter()
            .filter(|s| s.agent.as_deref() == Some(id))
            .map(|s| s.id.as_str())
            .collect();
        if !used.is_empty() {
            return Err(format!(
                "move or delete its services first: {}",
                used.join(", ")
            ));
        }
        let mut recs = self.recs.lock();
        let before = recs.len();
        recs.retain(|r| r.id != id);
        if recs.len() == before {
            return Err("no such agent".into());
        }
        self.save(&recs)?;
        drop(recs);
        self.kick(id);
        self.last_seen.remove(id);
        Ok(())
    }

    fn kick(&self, id: &str) {
        if let Some((_, c)) = self.conns.remove(id) {
            c.cancel.cancel();
        }
    }

    /// The agent a token belongs to.
    pub fn authenticate(&self, token: &str) -> Option<String> {
        let h = token_hash(token);
        self.recs
            .lock()
            .iter()
            .find(|r| constant_eq(&r.token_sha256, &h))
            .map(|r| r.id.clone())
    }

    /// Send each connected agent the services assigned to it.
    pub fn push_config(&self, cfg: &Config) {
        for c in self.conns.iter() {
            let _ = c.tx.send(config_for(cfg, c.key()));
        }
    }

    async fn ask(&self, agent: &str, make: impl FnOnce(u64) -> ToAgent) -> Result<Value, String> {
        let tx = self
            .conns
            .get(agent)
            .map(|c| c.tx.clone())
            .ok_or_else(|| format!("agent {agent} is offline"))?;
        let req_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, rx) = oneshot::channel();
        self.pending.insert(req_id, reply);
        if tx.send(make(req_id)).is_err() {
            self.pending.remove(&req_id);
            return Err(format!("agent {agent} is offline"));
        }
        match tokio::time::timeout(Duration::from_secs(20), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(format!("agent {agent} disconnected")),
            Err(_) => {
                self.pending.remove(&req_id);
                Err(format!("agent {agent} didn't answer in time"))
            }
        }
    }

    pub async fn write(&self, agent: &str, service: &str, tag: &str, value: Value) -> Result<Value, String> {
        let (service, tag) = (service.to_string(), tag.to_string());
        self.ask(agent, move |req_id| ToAgent::Write { req_id, service, tag, value })
            .await
    }

    pub async fn request(&self, agent: &str, op: Op) -> Result<Value, String> {
        self.ask(agent, move |req_id| ToAgent::Request { req_id, op }).await
    }
}

fn config_for(cfg: &Config, agent: &str) -> ToAgent {
    ToAgent::Config {
        services: cfg
            .services
            .iter()
            .filter(|s| s.agent.as_deref() == Some(agent))
            .cloned()
            .collect(),
    }
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Serve one agent's WebSocket until it closes, goes silent or is replaced.
pub async fn serve(app: App, agent: String, socket: WebSocket) {
    let hub = app.agents.clone();
    let conn_id = hub.next_id.fetch_add(1, Ordering::Relaxed);
    let cancel = CancellationToken::new();
    let (tx, mut rx) = mpsc::unbounded_channel::<ToAgent>();
    if let Some(old) = hub.conns.insert(
        agent.clone(),
        Conn {
            conn_id,
            tx: tx.clone(),
            cancel: cancel.clone(),
            version: String::new(),
            hostname: String::new(),
            since: Utc::now(),
        },
    ) {
        tracing::info!(agent, "replacing an older connection");
        old.cancel.cancel();
    }
    tracing::info!(agent, "agent connected");
    hub.last_seen.insert(agent.clone(), Utc::now());
    let _ = tx.send(config_for(&app.cfg.get(), &agent));

    let (mut sink, mut stream) = socket.split();
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    let silence = Duration::from_secs(60);
    let owns = |cfg: &Config, service: &str| {
        cfg.services
            .iter()
            .any(|s| s.id == service && s.agent.as_deref() == Some(agent.as_str()))
    };

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            Some(msg) = rx.recv() => {
                let text = serde_json::to_string(&msg).unwrap_or_default();
                if sink.send(Message::Text(text.into())).await.is_err() { break; }
            }
            _ = ping.tick() => {
                let _ = tx.send(ToAgent::Ping);
            }
            next = tokio::time::timeout(silence, stream.next()) => {
                let msg = match next {
                    Err(_) => { tracing::warn!(agent, "agent silent for 60 s; dropping"); break; }
                    Ok(None) | Ok(Some(Err(_))) => break,
                    Ok(Some(Ok(m))) => m,
                };
                hub.last_seen.insert(agent.clone(), Utc::now());
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Close(_) => break,
                    _ => continue,
                };
                let msg: ToHub = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => { tracing::warn!(agent, "bad message: {e}"); continue; }
                };
                let cfg = app.cfg.get();
                match msg {
                    ToHub::Hello { version, hostname } => {
                        if let Some(mut c) = hub.conns.get_mut(&agent) {
                            if c.conn_id == conn_id { c.version = version; c.hostname = hostname; }
                        }
                    }
                    ToHub::Values { batch } => {
                        for v in batch.into_iter().filter(|v| owns(&cfg, &v.service)) {
                            app.tags.set_at(&v.service, &v.tag, v.value, v.quality, v.ts, Origin::Service);
                        }
                    }
                    ToHub::Status { statuses } => {
                        for (id, st) in statuses.into_iter().filter(|(id, _)| owns(&cfg, id)) {
                            app.mgr.set_remote_status_full(&id, st);
                        }
                    }
                    ToHub::Backfill { points } => {
                        let n = points.len();
                        app.hist.insert(points.into_iter().filter(|p| owns(&cfg, &p.0)).collect());
                        tracing::info!(agent, "backfilled {n} history points");
                    }
                    ToHub::Reply { req_id, result } => {
                        if let Some((_, reply)) = hub.pending.remove(&req_id) {
                            let _ = reply.send(result);
                        }
                    }
                }
            }
        }
    }

    // A connection replaced by a newer one just leaves; otherwise (closed, silent,
    // kicked by a token change or deletion) its services go offline.
    let replaced = hub.conns.get(&agent).is_some_and(|c| c.conn_id != conn_id);
    if !replaced {
        hub.conns.remove_if(&agent, |_, c| c.conn_id == conn_id);
        let cfg = app.cfg.get();
        let when = Utc::now().format("%Y-%m-%d %H:%M UTC");
        for s in cfg.services.iter().filter(|s| s.agent.as_deref() == Some(agent.as_str())) {
            for t in &s.tags {
                app.tags.set_bad(&s.id, &t.name);
            }
            if s.enabled {
                app.mgr
                    .set_remote_status(&s.id, State::Error, format!("agent {agent} offline since {when}"));
            }
        }
        tracing::info!(agent, "agent disconnected");
    }
    let _ = sink.close().await;
}
