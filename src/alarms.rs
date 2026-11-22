//! Alarms and hooks.
//!
//! Alarms watch one tag each: high/low limits, an equals state, bad quality, staleness or
//! rate of change, with a deadband and an on-delay. They follow the usual alarm states: a
//! raised alarm stays listed until it has both cleared and been acknowledged.
//! Hooks run on raise, clear or acknowledge: a webhook, a tag write, an email or an MQTT
//! publish. Definitions live in `alarms.yaml`; events are logged to `alarm-log.jsonl`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::model::valid_id;
use crate::tags::{Quality, TagState, as_f64};
use crate::web::App;

// ------------------------------------------------------------------ definitions

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AlarmKind {
    /// Value above `limit`.
    High,
    /// Value below `limit`.
    Low,
    /// Value equals `value`.
    Equals,
    /// The tag's quality isn't good (device unreachable, agent offline).
    BadQuality,
    /// No update for `stale_s` seconds.
    Stale,
    /// Value changes faster than `rate_per_min` (either direction).
    Rate,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    #[default]
    Medium,
    High,
    Critical,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AlarmDef {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub service: String,
    pub tag: String,
    pub kind: AlarmKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    /// High/low/rate: how far back past the limit the value must go to clear.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub deadband: f64,
    /// Equals: the value that raises the alarm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_per_min: Option<f64>,
    /// The condition must hold this long before the alarm raises.
    #[serde(default, skip_serializing_if = "is_zero_u")]
    pub delay_s: u64,
    #[serde(default)]
    pub severity: Severity,
    /// Message template; placeholders as in hooks. Default: a sentence built from the condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Hook ids to run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}
fn is_zero(x: &f64) -> bool {
    *x == 0.0
}
fn is_zero_u(x: &u64) -> bool {
    *x == 0
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Raise,
    Clear,
    Ack,
    /// Sent by "Test" on a hook.
    Test,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookKind {
    /// POST (or any method) to a URL. Without `body`, the alarm event is sent as JSON.
    Webhook {
        url: String,
        #[serde(default = "post")]
        method: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<Value>,
    },
    /// Write `value` to a tag when the alarm raises, and `clear_value` (if set) when it clears.
    Write {
        service: String,
        tag: String,
        value: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        clear_value: Option<Value>,
    },
    Email {
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        /// `starttls` (default), `tls` or `none`.
        #[serde(default = "starttls")]
        security: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
        from: String,
        to: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subject: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
    },
    Mqtt {
        host: String,
        #[serde(default = "mqtt_port")]
        port: u16,
        topic: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
        #[serde(default)]
        qos: u8,
        #[serde(default)]
        retain: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
    },
}

fn post() -> String {
    "POST".into()
}
fn starttls() -> String {
    "starttls".into()
}
fn mqtt_port() -> u16 {
    1883
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HookDef {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(flatten)]
    pub kind: HookKind,
    /// Which events run it. Default: raise and clear.
    #[serde(default = "default_on")]
    pub on: Vec<EventKind>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn default_on() -> Vec<EventKind> {
    vec![EventKind::Raise, EventKind::Clear]
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AlarmConfig {
    #[serde(default)]
    pub alarms: Vec<AlarmDef>,
    #[serde(default)]
    pub hooks: Vec<HookDef>,
}

impl AlarmConfig {
    pub fn validate(&self) -> Result<(), String> {
        let mut ids = std::collections::HashSet::new();
        for h in &self.hooks {
            if !valid_id(&h.id) {
                return Err(format!("hook id `{}`: use 1-64 of a-z, 0-9, - and _", h.id));
            }
            if !ids.insert(&h.id) {
                return Err(format!("hook id `{}` appears twice", h.id));
            }
            if let HookKind::Mqtt { qos, .. } = &h.kind {
                if *qos > 1 {
                    return Err(format!("hook `{}`: qos is 0 or 1", h.id));
                }
            }
        }
        let mut ids = std::collections::HashSet::new();
        for a in &self.alarms {
            let what = format!("alarm `{}`", a.id);
            if !valid_id(&a.id) {
                return Err(format!("{what}: use 1-64 of a-z, 0-9, - and _ for the id"));
            }
            if !ids.insert(&a.id) {
                return Err(format!("{what} appears twice"));
            }
            match a.kind {
                AlarmKind::High | AlarmKind::Low if a.limit.is_none() => {
                    return Err(format!("{what}: high and low alarms need `limit`"));
                }
                AlarmKind::Equals if a.value.is_none() => {
                    return Err(format!("{what}: equals alarms need `value`"));
                }
                AlarmKind::Stale if a.stale_s.unwrap_or(0) == 0 => {
                    return Err(format!("{what}: stale alarms need `stale_s`"));
                }
                AlarmKind::Rate if a.rate_per_min.unwrap_or(0.0) <= 0.0 => {
                    return Err(format!("{what}: rate alarms need a positive `rate_per_min`"));
                }
                _ => {}
            }
            if a.deadband < 0.0 {
                return Err(format!("{what}: deadband can't be negative"));
            }
            if let Some(h) = a.hooks.iter().find(|h| !self.hooks.iter().any(|x| &&x.id == h)) {
                return Err(format!("{what}: no hook `{h}`"));
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ store

pub struct AlarmStore {
    path: PathBuf,
    cfg: Mutex<AlarmConfig>,
    version: AtomicU64,
}

impl AlarmStore {
    pub fn open(path: &Path) -> Result<Self> {
        let cfg = match std::fs::read_to_string(path) {
            Ok(t) if !t.trim().is_empty() => {
                serde_yaml::from_str(&t).with_context(|| format!("parsing {}", path.display()))?
            }
            _ => AlarmConfig::default(),
        };
        Ok(Self {
            path: path.to_path_buf(),
            cfg: Mutex::new(cfg),
            version: AtomicU64::new(1),
        })
    }

    pub fn get(&self) -> AlarmConfig {
        self.cfg.lock().clone()
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    pub fn to_yaml(&self) -> String {
        serde_yaml::to_string(&*self.cfg.lock()).unwrap_or_default()
    }

    /// Change the configuration; validated and saved, or nothing changes.
    pub fn update(&self, f: impl FnOnce(&mut AlarmConfig) -> Result<(), String>) -> Result<(), String> {
        let mut cfg = self.cfg.lock();
        let mut next = cfg.clone();
        f(&mut next)?;
        next.validate()?;
        let text = format!(
            "# Cowpoke alarms and hooks. Edited by the web UI and API.\n{}",
            serde_yaml::to_string(&next).map_err(|e| e.to_string())?
        );
        let tmp = self.path.with_extension("yaml.tmp");
        std::fs::write(&tmp, text)
            .and_then(|_| std::fs::rename(&tmp, &self.path))
            .map_err(|e| e.to_string())?;
        *cfg = next;
        self.version.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

// ------------------------------------------------------------------ state and events

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AlarmState {
    /// The condition holds (after the on-delay).
    pub active: bool,
    pub acked: bool,
    /// When it raised.
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub cleared: Option<DateTime<Utc>>,
    #[serde(default)]
    pub acked_by: Option<String>,
    /// The tag's value when it raised.
    #[serde(default)]
    pub value: Value,
    #[serde(default)]
    pub message: String,
    /// The condition holds but the on-delay hasn't run out.
    #[serde(skip)]
    pub pending_since: Option<DateTime<Utc>>,
}

impl AlarmState {
    /// Listed until it has cleared and been acknowledged.
    pub fn shown(&self) -> bool {
        self.active || (self.since.is_some() && !self.acked)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AlarmEvent {
    pub time: DateTime<Utc>,
    pub event: EventKind,
    pub alarm: String,
    pub name: String,
    pub severity: Severity,
    pub service: String,
    pub tag: String,
    pub value: Value,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct HookResult {
    pub time: DateTime<Utc>,
    pub ok: bool,
    pub detail: String,
}

/// Something the alarm UI should hear about.
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Notice {
    Alarm(AlarmEvent),
    Hook { hook: String, alarm: String, result: HookResult },
}

// ------------------------------------------------------------------ templates

pub fn event_vars(e: &AlarmEvent) -> impl Fn(&str) -> Option<Value> + '_ {
    move |k: &str| {
        Some(match k {
            "id" | "alarm" => json!(e.alarm),
            "name" => json!(e.name),
            "event" => json!(e.event),
            "severity" => json!(e.severity),
            "service" => json!(e.service),
            "tag" => json!(e.tag),
            "value" => e.value.clone(),
            "time" => json!(e.time.to_rfc3339()),
            "message" => json!(e.message),
            "user" => json!(e.user.clone().unwrap_or_default()),
            _ => return None,
        })
    }
}

/// Replace `{{name}}` placeholders in text. Unknown ones stay as they are.
pub fn fill_text(tpl: &str, vars: &dyn Fn(&str) -> Option<Value>) -> String {
    let mut out = String::with_capacity(tpl.len());
    let mut rest = tpl;
    while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(j) = after.find("}}") else {
            out.push_str(&rest[i..]);
            rest = "";
            break;
        };
        match vars(after[..j].trim()) {
            Some(Value::String(s)) => out.push_str(&s),
            Some(v) => out.push_str(&v.to_string()),
            None => out.push_str(&rest[i..i + j + 4]),
        }
        rest = &after[j + 2..];
    }
    out.push_str(rest);
    out
}

/// Fill a JSON template: a string that is exactly one placeholder becomes the typed value.
pub fn fill_json(v: &Value, vars: &dyn Fn(&str) -> Option<Value>) -> Value {
    match v {
        Value::String(s) => {
            let t = s.trim();
            if t.starts_with("{{") && t.ends_with("}}") && t.matches("{{").count() == 1 {
                if let Some(x) = vars(t[2..t.len() - 2].trim()) {
                    return x;
                }
            }
            Value::String(fill_text(s, vars))
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| fill_json(x, vars)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), fill_json(x, vars))).collect()),
        other => other.clone(),
    }
}

// ------------------------------------------------------------------ conditions

fn same_value(a: &Value, b: &Value) -> bool {
    match (as_f64(a), as_f64(b)) {
        (Some(x), Some(y)) if !(a.is_string() || b.is_string()) => x == y,
        _ => match (a, b) {
            (Value::String(x), Value::String(y)) => x == y,
            (Value::String(x), other) | (other, Value::String(x)) => x.trim() == other.to_string(),
            _ => a == b,
        },
    }
}

fn fmt_num(v: f64) -> String {
    let r = (v * 1000.0).round() / 1000.0;
    if r.fract() == 0.0 { format!("{r:.0}") } else { format!("{r}") }
}

/// Whether the alarm condition holds. `None` means "can't tell" (bad data): keep the current state.
fn condition(
    a: &AlarmDef,
    st: Option<&TagState>,
    rate: Option<f64>,
    active: bool,
    now: DateTime<Utc>,
) -> Option<bool> {
    match a.kind {
        AlarmKind::BadQuality => return Some(st.is_none_or(|s| s.quality != Quality::Good)),
        AlarmKind::Stale => {
            let limit = a.stale_s.unwrap_or(60) as i64;
            return Some(st.is_none_or(|s| (now - s.ts).num_seconds() > limit));
        }
        _ => {}
    }
    let st = st?;
    if st.quality != Quality::Good {
        return None;
    }
    let db = if active { a.deadband } else { 0.0 };
    match a.kind {
        AlarmKind::High => Some(as_f64(&st.value)? > a.limit? - db),
        AlarmKind::Low => Some(as_f64(&st.value)? < a.limit? + db),
        AlarmKind::Equals => Some(same_value(&st.value, a.value.as_ref()?)),
        AlarmKind::Rate => Some(rate?.abs() > a.rate_per_min? - db),
        AlarmKind::BadQuality | AlarmKind::Stale => unreachable!(),
    }
}

fn default_message(a: &AlarmDef, value: &Value, rate: Option<f64>, units: &str) -> String {
    let name = if a.name.is_empty() { &a.id } else { &a.name };
    let v = match value {
        Value::Number(n) => format!("{}{units}", fmt_num(n.as_f64().unwrap_or(0.0))),
        Value::Null => "no value".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    match a.kind {
        AlarmKind::High => format!("{name}: {v} above {}{units}", fmt_num(a.limit.unwrap_or(0.0))),
        AlarmKind::Low => format!("{name}: {v} below {}{units}", fmt_num(a.limit.unwrap_or(0.0))),
        AlarmKind::Equals => format!("{name}: {} is {v}", a.tag),
        AlarmKind::BadQuality => format!("{name}: {}.{} has bad quality", a.service, a.tag),
        AlarmKind::Stale => format!("{name}: no update from {}.{} for over {} s", a.service, a.tag, a.stale_s.unwrap_or(0)),
        AlarmKind::Rate => format!(
            "{name}: changing {}{units}/min (limit {})",
            fmt_num(rate.unwrap_or(0.0)),
            fmt_num(a.rate_per_min.unwrap_or(0.0))
        ),
    }
}

// ------------------------------------------------------------------ engine

const RATE_WINDOW_MS: i64 = 60_000;

pub struct Engine {
    app: App,
    pub store: Arc<AlarmStore>,
    states: Mutex<HashMap<String, AlarmState>>,
    /// Recent good values per (service, tag), for rate alarms.
    history: Mutex<HashMap<(String, String), VecDeque<(i64, f64)>>>,
    hook_results: Mutex<HashMap<String, HookResult>>,
    tx: broadcast::Sender<Notice>,
    log_path: PathBuf,
    state_path: PathBuf,
    http: reqwest::Client,
}

impl Engine {
    pub fn new(app: App, store: Arc<AlarmStore>) -> Arc<Self> {
        let state_path = app.data_dir.join("alarm-state.json");
        let states = std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Arc::new(Self {
            log_path: app.data_dir.join("alarm-log.jsonl"),
            state_path,
            app,
            store,
            states: Mutex::new(states),
            history: Default::default(),
            hook_results: Default::default(),
            tx: broadcast::channel(512).0,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .user_agent(concat!("cowpoke/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("http client"),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Notice> {
        self.tx.subscribe()
    }

    pub fn states(&self) -> HashMap<String, AlarmState> {
        self.states.lock().clone()
    }

    pub fn hook_results(&self) -> HashMap<String, HookResult> {
        self.hook_results.lock().clone()
    }

    fn save_states(&self) {
        let text = serde_json::to_string(&*self.states.lock()).unwrap_or_default();
        let _ = std::fs::write(&self.state_path, text);
    }

    fn rate(&self, service: &str, tag: &str) -> Option<f64> {
        let h = self.history.lock();
        let q = h.get(&(service.to_string(), tag.to_string()))?;
        let (first, last) = (q.front()?, q.back()?);
        let span = last.0 - first.0;
        // Need at least 10 s of data for a meaningful per-minute rate.
        (span >= 10_000).then(|| (last.1 - first.1) / (span as f64 / 60_000.0))
    }

    /// Run forever: follow values, evaluate twice a second.
    pub async fn run(self: Arc<Self>) {
        let mut events = self.app.tags.subscribe();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                ev = events.recv() => match ev {
                    Ok(e) => {
                        if e.state.quality == Quality::Good {
                            if let Some(v) = as_f64(&e.state.value) {
                                let t = e.state.ts.timestamp_millis();
                                let mut h = self.history.lock();
                                let q = h.entry((e.service, e.tag)).or_default();
                                q.push_back((t, v));
                                while q.front().is_some_and(|f| t - f.0 > RATE_WINDOW_MS) {
                                    q.pop_front();
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                },
                _ = tick.tick() => self.evaluate(),
            }
        }
    }

    fn evaluate(self: &Arc<Self>) {
        let cfg = self.store.get();
        let services = self.app.cfg.get();
        let now = Utc::now();
        let mut fired: Vec<(AlarmDef, AlarmEvent)> = vec![];
        let mut changed = false;
        {
            let mut states = self.states.lock();
            // Forget alarms that were removed.
            let before = states.len();
            states.retain(|id, _| cfg.alarms.iter().any(|a| &a.id == id));
            changed |= states.len() != before;

            for a in &cfg.alarms {
                let svc = services.services.iter().find(|s| s.id == a.service);
                let running = svc.is_some_and(|s| s.enabled);
                let state = states.entry(a.id.clone()).or_default();
                if !a.enabled || !running {
                    // A disabled alarm or a stopped service doesn't alarm; drop anything pending.
                    if state.shown() || state.pending_since.is_some() {
                        *state = AlarmState::default();
                        changed = true;
                    }
                    continue;
                }
                let units = svc
                    .and_then(|s| s.tag(&a.tag))
                    .and_then(|t| t.units.clone())
                    .map(|u| format!(" {u}"))
                    .unwrap_or_default();
                let tag = self.app.tags.get(&a.service, &a.tag);
                let rate = if a.kind == AlarmKind::Rate { self.rate(&a.service, &a.tag) } else { None };
                let Some(cond) = condition(a, tag.as_ref(), rate, state.active, now) else {
                    continue;
                };
                let value = tag.map(|t| t.value).unwrap_or(Value::Null);
                if cond && !state.active {
                    let since = *state.pending_since.get_or_insert(now);
                    if (now - since).num_milliseconds() >= a.delay_s as i64 * 1000 {
                        let message = match &a.message {
                            Some(t) => {
                                let v = value.clone();
                                fill_text(t, &|k: &str| match k {
                                    "value" => Some(v.clone()),
                                    "name" => Some(json!(a.name)),
                                    "tag" => Some(json!(a.tag)),
                                    "service" => Some(json!(a.service)),
                                    "limit" => a.limit.map(|l| json!(l)),
                                    _ => None,
                                })
                            }
                            None => default_message(a, &value, rate, &units),
                        };
                        *state = AlarmState {
                            active: true,
                            acked: false,
                            since: Some(now),
                            cleared: None,
                            acked_by: None,
                            value: value.clone(),
                            message: message.clone(),
                            pending_since: None,
                        };
                        changed = true;
                        fired.push((a.clone(), event(a, EventKind::Raise, value, message, None)));
                    }
                } else if !cond {
                    state.pending_since = None;
                    if state.active {
                        state.active = false;
                        state.cleared = Some(now);
                        let message = state.message.clone();
                        if state.acked {
                            *state = AlarmState::default();
                        }
                        changed = true;
                        fired.push((a.clone(), event(a, EventKind::Clear, value, message, None)));
                    }
                }
            }
        }
        if changed {
            self.save_states();
        }
        for (a, e) in fired {
            self.emit(&a, e);
        }
    }

    /// Acknowledge an alarm. Returns false if there's nothing to acknowledge.
    pub fn ack(self: &Arc<Self>, id: &str, user: &str) -> bool {
        let Some(a) = self.store.get().alarms.into_iter().find(|a| a.id == id) else {
            return false;
        };
        let ev = {
            let mut states = self.states.lock();
            let Some(st) = states.get_mut(id) else { return false };
            if !st.shown() || st.acked {
                return false;
            }
            st.acked = true;
            st.acked_by = Some(user.to_string());
            let ev = event(&a, EventKind::Ack, st.value.clone(), st.message.clone(), Some(user.to_string()));
            if !st.active {
                *st = AlarmState::default();
            }
            ev
        };
        self.save_states();
        self.emit(&a, ev);
        true
    }

    fn emit(self: &Arc<Self>, a: &AlarmDef, e: AlarmEvent) {
        tracing::info!(alarm = %a.id, "{:?}: {}", e.event, e.message);
        self.log(&e);
        let _ = self.tx.send(Notice::Alarm(e.clone()));
        let hooks = self.store.get().hooks;
        for id in &a.hooks {
            let Some(h) = hooks.iter().find(|h| &h.id == id) else { continue };
            if h.enabled && h.on.contains(&e.event) {
                self.clone().spawn_hook(h.clone(), e.clone());
            }
        }
    }

    /// Run one hook in the background, retrying transient failures.
    pub fn spawn_hook(self: Arc<Self>, h: HookDef, e: AlarmEvent) {
        tokio::spawn(async move {
            let mut last = Err(anyhow!("not run"));
            for attempt in 0..3 {
                last = self.run_hook(&h, &e).await;
                if last.is_ok() || matches!(h.kind, HookKind::Write { .. }) {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2u64.pow(attempt) * 2)).await;
            }
            let result = HookResult {
                time: Utc::now(),
                ok: last.is_ok(),
                detail: match &last {
                    Ok(d) => d.clone(),
                    Err(err) => format!("{err:#}"),
                },
            };
            if !result.ok {
                tracing::warn!(hook = %h.id, "hook failed: {}", result.detail);
            }
            self.hook_results.lock().insert(h.id.clone(), result.clone());
            let _ = self.tx.send(Notice::Hook { hook: h.id.clone(), alarm: e.alarm.clone(), result });
        });
    }

    async fn run_hook(&self, h: &HookDef, e: &AlarmEvent) -> Result<String> {
        let vars = event_vars(e);
        match &h.kind {
            HookKind::Webhook { url, method, headers, body } => {
                let method = reqwest::Method::from_bytes(method.to_uppercase().as_bytes())
                    .map_err(|_| anyhow!("bad method `{method}`"))?;
                let mut rb = self.http.request(method, fill_text(url, &vars));
                for (k, v) in headers {
                    rb = rb.header(k, fill_text(v, &vars));
                }
                rb = match body {
                    None => rb.json(e),
                    Some(Value::String(t)) => rb.body(fill_text(t, &vars)),
                    Some(v) => rb.json(&fill_json(v, &vars)),
                };
                let resp = rb.send().await?;
                let status = resp.status();
                if !status.is_success() {
                    let text = resp.text().await.unwrap_or_default();
                    bail!("HTTP {status}: {}", text.chars().take(200).collect::<String>());
                }
                Ok(format!("HTTP {status}"))
            }
            HookKind::Write { service, tag, value, clear_value } => {
                let v = match e.event {
                    EventKind::Raise | EventKind::Test => value.clone(),
                    EventKind::Clear => match clear_value {
                        Some(v) => v.clone(),
                        None => return Ok("no clear_value; nothing written".into()),
                    },
                    EventKind::Ack => return Ok("nothing to write on acknowledge".into()),
                };
                let v = fill_json(&v, &vars);
                let def = self
                    .app
                    .cfg
                    .service(service)
                    .ok_or_else(|| anyhow!("no service `{service}`"))?;
                let written = match &def.agent {
                    Some(agent) => self.app.agents.write(agent, service, tag, v).await,
                    None => self.app.mgr.write(&def, tag, &v).await,
                }
                .map_err(|e| anyhow!(e))?;
                Ok(format!("wrote {written} to {service}.{tag}"))
            }
            HookKind::Email { host, port, security, username, password, from, to, subject, body } => {
                use lettre::message::Mailbox;
                use lettre::transport::smtp::authentication::Credentials;
                use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
                let subject = fill_text(
                    subject.as_deref().unwrap_or("[Cowpoke] {{severity}} {{event}}: {{name}}"),
                    &vars,
                );
                let body = fill_text(
                    body.as_deref().unwrap_or(
                        "{{message}}\n\nAlarm: {{name}} ({{alarm}})\nEvent: {{event}}\nSeverity: {{severity}}\nTag: {{service}}.{{tag}} = {{value}}\nTime: {{time}}\n",
                    ),
                    &vars,
                );
                let mut msg = Message::builder()
                    .from(from.parse::<Mailbox>().context("bad `from` address")?)
                    .subject(subject);
                for t in to {
                    msg = msg.to(t.parse::<Mailbox>().with_context(|| format!("bad address `{t}`"))?);
                }
                let msg = msg.body(body)?;
                let mut tr = match security.as_str() {
                    "tls" => AsyncSmtpTransport::<Tokio1Executor>::relay(host)?,
                    "none" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host),
                    _ => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)?,
                };
                if let Some(p) = port {
                    tr = tr.port(*p);
                }
                if let (Some(u), Some(p)) = (username, password) {
                    tr = tr.credentials(Credentials::new(u.clone(), p.clone()));
                }
                let tr = tr.timeout(Some(Duration::from_secs(20))).build();
                tr.send(msg).await?;
                Ok(format!("sent to {}", to.join(", ")))
            }
            HookKind::Mqtt { host, port, topic, username, password, qos, retain, payload } => {
                use rumqttc::{AsyncClient, Event, MqttOptions, Outgoing, Packet, QoS};
                let id = format!("cowpoke-{}", crate::auth::random_token(4));
                let mut opts = MqttOptions::new(id, host.clone(), *port);
                opts.set_keep_alive(Duration::from_secs(10));
                if let (Some(u), Some(p)) = (username, password) {
                    opts.set_credentials(u.clone(), p.clone());
                }
                let (client, mut lp) = AsyncClient::new(opts, 8);
                let topic = fill_text(topic, &vars);
                let body = match payload {
                    None => serde_json::to_vec(e)?,
                    Some(Value::String(t)) => fill_text(t, &vars).into_bytes(),
                    Some(v) => serde_json::to_vec(&fill_json(v, &vars))?,
                };
                let q = if *qos == 1 { QoS::AtLeastOnce } else { QoS::AtMostOnce };
                client.publish(topic.clone(), q, *retain, body).await?;
                let done = async {
                    loop {
                        match lp.poll().await? {
                            Event::Incoming(Packet::PubAck(_)) if q == QoS::AtLeastOnce => return Ok::<_, anyhow::Error>(()),
                            Event::Outgoing(Outgoing::Publish(_)) if q == QoS::AtMostOnce => return Ok(()),
                            _ => {}
                        }
                    }
                };
                tokio::time::timeout(Duration::from_secs(10), done)
                    .await
                    .map_err(|_| anyhow!("timed out talking to {host}:{port}"))??;
                let _ = client.disconnect().await;
                let _ = tokio::time::timeout(Duration::from_millis(500), lp.poll()).await;
                Ok(format!("published to {topic}"))
            }
        }
    }

    fn log(&self, e: &AlarmEvent) {
        use std::io::Write;
        if std::fs::metadata(&self.log_path).is_ok_and(|m| m.len() > 5_000_000) {
            let _ = std::fs::rename(&self.log_path, self.log_path.with_extension("jsonl.1"));
        }
        let line = serde_json::to_string(e).unwrap_or_default();
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .and_then(|mut f| writeln!(f, "{line}"));
    }

    /// The newest `limit` events, newest first.
    pub fn log_tail(&self, limit: usize) -> Vec<AlarmEvent> {
        let text = std::fs::read_to_string(&self.log_path).unwrap_or_default();
        text.lines()
            .rev()
            .filter_map(|l| serde_json::from_str(l).ok())
            .take(limit)
            .collect()
    }
}

fn event(a: &AlarmDef, kind: EventKind, value: Value, message: String, user: Option<String>) -> AlarmEvent {
    AlarmEvent {
        time: Utc::now(),
        event: kind,
        alarm: a.id.clone(),
        name: if a.name.is_empty() { a.id.clone() } else { a.name.clone() },
        severity: a.severity,
        service: a.service.clone(),
        tag: a.tag.clone(),
        value,
        message,
        user,
    }
}

/// A sample event for testing a hook.
pub fn test_event(user: &str) -> AlarmEvent {
    AlarmEvent {
        time: Utc::now(),
        event: EventKind::Test,
        alarm: "test".into(),
        name: "Test alarm".into(),
        severity: Severity::Medium,
        service: "test".into(),
        tag: "test".into(),
        value: json!(42),
        message: "This is a test from Cowpoke.".into(),
        user: Some(user.to_string()),
    }
}

impl Engine {
    /// Run a hook once with a test event and wait for the result.
    pub async fn test_hook(&self, h: &HookDef, user: &str) -> HookResult {
        let e = test_event(user);
        let r = self.run_hook(h, &e).await;
        let result = HookResult {
            time: Utc::now(),
            ok: r.is_ok(),
            detail: match r {
                Ok(d) => d,
                Err(e) => format!("{e:#}"),
            },
        };
        self.hook_results.lock().insert(h.id.clone(), result.clone());
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags::TagState;

    fn alarm(kind: AlarmKind) -> AlarmDef {
        serde_json::from_value(json!({"id": "a", "service": "s", "tag": "t", "kind": kind})).unwrap()
    }

    fn st(v: Value) -> TagState {
        TagState { value: v, quality: Quality::Good, ts: Utc::now() }
    }

    #[test]
    fn limits_with_deadband() {
        let mut a = alarm(AlarmKind::High);
        a.limit = Some(90.0);
        a.deadband = 2.0;
        let now = Utc::now();
        assert_eq!(condition(&a, Some(&st(json!(91))), None, false, now), Some(true));
        assert_eq!(condition(&a, Some(&st(json!(89))), None, false, now), Some(false));
        // Active: stays raised until below 88.
        assert_eq!(condition(&a, Some(&st(json!(89))), None, true, now), Some(true));
        assert_eq!(condition(&a, Some(&st(json!(87.9))), None, true, now), Some(false));
        // Bad data: no decision.
        let mut bad = st(json!(95));
        bad.quality = Quality::Bad;
        assert_eq!(condition(&a, Some(&bad), None, false, now), None);
    }

    #[test]
    fn equals_quality_stale_rate() {
        let now = Utc::now();
        let mut a = alarm(AlarmKind::Equals);
        a.value = Some(json!(true));
        assert_eq!(condition(&a, Some(&st(json!(true))), None, false, now), Some(true));
        a.value = Some(json!("3"));
        assert_eq!(condition(&a, Some(&st(json!(3))), None, false, now), Some(true));

        let q = alarm(AlarmKind::BadQuality);
        let mut bad = st(json!(1));
        bad.quality = Quality::Bad;
        assert_eq!(condition(&q, Some(&bad), None, false, now), Some(true));
        assert_eq!(condition(&q, None, None, false, now), Some(true));

        let mut s = alarm(AlarmKind::Stale);
        s.stale_s = Some(30);
        let mut old = st(json!(1));
        old.ts = now - chrono::Duration::seconds(31);
        assert_eq!(condition(&s, Some(&old), None, false, now), Some(true));

        let mut r = alarm(AlarmKind::Rate);
        r.rate_per_min = Some(5.0);
        assert_eq!(condition(&r, Some(&st(json!(1))), Some(-6.0), false, now), Some(true));
        assert_eq!(condition(&r, Some(&st(json!(1))), None, false, now), None);
    }

    #[test]
    fn templates_and_validation() {
        let e = test_event("ann");
        assert_eq!(fill_text("{{name}} {{event}} = {{value}} by {{user}} {{nope}}", &event_vars(&e)), "Test alarm test = 42 by ann {{nope}}");
        assert_eq!(fill_json(&json!({"v": "{{value}}", "t": "x {{tag}}"}), &event_vars(&e)), json!({"v": 42, "t": "x test"}));

        let cfg: AlarmConfig = serde_yaml::from_str(
            "alarms:\n  - { id: hi, service: s, tag: t, kind: high, limit: 5, hooks: [slack] }\nhooks:\n  - { id: slack, kind: webhook, url: 'http://x' }\n",
        )
        .unwrap();
        cfg.validate().unwrap();
        let mut broken = cfg.clone();
        broken.alarms[0].hooks = vec!["nope".into()];
        assert!(broken.validate().unwrap_err().contains("no hook"));
        broken.alarms[0].hooks.clear();
        broken.alarms[0].limit = None;
        assert!(broken.validate().is_err());
    }
}
