//! `cowpoke agent`: run the services a hub assigns, on the plant network.
//!
//! The agent dials out (no inbound firewall holes), keeps its services running when the
//! hub is unreachable, records history locally while offline and backfills it on reconnect.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::sync::CancellationToken;

use super::{Op, ToAgent, ToHub, ValueMsg};
use crate::historian::Historian;
use crate::manager::Manager;
use crate::model::{Config, Kind, ServiceDef};
use crate::services::opcua;
use crate::tags::TagStore;

/// How far back a reconnect backfills.
const MAX_BACKFILL: chrono::Duration = chrono::Duration::days(7);
const SILENCE: Duration = Duration::from_secs(60);

#[derive(Serialize, Deserialize, Default)]
struct LinkState {
    /// The last moment the hub was known to be listening. Backfill starts here.
    last_connected: Option<DateTime<Utc>>,
}

pub struct Edge {
    data_dir: PathBuf,
    pub(crate) tags: TagStore,
    pub(crate) mgr: Arc<Manager>,
    pub(crate) hist: Arc<Historian>,
    cfg: Mutex<Config>,
    state: Mutex<LinkState>,
    /// The name this agent gives the hub; a pool tells its workers apart by it.
    node: String,
}

impl Edge {
    pub fn new(data_dir: &Path) -> Result<Arc<Self>> {
        Self::with_node(data_dir, node_name())
    }

    pub fn with_node(data_dir: &Path, node: String) -> Result<Arc<Self>> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("can't create {}", data_dir.display()))?;
        let tags = TagStore::new();
        let edge = Arc::new(Self {
            data_dir: data_dir.to_path_buf(),
            mgr: Manager::new_edge(tags.clone(), data_dir.to_path_buf()),
            hist: Historian::new(data_dir),
            tags,
            cfg: Mutex::new(Config::default()),
            state: Mutex::new(
                std::fs::read_to_string(data_dir.join("agent-state.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or_default(),
            ),
            node,
        });
        Ok(edge)
    }

    fn cache_path(&self) -> PathBuf {
        self.data_dir.join("agent-config.yaml")
    }

    fn save_state(&self) {
        let text = serde_json::to_string(&*self.state.lock()).unwrap_or_default();
        let _ = std::fs::write(self.data_dir.join("agent-state.json"), text);
    }

    /// Start the services from the last configuration the hub sent, if any.
    pub async fn start_cached(&self) {
        let Ok(text) = std::fs::read_to_string(self.cache_path()) else {
            return;
        };
        match crate::store::parse(&text) {
            Ok(cfg) => {
                tracing::info!("starting {} services from the cached configuration", cfg.services.len());
                self.apply(cfg, false).await;
            }
            Err(e) => tracing::warn!("ignoring the cached configuration: {e}"),
        }
    }

    async fn apply(&self, cfg: Config, save: bool) {
        if save {
            if let Ok(text) = serde_yaml::to_string(&cfg) {
                let _ = std::fs::write(self.cache_path(), text);
            }
        }
        self.hist.configure(&cfg);
        self.mgr.apply(&cfg).await;
        *self.cfg.lock() = cfg;
    }

    pub(crate) fn service(&self, id: &str) -> Option<ServiceDef> {
        self.cfg.lock().services.iter().find(|s| s.id == id).cloned()
    }

    fn snapshot(&self) -> Vec<ValueMsg> {
        let ids: Vec<String> = self.cfg.lock().services.iter().map(|s| s.id.clone()).collect();
        ids.into_iter()
            .flat_map(|id| {
                self.tags
                    .service_tags(&id)
                    .into_iter()
                    .map(move |(tag, st)| ValueMsg {
                        service: id.clone(),
                        tag,
                        value: st.value,
                        quality: st.quality,
                        ts: st.ts,
                    })
            })
            .collect()
    }

    fn backfill(&self, since: DateTime<Utc>, until: DateTime<Utc>) -> Vec<(String, String, i64, f64)> {
        let since = since.max(until - MAX_BACKFILL);
        let (f, t) = (since.timestamp_millis(), until.timestamp_millis());
        let mut out = vec![];
        for (s, tag) in self.hist.tracked() {
            for (ts, v) in self.hist.raw(&s, &tag, f, t) {
                out.push((s.clone(), tag.clone(), ts, v));
            }
        }
        out
    }

    async fn handle_op(&self, op: Op) -> Result<Value, String> {
        let (service, f): (String, Box<dyn FnOnce(&Path) -> Result<Value, String> + Send>) = match op {
            Op::CertList { service } => (
                service,
                Box::new(|dir: &Path| {
                    let own = opcua::pki_dir(dir).join("own").join("cert.der");
                    let own_tp = opcua::crypto_thumbprint(&own);
                    Ok(json!({"own_thumbprint": own_tp, "certificates": opcua::list_certs(dir)}))
                }),
            ),
            Op::CertAction { service, file, action } => (
                service,
                Box::new(move |dir: &Path| opcua::set_cert(dir, &file, &action).map(|_| Value::Null)),
            ),
        };
        let def = self.service(&service).ok_or("no such service on this agent")?;
        if def.kind != Kind::Opcua {
            return Err("only OPC UA services have certificates".into());
        }
        f(&self.data_dir.join("services").join(&service))
    }
}

