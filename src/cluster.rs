//! Hub high availability: two hubs, one active and one standby.
//!
//! Each hub is started with `COWPOKE_PEER` (the other hub's URL) and the same
//! `COWPOKE_CLUSTER_KEY`. The standby serves no UI or services. It mirrors the active
//! hub's data folder (configuration, users, agents, alarms, boards, certificates) every
//! 2 s, and takes the live tag values over a WebSocket into its own historian, so its
//! history is complete when it takes over. When the active hub has been unreachable for
//! `COWPOKE_FAILOVER_S` seconds (15 by default), the standby becomes active.
//!
//! Every time a hub becomes active it raises the cluster **epoch**. If both end up
//! active (a network split), the one with the higher epoch (then the lower node name)
//! stays; the other stops its services and exits so Docker restarts it as the standby.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::agent::ValueMsg;
use crate::historian::Historian;
use crate::tags::{Origin, TagStore};
use crate::web::App;

const KEY_HEADER: &str = "x-cowpoke-cluster-key";
/// Files bigger than this aren't mirrored (packet captures and the like).
const MAX_FILE: u64 = 64 * 1024 * 1024;
/// How far back a standby catches up history it missed.
const MAX_CATCH_UP_MS: i64 = 7 * 24 * 3600 * 1000;
/// Exit code of a hub that stepped down; Docker's restart policy brings it back as standby.
pub const STEPPED_DOWN: i32 = 3;

#[derive(Clone, Debug)]
pub struct Settings {
    /// The other hub's base URL, `http://hub-b:8080`.
    pub peer: String,
    pub key: String,
    pub node: String,
    pub failover: Duration,
}

impl Settings {
    /// From the environment; `None` when `COWPOKE_PEER` isn't set (a single hub).
    pub fn from_env() -> Result<Option<Self>> {
        let Some(peer) = std::env::var("COWPOKE_PEER").ok().filter(|p| !p.trim().is_empty()) else {
            return Ok(None);
        };
        let peer = peer.trim().trim_end_matches('/').to_string();
        if !peer.starts_with("http://") && !peer.starts_with("https://") {
            bail!("COWPOKE_PEER must start with http:// or https:// (got `{peer}`)");
        }
        let key = std::env::var("COWPOKE_CLUSTER_KEY").unwrap_or_default();
        if key.len() < 16 {
            bail!("set COWPOKE_CLUSTER_KEY to the same secret (16 characters or more) on both hubs");
        }
        let failover = std::env::var("COWPOKE_FAILOVER_S")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(15)
            .max(3);
        Ok(Some(Self {
            peer,
            key,
            node: crate::agent::edge::node_name(),
            failover: Duration::from_secs(failover),
        }))
    }
}

/// What a hub says about itself at `GET /api/cluster/state`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PeerState {
    pub node: String,
    /// `active` or `standby`.
    pub role: String,
    pub epoch: u64,
}

/// `cluster.json`: kept on each hub, never mirrored.
#[derive(Serialize, Deserialize, Default)]
struct Saved {
    epoch: u64,
    /// When the standby last had live values from the active hub (history catches up from here).
    #[serde(default)]
    last_sync_ms: Option<i64>,
}

fn saved_path(data_dir: &Path) -> PathBuf {
    data_dir.join("cluster.json")
}

