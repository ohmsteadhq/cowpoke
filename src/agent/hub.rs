//! The hub's side of edge agents: the agent registry (`agents.json`), the WebSocket
//! each agent keeps open, and relaying writes, values, statuses and backfill.
//!
//! An agent can be a **pool**: any number of worker containers connect with the pool's
//! one token, each under its own node name. Services name the pool (`agent: workers`);
//! the hub places each on one online worker, keeps it there, and moves it to another
//! when that worker stops. A connection's key is the agent id, or `pool/node` for a worker.

use std::collections::{HashMap, HashSet};
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
    /// A pool of workers instead of one agent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pool: bool,
}

/// Who a token belongs to.
pub enum Caller {
    Agent(String),
    Pool(String),
}

/// How long after the hub starts it waits for workers to report what they already run
/// before placing services on others (after a restart or a failover).
const PLACE_GRACE: Duration = Duration::from_secs(10);
/// After a worker joins a pool, how long new services wait before being placed, so that
/// workers started together (`--scale worker=3`) share them instead of the first taking all.
const SETTLE: Duration = Duration::from_secs(3);

struct Conn {
    conn_id: u64,
    tx: mpsc::UnboundedSender<ToAgent>,
    cancel: CancellationToken,
    version: String,
    hostname: String,
    since: DateTime<Utc>,
    /// The pool, for a worker.
    pool: Option<String>,
    /// Services the worker said it was already running when it connected.
    running: Vec<String>,
    /// A worker gets its configuration after its hello (so its claims count).
    ready: bool,
}

type Pending = DashMap<u64, oneshot::Sender<Result<Value, String>>>;

pub struct Hub {
    path: PathBuf,
    recs: Mutex<Vec<AgentRec>>,
    conns: DashMap<String, Conn>,
    last_seen: DashMap<String, DateTime<Utc>>,
    pending: Pending,
    next_id: AtomicU64,
    /// Pooled service id → the worker (`pool/node`) running it.
    placement: Mutex<HashMap<String, String>>,
    started: std::time::Instant,
    /// When a worker last joined each pool.
    joined: Mutex<HashMap<String, std::time::Instant>>,
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
            placement: Mutex::new(HashMap::new()),
            started: std::time::Instant::now(),
            joined: Mutex::new(HashMap::new()),
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
                if r.pool {
                    return self.pool_json(r, cfg);
                }
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

    fn pool_json(&self, r: &AgentRec, cfg: &Config) -> Value {
        let placement = self.placement.lock().clone();
        let mut members: Vec<Value> = self
            .conns
            .iter()
            .filter(|c| c.pool.as_deref() == Some(r.id.as_str()))
            .map(|c| {
                let key = c.key().clone();
                let services: Vec<&String> = placement.iter().filter(|(_, m)| **m == key).map(|(s, _)| s).collect();
                json!({
                    "node": key.split_once('/').map(|x| x.1).unwrap_or_default(),
                    "version": c.version, "hostname": c.hostname, "connected_since": c.since,
                    "services": services,
                })
            })
            .collect();
        members.sort_by(|a, b| a["node"].as_str().cmp(&b["node"].as_str()));
        json!({
            "id": r.id,
            "name": if r.name.is_empty() { r.id.clone() } else { r.name.clone() },
            "pool": true,
            "online": !members.is_empty(),
            "members": members,
            "last_seen": self.last_seen.get(&r.id).map(|t| *t),
            "created": r.created,
            "services": cfg.services.iter().filter(|s| s.agent.as_deref() == Some(r.id.as_str())).count(),
        })
    }

    /// Register an agent. Returns its token, which is never stored or shown again.
    pub fn create(&self, id: &str, name: &str) -> Result<String, String> {
        self.create_kind(id, name, false)
    }