/// `COWPOKE_HUB` (`https://cowpoke.example.com`) → the agent WebSocket URL.
pub fn ws_url(hub: &str) -> Result<String> {
    let hub = hub.trim().trim_end_matches('/');
    let rest = if let Some(r) = hub.strip_prefix("https://") {
        format!("wss://{r}")
    } else if let Some(r) = hub.strip_prefix("http://") {
        format!("ws://{r}")
    } else if hub.starts_with("ws://") || hub.starts_with("wss://") {
        hub.to_string()
    } else {
        bail!("COWPOKE_HUB must start with https:// or http:// (got `{hub}`)");
    };
    Ok(format!("{rest}/api/agents/connect"))
}

/// Run the agent forever (until `cancel`). `hub` is one URL, or several separated by
/// commas (an active hub and its standby): the agent tries them in turn.
pub async fn run(edge: Arc<Edge>, hub: String, token: String, cancel: CancellationToken) -> Result<()> {
    let urls: Vec<String> = hub.split(',').filter(|h| !h.trim().is_empty()).map(ws_url).collect::<Result<_>>()?;
    if urls.is_empty() {
        bail!("COWPOKE_HUB is empty");
    }
    tokio::spawn(edge.hist.clone().run(edge.tags.clone()));
    edge.start_cached().await;
    let mut backoff = Duration::from_secs(1);
    let mut next = 0;
    loop {
        let started = tokio::time::Instant::now();
        let url = urls[next % urls.len()].clone();
        let res = tokio::select! {
            _ = cancel.cancelled() => break,
            r = session(&edge, &url, &token, &cancel, SILENCE) => r,
        };
        if cancel.is_cancelled() {
            break;
        }
        if let Err(e) = res {
            tracing::warn!("hub link: {e:#}");
        }
        if started.elapsed() > Duration::from_secs(30) {
            backoff = Duration::from_secs(1);
        }
        // With several hubs, try the next one at once; back off after a full round.
        next += 1;
        if urls.len() > 1 && next % urls.len() != 0 {
            continue;
        }
        tracing::info!("reconnecting in {backoff:?}");
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(backoff) => {}
        }
        // With a standby hub, retry soon: the other one may be taking over right now.
        let most = if urls.len() > 1 { 5 } else { 30 };
        backoff = (backoff * 2).min(Duration::from_secs(most));
    }
    edge.save_state();
    edge.mgr.shutdown().await;
    edge.hist.flush();
    Ok(())
}