fn load(data_dir: &Path) -> Saved {
    std::fs::read_to_string(saved_path(data_dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save(data_dir: &Path, s: &Saved) {
    let tmp = data_dir.join("cluster.json.tmp");
    if std::fs::write(&tmp, serde_json::to_string_pretty(s).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(tmp, saved_path(data_dir));
    }
}

/// The active hub's cluster identity, for its endpoints and watchdog.
struct Active {
    settings: Settings,
    epoch: AtomicU64,
    config_path: PathBuf,
}

static ACTIVE: OnceLock<Active> = OnceLock::new();

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("http client")
}

fn key_ok(headers: &HeaderMap, key: &str) -> bool {
    let got = headers.get(KEY_HEADER).and_then(|h| h.to_str().ok()).unwrap_or("");
    got.len() == key.len() && got.bytes().zip(key.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0
}

/// The peer's state, or `None` if it can't be reached (or answers with the wrong key).
pub async fn peer_state(s: &Settings) -> Option<PeerState> {
    let r = http()
        .get(format!("{}/api/cluster/state", s.peer))
        .header(KEY_HEADER, &s.key)
        .send()
        .await
        .ok()?;
    if r.status() == StatusCode::FORBIDDEN {
        tracing::error!("the peer hub refused our cluster key: set the same COWPOKE_CLUSTER_KEY on both");
        return None;
    }
    r.json().await.ok()
}

/// Does (epoch, node) `a` win over `b`? Higher epoch, then the lower node name.
fn wins(a_epoch: u64, a_node: &str, b_epoch: u64, b_node: &str) -> bool {
    (a_epoch, std::cmp::Reverse(a_node)) > (b_epoch, std::cmp::Reverse(b_node))
}

/// `http://hub-b:8080/` → `hub-b`.
fn peer_host(url: &str) -> &str {
    let rest = url.split_once("://").map(|x| x.1).unwrap_or(url);
    let host = rest.split(['/', '?']).next().unwrap_or(rest);
    if let Some(v6) = host.strip_prefix('[') {
        return v6.split(']').next().unwrap_or(v6);
    }
    match host.rsplit_once(':') {
        Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h,
        _ => host,
    }
}

#[derive(PartialEq, Eq, Debug)]
pub enum Role {
    Active,
    Standby,
}

/// At start: standby if the peer is active (or should be); active if the peer stays
/// unreachable for a few seconds (it may just be starting too).
pub async fn decide(data_dir: &Path, s: &Settings) -> Role {
    let mine = load(data_dir).epoch;
    for attempt in 0..3 {
        match peer_state(s).await {
            Some(p) if p.role == "active" => return Role::Standby,
            Some(p) => {
                return if wins(mine, &s.node, p.epoch, &p.node) { Role::Active } else { Role::Standby };
            }
            None if attempt < 2 => tokio::time::sleep(Duration::from_secs(2)).await,
            None => {}
        }
    }
    // Both hubs may be starting at once and not see each other yet. The one whose name
    // sorts after the peer's host waits longer, so the other is usually active first
    // and this one finds it (instead of both starting active and one stepping down).
    if s.node.as_str() > peer_host(&s.peer) {
        tokio::time::sleep(Duration::from_secs(8)).await;
        if let Some(p) = peer_state(s).await {
            return if p.role != "active" && wins(mine, &s.node, p.epoch, &p.node) { Role::Active } else { Role::Standby };
        }
    }
    tracing::warn!("peer hub {} unreachable; starting as the active hub", s.peer);
    Role::Active
}

/// Become active: raise the epoch past anything seen, and serve the cluster endpoints.
pub fn activate(data_dir: &Path, config_path: &Path, s: &Settings, peer_epoch: u64) -> u64 {
    let mut saved = load(data_dir);
    saved.epoch = saved.epoch.max(peer_epoch) + 1;
    save(data_dir, &saved);
    let _ = ACTIVE.set(Active {
        settings: s.clone(),
        epoch: AtomicU64::new(saved.epoch),
        config_path: config_path.to_path_buf(),
    });
    tracing::info!(node = s.node, epoch = saved.epoch, "active hub");
    saved.epoch
}

/// On the active hub: check the peer every 3 s; if it is active too and wins, cancel
/// `demote` (the hub then stops and exits with [`STEPPED_DOWN`]).
pub async fn watch(demote: CancellationToken) {
    let Some(a) = ACTIVE.get() else { return };
    let mut t = tokio::time::interval(Duration::from_secs(3));
    loop {
        t.tick().await;
        if let Some(p) = peer_state(&a.settings).await {
            let mine = a.epoch.load(Ordering::Relaxed);
            if p.role == "active" && wins(p.epoch, &p.node, mine, &a.settings.node) {
                tracing::warn!(
                    "peer {} is active with epoch {} (ours {mine}); stepping down",
                    p.node,
                    p.epoch
                );
                demote.cancel();
                return;
            }
        }
    }
}

// ------------------------------------------------------------------ active hub's endpoints

pub fn router(app: App) -> Router {
    Router::new()
        .route("/api/cluster/state", get(state))
        .route("/api/cluster/files", post(files))
        .route("/api/cluster/history", post(history))
        .route("/api/cluster/stream", get(stream))
        .with_state(app)
}

fn guard(headers: &HeaderMap) -> Result<&'static Active, Response> {
    let Some(a) = ACTIVE.get() else {
        return Err((StatusCode::NOT_FOUND, "clustering is off").into_response());
    };
    if !key_ok(headers, &a.settings.key) {
        return Err((StatusCode::FORBIDDEN, "wrong cluster key").into_response());
    }
    Ok(a)
}

async fn state(headers: HeaderMap) -> Response {
    match guard(&headers) {
        Ok(a) => Json(PeerState {
            node: a.settings.node.clone(),
            role: "active".into(),
            epoch: a.epoch.load(Ordering::Relaxed),
        })
        .into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct FilesReq {
    /// The standby's files: relative path → SHA-256 (hex).
    manifest: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Default)]
struct FilesResp {
    /// Changed or new files: (relative path, base64 contents).
    files: Vec<(String, String)>,
    /// Paths the standby has that the active hub doesn't.
    deleted: Vec<String>,
}

/// The name the configuration file travels under when it lives outside the data folder.
const CONFIG_OUTSIDE: &str = "@config";

/// Every mirrored file in the data folder: relative path → absolute path.
fn mirrored(data_dir: &Path, config_path: &Path) -> HashMap<String, PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut HashMap<String, PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(rel) = p.strip_prefix(root) else { continue };
            let rel = rel.to_string_lossy().replace('\\', "/");
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                if rel == "history" {
                    continue; // each hub keeps its own (the standby records from the live stream)
                }
                walk(root, &p, out);
            } else if md.is_file()
                && md.len() <= MAX_FILE
                && !rel.ends_with(".tmp")
                && !rel.starts_with("cluster.json")
                && rel != "baselines.json"
            {
                out.insert(rel, p);
            }
        }
    }
    let mut out = HashMap::new();
    walk(data_dir, data_dir, &mut out);
    if !config_path.starts_with(data_dir) && config_path.exists() {
        out.insert(CONFIG_OUTSIDE.into(), config_path.to_path_buf());
    }
    out
}

