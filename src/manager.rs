//! Starts, stops and supervises services, and routes tag writes to them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::model::{Config, Mode, ServiceDef};
use crate::services;
use crate::tags::{Origin, Quality, TagStore, coerce_tag};

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Stopped,
    Starting,
    Running,
    /// Running, but the last poll or request failed.
    Degraded,
    Error,
}

#[derive(Serialize, Clone, Debug)]
pub struct Status {
    pub state: State,
    /// What the service is doing (`listening on 0.0.0.0:502`) or why it failed.
    pub detail: String,
    pub since: DateTime<Utc>,
}

impl Status {
    fn new(state: State, detail: impl Into<String>) -> Self {
        Self {
            state,
            detail: detail.into(),
            since: Utc::now(),
        }
    }
}

#[derive(Clone)]
pub struct StatusHandle(Arc<Mutex<Status>>);

impl StatusHandle {
    pub fn running(&self, detail: impl Into<String>) {
        *self.0.lock() = Status::new(State::Running, detail);
    }
    pub fn degraded(&self, detail: impl Into<String>) {
        *self.0.lock() = Status::new(State::Degraded, detail);
    }
    pub fn get(&self) -> Status {
        self.0.lock().clone()
    }
    fn set(&self, s: Status) {
        *self.0.lock() = s;
    }
}

pub struct WriteReq {
    pub tag: String,
    pub value: Value,
    pub reply: oneshot::Sender<Result<(), String>>,
}

/// What a running service gets.
pub struct Ctx {
    pub def: ServiceDef,
    pub tags: TagStore,
    /// Per-service folder for certificates and the like.
    pub dir: PathBuf,
    pub cancel: CancellationToken,
    pub writes: mpsc::Receiver<WriteReq>,
    pub status: StatusHandle,
}

impl Ctx {
    pub fn settings<T: serde::de::DeserializeOwned>(&self) -> anyhow::Result<T> {
        serde_json::from_value(self.def.settings.clone())
            .map_err(|e| anyhow::anyhow!("settings: {e}"))
    }
}

struct Running {
    def: ServiceDef,
    cancel: CancellationToken,
    writes: mpsc::Sender<WriteReq>,
    task: tokio::task::JoinHandle<()>,
}

pub struct Manager {
    tags: TagStore,
    data_dir: PathBuf,
    running: tokio::sync::Mutex<HashMap<String, Running>>,
    statuses: Mutex<HashMap<String, StatusHandle>>,
}