/// One connection to the hub, until it drops.
/// `silence` is how long the hub may send nothing before the link counts as dead.
async fn session(edge: &Arc<Edge>, url: &str, token: &str, cancel: &CancellationToken, silence: Duration) -> Result<()> {
    let mut req = url.into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse()?);
    // A worker of a pool is known by its node name (the container's hostname by default).
    req.headers_mut().insert("X-Cowpoke-Node", edge.node.parse()?);
    let (ws, _) = tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(req))
        .await
        .map_err(|_| anyhow!("timed out connecting to {url}"))?
        .with_context(|| format!("connecting to {url}"))?;
    let connected_at = Utc::now();
    tracing::info!("connected to {url}");
    let (mut sink, mut stream) = ws.split();

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ToHub>();
    let send = |m: ToHub| {
        let _ = out_tx.send(m);
    };
    send(ToHub::Hello {
        version: env!("CARGO_PKG_VERSION").to_string(),
        hostname: hostname(),
        running: edge.cfg.lock().services.iter().filter(|s| s.enabled).map(|s| s.id.clone()).collect(),
    });

    let since = edge.state.lock().last_connected;
    let mut events = edge.tags.subscribe();
    let mut got_config = false;
    let mut buffer: HashMap<(String, String), ValueMsg> = HashMap::new();
    let mut flush = tokio::time::interval(Duration::from_millis(200));
    let mut status = tokio::time::interval(Duration::from_secs(2));
    let mut alive = tokio::time::interval(Duration::from_secs(10));
    // One deadline for the whole session, pushed back only when a frame
    // arrives (the ticks above must not restart it).
    let deadline = tokio::time::sleep(silence);
    tokio::pin!(deadline);

    let result: Result<()> = loop {
        tokio::select! {
            _ = cancel.cancelled() => break Ok(()),
            Some(m) = out_rx.recv() => {
                let text = serde_json::to_string(&m)?;
                if let Err(e) = sink.send(Message::Text(text.into())).await { break Err(e.into()); }
            }
            ev = events.recv() => match ev {
                Ok(e) => {
                    if got_config {
                        buffer.insert((e.service.clone(), e.tag.clone()), ValueMsg {
                            service: e.service, tag: e.tag, value: e.state.value, quality: e.state.quality, ts: e.state.ts,
                        });
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Too many changes to keep up: send everything as it is now.
                    for v in edge.snapshot() { buffer.insert((v.service.clone(), v.tag.clone()), v); }
                }
                Err(_) => break Ok(()),
            },
            _ = flush.tick() => {
                if !buffer.is_empty() {
                    send(ToHub::Values { batch: buffer.drain().map(|(_, v)| v).collect() });
                }
            }
            _ = status.tick() => {
                if got_config {
                    let ids: Vec<String> = edge.cfg.lock().services.iter().map(|s| s.id.clone()).collect();
                    let all = edge.mgr.all_statuses();
                    send(ToHub::Status { statuses: ids.into_iter().filter_map(|id| all.get(&id).cloned().map(|s| (id, s))).collect() });
                }
            }
            _ = alive.tick() => {
                edge.state.lock().last_connected = Some(Utc::now());
                edge.save_state();
            }
            _ = &mut deadline => break Err(anyhow!("hub silent for 60 s")),
            next = stream.next() => {
                let msg = match next {
                    None => break Err(anyhow!("hub closed the connection")),
                    Some(Err(e)) => break Err(e.into()),
                    Some(Ok(m)) => m,
                };
                deadline.as_mut().reset(tokio::time::Instant::now() + silence);
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Close(_) => break Err(anyhow!("hub closed the connection")),
                    _ => continue,
                };
                let msg: ToAgent = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => { tracing::warn!("bad message from hub: {e}"); continue; }
                };
                match msg {
                    ToAgent::Ping => {}
                    ToAgent::Config { services } => {
                        tracing::info!("hub assigned {} services", services.len());
                        edge.apply(Config { services }, true).await;
                        if !got_config {
                            got_config = true;
                            send(ToHub::Values { batch: edge.snapshot() });
                            // History recorded while the link was down.
                            if let Some(since) = since {
                                let pts = edge.backfill(since, connected_at);
                                if !pts.is_empty() {
                                    tracing::info!("backfilling {} history points", pts.len());
                                }
                                for chunk in pts.chunks(5000) {
                                    send(ToHub::Backfill { points: chunk.to_vec() });
                                }
                            }
                        }
                    }
                    ToAgent::Write { req_id, service, tag, value } => {
                        let edge = edge.clone();
                        let out = out_tx.clone();
                        tokio::spawn(async move {
                            let result = match edge.service(&service) {
                                Some(def) => edge.mgr.write(&def, &tag, &value).await,
                                None => Err("no such service on this agent".into()),
                            };
                            let _ = out.send(ToHub::Reply { req_id, result });
                        });
                    }
                    ToAgent::Request { req_id, op } => {
                        let result = edge.handle_op(op).await;
                        send(ToHub::Reply { req_id, result });
                    }
                }
            }
        }
    };
    // Mark the link down from the last moment it was known good.
    edge.state.lock().last_connected = Some(Utc::now());
    edge.save_state();
    let _ = sink.close().await;
    result
}

/// `COWPOKE_NODE`, or the hostname, as an id (`a-z 0-9 - _`).
pub fn node_name() -> String {
    let raw = std::env::var("COWPOKE_NODE").ok().filter(|n| !n.is_empty()).unwrap_or_else(hostname);
    crate::model::to_id(&raw)
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| opcua_hostname())
        .unwrap_or_else(|| "unknown".into())
}

fn opcua_hostname() -> Option<String> {
    ::opcua::crypto::X509Data::computer_hostnames().into_iter().next()
}

#[cfg(test)]
mod tests {
    #[test]
    fn hub_urls() {
        assert_eq!(super::ws_url("https://hub.example.com/").unwrap(), "wss://hub.example.com/api/agents/connect");
        assert_eq!(super::ws_url("http://10.0.0.5:8080").unwrap(), "ws://10.0.0.5:8080/api/agents/connect");
        assert!(super::ws_url("hub.example.com").is_err());
    }

    /// A hub that accepts the link and then says nothing must be dropped after
    /// the silence window, even though the flush/status/alive ticks keep firing
    /// (they used to restart the deadline, so it never fired).
    #[tokio::test]
    async fn silent_hub_is_dropped() {
        use futures::StreamExt;
        use std::time::{Duration, Instant};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            // Read whatever the agent sends, never answer.
            let (_sink, mut stream) = ws.split();
            while stream.next().await.is_some() {}
        });

        let dir = tempfile::tempdir().unwrap();
        let edge = super::Edge::new(dir.path()).unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let started = Instant::now();
        let r = tokio::time::timeout(
            Duration::from_secs(10),
            super::session(&edge, &format!("ws://127.0.0.1:{port}"), "t", &cancel, Duration::from_secs(1)),
        )
        .await
        .expect("session never noticed the silent hub");
        let err = r.expect_err("a silent hub must end the session").to_string();
        assert!(err.contains("silent"), "unexpected error: {err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