fn sha(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

async fn files(State(app): State<App>, headers: HeaderMap, Json(req): Json<FilesReq>) -> Response {
    let a = match guard(&headers) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let data_dir = app.data_dir.clone();
    let config_path = a.config_path.clone();
    let resp = tokio::task::spawn_blocking(move || {
        let ours = mirrored(&data_dir, &config_path);
        let mut out = FilesResp::default();
        for (rel, path) in &ours {
            let Ok(bytes) = std::fs::read(path) else { continue };
            if req.manifest.get(rel) != Some(&sha(&bytes)) {
                out.files.push((rel.clone(), base64::engine::general_purpose::STANDARD.encode(bytes)));
            }
        }
        out.deleted = req.manifest.keys().filter(|k| !ours.contains_key(*k)).cloned().collect();
        out
    })
    .await
    .unwrap_or_default();
    Json(resp).into_response()
}

#[derive(Deserialize)]
struct HistoryReq {
    since_ms: i64,
}

async fn history(State(app): State<App>, headers: HeaderMap, Json(req): Json<HistoryReq>) -> Response {
    if let Err(r) = guard(&headers) {
        return r;
    }
    let now = chrono::Utc::now().timestamp_millis();
    let since = req.since_ms.max(now - MAX_CATCH_UP_MS);
    let hist = app.hist.clone();
    let points = tokio::task::spawn_blocking(move || {
        hist.flush();
        let mut out: Vec<(String, String, i64, f64)> = vec![];
        for (s, t) in hist.tracked() {
            for (ts, v) in hist.raw(&s, &t, since, now) {
                out.push((s.clone(), t.clone(), ts, v));
            }
        }
        out
    })
    .await
    .unwrap_or_default();
    Json(json!({ "points": points })).into_response()
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamMsg {
    Values { batch: Vec<ValueMsg> },
    Ping { epoch: u64 },
}

async fn stream(State(app): State<App>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    if let Err(r) = guard(&headers) {
        return r;
    }
    ws.on_upgrade(move |socket| send_stream(app, socket))
}

/// Every tag value now, then changes in 200 ms batches, and a ping every second.
async fn send_stream(app: App, socket: WebSocket) {
    let (mut sink, mut incoming) = socket.split();
    let mut events = app.tags.subscribe();
    let snapshot = |app: &App| -> Vec<ValueMsg> {
        app.cfg
            .get()
            .services
            .iter()
            .flat_map(|s| {
                let id = s.id.clone();
                app.tags.service_tags(&s.id).into_iter().map(move |(tag, st)| ValueMsg {
                    service: id.clone(),
                    tag,
                    value: st.value,
                    quality: st.quality,
                    ts: st.ts,
                })
            })
            .collect()
    };
    let send = |m: &StreamMsg| Message::Text(serde_json::to_string(m).unwrap_or_default().into());
    if sink.send(send(&StreamMsg::Values { batch: snapshot(&app) })).await.is_err() {
        return;
    }
    let mut buffer: HashMap<(String, String), ValueMsg> = HashMap::new();
    let mut flush = tokio::time::interval(Duration::from_millis(200));
    let mut ping = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Ok(e) => {
                    buffer.insert((e.service.clone(), e.tag.clone()), ValueMsg {
                        service: e.service, tag: e.tag, value: e.state.value, quality: e.state.quality, ts: e.state.ts,
                    });
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    for v in snapshot(&app) { buffer.insert((v.service.clone(), v.tag.clone()), v); }
                }
                Err(_) => break,
            },
            _ = flush.tick() => {
                if !buffer.is_empty() {
                    let batch = buffer.drain().map(|(_, v)| v).collect();
                    if sink.send(send(&StreamMsg::Values { batch })).await.is_err() { break; }
                }
            }
            _ = ping.tick() => {
                let epoch = ACTIVE.get().map(|a| a.epoch.load(Ordering::Relaxed)).unwrap_or(0);
                if sink.send(send(&StreamMsg::Ping { epoch })).await.is_err() { break; }
            }
            m = incoming.next() => if matches!(m, None | Some(Err(_)) | Some(Ok(Message::Close(_)))) { break },
        }
    }
}