impl Manager {
    pub fn new(tags: TagStore, data_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            tags,
            data_dir,
            running: Default::default(),
            statuses: Default::default(),
        })
    }

    pub fn status(&self, id: &str) -> Status {
        self.statuses
            .lock()
            .get(id)
            .map(|h| h.get())
            .unwrap_or_else(|| Status::new(State::Stopped, ""))
    }

    /// Bring running services in line with the configuration: start enabled ones,
    /// stop disabled or removed ones, restart ones whose definition changed.
    pub async fn apply(&self, cfg: &Config) {
        let mut running = self.running.lock().await;
        let wanted: HashMap<&str, &ServiceDef> = cfg
            .services
            .iter()
            .filter(|s| s.enabled)
            .map(|s| (s.id.as_str(), s))
            .collect();

        let ids: Vec<String> = running.keys().cloned().collect();
        for id in ids {
            let keep = wanted
                .get(id.as_str())
                .is_some_and(|d| same(d, &running[&id].def));
            if !keep {
                let r = running.remove(&id).unwrap();
                stop(r).await;
                if let Some(h) = self.statuses.lock().get(&id) {
                    h.set(Status::new(State::Stopped, ""));
                }
            }
        }
        // Forget values of services that no longer exist.
        let known: std::collections::HashSet<&str> =
            cfg.services.iter().map(|s| s.id.as_str()).collect();
        self.statuses.lock().retain(|id, _| known.contains(id.as_str()));
        for s in &cfg.services {
            if !s.enabled && !running.contains_key(&s.id) {
                self.tags.remove_service(&s.id);
            }
        }

        for (id, def) in wanted {
            if running.contains_key(id) {
                continue;
            }
            let r = self.spawn(def.clone());
            running.insert(id.to_string(), r);
        }
    }

    fn spawn(&self, def: ServiceDef) -> Running {
        let status = self
            .statuses
            .lock()
            .entry(def.id.clone())
            .or_insert_with(|| StatusHandle(Arc::new(Mutex::new(Status::new(State::Stopped, "")))))
            .clone();
        let cancel = CancellationToken::new();
        let (wtx, wrx) = mpsc::channel(64);
        let dir = self.data_dir.join("services").join(&def.id);
        let _ = std::fs::create_dir_all(&dir);

        if def.mode == Mode::Server {
            self.tags.seed(&def.id, &def.tags);
        } else {
            self.tags.remove_service(&def.id);
        }

        let mut ctx = Ctx {
            def: def.clone(),
            tags: self.tags.clone(),
            dir,
            cancel: cancel.clone(),
            writes: wrx,
            status: status.clone(),
        };
        let task = tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                status.set(Status::new(State::Starting, ""));
                let res = services::run(&mut ctx).await;
                if ctx.cancel.is_cancelled() {
                    break;
                }
                let msg = match res {
                    Ok(()) => "stopped unexpectedly".to_string(),
                    Err(e) => format!("{e:#}"),
                };
                tracing::warn!(service = %ctx.def.id, "{msg}; retrying in {backoff:?}");
                status.set(Status::new(State::Error, format!("{msg} (retrying)")));
                if ctx.def.mode == Mode::Client {
                    for t in &ctx.def.tags {
                        ctx.tags.set_bad(&ctx.def.id, &t.name);
                    }
                }
                tokio::select! {
                    _ = ctx.cancel.cancelled() => break,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
            status.set(Status::new(State::Stopped, ""));
        });
        Running {
            def,
            cancel,
            writes: wtx,
            task,
        }
    }

    /// Write a tag from the UI or API.
    ///
    /// Server mode: the value is stored and the service serves it.
    /// Client mode: the value is written to the device; the store updates on the next read.
    pub async fn write(&self, def: &ServiceDef, tag: &str, value: &Value) -> Result<Value, String> {
        let t = def
            .tags
            .iter()
            .find(|t| t.name == tag)
            .ok_or_else(|| format!("no tag `{tag}`"))?;
        let v = coerce_tag(t, value)?;
        match def.mode {
            Mode::Server => {
                self.tags.set(&def.id, tag, v.clone(), Quality::Good, Origin::User);
                Ok(v)
            }
            Mode::Client => {
                if !t.writable {
                    return Err(format!("tag `{tag}` is read-only"));
                }
                let tx = {
                    let running = self.running.lock().await;
                    running
                        .get(&def.id)
                        .map(|r| r.writes.clone())
                        .ok_or("the service isn't running")?
                };
                let (reply, rx) = oneshot::channel();
                tx.send(WriteReq {
                    tag: tag.to_string(),
                    value: v.clone(),
                    reply,
                })
                .await
                .map_err(|_| "the service isn't running")?;
                match tokio::time::timeout(Duration::from_secs(15), rx).await {
                    Ok(Ok(r)) => r.map(|_| v),
                    Ok(Err(_)) => Err("the service stopped before writing".into()),
                    Err(_) => Err("timed out writing to the device".into()),
                }
            }
        }
    }

    pub async fn shutdown(&self) {
        let mut running = self.running.lock().await;
        for (_, r) in running.drain() {
            stop(r).await;
        }
    }
}

async fn stop(r: Running) {
    r.cancel.cancel();
    let mut task = r.task;
    if tokio::time::timeout(Duration::from_secs(5), &mut task).await.is_err() {
        tracing::warn!(service = %r.def.id, "didn't stop within 5 s; aborting");
        task.abort();
    }
}

fn same(a: &ServiceDef, b: &ServiceDef) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}