    /// Register an agent, or a pool of workers. Returns its token.
    pub fn create_kind(&self, id: &str, name: &str, pool: bool) -> Result<String, String> {
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
            pool,
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

    /// Drop an agent's connection, or every worker of a pool.
    fn kick(&self, id: &str) {
        let keys: Vec<String> = self
            .conns
            .iter()
            .filter(|c| c.key() == id || c.pool.as_deref() == Some(id))
            .map(|c| c.key().clone())
            .collect();
        for k in keys {
            if let Some((_, c)) = self.conns.remove(&k) {
                c.cancel.cancel();
            }
        }
    }

    /// The agent or pool a token belongs to.
    pub fn authenticate(&self, token: &str) -> Option<Caller> {
        let h = token_hash(token);
        self.recs
            .lock()
            .iter()
            .find(|r| constant_eq(&r.token_sha256, &h))
            .map(|r| if r.pool { Caller::Pool(r.id.clone()) } else { Caller::Agent(r.id.clone()) })
    }

    pub fn is_pool(&self, id: &str) -> bool {
        self.recs.lock().iter().any(|r| r.id == id && r.pool)
    }

    /// Place pooled services, then send each connected agent and worker its services.
    pub fn push_config(&self, cfg: &Config) {
        self.place(cfg);
        self.send_config(cfg);
    }

    /// Re-place pooled services (a worker came or went); send configs if anything moved.
    pub fn tick(&self, cfg: &Config) -> bool {
        let moved = self.place(cfg);
        if moved {
            self.send_config(cfg);
        }
        moved
    }

    fn send_config(&self, cfg: &Config) {
        let placement = self.placement.lock().clone();
        for c in self.conns.iter().filter(|c| c.ready) {
            let _ = c.tx.send(config_for(cfg, c.key(), &placement));
        }
    }

    /// Where a pooled service runs: the worker key, if placed.
    pub fn placed(&self, service: &str) -> Option<String> {
        self.placement.lock().get(service).cloned()
    }

    /// Keep each pooled service on its worker while that worker is connected; put the rest
    /// on the worker that already runs it, or else the least loaded one. Returns true if
    /// any placement changed.
    fn place(&self, cfg: &Config) -> bool {
        self.place_with(cfg, self.started.elapsed() < PLACE_GRACE)
    }

    /// `wait`: hold back services no worker claims (start-up grace, or a pool settling).
    fn place_with(&self, cfg: &Config, wait: bool) -> bool {
        let settling: HashSet<String> = self
            .joined
            .lock()
            .iter()
            .filter(|(_, t)| t.elapsed() < SETTLE)
            .map(|(p, _)| p.clone())
            .collect();
        let pools: HashSet<String> = self.recs.lock().iter().filter(|r| r.pool).map(|r| r.id.clone()).collect();
        // Connected workers keep their services, even in the moment between a reconnect
        // and its hello; only ready ones (hello received) get new services.
        let connected: HashSet<String> = self.conns.iter().filter(|c| c.pool.is_some()).map(|c| c.key().clone()).collect();
        let mut workers: HashMap<String, Vec<(String, Vec<String>)>> = HashMap::new();
        for c in self.conns.iter().filter(|c| c.ready) {
            if let Some(p) = &c.pool {
                workers.entry(p.clone()).or_default().push((c.key().clone(), c.running.clone()));
            }
        }
        for w in workers.values_mut() {
            w.sort();
        }
        let mut placement = self.placement.lock();
        let before = placement.clone();
        let wanted: HashMap<&str, &str> = cfg
            .services
            .iter()
            .filter(|s| s.enabled)
            .filter_map(|s| s.agent.as_deref().filter(|a| pools.contains(*a)).map(|a| (s.id.as_str(), a)))
            .collect();
        // Forget services that are gone, disabled or moved to another pool, and workers that left.
        placement.retain(|svc, key| {
            wanted.get(svc.as_str()).is_some_and(|pool| key.split_once('/').is_some_and(|k| k.0 == *pool))
                && connected.contains(key)
        });
        let mut ids: Vec<(&str, &str)> = wanted.into_iter().collect();
        ids.sort();
        for (svc, pool) in ids {
            if placement.contains_key(svc) {
                continue;
            }
            let Some(ws) = workers.get(pool).filter(|w| !w.is_empty()) else { continue };
            let claimed = ws.iter().find(|(_, run)| run.iter().any(|r| r == svc)).map(|(k, _)| k.clone());
            let pick = match claimed {
                Some(k) => k,
                // Right after the hub starts, workers that already run a service are still
                // connecting: wait for them instead of starting a second copy elsewhere.
                // While workers are joining a pool, wait for the rest to arrive.
                None if wait || settling.contains(pool) => continue,
                None => {
                    let load = |k: &str| placement.values().filter(|v| *v == k).count();
                    ws.iter().map(|(k, _)| k).min_by_key(|k| (load(k), (*k).clone())).cloned().unwrap()
                }
            };
            placement.insert(svc.to_string(), pick);
        }
        *placement != before
    }

    /// Spread a pool's services evenly over its online workers (moves the fewest needed).
    /// Returns how many services moved.
    pub fn rebalance(&self, pool: &str, cfg: &Config) -> Result<usize, String> {
        if !self.is_pool(pool) {
            return Err("not a pool".into());
        }
        let workers: Vec<String> = self
            .conns
            .iter()
            .filter(|c| c.ready && c.pool.as_deref() == Some(pool))
            .map(|c| c.key().clone())
            .collect();
        if workers.is_empty() {
            return Err("no worker of this pool is online".into());
        }
        let moved = {
            let mut placement = self.placement.lock();
            let total = placement.values().filter(|k| workers.contains(k)).count();
            let most = total.div_ceil(workers.len());
            let mut moved = 0;
            for w in &workers {
                let mut svcs: Vec<String> = placement.iter().filter(|(_, k)| *k == w).map(|(s, _)| s.clone()).collect();
                svcs.sort();
                while svcs.len() > most {
                    placement.remove(&svcs.pop().unwrap());
                    moved += 1;
                }
            }
            moved
        };
        // The freed services go to the least loaded workers, now.
        self.joined.lock().remove(pool);
        self.place_with(cfg, false);
        self.send_config(cfg);
        Ok(moved)
    }

    /// The connection that runs `service` for agent or pool `agent`.
    fn route(&self, agent: &str, service: &str) -> Result<String, String> {
        if self.is_pool(agent) {
            return self
                .placed(service)
                .ok_or_else(|| format!("no worker of pool {agent} is running {service} right now"));
        }
        Ok(agent.to_string())
    }

    async fn ask(&self, agent: &str, service: &str, make: impl FnOnce(u64) -> ToAgent) -> Result<Value, String> {
        let agent = &self.route(agent, service)?;
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
        let (svc, tag) = (service.to_string(), tag.to_string());
        self.ask(agent, service, move |req_id| ToAgent::Write { req_id, service: svc, tag, value })
            .await
    }

    pub async fn request(&self, agent: &str, op: Op) -> Result<Value, String> {
        let service = match &op {
            Op::CertList { service } | Op::CertAction { service, .. } => service.clone(),
        };
        self.ask(agent, &service, move |req_id| ToAgent::Request { req_id, op }).await
    }
}

/// Does connection `key` run `service`? A plain agent runs its own services; a worker
/// (`pool/node`) runs the pool's services placed on it.
fn runs(cfg_agent: Option<&str>, service: &str, key: &str, placement: &HashMap<String, String>) -> bool {
    match key.split_once('/') {
        None => cfg_agent == Some(key),
        Some((pool, _)) => cfg_agent == Some(pool) && placement.get(service).is_some_and(|k| k == key),
    }
}

fn config_for(cfg: &Config, key: &str, placement: &HashMap<String, String>) -> ToAgent {
    ToAgent::Config {
        services: cfg
            .services
            .iter()
            .filter(|s| runs(s.agent.as_deref(), &s.id, key, placement))
            .cloned()
            .collect(),
    }
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Serve one agent's (or worker's) WebSocket until it closes, goes silent or is replaced.
/// `agent` is the connection key: the agent id, or `pool/node` for a worker of `pool`.
pub async fn serve(app: App, agent: String, pool: Option<String>, socket: WebSocket) {
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
            pool: pool.clone(),
            running: vec![],
            ready: pool.is_none(),
        },
    ) {
        tracing::info!(agent, "replacing an older connection");
        old.cancel.cancel();
    }
    tracing::info!(agent, "agent connected");
    hub.last_seen.insert(pool.clone().unwrap_or_else(|| agent.clone()), Utc::now());
    if pool.is_none() {
        let _ = tx.send(config_for(&app.cfg.get(), &agent, &HashMap::new()));
    }

    let (mut sink, mut stream) = socket.split();
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    // Workers report every 2 s, so a silent one is noticed (and its services moved) sooner.
    let silence = Duration::from_secs(if pool.is_some() { 15 } else { 60 });
    // One deadline for the whole connection, pushed back only when a frame
    // arrives: rebuilding it inside select! would restart it whenever the ping
    // tick or an outgoing message won, so a silent agent was never dropped.
    let deadline = tokio::time::sleep(silence);
    tokio::pin!(deadline);
    let owns = |cfg: &Config, service: &str| {
        let placement = hub.placement.lock();
        cfg.services
            .iter()
            .any(|s| s.id == service && runs(s.agent.as_deref(), &s.id, &agent, &placement))
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
            _ = &mut deadline => {
                tracing::warn!(agent, "agent silent for 60 s; dropping");
                break;
            }
            next = stream.next() => {
                let msg = match next {
                    None | Some(Err(_)) => break,
                    Some(Ok(m)) => m,
                };
                deadline.as_mut().reset(tokio::time::Instant::now() + silence);
                hub.last_seen.insert(pool.clone().unwrap_or_else(|| agent.clone()), Utc::now());
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
                    ToHub::Hello { version, hostname, running } => {
                        let mut became_ready = false;
                        if let Some(mut c) = hub.conns.get_mut(&agent) {
                            if c.conn_id == conn_id {
                                c.version = version;
                                c.hostname = hostname;
                                c.running = running;
                                became_ready = !c.ready;
                                c.ready = true;
                            }
                        }
                        if became_ready {
                            if let Some(p) = &pool {
                                hub.joined.lock().insert(p.clone(), std::time::Instant::now());
                            }
                            // A worker: place what it claims (and anything waiting), then send configs.
                            hub.push_config(&cfg);
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
        let cfg = app.cfg.get();
        let lost: Vec<String> = {
            let placement = hub.placement.lock();
            cfg.services
                .iter()
                .filter(|s| runs(s.agent.as_deref(), &s.id, &agent, &placement))
                .map(|s| s.id.clone())
                .collect()
        };
        hub.conns.remove_if(&agent, |_, c| c.conn_id == conn_id);
        let when = Utc::now().format("%Y-%m-%d %H:%M UTC");
        for s in cfg.services.iter().filter(|s| lost.contains(&s.id)) {
            for t in &s.tags {
                app.tags.set_bad(&s.id, &t.name);
            }
            if s.enabled {
                let detail = match &pool {
                    Some(p) => format!("worker {agent} stopped at {when}; moving to another worker of pool {p}"),
                    None => format!("agent {agent} offline since {when}"),
                };
                app.mgr.set_remote_status(&s.id, State::Error, detail);
            }
        }
        if pool.is_some() {
            // Move its services to the other workers now.
            if hub.tick(&cfg) {
                tracing::info!(agent, "moved {} services to other workers", lost.len());
            }
        }
        tracing::info!(agent, "agent disconnected");
    }
    let _ = sink.close().await;
}