// ------------------------------------------------------------------ standby

/// What the standby knows while it runs.
struct Standby {
    data_dir: PathBuf,
    config_path: PathBuf,
    settings: Settings,
    tags: TagStore,
    hist: std::sync::Arc<Historian>,
    /// The highest epoch the active hub reported.
    peer_epoch: AtomicU64,
}

impl Standby {
    /// One mirror round. Returns true if the configuration file changed.
    async fn sync_files(&self) -> Result<bool> {
        let manifest = {
            let (d, c) = (self.data_dir.clone(), self.config_path.clone());
            tokio::task::spawn_blocking(move || {
                mirrored(&d, &c)
                    .into_iter()
                    .filter_map(|(rel, p)| std::fs::read(&p).ok().map(|b| (rel, sha(&b))))
                    .collect::<HashMap<_, _>>()
            })
            .await?
        };
        let resp: FilesResp = http()
            .post(format!("{}/api/cluster/files", self.settings.peer))
            .header(KEY_HEADER, &self.settings.key)
            .timeout(Duration::from_secs(60))
            .json(&json!({ "manifest": manifest }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut config_changed = false;
        for (rel, b64) in resp.files {
            let path = self.local_path(&rel)?;
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_file_name(format!(
                "{}.mirror.tmp",
                path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
            ));
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &path)?;
            config_changed |= path == self.config_path;
        }
        for rel in resp.deleted {
            let path = self.local_path(&rel)?;
            let _ = std::fs::remove_file(&path);
            config_changed |= path == self.config_path;
        }
        Ok(config_changed)
    }

    /// A mirrored path inside our data folder (never outside it).
    fn local_path(&self, rel: &str) -> Result<PathBuf> {
        if rel == CONFIG_OUTSIDE {
            return Ok(self.config_path.clone());
        }
        if rel.is_empty() || rel.starts_with('/') || rel.split('/').any(|c| c == ".." || c.is_empty()) || rel.contains('\\') {
            bail!("refusing mirrored path `{rel}`");
        }
        Ok(self.data_dir.join(rel))
    }

    fn configure_history(&self) {
        if let Ok(text) = std::fs::read_to_string(&self.config_path) {
            match crate::store::parse(&text) {
                Ok(cfg) => self.hist.configure(&cfg),
                Err(e) => tracing::warn!("mirrored configuration doesn't parse: {e}"),
            }
        }
    }

    /// History the active hub recorded since we last had live values.
    async fn catch_up(&self) -> Result<()> {
        let since = load(&self.data_dir).last_sync_ms.unwrap_or(0);
        let v: Value = http()
            .post(format!("{}/api/cluster/history", self.settings.peer))
            .header(KEY_HEADER, &self.settings.key)
            .timeout(Duration::from_secs(120))
            .json(&json!({ "since_ms": since }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let points: Vec<(String, String, i64, f64)> = serde_json::from_value(v["points"].clone()).unwrap_or_default();
        if !points.is_empty() {
            tracing::info!("caught up {} history points from the active hub", points.len());
            self.hist.insert(points);
        }
        Ok(())
    }

    fn note_sync(&self) {
        let mut s = load(&self.data_dir);
        s.last_sync_ms = Some(chrono::Utc::now().timestamp_millis());
        save(&self.data_dir, &s);
    }

    /// Follow the active hub until the link fails.
    async fn follow(&self, stop: &CancellationToken) -> Result<()> {
        self.sync_files().await?;
        self.configure_history();
        self.catch_up().await?;
        let url = format!(
            "{}/api/cluster/stream",
            self.settings.peer.replacen("http", "ws", 1)
        );
        let mut req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(url.as_str())?;
        req.headers_mut().insert(KEY_HEADER, self.settings.key.parse()?);
        let (ws, _) = tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::connect_async(req))
            .await
            .map_err(|_| anyhow!("timed out opening the live stream"))?
            .context("opening the live stream")?;
        let (_sink, mut incoming) = ws.split();
        tracing::info!("following the active hub at {}", self.settings.peer);
        let mut files = tokio::time::interval(Duration::from_secs(2));
        let mut mark = tokio::time::interval(Duration::from_secs(10));
        let silence = Duration::from_secs(5);
        let deadline = tokio::time::sleep(silence);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = stop.cancelled() => return Ok(()),
                _ = &mut deadline => bail!("the active hub went silent"),
                _ = files.tick() => {
                    match self.sync_files().await {
                        Ok(true) => self.configure_history(),
                        Ok(false) => {}
                        Err(e) => tracing::warn!("mirroring files: {e:#}"),
                    }
                }
                _ = mark.tick() => self.note_sync(),
                m = incoming.next() => {
                    let text = match m {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => t,
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => bail!("the active hub closed the stream"),
                        Some(Err(e)) => bail!("live stream: {e}"),
                        Some(Ok(_)) => continue,
                    };
                    deadline.as_mut().reset(tokio::time::Instant::now() + silence);
                    match serde_json::from_str::<StreamMsg>(&text) {
                        Ok(StreamMsg::Values { batch }) => {
                            for v in batch {
                                self.tags.set_at(&v.service, &v.tag, v.value, v.quality, v.ts, Origin::Service);
                            }
                        }
                        Ok(StreamMsg::Ping { epoch }) => { self.peer_epoch.fetch_max(epoch, Ordering::Relaxed); }
                        Err(e) => tracing::warn!("bad stream message: {e}"),
                    }
                }
            }
        }
    }
}

/// What a standby answers on its web port: health 503 (so a load balancer skips it),
/// its cluster state, and 503 for everything else.
fn standby_router(node: String, data_dir: PathBuf, key: String) -> Router {
    let st = move |headers: HeaderMap| {
        let (node, data_dir, key) = (node.clone(), data_dir.clone(), key.clone());
        async move {
            if !key_ok(&headers, &key) {
                return (StatusCode::FORBIDDEN, "wrong cluster key").into_response();
            }
            Json(PeerState { node, role: "standby".into(), epoch: load(&data_dir).epoch }).into_response()
        }
    };
    Router::new()
        .route("/healthz", get(|| async { (StatusCode::SERVICE_UNAVAILABLE, "standby") }))
        .route("/api/cluster/state", get(st))
        .fallback(|| async {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "this Cowpoke hub is the standby; use the active hub"})),
            )
        })
}

/// Run as the standby until the active hub is gone for the failover time (returns the
/// peer's last epoch: become active) or until `shutdown` (returns `None`).
pub async fn run_standby(
    data_dir: &Path,
    config_path: &Path,
    s: &Settings,
    bind: &str,
    shutdown: CancellationToken,
) -> Result<Option<u64>> {
    std::fs::create_dir_all(data_dir)?;
    let tags = TagStore::new();
    let hist = Historian::new(data_dir);
    let hist_task = tokio::spawn(hist.clone().run(tags.clone()));
    let sb = Standby {
        data_dir: data_dir.to_path_buf(),
        config_path: config_path.to_path_buf(),
        settings: s.clone(),
        tags,
        hist: hist.clone(),
        peer_epoch: AtomicU64::new(0),
    };

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("can't listen on {bind}"))?;
    let web_stop = CancellationToken::new();
    let server = tokio::spawn({
        let stop = web_stop.clone();
        let router = standby_router(s.node.clone(), data_dir.to_path_buf(), s.key.clone());
        async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move { stop.cancelled().await })
                .await;
        }
    });
    tracing::info!(node = s.node, "standby hub following {}", s.peer);

    let mut last_ok = tokio::time::Instant::now();
    let promoted = loop {
        if shutdown.is_cancelled() {
            break None;
        }
        let started = tokio::time::Instant::now();
        match sb.follow(&shutdown).await {
            Ok(()) => break None, // shutdown
            Err(e) => tracing::warn!("standby link: {e:#}"),
        }
        if started.elapsed() > Duration::from_secs(3) {
            last_ok = tokio::time::Instant::now();
        }
        sb.note_sync();
        // Is the active hub gone, or just briefly unreachable?
        if last_ok.elapsed() >= s.failover {
            match peer_state(s).await {
                Some(p) if p.role == "active" => {
                    sb.peer_epoch.fetch_max(p.epoch, Ordering::Relaxed);
                    last_ok = tokio::time::Instant::now();
                }
                Some(p) if wins(load(data_dir).epoch, &s.node, p.epoch, &p.node) => {
                    break Some(sb.peer_epoch.load(Ordering::Relaxed).max(p.epoch));
                }
                Some(_) => last_ok = tokio::time::Instant::now(),
                None => {
                    tracing::warn!("active hub unreachable for {} s: taking over", s.failover.as_secs());
                    break Some(sb.peer_epoch.load(Ordering::Relaxed));
                }
            }
        }
        tokio::select! {
            _ = shutdown.cancelled() => break None,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    };

    web_stop.cancel();
    let _ = server.await;
    hist_task.abort();
    let _ = hist_task.await;
    let h = hist.clone();
    let _ = tokio::task::spawn_blocking(move || h.flush()).await;
    Ok(promoted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_hosts() {
        assert_eq!(peer_host("http://hub-b:8080"), "hub-b");
        assert_eq!(peer_host("https://hub-b.example.com/"), "hub-b.example.com");
        assert_eq!(peer_host("http://10.0.0.5:8080/x"), "10.0.0.5");
        assert_eq!(peer_host("http://[fd00::5]:8080"), "fd00::5");
    }

    #[test]
    fn tie_breaks() {
        assert!(wins(2, "b", 1, "a"));
        assert!(wins(1, "a", 1, "b"));
        assert!(!wins(1, "b", 1, "a"));
        assert!(!wins(1, "a", 1, "a"));
    }

    #[test]
    fn mirrored_files_skip_history_and_cluster_state() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("history/a")).unwrap();
        std::fs::create_dir_all(d.path().join("services/x/pki/own")).unwrap();
        for f in ["cowpoke.yaml", "users.json", "cluster.json", "agents.json.tmp", "history/a/1", "services/x/pki/own/cert.der"] {
            std::fs::write(d.path().join(f), "x").unwrap();
        }
        let mut got: Vec<String> = mirrored(d.path(), &d.path().join("cowpoke.yaml")).into_keys().collect();
        got.sort();
        assert_eq!(got, ["cowpoke.yaml", "services/x/pki/own/cert.der", "users.json"]);
    }

    #[test]
    fn standby_refuses_paths_outside_its_folder() {
        let d = tempfile::tempdir().unwrap();
        let sb = Standby {
            data_dir: d.path().into(),
            config_path: d.path().join("cowpoke.yaml"),
            settings: Settings { peer: "http://x".into(), key: "k".repeat(16), node: "a".into(), failover: Duration::from_secs(5) },
            tags: TagStore::new(),
            hist: Historian::new(d.path()),
            peer_epoch: AtomicU64::new(0),
        };
        assert!(sb.local_path("../etc/passwd").is_err());
        assert!(sb.local_path("/etc/passwd").is_err());
        assert!(sb.local_path("a//b").is_err());
        assert!(sb.local_path("services/x/pki/own/cert.der").is_ok());
        assert_eq!(sb.local_path(CONFIG_OUTSIDE).unwrap(), d.path().join("cowpoke.yaml"));
    }
}
