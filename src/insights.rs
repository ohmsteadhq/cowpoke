//! Baselines and insights: the foreman's first phase, with no model in it.
//!
//! The baseline watches every tag change and samples every service's status, building
//! per-tag statistics (update and change rates, quality, range, noise, gaps between
//! changes). Analyzers turn those, the service definitions and the alarm log into
//! insights: communication changes (polling faster than tags change, flapping
//! connections, intermittent bad reads, historian deadbands) and alarm hygiene
//! (chattering, standing, flooding, unacknowledged alarms). Insights are suggestions
//! with evidence; nothing is changed without a person.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::alarms::{AlarmEvent, AlarmState, EventKind};
use crate::manager::State;
use crate::model::{Config, DataType, HistoryMode, Mode, ServiceDef};
use crate::tags::{Quality, TagEvent, as_f64};

/// Device clock offsets (device time − Cowpoke's time, ms) per service, from timestamps
/// devices send: OPC UA server timestamps, Sparkplug payload timestamps.
static CLOCKS: std::sync::LazyLock<dashmap::DashMap<String, Vec<f64>>> = std::sync::LazyLock::new(Default::default);

/// Record one clock offset for a service's device.
pub fn note_clock_offset(service: &str, offset_ms: f64) {
    let mut v = CLOCKS.entry(service.to_string()).or_default();
    if v.len() >= 64 {
        v.remove(0);
    }
    v.push(offset_ms);
}

/// Median offset and sample count per service.
pub fn clock_offsets() -> Vec<(String, f64, usize)> {
    CLOCKS.iter().filter_map(|e| median(e.value()).map(|m| (e.key().clone(), m, e.value().len()))).collect()
}

/// Samples kept for medians (gaps between changes, step sizes).
const RESERVOIR: usize = 256;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct TagStats {
    pub first: Option<DateTime<Utc>>,
    pub last_update: Option<DateTime<Utc>>,
    pub last_change: Option<DateTime<Utc>>,
    /// Every update the service delivered (polls included).
    pub updates: u64,
    /// Updates whose value differed from the one before.
    pub changes: u64,
    pub bad: u64,
    /// Running mean and variance (Welford) of numeric values.
    pub n: u64,
    pub mean: f64,
    pub m2: f64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Seconds between value changes (the most recent samples).
    pub gaps: Vec<f64>,
    /// Size of each numeric change (the most recent samples).
    pub steps: Vec<f64>,
    /// Per-hour statistics of numeric values, the last 72 hours.
    #[serde(default)]
    pub hours: Vec<Bucket>,
    /// The latest numeric values (ms since epoch, value), for short-term trends.
    #[serde(default)]
    pub recent: Vec<(i64, f64)>,
    /// Values more than 6 standard deviations from the mean.
    #[serde(default)]
    pub spikes: u64,
    /// Values at an integer type's limits (32767, 65535, …): a broken wire or a sensor out of range.
    #[serde(default)]
    pub rails: u64,
    /// On/off tags: the current state and since when, and how long earlier runs of
    /// each state lasted and the time between switch-ons (seconds).
    #[serde(default)]
    pub state: Option<bool>,
    #[serde(default)]
    pub state_since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub on_runs: Vec<f64>,
    #[serde(default)]
    pub off_runs: Vec<f64>,
    #[serde(default)]
    pub periods: Vec<f64>,
    #[serde(default)]
    pub last_rise: Option<DateTime<Utc>>,
    #[serde(skip)]
    last_value: Option<Value>,
}

/// One hour of a tag's numeric values.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Bucket {
    /// Hours since the epoch.
    pub hour: i64,
    pub n: u64,
    pub mean: f64,
    pub m2: f64,
}

impl Bucket {
    pub fn std(&self) -> f64 {
        if self.n > 1 { (self.m2 / (self.n - 1) as f64).sqrt() } else { 0.0 }
    }
}

const RAILS: [f64; 6] = [32767.0, -32768.0, 65535.0, 2147483647.0, -2147483648.0, 4294967295.0];

fn push(v: &mut Vec<f64>, x: f64) {
    if v.len() >= RESERVOIR {
        v.remove(0);
    }
    v.push(x);
}

pub fn median(v: &[f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(s[s.len() / 2])
}

impl TagStats {
    pub fn observe(&mut self, ts: DateTime<Utc>, v: &Value, q: Quality) {
        self.first.get_or_insert(ts);
        self.last_update = Some(ts);
        self.updates += 1;
        if q == Quality::Bad {
            self.bad += 1;
            return;
        }
        let changed = self.last_value.as_ref() != Some(v);
        if changed {
            if self.last_value.is_some() {
                self.changes += 1;
                if let Some(prev) = self.last_change {
                    push(&mut self.gaps, (ts - prev).num_milliseconds().max(0) as f64 / 1000.0);
                }
                if let (Some(a), Some(b)) = (self.last_value.as_ref().and_then(as_f64), as_f64(v)) {
                    push(&mut self.steps, (b - a).abs());
                }
            }
            self.last_change = Some(ts);
            self.last_value = Some(v.clone());
        }
        if let Value::Bool(b) = v {
            if self.state != Some(*b) {
                if let (Some(prev), Some(since)) = (self.state, self.state_since) {
                    let d = (ts - since).num_milliseconds().max(0) as f64 / 1000.0;
                    push(if prev { &mut self.on_runs } else { &mut self.off_runs }, d);
                }
                if *b {
                    if let Some(r) = self.last_rise {
                        push(&mut self.periods, (ts - r).num_milliseconds().max(0) as f64 / 1000.0);
                    }
                    self.last_rise = Some(ts);
                }
                self.state = Some(*b);
                self.state_since = Some(ts);
            }
            return;
        }
        if let Some(x) = as_f64(v).filter(|x| x.is_finite()) {
            if RAILS.contains(&x) {
                self.rails += 1;
            }
            let sd = self.std();
            if self.n > 100 && sd > 0.0 && (x - self.mean).abs() > 6.0 * sd {
                self.spikes += 1;
            }
            let hour = ts.timestamp().div_euclid(3600);
            if self.hours.last().is_none_or(|b| b.hour != hour) {
                self.hours.push(Bucket { hour, ..Default::default() });
                if self.hours.len() > 72 {
                    self.hours.remove(0);
                }
            }
            let b = self.hours.last_mut().unwrap();
            b.n += 1;
            let d = x - b.mean;
            b.mean += d / b.n as f64;
            b.m2 += d * (x - b.mean);
            if self.recent.len() >= 120 {
                self.recent.remove(0);
            }
            self.recent.push((ts.timestamp_millis(), x));
            self.n += 1;
            let d = x - self.mean;
            self.mean += d / self.n as f64;
            self.m2 += d * (x - self.mean);
            self.min = Some(self.min.map_or(x, |m| m.min(x)));
            self.max = Some(self.max.map_or(x, |m| m.max(x)));
        }
    }

    pub fn std(&self) -> f64 {
        if self.n > 1 { (self.m2 / (self.n - 1) as f64).sqrt() } else { 0.0 }
    }

    /// How long this tag has been watched.
    pub fn span(&self) -> TimeDelta {
        match (self.first, self.last_update) {
            (Some(a), Some(b)) => b - a,
            _ => TimeDelta::zero(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ServiceStats {
    pub samples: u64,
    pub degraded: u64,
    pub errors: u64,
    /// Running ↔ not-running transitions.
    pub transitions: u64,
    pub last_state: Option<String>,
    pub last_detail: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Area {
    Network,
    Comms,
    Equipment,
    Process,
    Data,
    Alarms,
    Historian,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// Worth knowing.
    Info,
    /// A change would help.
    Suggest,
    /// Something is probably wrong.
    Warn,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Insight {
    /// Stable across runs, so a dismissal sticks.
    pub id: String,
    pub area: Area,
    pub level: Level,
    pub title: String,
    pub detail: String,
    pub evidence: Vec<String>,
    /// The change to make, in words.
    pub suggestion: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// The change as data, for the UI to pre-fill: `{"setting": key, "value": v}` on the
    /// service's settings, or `{"history": cfg}` replacing `tag`'s history settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<Value>,
}

fn id(parts: &[&str]) -> String {
    parts.join(":")
}

fn secs(s: f64) -> String {
    if s < 90.0 {
        format!("{s:.0} s")
    } else if s < 5400.0 {
        format!("{:.0} min", s / 60.0)
    } else {
        format!("{:.1} h", s / 3600.0)
    }
}

/// A round, human poll interval at or below `ms`.
fn round_ms(ms: f64) -> u64 {
    const STEPS: [u64; 12] = [100, 250, 500, 1000, 2000, 5000, 10_000, 15_000, 30_000, 60_000, 300_000, 600_000];
    STEPS.iter().rev().copied().find(|&s| (s as f64) <= ms).unwrap_or(100)
}

/// A round deadband near `x`.
fn round_db(x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    let p = 10f64.powf(x.log10().floor());
    let m = x / p;
    let r = if m < 1.5 { 1.0 } else if m < 3.5 { 2.0 } else if m < 7.5 { 5.0 } else { 10.0 };
    r * p
}

/// The poll period a client service uses, in ms.
fn poll_ms(def: &ServiceDef) -> Option<f64> {
    if def.mode != Mode::Client {
        return None;
    }
    let s = &def.settings;
    if def.kind == crate::model::Kind::Opcua && s.get("read").and_then(|v| v.as_str()) == Some("subscribe") {
        return None; // pushed by the server, like MQTT
    }
    s.get("poll_ms").or_else(|| s.get("interval_ms")).and_then(|v| v.as_f64()).or_else(|| {
        use crate::model::Kind::*;
        match def.kind {
            Modbus | Opcua | Iolink | Eip | S7 => Some(1000.0),
            Api => Some(5000.0),
            _ => None, // MQTT and monitors are pushed, not polled
        }
    })
}

/// Thresholds; the defaults suit a plant watched for an hour or more.
#[derive(Clone, Debug)]
pub struct Tuning {
    /// Minimum time a tag must be watched before it gets insights.
    pub min_watch: TimeDelta,
    pub min_updates: u64,
    /// Polling counts as too fast when tags change this many poll periods apart (median).
    pub overpoll_factor: f64,
    pub bad_ratio: f64,
    pub flap_transitions: u64,
    pub frozen_after: TimeDelta,
    pub chatter_raises: usize,
    pub chatter_window: TimeDelta,
    pub flood_raises: usize,
    pub flood_window: TimeDelta,
    pub standing_after: TimeDelta,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            min_watch: TimeDelta::minutes(30),
            min_updates: 50,
            overpoll_factor: 20.0,
            bad_ratio: 0.05,
            flap_transitions: 6,
            frozen_after: TimeDelta::hours(6),
            chatter_raises: 5,
            chatter_window: TimeDelta::minutes(10),
            flood_raises: 10,
            flood_window: TimeDelta::minutes(10),
            standing_after: TimeDelta::hours(24),
        }
    }
}

impl Tuning {
    /// Minutes instead of hours, for demos and trying it out (`COWPOKE_INSIGHTS_QUICK=1`).
    pub fn quick() -> Self {
        Self {
            min_watch: TimeDelta::minutes(1),
            min_updates: 20,
            frozen_after: TimeDelta::minutes(2),
            standing_after: TimeDelta::minutes(5),
            ..Self::default()
        }
    }
}

/// Everything the analyzers look at, gathered at one moment.
pub struct Snapshot<'a> {
    pub now: DateTime<Utc>,
    pub cfg: &'a Config,
    pub tags: &'a HashMap<(String, String), TagStats>,
    pub services: &'a HashMap<String, ServiceStats>,
    pub alarm_log: &'a [AlarmEvent],
    pub alarm_states: &'a HashMap<String, AlarmState>,
    pub alarm_names: &'a HashMap<String, String>,
    /// Each traffic monitor's talkers and address conflicts.
    pub monitors: &'a [(String, Vec<crate::monitor::Talker>, Vec<crate::monitor::AddressConflict>)],
    pub alarm_defs: &'a [crate::alarms::AlarmDef],
    /// Command/feedback pairs and what has happened to each.
    pub pairs: &'a [(Pair, PairState)],
    /// Changes made to devices: writes through Cowpoke, program and run/stop actions seen on the wire.
    pub changes: &'a [Change],
    /// The traffic monitors' recent events.
    pub monitor_events: &'a [crate::monitor::Event],
    /// Device clock offsets: (service, median ms, samples).
    pub clocks: &'a [(String, f64, usize)],
}

/// A change made to a device, to compare what happened before and after.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Change {
    pub ts: DateTime<Utc>,
    /// Who made it: "Cowpoke" for writes through Cowpoke, else the client's address.
    pub by: String,
    /// The Cowpoke service it concerns, when known.
    pub service: Option<String>,
    /// The device address (`ip:port`), for changes seen on the wire.
    pub device: Option<String>,
    pub what: String,
}

// ------------------------------------------------------------------ configuration

/// `insights.yaml` in the data folder: what Cowpoke can't work out alone.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct InsightsConfig {
    /// Commands and the feedback that should follow them.
    #[serde(default)]
    pub pairs: Vec<Pair>,
    /// Send a summary of new insights through alarm hooks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<Digest>,
}

/// A daily or weekly summary of new insights, sent through alarm hooks
/// (email, webhook, MQTT): `{{message}}` holds the text, `{{value}}` the count.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Digest {
    /// Hook ids from the Alarms page.
    pub hooks: Vec<String>,
    /// Hour of day to send it (UTC).
    #[serde(default = "default_digest_hour")]
    pub hour: u32,
    /// Mondays only, instead of every day.
    #[serde(default)]
    pub weekly: bool,
}

fn default_digest_hour() -> u32 {
    7
}

/// A command and its feedback: a valve and its position, a motor and its current.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Pair {
    /// What to call it, e.g. "Feed pump".
    pub name: String,
    /// `service/tag` of the command (a run bool or a setpoint).
    pub command: String,
    /// `service/tag` of the feedback (running bool, current, position).
    pub feedback: String,
    #[serde(default)]
    pub kind: PairKind,
    /// `follows`: how far feedback may differ from the command (engineering units).
    #[serde(default)]
    pub tolerance: f64,
    /// Seconds the feedback gets to respond before it counts as missing.
    #[serde(default = "default_delay")]
    pub delay_s: u64,
}

fn default_delay() -> u64 {
    10
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PairKind {
    /// While the command is on (true or non-zero), the feedback must be on too.
    #[default]
    Runs,
    /// The feedback must come within `tolerance` of the command (a position or speed).
    Follows,
}

impl Pair {
    fn split(s: &str) -> Option<(String, String)> {
        s.split_once('/').map(|(a, b)| (a.to_string(), b.to_string()))
    }
    pub fn validate(&self) -> Result<(), String> {
        if Self::split(&self.command).is_none() || Self::split(&self.feedback).is_none() {
            return Err(format!("pair `{}`: command and feedback are `service/tag`", self.name));
        }
        Ok(())
    }
}

/// What a pair has done: when the current mismatch began, and past ones.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct PairState {
    pub mismatch_since: Option<DateTime<Utc>>,
    /// Mismatches that lasted longer than the delay.
    pub incidents: u64,
    pub last_incident: Option<DateTime<Utc>>,
    /// The values at the last incident.
    pub last_detail: String,
    #[serde(skip)]
    counted: bool,
}

fn on(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        _ => as_f64(v).is_some_and(|x| x.abs() > 1e-9),
    }
}

/// Check one pair with the latest values of both tags.
pub fn check_pair(p: &Pair, st: &mut PairState, now: DateTime<Utc>, cmd: &Value, fb: &Value) {
    let bad = match p.kind {
        PairKind::Runs => on(cmd) && !on(fb),
        PairKind::Follows => match (as_f64(cmd), as_f64(fb)) {
            (Some(c), Some(f)) => (c - f).abs() > p.tolerance.max(1e-9),
            _ => false,
        },
    };
    if !bad {
        st.mismatch_since = None;
        st.counted = false;
        return;
    }
    let since = *st.mismatch_since.get_or_insert(now);
    if !st.counted && (now - since).num_seconds() >= p.delay_s as i64 {
        st.counted = true;
        st.incidents += 1;
        st.last_incident = Some(now);
        st.last_detail = format!("command {cmd}, feedback {fb}");
    }
}

/// Run every analyzer.
pub fn analyze(s: &Snapshot, t: &Tuning) -> Vec<Insight> {
    let mut out = vec![];
    network(s, &mut out);
    comms(s, t, &mut out);
    equipment(s, t, &mut out);
    process(s, t, &mut out);
    pairs(s, &mut out);
    effects(s, &mut out);
    off_hours(s, &mut out);
    data_quality(s, t, &mut out);
    clocks(s, &mut out);
    historian(s, t, &mut out);
    alarms(s, t, &mut out);
    out.sort_by(|a, b| b.level.cmp(&a.level).then(a.area.cmp(&b.area)).then(a.id.cmp(&b.id)));
    out
}

/// From the traffic monitor: what the wire says about devices and links.
fn network(s: &Snapshot, out: &mut Vec<Insight>) {
    for (mon, talkers, conflicts) in s.monitors {
        for c in conflicts {
            out.push(Insight {
                id: id(&["dupip", mon, &c.ip]),
                area: Area::Network,
                level: Level::Warn,
                title: format!("Two network cards answer for {}", c.ip),
                detail: "More than one device is using this IP address, so traffic for one goes to the other at random: intermittent timeouts and wrong values.".into(),
                evidence: c.macs.iter().map(|m| format!("MAC {m}")).chain([format!("since {}", c.first_seen.format("%b %d %H:%M"))]).collect(),
                suggestion: "Find both devices by MAC (switch tables) and give one a free address. A redundant router pair (VRRP) can look like this too.".into(),
                service: Some(mon.clone()),
                tag: None,
                fix: None,
            });
        }
        let mut open_per_device: HashMap<&str, (i64, usize)> = HashMap::new();
        for tk in talkers {
            let pair = format!("{} → {}", tk.client, tk.server);
            let tid = format!("{}>{}", tk.client, tk.server);
            let span = (tk.last_seen - tk.first_seen).num_seconds().max(1) as f64;
            let e = open_per_device.entry(&tk.server).or_default();
            e.0 += tk.open;
            e.1 += (tk.open > 0) as usize;
            let asked = tk.answered + tk.timeouts;
            if tk.timeouts >= 5 && tk.timeouts as f64 / asked.max(1) as f64 >= 0.02 {
                out.push(Insight {
                    id: id(&["timeouts", mon, &tid]),
                    area: Area::Network,
                    level: Level::Warn,
                    title: format!("{pair}: {} requests went unanswered", tk.timeouts),
                    detail: format!("{:.1}% of requests got no answer within 5 s. The device is overloaded, rebooting, or the path drops packets.", tk.timeouts as f64 * 100.0 / asked as f64),
                    evidence: vec![format!("{} answered, {} unanswered", tk.answered, tk.timeouts)],
                    suggestion: "Check the device's load and connection limit, and the switch port's error counters.".into(),
                    service: Some(mon.clone()), tag: None, fix: None,
                });
            }
            if let (Some(p50), Some(p95)) = (tk.latency_p50_ms, tk.latency_p95_ms) {
                if tk.answered >= 50 && p95 >= 250.0 {
                    out.push(Insight {
                        id: id(&["slow", mon, &tid]),
                        area: Area::Network,
                        level: Level::Suggest,
                        title: format!("{} answers slowly", tk.server),
                        detail: "On a plant network a PLC normally answers within a few tens of milliseconds. Slow answers mean a busy CPU, too many clients, or a congested link.".into(),
                        evidence: vec![format!("median {p50:.0} ms, 95th percentile {p95:.0} ms over {} requests from {}", tk.answered, tk.client)],
                        suggestion: "Poll less often or in bigger blocks, spread clients over time, and check the PLC's cycle and communication load.".into(),
                        service: Some(mon.clone()), tag: None, fix: None,
                    });
                }
            }
            if tk.retransmits >= 10 && tk.retransmits as f64 / tk.segments.max(1) as f64 >= 0.01 {
                out.push(Insight {
                    id: id(&["retrans", mon, &tid]),
                    area: Area::Network,
                    level: Level::Warn,
                    title: format!("{pair}: packets are being resent"),
                    detail: format!("{:.1}% of data packets were retransmissions: something on the path drops packets.", tk.retransmits as f64 * 100.0 / tk.segments.max(1) as f64),
                    evidence: vec![format!("{} of {} segments", tk.retransmits, tk.segments)],
                    suggestion: "Check cables and connectors, the switch port's CRC and error counters, duplex settings, and noise near VFD wiring.".into(),
                    service: Some(mon.clone()), tag: None, fix: None,
                });
            }
            let per_hour = tk.connections as f64 * 3600.0 / span;
            if span >= 600.0 && tk.connections >= 10 && per_hour >= 30.0 {
                out.push(Insight {
                    id: id(&["churn", mon, &tid]),
                    area: Area::Network,
                    level: Level::Suggest,
                    title: format!("{pair}: reconnects about {per_hour:.0} times an hour"),
                    detail: "Opening a new connection for every few requests costs the device time and can hit its connection limit.".into(),
                    evidence: vec![format!("{} connections in {}", tk.connections, secs(span))],
                    suggestion: "Configure the client to keep its connection open, or check why the device closes it.".into(),
                    service: Some(mon.clone()), tag: None, fix: None,
                });
            }
            let rate = (tk.bytes_up + tk.bytes_down) as f64 / span;
            if span >= 60.0 && rate >= 1_000_000.0 {
                out.push(Insight {
                    id: id(&["bandwidth", mon, &tid]),
                    area: Area::Network,
                    level: Level::Info,
                    title: format!("{pair} moves {:.1} MB/s", rate / 1e6),
                    detail: "Far more traffic than polling usually needs.".into(),
                    evidence: vec![format!("{:.0} MB up, {:.0} MB down", tk.bytes_up as f64 / 1e6, tk.bytes_down as f64 / 1e6)],
                    suggestion: "Check what it reads: large blocks polled fast, file transfers, or a misconfigured client.".into(),
                    service: Some(mon.clone()), tag: None, fix: None,
                });
            }
        }
        for (dev, (open, clients)) in open_per_device {
            if open >= 8 {
                out.push(Insight {
                    id: id(&["crowded", mon, dev]),
                    area: Area::Network,
                    level: Level::Info,
                    title: format!("{dev} has {open} connections open from {clients} clients"),
                    detail: "PLCs allow a limited number of connections (an S7-1200 a handful for HMIs and PUT/GET). New clients get refused once it's full.".into(),
                    evidence: vec![],
                    suggestion: "Let fewer clients read the device directly: one gateway (or Cowpoke) reading, the others reading from it.".into(),
                    service: Some(mon.clone()), tag: None, fix: None,
                });
            }
        }
    }
}

fn comms(s: &Snapshot, t: &Tuning, out: &mut Vec<Insight>) {
    for def in &s.cfg.services {
        let name = def.name_or_id();
        // A connection that keeps dropping.
        if let Some(st) = s.services.get(&def.id) {
            if st.transitions >= t.flap_transitions {
                out.push(Insight {
                    id: id(&["flap", &def.id]),
                    area: Area::Comms,
                    level: Level::Warn,
                    title: format!("{name} keeps losing its connection"),
                    detail: format!("It went down and came back {} times while watched.", st.transitions / 2),
                    evidence: vec![format!("last status: {}", if st.last_detail.is_empty() { "-" } else { &st.last_detail })],
                    suggestion: "Check the network path and the device's connection limit; raise the timeout if the device answers slowly.".into(),
                    service: Some(def.id.clone()),
                    tag: None,
                    fix: None,
                });
            }
        }
        let Some(poll) = poll_ms(def) else { continue };
        let mut slow: Vec<(String, f64)> = vec![];
        let mut watched = 0;
        for tg in &def.tags {
            let Some(st) = s.tags.get(&(def.id.clone(), tg.name.clone())) else { continue };
            if st.span() < t.min_watch || st.updates < t.min_updates {
                continue;
            }
            watched += 1;
            // Intermittent bad reads.
            let ratio = st.bad as f64 / st.updates as f64;
            if ratio >= t.bad_ratio && ratio < 0.95 && st.bad >= 10 {
                out.push(Insight {
                    id: id(&["bad", &def.id, &tg.name]),
                    area: Area::Comms,
                    level: Level::Warn,
                    title: format!("{name} · {} reads bad {:.0}% of the time", tg.name, ratio * 100.0),
                    detail: "Reads succeed sometimes and fail sometimes: a timeout, a busy device or a marginal link, rather than a wrong address.".into(),
                    evidence: vec![format!("{} bad of {} updates", st.bad, st.updates)],
                    suggestion: "Raise the timeout, poll less often, or check the cable and the device's load.".into(),
                    service: Some(def.id.clone()),
                    tag: Some(tg.name.clone()),
                    fix: None,
                });
            }
            // A value that never moves.
            let numeric = tg.datatype.is_numeric() && tg.datatype != DataType::Bool;
            if numeric && !tg.writable && st.changes == 0 && st.bad == 0 && st.span() >= t.frozen_after {
                out.push(Insight {
                    id: id(&["frozen", &def.id, &tg.name]),
                    area: Area::Comms,
                    level: Level::Info,
                    title: format!("{name} · {} hasn't changed in {}", tg.name, secs(st.span().num_seconds() as f64)),
                    detail: "A live measurement that never moves is often a stuck sensor, a frozen value in the device, or the wrong address.".into(),
                    evidence: vec![format!("{} reads, all the same value", st.updates)],
                    suggestion: "Compare with the device's own display; check the address and the sensor.".into(),
                    service: Some(def.id.clone()),
                    tag: Some(tg.name.clone()),
                    fix: None,
                });
            }
            // Polling much faster than the value changes.
            let gap = median(&st.gaps).unwrap_or_else(|| st.span().num_seconds() as f64);
            if gap * 1000.0 >= t.overpoll_factor * poll && gap >= 60.0 {
                slow.push((tg.name.clone(), gap));
            }
        }
        if watched >= 1 && slow.len() == watched {
            let fastest = slow.iter().map(|x| x.1).fold(f64::INFINITY, f64::min);
            let suggest = round_ms((fastest * 1000.0 / 4.0).max(poll));
            let key = if def.settings.get("interval_ms").is_some() || def.kind == crate::model::Kind::Api { "interval_ms" } else { "poll_ms" };
            if suggest > poll as u64 {
                let mut ev: Vec<String> = slow
                    .iter()
                    .take(6)
                    .map(|(n, g)| format!("{n}: changes every {} (median)", secs(*g)))
                    .collect();
                ev.insert(0, format!("polled every {}", secs(poll / 1000.0)));
                out.push(Insight {
                    id: id(&["overpoll", &def.id]),
                    area: Area::Comms,
                    level: Level::Suggest,
                    title: format!("{name} polls far faster than its tags change"),
                    detail: format!(
                        "Every tag changes at least {:.0}× slower than the poll. Slower polling frees the device and the network for what matters.",
                        fastest * 1000.0 / poll
                    ),
                    evidence: ev,
                    suggestion: format!("Set {key} to {suggest} (still 4 reads per change of the fastest tag)."),
                    service: Some(def.id.clone()),
                    tag: None,
                    fix: Some(serde_json::json!({"setting": key, "value": suggest})),
                });
            }
        }
    }
}

/// Linear fit y = a + b·x; returns (slope b, R²).
fn fit(pts: &[(f64, f64)]) -> Option<(f64, f64)> {
    let n = pts.len() as f64;
    if pts.len() < 3 {
        return None;
    }
    let (mx, my) = (pts.iter().map(|p| p.0).sum::<f64>() / n, pts.iter().map(|p| p.1).sum::<f64>() / n);
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (x, y) in pts {
        sxy += (x - mx) * (y - my);
        sxx += (x - mx).powi(2);
        syy += (y - my).powi(2);
    }
    if sxx == 0.0 {
        return None;
    }
    let b = sxy / sxx;
    let r2 = if syy == 0.0 { 0.0 } else { (sxy * sxy) / (sxx * syy) };
    Some((b, r2))
}

/// Tags read from real devices (client services), with their stats.
fn device_tags<'a>(s: &'a Snapshot) -> impl Iterator<Item = (&'a ServiceDef, &'a crate::model::TagDef, &'a TagStats)> {
    s.cfg.services.iter().filter(|d| d.mode == Mode::Client && d.kind != crate::model::Kind::Monitor).flat_map(move |d| {
        d.tags.iter().filter_map(move |t| s.tags.get(&(d.id.clone(), t.name.clone())).map(|st| (d, t, st)))
    })
}

fn equipment(s: &Snapshot, t: &Tuning, out: &mut Vec<Insight>) {
    let now_hour = s.now.timestamp().div_euclid(3600);
    for (def, tg, st) in device_tags(s) {
        if st.span() < t.min_watch || st.n < t.min_updates {
            continue;
        }
        let name = format!("{} · {}", def.name_or_id(), tg.name);
        let mk = |kind: &str, level, title: String, detail: &str, evidence: Vec<String>, suggestion: &str| Insight {
            id: id(&[kind, &def.id, &tg.name]),
            area: Area::Equipment,
            level,
            title,
            detail: detail.into(),
            evidence,
            suggestion: suggestion.into(),
            service: Some(def.id.clone()),
            tag: Some(tg.name.clone()),
            fix: None,
        };
        // Pinned at an integer type's limit.
        if st.rails >= 10 && st.rails as f64 / st.n as f64 >= 0.01 {
            out.push(mk("rails", Level::Warn, format!("{name} reads at the end of its range"),
                "Values like 32767 or 65535 are what an analog input reports for a broken wire, a shorted or overranged sensor.",
                vec![format!("{} of {} readings at a range limit", st.rails, st.n)],
                "Check the sensor and its wiring, and the input card's diagnostics."));
        }
        // Rare spikes.
        if st.spikes >= 5 && (st.spikes as f64) < st.n as f64 * 0.01 {
            out.push(mk("spikes", Level::Info, format!("{name} has occasional spikes"),
                "Single readings far from the rest: electrical noise, a loose connection, or a sensor starting to fail.",
                vec![format!("{} readings more than 6 standard deviations out, of {}", st.spikes, st.n)],
                "Check shielding and grounding near drives, and the connector; consider filtering in the PLC."));
        }
        // Complete hours only.
        let done: Vec<&Bucket> = st.hours.iter().filter(|b| b.hour < now_hour && b.n >= 30).collect();
        // Noise rising: the last hour against the hours before.
        if done.len() >= 7 {
            let (last, before) = done.split_last().unwrap();
            let base = median(&before.iter().map(|b| b.std()).collect::<Vec<_>>()).unwrap_or(0.0);
            if base > 0.0 && last.std() >= 3.0 * base {
                out.push(mk("noise", Level::Suggest, format!("{name} is getting noisier"),
                    "Its readings scatter far more than they used to at the same time of measuring: a failing sensor, interference, or a mechanical problem (cavitation, vibration).",
                    vec![format!("last hour std {:.4}, usual {:.4}", last.std(), base)],
                    "Compare with a local gauge; check the sensor, its cable and what changed nearby."));
            }
        }
        // Slow drift over a day.
        let day: Vec<&&Bucket> = done.iter().filter(|b| b.hour >= now_hour - 24).collect();
        if day.len() >= 12 {
            let pts: Vec<(f64, f64)> = day.iter().map(|b| (b.hour as f64, b.mean)).collect();
            if let Some((slope, r2)) = fit(&pts) {
                let change = slope * (pts.last().unwrap().0 - pts[0].0);
                let noise = median(&day.iter().map(|b| b.std()).collect::<Vec<_>>()).unwrap_or(0.0);
                let range = st.max.zip(st.min).map(|(a, b)| a - b).unwrap_or(0.0);
                if r2 >= 0.8 && change.abs() >= 4.0 * noise.max(1e-9) && change.abs() >= 0.05 * range {
                    out.push(mk("drift", Level::Info, format!("{name} has drifted {}{:.3} in a day", if change > 0.0 { "+" } else { "" }, change),
                        "A steady, one-way trend hour after hour. Normal for a level filling or a filter loading; otherwise a sign of sensor drift or wear.",
                        vec![format!("{} hours, fit R² {r2:.2}", pts.len())],
                        "If the process shouldn't trend, compare with a reference instrument and recalibrate."));
                }
            }
        }
        // Heading for an alarm limit.
        if st.recent.len() >= 20 {
            let t0 = st.recent[0].0;
            let pts: Vec<(f64, f64)> = st.recent.iter().map(|(ms, v)| ((ms - t0) as f64 / 1000.0, *v)).collect();
            let span = pts.last().unwrap().0;
            if let (Some((slope, r2)), true) = (fit(&pts), span >= 300.0) {
                let last = pts.last().unwrap().1;
                for a in s.alarm_defs.iter().filter(|a| a.enabled && a.service == def.id && a.tag == tg.name) {
                    let (Some(limit), true) = (a.limit, r2 >= 0.7) else { continue };
                    let eta = match a.kind {
                        crate::alarms::AlarmKind::High if slope > 0.0 && last < limit => (limit - last) / slope,
                        crate::alarms::AlarmKind::Low if slope < 0.0 && last > limit => (limit - last) / slope,
                        _ => continue,
                    };
                    if eta <= 7200.0 {
                        let an = s.alarm_names.get(&a.id).cloned().unwrap_or_else(|| a.id.clone());
                        out.push(Insight {
                            id: id(&["forecast", &a.id]),
                            area: Area::Equipment,
                            level: Level::Warn,
                            title: format!("{name} will reach alarm “{an}” in about {}", secs(eta)),
                            detail: format!("At its current rate ({slope:+.4} per second) it crosses {limit} soon."),
                            evidence: vec![format!("now {last:.3}, trend over the last {}", secs(span))],
                            suggestion: "Act before the alarm: check why it's moving and whether to intervene.".into(),
                            service: Some(def.id.clone()),
                            tag: Some(tg.name.clone()),
                            fix: None,
                        });
                    }
                }
            }
        }
    }
}

/// On/off tags as process steps: stuck in a state, cycles getting slower.
fn process(s: &Snapshot, t: &Tuning, out: &mut Vec<Insight>) {
    for (def, tg, st) in device_tags(s) {
        if tg.datatype != DataType::Bool {
            continue;
        }
        let name = format!("{} · {}", def.name_or_id(), tg.name);
        if let (Some(state), Some(since)) = (st.state, st.state_since) {
            let runs = if state { &st.on_runs } else { &st.off_runs };
            let longest = runs.iter().cloned().fold(0.0, f64::max);
            let held = (s.now - since).num_milliseconds() as f64 / 1000.0;
            if runs.len() >= 10 && held >= (1.5 * longest).max(60.0) {
                out.push(Insight {
                    id: id(&["stuck", &def.id, &tg.name]),
                    area: Area::Process,
                    level: Level::Warn,
                    title: format!("{name} has been {} for {}", if state { "on" } else { "off" }, secs(held)),
                    detail: format!("It has switched {} times while watched and never stayed this way longer than {}. A step may be waiting on an interlock or a device.", runs.len(), secs(longest)),
                    evidence: vec![format!("{} since {}", if state { "on" } else { "off" }, since.format("%H:%M:%S"))],
                    suggestion: "Check what this step waits for: interlocks, a sensor that didn't come on, a stopped device.".into(),
                    service: Some(def.id.clone()),
                    tag: Some(tg.name.clone()),
                    fix: None,
                });
            }
        }
        if st.periods.len() >= 30 {
            let (old, new) = st.periods.split_at(st.periods.len() - 10);
            let (a, b) = (median(old).unwrap_or(0.0), median(new).unwrap_or(0.0));
            if a > 0.0 && b >= 1.25 * a && st.span() >= t.min_watch {
                out.push(Insight {
                    id: id(&["slower", &def.id, &tg.name]),
                    area: Area::Process,
                    level: Level::Suggest,
                    title: format!("{name} cycles {:.0}% slower", (b / a - 1.0) * 100.0),
                    detail: "The time between switch-ons has grown: a cycle or step is taking longer than it used to.".into(),
                    evidence: vec![format!("last 10 cycles {} (median), before {}", secs(b), secs(a))],
                    suggestion: "Look for what got slower: a valve, a heater, a conveyor, or a changed setpoint.".into(),
                    service: Some(def.id.clone()),
                    tag: Some(tg.name.clone()),
                    fix: None,
                });
            }
        }
    }
}

/// The Cowpoke client services that read the device at `addr` (`ip:port`).
fn services_at<'a>(s: &'a Snapshot, addr: &str) -> Vec<&'a ServiceDef> {
    let Some((ip, port)) = addr.rsplit_once(':') else { return vec![] };
    s.cfg
        .services
        .iter()
        .filter(|d| d.mode == Mode::Client)
        .filter(|d| {
            let host = d.settings.get("host").and_then(|h| h.as_str()).unwrap_or("");
            let p = d.settings.get("port").and_then(|p| p.as_u64()).or_else(|| crate::services::listen_port(&ServiceDef { mode: Mode::Server, ..(*d).clone() }).map(u64::from));
            host == ip && p.map(|p| p.to_string()).as_deref() == Some(port)
        })
        .collect()
}

/// Did the tags a change could affect behave differently afterwards?
fn effects(s: &Snapshot, out: &mut Vec<Insight>) {
    let now_hour = s.now.timestamp().div_euclid(3600);
    for c in s.changes {
        let hour = c.ts.timestamp().div_euclid(3600);
        // Two complete hours on each side.
        if now_hour - hour < 3 || s.now - c.ts > TimeDelta::days(3) {
            continue;
        }
        let mut targets: Vec<&ServiceDef> = vec![];
        if let Some(sv) = c.service.as_ref().and_then(|id| s.cfg.services.iter().find(|d| &d.id == id)) {
            targets.push(sv);
        }
        if let Some(dev) = &c.device {
            targets.extend(services_at(s, dev));
        }
        let mut shifts = vec![];
        for def in targets {
            for tg in &def.tags {
                let Some(st) = s.tags.get(&(def.id.clone(), tg.name.clone())) else { continue };
                let before: Vec<&Bucket> = st.hours.iter().filter(|b| b.hour < hour && b.hour >= hour - 6 && b.n >= 10).collect();
                let after: Vec<&Bucket> = st.hours.iter().filter(|b| b.hour > hour && b.hour < now_hour && b.n >= 10).collect();
                if before.len() < 2 || after.len() < 2 {
                    continue;
                }
                let mean = |v: &[&Bucket]| v.iter().map(|b| b.mean).sum::<f64>() / v.len() as f64;
                let (a, b) = (mean(&before), mean(&after));
                let noise = median(&before.iter().chain(after.iter()).map(|b| b.std()).collect::<Vec<_>>()).unwrap_or(0.0);
                if (b - a).abs() >= 3.0 * noise.max(1e-9) && (b - a).abs() >= 0.02 * a.abs().max(1e-9) {
                    shifts.push(format!("{} · {}: {a:.3} → {b:.3} (average)", def.name_or_id(), tg.name));
                }
            }
        }
        if shifts.is_empty() {
            continue;
        }
        out.push(Insight {
            id: id(&["effect", &c.ts.timestamp().to_string(), &c.what]),
            area: Area::Process,
            level: Level::Suggest,
            title: format!("After “{}” ({} by {}), {} tag{} changed", c.what, c.ts.format("%b %d %H:%M"), c.by, shifts.len(), if shifts.len() > 1 { "s" } else { "" }),
            detail: "These tags averaged differently in the hours after the change than in the hours before. It may be the intended effect, or a side effect worth knowing about.".into(),
            evidence: shifts.into_iter().take(8).collect(),
            suggestion: "Confirm the change did what was meant; if a shift wasn't intended, start with this change.".into(),
            service: c.service.clone(),
            tag: None,
            fix: None,
        });
    }
}

/// Writes and program or control actions at hours a client has never been active.
fn off_hours(s: &Snapshot, out: &mut Vec<Insight>) {
    use chrono::Timelike;
    use crate::monitor::Action;
    let since = s.now - TimeDelta::hours(24);
    for (mon, talkers, _) in s.monitors {
        for tk in talkers {
            let total: u64 = tk.action_hours.iter().sum();
            if total < 100 {
                continue;
            }
            let odd: Vec<&crate::monitor::Event> = s
                .monitor_events
                .iter()
                .filter(|e| e.ts >= since && e.client == tk.client && e.server == tk.server)
                .filter(|e| matches!(e.action, Action::Write | Action::Program | Action::Control))
                .filter(|e| (tk.action_hours[e.ts.hour() as usize] as f64) < 0.01 * total as f64)
                .collect();
            let Some(last) = odd.last() else { continue };
            let program = odd.iter().any(|e| e.action == Action::Program);
            out.push(Insight {
                id: id(&["offhours", mon, &format!("{}>{}", tk.client, tk.server)]),
                area: Area::Network,
                level: if program { Level::Warn } else { Level::Suggest },
                title: format!("{} → {}: {} action{} at an unusual hour", tk.client, tk.server, odd.len(), if odd.len() > 1 { "s" } else { "" }),
                detail: format!("This client normally writes to the device at other times of day ({total} actions learned). Off-hours changes are often maintenance, but they're also how mistakes and intrusions show up."),
                evidence: odd.iter().rev().take(5).map(|e| format!("{} UTC: {}", e.ts.format("%H:%M"), e.what)).collect(),
                suggestion: format!("Check who was connected from {} at {} UTC and why.", tk.client, last.ts.format("%H:%M")),
                service: Some(mon.clone()),
                tag: None,
                fix: None,
            });
        }
    }
}

fn pairs(s: &Snapshot, out: &mut Vec<Insight>) {
    for (p, st) in s.pairs {
        let current = st.mismatch_since.filter(|m| (s.now - *m).num_seconds() >= p.delay_s as i64);
        let recent = st.last_incident.is_some_and(|t| s.now - t <= TimeDelta::hours(24));
        if current.is_none() && !recent {
            continue;
        }
        let what = match p.kind {
            PairKind::Runs => "is commanded on but its feedback stays off",
            PairKind::Follows => "doesn't follow its command",
        };
        out.push(Insight {
            id: id(&["pair", &p.name]),
            area: Area::Equipment,
            level: Level::Warn,
            title: match current {
                Some(m) => format!("{} {what} (for {})", p.name, secs((s.now - m).num_seconds() as f64)),
                None => format!("{} {what} at times", p.name),
            },
            detail: format!("{} → {}: a stuck valve, a tripped motor, a failed feedback sensor or a wiring fault.", p.command, p.feedback),
            evidence: vec![format!("{} time(s), last {}", st.incidents, st.last_incident.map(|t| t.format("%b %d %H:%M").to_string()).unwrap_or("-".into())), st.last_detail.clone()],
            suggestion: "Check the actuator and its feedback device on site; look at the drive or starter for a trip.".into(),
            service: Pair::split(&p.command).map(|x| x.0),
            tag: Pair::split(&p.command).map(|x| x.1),
            fix: None,
        });
    }
}

/// Where a client tag reads from: the device and the address in it, as one comparable string.
fn source_key(def: &ServiceDef, t: &crate::model::TagDef) -> Option<String> {
    use crate::model::Kind::*;
    let st = &def.settings;
    let host = st.get("host").and_then(|h| h.as_str()).unwrap_or_default().to_ascii_lowercase();
    let port = st.get("port").and_then(|p| p.as_u64()).map(|p| p.to_string()).unwrap_or_default();
    let dev = format!("{host}:{port}");
    Some(match def.kind {
        Modbus => format!("modbus {dev} unit {} {:?} {}", t.unit(), t.table(), t.register?),
        Eip => format!("eip {dev} {}", t.cip_tag.as_deref()?.to_ascii_lowercase()),
        S7 => format!("s7 {dev} {}", t.s7_address.as_deref()?.to_ascii_uppercase()),
        Opcua => format!("opcua {} {}", st.get("url").and_then(|u| u.as_str()).unwrap_or_default(), t.node_id.as_deref()?),
        Iolink => format!("iolink {dev} port {} {:?} {}", t.port?, t.pd(), t.bit_offset.unwrap_or(0)),
        Mqtt => format!("mqtt {dev} {} {}", t.topic.as_deref().or(t.metric.as_deref())?, t.pointer.as_deref().unwrap_or("")),
        Api => format!("api {} {}", st.get("url").and_then(|u| u.as_str()).unwrap_or_default(), t.pointer.as_deref().unwrap_or("")),
        Monitor => return None,
    })
}

/// Devices whose clocks are off: their own timestamps (history, events, alarms in the device) are wrong.
fn clocks(s: &Snapshot, out: &mut Vec<Insight>) {
    for (svc, ms, n) in s.clocks {
        if *n < 5 || ms.abs() < 5000.0 {
            continue;
        }
        let Some(def) = s.cfg.services.iter().find(|d| &d.id == svc) else { continue };
        out.push(Insight {
            id: id(&["clock", svc]),
            area: Area::Data,
            level: if ms.abs() >= 60_000.0 { Level::Warn } else { Level::Suggest },
            title: format!("{}'s device clock is {} {}", def.name_or_id(), secs(ms.abs() / 1000.0), if *ms > 0.0 { "ahead" } else { "behind" }),
            detail: "Timestamps the device sets itself (its event log, alarms, Sparkplug and OPC UA source times) don't line up with the rest of the plant, which makes sequences of events hard to reconstruct.".into(),
            evidence: vec![format!("median offset {:.1} s over {n} samples, compared with this Cowpoke's clock", ms / 1000.0)],
            suggestion: "Point the device at the plant's NTP server, or set its clock; check this Cowpoke's clock too if several devices show the same offset.".into(),
            service: Some(svc.clone()),
            tag: None,
            fix: None,
        });
    }
}

fn data_quality(s: &Snapshot, t: &Tuning, out: &mut Vec<Insight>) {
    let now_hour = s.now.timestamp().div_euclid(3600);
    // The same device address read under more than one name.
    let mut by_source: BTreeMap<String, Vec<(&ServiceDef, &crate::model::TagDef)>> = BTreeMap::new();
    for def in s.cfg.services.iter().filter(|d| d.mode == Mode::Client) {
        for tg in &def.tags {
            if let Some(k) = source_key(def, tg) {
                by_source.entry(k).or_default().push((def, tg));
            }
        }
    }
    for (src, readers) in by_source.iter().filter(|(_, r)| r.len() > 1) {
        let names: Vec<String> = readers.iter().map(|(d, t)| format!("{}/{}", d.id, t.name)).collect();
        // Do they agree? Compare the last complete hour's averages.
        let last_mean = |d: &ServiceDef, tg: &crate::model::TagDef| {
            s.tags.get(&(d.id.clone(), tg.name.clone())).and_then(|st| st.hours.iter().rev().find(|b| b.hour < now_hour && b.n >= 10).map(|b| b.mean))
        };
        let means: Vec<Option<f64>> = readers.iter().map(|(d, tg)| last_mean(d, tg)).collect();
        let known: Vec<f64> = means.iter().flatten().copied().collect();
        let disagree = known.len() >= 2 && {
            let (lo, hi) = known.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), x| (a.min(*x), b.max(*x)));
            (hi - lo).abs() > 0.01 * hi.abs().max(lo.abs()).max(1e-9)
        };
        if disagree {
            out.push(Insight {
                id: id(&["disagree", src]),
                area: Area::Data,
                level: Level::Warn,
                title: format!("{} read the same address but disagree", names.join(" and ")),
                detail: "They read the same place in the same device, so they should show the same value. Different scale, offset, word order or datatype make one of them wrong.".into(),
                evidence: readers.iter().zip(&means).map(|((d, tg), m)| format!("{}/{}: {} (scale {}, {:?})", d.id, tg.name, m.map(|x| format!("{x:.3}")).unwrap_or("-".into()), tg.scale.unwrap_or(1.0), tg.datatype)).collect(),
                suggestion: "Check which one matches the device's own display, and correct the other's scale, datatype or word order.".into(),
                service: Some(readers[0].0.id.clone()),
                tag: Some(readers[0].1.name.clone()),
                fix: None,
            });
        } else {
            out.push(Insight {
                id: id(&["duplicate", src]),
                area: Area::Data,
                level: Level::Info,
                title: format!("{} read the same address", names.join(" and ")),
                detail: "The same value under more than one name: twice the polling, and two names for one thing in history and alarms.".into(),
                evidence: vec![src.clone()],
                suggestion: "Keep one tag and point everything that uses the other at it.".into(),
                service: Some(readers[0].0.id.clone()),
                tag: Some(readers[0].1.name.clone()),
                fix: None,
            });
        }
    }
    for def in s.cfg.services.iter().filter(|d| d.mode == Mode::Client) {
        // A tag that stopped updating while the rest of its service carries on.
        if let Some(poll) = poll_ms(def) {
            let fresh_limit = TimeDelta::milliseconds((poll * 3.0).max(30_000.0) as i64);
            let stale_limit = TimeDelta::milliseconds((poll * 20.0).max(300_000.0) as i64);
            let stats: Vec<(&crate::model::TagDef, &TagStats)> = def.tags.iter().filter_map(|tg| s.tags.get(&(def.id.clone(), tg.name.clone())).map(|st| (tg, st))).collect();
            let siblings_fresh = stats.iter().filter(|(_, st)| st.last_update.is_some_and(|u| s.now - u <= fresh_limit)).count();
            for (tg, st) in &stats {
                let Some(last) = st.last_update else { continue };
                if siblings_fresh >= 1 && s.now - last >= stale_limit && st.updates >= t.min_updates {
                    out.push(Insight {
                        id: id(&["gap", &def.id, &tg.name]),
                        area: Area::Data,
                        level: Level::Warn,
                        title: format!("{} · {} stopped updating {} ago", def.name_or_id(), tg.name, secs((s.now - last).num_seconds() as f64)),
                        detail: "Other tags of the same service still update, so the connection is fine: this one tag isn't being read, or its reads fail silently.".into(),
                        evidence: vec![format!("last update {}", last.format("%b %d %H:%M:%S"))],
                        suggestion: "Check the tag's address and the service's status detail; restart the service if it persists.".into(),
                        service: Some(def.id.clone()),
                        tag: Some(tg.name.clone()),
                        fix: None,
                    });
                }
            }
        }
        // Percentages outside 0–100: a scale or unit mistake.
        for tg in &def.tags {
            let Some(st) = s.tags.get(&(def.id.clone(), tg.name.clone())) else { continue };
            let pct = tg.units.as_deref().is_some_and(|u| u.trim() == "%");
            if pct && st.n >= t.min_updates && (st.max.unwrap_or(0.0) > 105.0 || st.min.unwrap_or(0.0) < -5.0) {
                out.push(Insight {
                    id: id(&["units", &def.id, &tg.name]),
                    area: Area::Data,
                    level: Level::Suggest,
                    title: format!("{} · {} is in % but reads {:.1} to {:.1}", def.name_or_id(), tg.name, st.min.unwrap_or(0.0), st.max.unwrap_or(0.0)),
                    detail: "A percentage outside 0–100 usually means a missing scale (raw counts, or tenths of a percent) or the wrong units.".into(),
                    evidence: vec![format!("{} readings", st.n)],
                    suggestion: "Set the scale so the range maps to 0–100 (raw 0–1000 → scale 0.1, 0–27648 → 100/27648), or fix the units.".into(),
                    service: Some(def.id.clone()),
                    tag: Some(tg.name.clone()),
                    fix: None,
                });
            }
        }
    }
}

fn historian(s: &Snapshot, t: &Tuning, out: &mut Vec<Insight>) {
    for def in &s.cfg.services {
        for tg in &def.tags {
            let Some(h) = &tg.history else { continue };
            let Some(st) = s.tags.get(&(def.id.clone(), tg.name.clone())) else { continue };
            if st.span() < t.min_watch || st.steps.len() < 20 {
                continue;
            }
            let hours = (st.span().num_seconds() as f64 / 3600.0).max(1e-6);
            let noise = median(&st.steps).unwrap_or(0.0);
            let range = st.max.zip(st.min).map(|(a, b)| a - b).unwrap_or(0.0);
            match h.mode {
                HistoryMode::Deadband => {
                    let over = st.steps.iter().filter(|x| **x > h.deadband).count() as f64 / st.steps.len() as f64;
                    let per_hour = st.changes as f64 / hours * over;
                    let suggest = round_db(noise * 2.0);
                    if per_hour > 360.0 && h.deadband < noise && suggest > h.deadband && suggest < range / 20.0 {
                        out.push(Insight {
                            id: id(&["deadband", &def.id, &tg.name]),
                            area: Area::Historian,
                            level: Level::Suggest,
                            title: format!("{} · {} records mostly noise", def.name_or_id(), tg.name),
                            detail: format!("About {per_hour:.0} points an hour pass a deadband of {}, smaller than the typical step of {noise:.3}.", h.deadband),
                            evidence: vec![
                                format!("range {:.3} to {:.3}", st.min.unwrap_or(0.0), st.max.unwrap_or(0.0)),
                                format!("median step {noise:.4}, std {:.4}", st.std()),
                            ],
                            suggestion: format!("Set the history deadband to {suggest} (about twice the typical step), well under the tag's range."),
                            service: Some(def.id.clone()),
                            tag: Some(tg.name.clone()),
                            fix: Some(serde_json::json!({"history": {"mode": "deadband", "deadband": suggest, "interval_ms": h.interval_ms, "retention_days": h.retention_days}})),
                        });
                    }
                }
                HistoryMode::Interval => {
                    let changes_per_hour = st.changes as f64 / hours;
                    let samples_per_hour = 3_600_000.0 / h.interval_ms.max(1) as f64;
                    if changes_per_hour * 10.0 < samples_per_hour {
                        out.push(Insight {
                            id: id(&["interval", &def.id, &tg.name]),
                            area: Area::Historian,
                            level: Level::Suggest,
                            title: format!("{} · {} is recorded far more often than it changes", def.name_or_id(), tg.name),
                            detail: format!("It's recorded {samples_per_hour:.0} times an hour but changes about {changes_per_hour:.1} times."),
                            evidence: vec![format!("interval {}", secs(h.interval_ms as f64 / 1000.0))],
                            suggestion: format!("Record on change instead: mode deadband with a deadband near {}.", round_db(noise)),
                            service: Some(def.id.clone()),
                            tag: Some(tg.name.clone()),
                            fix: Some(serde_json::json!({"history": {"mode": "deadband", "deadband": round_db(noise), "interval_ms": h.interval_ms, "retention_days": h.retention_days}})),
                        });
                    }
                }
            }
        }
    }
}

fn alarms(s: &Snapshot, t: &Tuning, out: &mut Vec<Insight>) {
    let since = s.now - TimeDelta::hours(24);
    let mut raises: BTreeMap<&str, Vec<DateTime<Utc>>> = BTreeMap::new();
    let mut all: Vec<(DateTime<Utc>, &str)> = vec![];
    for e in s.alarm_log.iter().filter(|e| e.time >= since && e.event == EventKind::Raise) {
        raises.entry(&e.alarm).or_default().push(e.time);
        all.push((e.time, &e.alarm));
    }
    let name = |id: &str| s.alarm_names.get(id).cloned().unwrap_or_else(|| id.to_string());
    // Chattering: many raises of one alarm in a short window.
    for (alarm, times) in &mut raises {
        times.sort();
        let worst = max_in_window(times, t.chatter_window);
        if worst >= t.chatter_raises {
            out.push(Insight {
                id: id(&["chatter", alarm]),
                area: Area::Alarms,
                level: Level::Warn,
                title: format!("Alarm “{}” chatters", name(alarm)),
                detail: format!("It raised {worst} times within {} at its worst, {} times in the last day.", secs(t.chatter_window.num_seconds() as f64), times.len()),
                evidence: vec![format!("last raised {}", times.last().map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_default())],
                suggestion: "Add a deadband (so it clears only well past the limit) or an on-delay (so a brief crossing doesn't raise it).".into(),
                service: None,
                tag: None,
                fix: None,
            });
        }
    }
    // Floods: many raises across alarms in a short window.
    all.sort();
    let times: Vec<DateTime<Utc>> = all.iter().map(|x| x.0).collect();
    let (worst, start) = max_window_start(&times, t.flood_window);
    if worst >= t.flood_raises {
        let end = start + t.flood_window;
        let mut by: HashMap<&str, usize> = HashMap::new();
        for (ts, a) in &all {
            if *ts >= start && *ts < end {
                *by.entry(a).or_default() += 1;
            }
        }
        let mut top: Vec<(&str, usize)> = by.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1));
        out.push(Insight {
            id: id(&["flood", &start.format("%Y%m%d%H%M").to_string()]),
            area: Area::Alarms,
            level: Level::Warn,
            title: format!("Alarm flood: {worst} alarms in {}", secs(t.flood_window.num_seconds() as f64)),
            detail: format!("Starting {}. Floods bury the alarm that matters; often one cause raises many.", start.format("%b %d %H:%M")),
            evidence: top.iter().take(5).map(|(a, n)| format!("{}: {n}×", name(a))).collect(),
            suggestion: "Find the first alarm in the flood and the cause behind it; consider suppressing alarms that follow from it.".into(),
            service: None,
            tag: None,
            fix: None,
        });
    }
    // Standing and unacknowledged alarms.
    for (alarm, st) in s.alarm_states {
        let Some(since) = st.since else { continue };
        let age = s.now - since;
        if st.active && age >= t.standing_after {
            out.push(Insight {
                id: id(&["standing", alarm]),
                area: Area::Alarms,
                level: Level::Suggest,
                title: format!("Alarm “{}” has been active for {}", name(alarm), secs(age.num_seconds() as f64)),
                detail: "An alarm that is always on teaches operators to ignore it.".into(),
                evidence: vec![format!("value when raised: {}", st.value)],
                suggestion: "Fix the cause, change the limit to what is normal now, or turn the alarm off if it no longer means anything.".into(),
                service: None,
                tag: None,
                fix: None,
            });
        } else if !st.active && !st.acked && st.cleared.is_some_and(|c| s.now - c >= t.standing_after) {
            out.push(Insight {
                id: id(&["unacked", alarm]),
                area: Area::Alarms,
                level: Level::Info,
                title: format!("Alarm “{}” cleared but was never acknowledged", name(alarm)),
                detail: "Nobody acknowledged it: it may not reach the right people, or may not matter.".into(),
                evidence: vec![format!("raised {}", since.format("%b %d %H:%M"))],
                suggestion: "Check who gets it (hooks), or lower its severity.".into(),
                service: None,
                tag: None,
                fix: None,
            });
        }
    }
}

/// The most events inside any window of length `w` (times sorted).
fn max_in_window(times: &[DateTime<Utc>], w: TimeDelta) -> usize {
    max_window_start(times, w).0
}

fn max_window_start(times: &[DateTime<Utc>], w: TimeDelta) -> (usize, DateTime<Utc>) {
    let (mut best, mut start, mut j) = (0, Utc::now(), 0);
    for i in 0..times.len() {
        while times[i] - times[j] >= w {
            j += 1;
        }
        if i + 1 - j > best {
            best = i + 1 - j;
            start = times[j];
        }
    }
    (best, start)
}

// ------------------------------------------------------------------ the service

#[derive(Serialize, Deserialize, Default)]
struct Saved {
    tags: Vec<((String, String), TagStats)>,
    services: HashMap<String, ServiceStats>,
    dismissed: HashSet<String>,
    #[serde(default)]
    pairs: HashMap<String, PairState>,
    #[serde(default)]
    changes: Vec<Change>,
    #[serde(default)]
    first_seen: HashMap<String, DateTime<Utc>>,
    #[serde(default)]
    resolved: Vec<Resolved>,
    #[serde(default)]
    last_digest: Option<DateTime<Utc>>,
}

#[derive(Serialize, Clone, Default)]
pub struct Report {
    pub generated: Option<DateTime<Utc>>,
    pub watching_since: Option<DateTime<Utc>>,
    pub tags_watched: usize,
    pub insights: Vec<Insight>,
    pub dismissed: usize,
}

/// An insight that no longer applies: fixed, or the condition passed.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Resolved {
    pub id: String,
    pub area: Area,
    pub title: String,
    pub service: Option<String>,
    pub first_seen: DateTime<Utc>,
    pub resolved: DateTime<Utc>,
}

pub struct Insights {
    path: PathBuf,
    config_path: PathBuf,
    config: Mutex<InsightsConfig>,
    /// Pair name → state.
    pair_states: Mutex<HashMap<String, PairState>>,
    /// Latest value per (service, tag), for pair checks.
    latest: Mutex<HashMap<(String, String), Value>>,
    changes: Mutex<Vec<Change>>,
    /// When each open insight first appeared.
    first_seen: Mutex<HashMap<String, DateTime<Utc>>>,
    resolved: Mutex<Vec<Resolved>>,
    last_digest: Mutex<Option<DateTime<Utc>>>,
    tags: Mutex<HashMap<(String, String), TagStats>>,
    services: Mutex<HashMap<String, ServiceStats>>,
    dismissed: Mutex<HashSet<String>>,
    report: Mutex<Report>,
    pub tuning: Tuning,
}

impl Insights {
    pub fn open(path: PathBuf) -> Arc<Self> {
        let saved: Saved = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let config_path = path.with_file_name("insights.yaml");
        let config = match std::fs::read_to_string(&config_path) {
            Ok(text) => serde_yaml::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!("{}: {e}; ignoring it", config_path.display());
                InsightsConfig::default()
            }),
            Err(_) => InsightsConfig::default(),
        };
        Arc::new(Self {
            path,
            config_path,
            config: Mutex::new(config),
            pair_states: Mutex::new(saved.pairs),
            latest: Mutex::new(HashMap::new()),
            changes: Mutex::new(saved.changes),
            first_seen: Mutex::new(saved.first_seen),
            resolved: Mutex::new(saved.resolved),
            last_digest: Mutex::new(saved.last_digest),
            tags: Mutex::new(saved.tags.into_iter().collect()),
            services: Mutex::new(saved.services),
            dismissed: Mutex::new(saved.dismissed),
            report: Mutex::new(Report::default()),
            tuning: if std::env::var("COWPOKE_INSIGHTS_QUICK").is_ok_and(|v| v == "1") { Tuning::quick() } else { Tuning::default() },
        })
    }

    pub fn observe(&self, e: &TagEvent) {
        self.tags
            .lock()
            .entry((e.service.clone(), e.tag.clone()))
            .or_default()
            .observe(e.state.ts, &e.state.value, e.state.quality);
        if e.state.quality != Quality::Good {
            return;
        }
        let key = format!("{}/{}", e.service, e.tag);
        let cfg = self.config.lock();
        let involved: Vec<&Pair> = cfg.pairs.iter().filter(|p| p.command == key || p.feedback == key).collect();
        if involved.is_empty() {
            return;
        }
        let mut latest = self.latest.lock();
        latest.insert((e.service.clone(), e.tag.clone()), e.state.value.clone());
        let mut states = self.pair_states.lock();
        for p in involved {
            let (Some(c), Some(f)) = (Pair::split(&p.command), Pair::split(&p.feedback)) else { continue };
            if let (Some(cv), Some(fv)) = (latest.get(&c), latest.get(&f)) {
                check_pair(p, states.entry(p.name.clone()).or_default(), e.state.ts, cv, fv);
            }
        }
    }

    /// Note a change: a value written through Cowpoke, or a program or control action on the wire.
    pub fn note_change(&self, c: Change) {
        let mut v = self.changes.lock();
        // A burst of writes to one tag is one change.
        if let Some(last) = v.last_mut() {
            if last.service == c.service && last.device == c.device && last.by == c.by && c.ts - last.ts < TimeDelta::minutes(10) && last.what.split(' ').next() == c.what.split(' ').next() {
                *last = c;
                return;
            }
        }
        v.push(c);
        let cutoff = Utc::now() - TimeDelta::days(7);
        v.retain(|x| x.ts >= cutoff);
        if v.len() > 500 {
            let n = v.len() - 500;
            v.drain(..n);
        }
    }

    /// Re-check pairs whose values haven't changed (a mismatch outlasting the delay).
    pub fn tick_pairs(&self, now: DateTime<Utc>) {
        let cfg = self.config.lock();
        let latest = self.latest.lock();
        let mut states = self.pair_states.lock();
        for p in &cfg.pairs {
            let (Some(c), Some(f)) = (Pair::split(&p.command), Pair::split(&p.feedback)) else { continue };
            if let (Some(cv), Some(fv)) = (latest.get(&c), latest.get(&f)) {
                check_pair(p, states.entry(p.name.clone()).or_default(), now, cv, fv);
            }
        }
    }

    pub fn config_yaml(&self) -> String {
        serde_yaml::to_string(&*self.config.lock()).unwrap_or_default()
    }

    /// Replace `insights.yaml`.
    pub fn set_config(&self, text: &str) -> Result<(), String> {
        let c: InsightsConfig = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
        for p in &c.pairs {
            p.validate()?;
        }
        if c.digest.as_ref().is_some_and(|d| d.hour > 23) {
            return Err("digest.hour is 0 to 23 (UTC)".into());
        }
        let mut names = HashSet::new();
        for p in &c.pairs {
            if !names.insert(&p.name) {
                return Err(format!("pair `{}` appears twice", p.name));
            }
        }
        std::fs::write(&self.config_path, serde_yaml::to_string(&c).unwrap_or_default()).map_err(|e| e.to_string())?;
        self.pair_states.lock().retain(|k, _| c.pairs.iter().any(|p| &p.name == k));
        *self.config.lock() = c;
        Ok(())
    }

    pub fn sample_status(&self, id: &str, state: State, detail: &str) {
        let mut m = self.services.lock();
        let st = m.entry(id.to_string()).or_default();
        st.samples += 1;
        let s = match state {
            State::Running => "running",
            State::Degraded => "degraded",
            State::Error => "error",
            State::Starting => "starting",
            State::Stopped => "stopped",
        };
        match state {
            State::Degraded => st.degraded += 1,
            State::Error => st.errors += 1,
            _ => {}
        }
        let up = |x: &str| x == "running";
        if let Some(prev) = &st.last_state {
            if up(prev) != up(s) && !matches!(state, State::Stopped | State::Starting) && prev != "stopped" && prev != "starting" {
                st.transitions += 1;
            }
        }
        st.last_state = Some(s.to_string());
        if !detail.is_empty() {
            st.last_detail = detail.to_string();
        }
    }

    /// Forget a service's history (its definition changed, so old stats would mislead).
    pub fn forget_service(&self, id: &str) {
        self.tags.lock().retain(|k, _| k.0 != id);
        self.services.lock().remove(id);
    }

    pub fn dismiss(&self, id: &str, on: bool) {
        let mut d = self.dismissed.lock();
        if on { d.insert(id.to_string()) } else { d.remove(id) };
        drop(d);
        let dismissed = self.dismissed.lock().clone();
        let mut r = self.report.lock();
        r.dismissed = r.insights.iter().filter(|i| dismissed.contains(&i.id)).count();
        self.save();
    }

    pub fn recompute(&self, cfg: &Config, alarm_log: &[AlarmEvent], alarm_states: &HashMap<String, AlarmState>, alarm_names: &HashMap<String, String>, alarm_defs: &[crate::alarms::AlarmDef]) {
        self.tick_pairs(Utc::now());
        let tags = self.tags.lock().clone();
        let services = self.services.lock().clone();
        let monitors = crate::monitor::health();
        let states = self.pair_states.lock().clone();
        let pairs: Vec<(Pair, PairState)> = self
            .config
            .lock()
            .pairs
            .iter()
            .map(|p| (p.clone(), states.get(&p.name).cloned().unwrap_or_default()))
            .collect();
        let events = crate::monitor::recent_events();
        for e in events.iter().filter(|e| matches!(e.action, crate::monitor::Action::Program | crate::monitor::Action::Control)) {
            let c = Change { ts: e.ts, by: e.client.clone(), service: None, device: Some(e.server.clone()), what: e.what.clone() };
            if !self.changes.lock().contains(&c) {
                self.note_change(c);
            }
        }
        let changes = self.changes.lock().clone();
        let clocks = clock_offsets();
        let snap = Snapshot {
            now: Utc::now(), cfg, tags: &tags, services: &services, alarm_log, alarm_states, alarm_names,
            monitors: &monitors, alarm_defs, pairs: &pairs, changes: &changes, monitor_events: &events, clocks: &clocks,
        };
        let all = analyze(&snap, &self.tuning);
        self.track(&all, snap.now);
        let dismissed = self.dismissed.lock().clone();
        let mut r = self.report.lock();
        r.generated = Some(snap.now);
        r.watching_since = tags.values().filter_map(|t| t.first).min();
        r.tags_watched = tags.len();
        r.dismissed = all.iter().filter(|i| dismissed.contains(&i.id)).count();
        r.insights = all;
    }

    /// Note when insights first appeared, and which went away since the last run.
    fn track(&self, all: &[Insight], now: DateTime<Utc>) {
        let mut first = self.first_seen.lock();
        let mut resolved = self.resolved.lock();
        let open: HashSet<&str> = all.iter().map(|i| i.id.as_str()).collect();
        let prev = self.report.lock().insights.clone();
        for i in prev.iter().filter(|i| !open.contains(i.id.as_str())) {
            resolved.push(Resolved {
                id: i.id.clone(),
                area: i.area,
                title: i.title.clone(),
                service: i.service.clone(),
                first_seen: first.get(&i.id).copied().unwrap_or(now),
                resolved: now,
            });
        }
        let week = now - TimeDelta::days(7);
        resolved.retain(|r| r.resolved >= week && !open.contains(r.id.as_str()));
        first.retain(|k, _| open.contains(k.as_str()));
        for i in all {
            first.entry(i.id.clone()).or_insert(now);
        }
    }

    /// The text of a digest: insights first seen since `since` (or all open ones), and counts.
    pub fn digest_text(&self, since: Option<DateTime<Utc>>) -> (usize, String) {
        let r = self.report.lock().clone();
        let first = self.first_seen.lock().clone();
        let dismissed = self.dismissed.lock().clone();
        let open: Vec<&Insight> = r.insights.iter().filter(|i| !dismissed.contains(&i.id)).collect();
        let new: Vec<&&Insight> = open.iter().filter(|i| since.is_none_or(|s| first.get(&i.id).is_none_or(|f| *f > s))).collect();
        let checks = open.iter().filter(|i| i.level == Level::Warn).count();
        let gone = self.resolved.lock().iter().filter(|x| since.is_none_or(|s| x.resolved > s)).count();
        let mut text = format!("Cowpoke insights: {} open ({checks} to check), {} new, {gone} resolved.\n", open.len(), new.len());
        for i in new.iter().take(20) {
            let lvl = match i.level { Level::Warn => "CHECK", Level::Suggest => "SUGGESTION", Level::Info => "NOTE" };
            text.push_str(&format!("\n[{lvl}] {}\n  {}\n", i.title, i.suggestion));
        }
        if new.len() > 20 {
            text.push_str(&format!("\n…and {} more on the Insights page.\n", new.len() - 20));
        }
        (new.len(), text)
    }

    /// Whether a digest is due now, per the config.
    pub fn digest_due(&self, now: DateTime<Utc>) -> Option<Digest> {
        use chrono::{Datelike, Timelike};
        let d = self.config.lock().digest.clone()?;
        if now.hour() != d.hour || (d.weekly && now.weekday() != chrono::Weekday::Mon) {
            return None;
        }
        let last = *self.last_digest.lock();
        if last.is_some_and(|l| now - l < TimeDelta::hours(20)) {
            return None;
        }
        Some(d)
    }

    pub fn mark_digest_sent(&self, at: DateTime<Utc>) {
        *self.last_digest.lock() = Some(at);
        self.save();
    }

    pub fn last_digest(&self) -> Option<DateTime<Utc>> {
        *self.last_digest.lock()
    }

    /// The report; dismissed insights are flagged rather than dropped.
    pub fn report(&self) -> Value {
        let r = self.report.lock().clone();
        let d = self.dismissed.lock().clone();
        let first = self.first_seen.lock().clone();
        let mut v = serde_json::to_value(&r).unwrap_or_default();
        v["resolved"] = serde_json::to_value(&*self.resolved.lock()).unwrap_or_default();
        if let Some(list) = v.get_mut("insights").and_then(|x| x.as_array_mut()) {
            for i in list {
                let dis = i["id"].as_str().is_some_and(|id| d.contains(id));
                i["dismissed"] = Value::Bool(dis);
                if let Some(f) = i["id"].as_str().and_then(|id| first.get(id)) {
                    i["since"] = serde_json::to_value(f).unwrap_or_default();
                }
            }
        }
        v
    }

    pub fn save(&self) {
        let saved = Saved {
            tags: self.tags.lock().iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            services: self.services.lock().clone(),
            dismissed: self.dismissed.lock().clone(),
            pairs: self.pair_states.lock().clone(),
            changes: self.changes.lock().clone(),
            first_seen: self.first_seen.lock().clone(),
            resolved: self.resolved.lock().clone(),
            last_digest: *self.last_digest.lock(),
        };
        if let Ok(b) = serde_json::to_vec(&saved) {
            let tmp = self.path.with_extension("tmp");
            let _ = std::fs::write(&tmp, b).and_then(|_| std::fs::rename(&tmp, &self.path));
        }
    }
}

/// Watch tag events, sample statuses every 10 s, recompute every 15 min and save every 5.
pub async fn run(ins: Arc<Insights>, app: crate::web::App, engine: Arc<crate::alarms::Engine>) {
    let mut events = app.tags.subscribe();
    let mut status = tokio::time::interval(Duration::from_secs(10));
    let mut compute = tokio::time::interval(Duration::from_secs(900));
    let mut save = tokio::time::interval(Duration::from_secs(300));
    loop {
        tokio::select! {
            e = events.recv() => match e {
                // Values typed in the UI or generated by simulators aren't the process.
                Ok(e) if e.origin == crate::tags::Origin::Service => ins.observe(&e),
                // Someone set a value through Cowpoke: a change whose effect is worth watching.
                Ok(e) if e.origin == crate::tags::Origin::User => ins.note_change(Change {
                    ts: e.state.ts,
                    by: "Cowpoke".into(),
                    service: Some(e.service.clone()),
                    device: None,
                    what: format!("{} set to {}", e.tag, e.state.value),
                }),
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return,
            },
            _ = status.tick() => {
                for (id, st) in app.mgr.all_statuses() {
                    ins.sample_status(&id, st.state, &st.detail);
                }
                ins.tick_pairs(Utc::now());
            }
            _ = compute.tick() => {
                recompute_now(&ins, &app, &engine);
                if let Some(d) = ins.digest_due(Utc::now()) {
                    send_digest(&ins, &engine, &d.hooks, ins.last_digest());
                }
            }
            _ = save.tick() => ins.save(),
        }
    }
}

/// Send the digest through the named hooks (write hooks are skipped: they set tags).
pub fn send_digest(ins: &Insights, engine: &Arc<crate::alarms::Engine>, hooks: &[String], since: Option<DateTime<Utc>>) -> usize {
    let (count, text) = ins.digest_text(since);
    let defs = engine.store.get().hooks;
    let event = AlarmEvent {
        time: Utc::now(),
        event: EventKind::Test,
        alarm: "insights-digest".into(),
        name: "Insights digest".into(),
        severity: crate::alarms::Severity::Low,
        service: String::new(),
        tag: String::new(),
        value: Value::from(count),
        message: text,
        user: None,
    };
    let mut sent = 0;
    for h in defs.into_iter().filter(|h| h.enabled && hooks.contains(&h.id)) {
        if matches!(h.kind, crate::alarms::HookKind::Write { .. }) {
            continue;
        }
        engine.clone().spawn_hook(h, event.clone());
        sent += 1;
    }
    ins.mark_digest_sent(event.time);
    sent
}

pub fn recompute_now(ins: &Insights, app: &crate::web::App, engine: &crate::alarms::Engine) {
    let cfg = app.cfg.get();
    let log = engine.log_tail(20_000);
    let defs = engine.store.get().alarms;
    let names: HashMap<String, String> = defs
        .iter()
        .map(|a| (a.id.clone(), if a.name.is_empty() { a.id.clone() } else { a.name.clone() }))
        .collect();
    ins.recompute(&cfg, &log, &engine.states(), &names, &defs);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HistoryCfg, TagDef};
    use chrono::DurationRound;
    use serde_json::json;

    fn svc(yaml: &str) -> ServiceDef {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn feed(st: &mut TagStats, start: DateTime<Utc>, every_s: i64, n: i64, value: impl Fn(i64) -> Value) {
        for i in 0..n {
            st.observe(start + TimeDelta::seconds(i * every_s), &value(i), Quality::Good);
        }
    }

    fn snap<'a>(cfg: &'a Config, tags: &'a HashMap<(String, String), TagStats>, log: &'a [AlarmEvent]) -> Vec<Insight> {
        let services = HashMap::new();
        let states = HashMap::new();
        let names = HashMap::new();
        let s = Snapshot { now: Utc::now(), cfg, tags, services: &services, alarm_log: log, alarm_states: &states, alarm_names: &names, monitors: &[], alarm_defs: &[], pairs: &[], changes: &[], monitor_events: &[], clocks: &[] };
        analyze(&s, &Tuning::default())
    }

    #[test]
    fn overpolling_and_frozen() {
        let def = svc("id: plc\nkind: modbus\nmode: client\nsettings: { host: x, poll_ms: 250 }\ntags:\n  - { name: temp, datatype: float32, register: 0, writable: false }\n  - { name: level, datatype: float32, register: 2, writable: false }\n");
        let cfg = Config { services: vec![def] };
        let start = Utc::now() - TimeDelta::hours(8);
        let mut tags = HashMap::new();
        // temp changes every 5 minutes; level never changes. Both polled every 250 ms (sampled every 10 s here).
        let mut t = TagStats::default();
        feed(&mut t, start, 10, 2880, |i| json!(20.0 + (i / 30) as f64 * 0.1));
        tags.insert(("plc".to_string(), "temp".to_string()), t);
        let mut l = TagStats::default();
        feed(&mut l, start, 10, 2880, |_| json!(42.0));
        tags.insert(("plc".to_string(), "level".to_string()), l);
        let out = snap(&cfg, &tags, &[]);
        let ids: Vec<&str> = out.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"overpoll:plc"), "{out:#?}");
        assert!(ids.contains(&"frozen:plc:level"), "{out:#?}");
        let o = out.iter().find(|i| i.id == "overpoll:plc").unwrap();
        assert!(o.suggestion.contains("poll_ms to 60000"), "{}", o.suggestion);
        assert_eq!(o.fix, Some(json!({"setting": "poll_ms", "value": 60000})));
    }

    #[test]
    fn intermittent_bad_reads() {
        let def = svc("id: plc\nkind: s7\nmode: client\nsettings: { host: x }\ntags:\n  - { name: v, datatype: int16, s7_address: DB1.DBW0 }\n");
        let cfg = Config { services: vec![def] };
        let start = Utc::now() - TimeDelta::hours(1);
        let mut st = TagStats::default();
        for i in 0..400 {
            let q = if i % 10 == 0 { Quality::Bad } else { Quality::Good };
            st.observe(start + TimeDelta::seconds(i * 9), &json!(i), q);
        }
        let mut tags = HashMap::new();
        tags.insert(("plc".to_string(), "v".to_string()), st);
        let out = snap(&cfg, &tags, &[]);
        assert!(out.iter().any(|i| i.id == "bad:plc:v" && i.level == Level::Warn), "{out:#?}");
        assert!(!out.iter().any(|i| i.id == "overpoll:plc"), "changes every poll: {out:#?}");
    }

    #[test]
    fn noisy_historian_deadband() {
        let mut def = svc("id: plc\nkind: modbus\nmode: client\nsettings: { host: x, poll_ms: 1000 }\ntags:\n  - { name: flow, datatype: float32, register: 0 }\n");
        def.tags[0].history = Some(HistoryCfg { mode: HistoryMode::Deadband, deadband: 0.0, interval_ms: 1000, retention_days: 30 });
        let cfg = Config { services: vec![def] };
        let start = Utc::now() - TimeDelta::hours(1);
        let mut st = TagStats::default();
        // 100 ± 0.05 noise every second.
        feed(&mut st, start, 1, 3600, |i| json!(100.0 + if i % 2 == 0 { 0.05 } else { -0.05 } + (i / 600) as f64 * 5.0));
        let mut tags = HashMap::new();
        tags.insert(("plc".to_string(), "flow".to_string()), st);
        let out = snap(&cfg, &tags, &[]);
        let d = out.iter().find(|i| i.id == "deadband:plc:flow").expect("deadband insight");
        assert!(d.suggestion.contains("0.2"), "{}", d.suggestion);
        assert_eq!(d.fix.as_ref().unwrap()["history"]["deadband"], json!(0.2));
        let _ = TagDef::default();
    }

    #[test]
    fn chattering_and_flood() {
        let now = Utc::now();
        let mk = |alarm: &str, min_ago: i64, sec: i64| AlarmEvent {
            time: now - TimeDelta::minutes(min_ago) + TimeDelta::seconds(sec),
            event: EventKind::Raise,
            alarm: alarm.into(),
            name: alarm.into(),
            severity: Default::default(),
            service: "s".into(),
            tag: "t".into(),
            value: json!(1),
            message: String::new(),
            user: None,
        };
        let mut log = vec![];
        for i in 0..6 {
            log.push(mk("hi-level", 60, i * 60));
        }
        for (i, a) in ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k"].iter().enumerate() {
            log.push(mk(a, 30, i as i64 * 20));
        }
        let cfg = Config::default();
        let out = snap(&cfg, &HashMap::new(), &log);
        assert!(out.iter().any(|i| i.id == "chatter:hi-level"), "{out:#?}");
        assert!(out.iter().any(|i| i.id.starts_with("flood:")), "{out:#?}");
    }

    fn snap_with(cfg: &Config, tags: &HashMap<(String, String), TagStats>, defs: &[crate::alarms::AlarmDef], pairs: &[(Pair, PairState)]) -> Vec<Insight> {
        let (services, states, names) = (HashMap::new(), HashMap::new(), HashMap::new());
        let s = Snapshot { now: Utc::now(), cfg, tags, services: &services, alarm_log: &[], alarm_states: &states, alarm_names: &names, monitors: &[], alarm_defs: defs, pairs, changes: &[], monitor_events: &[], clocks: &[] };
        analyze(&s, &Tuning::default())
    }

    fn plc(tags: &str) -> Config {
        Config { services: vec![svc(&format!("id: plc\nkind: modbus\nmode: client\nsettings: {{ host: x, poll_ms: 1000 }}\ntags:\n{tags}"))] }
    }

    #[test]
    fn equipment_rails_noise_drift() {
        let cfg = plc("  - { name: pressure, datatype: int16, register: 0 }\n  - { name: temp, datatype: float32, register: 2 }\n  - { name: level, datatype: float32, register: 4 }\n");
        let start = (Utc::now() - TimeDelta::hours(10)).duration_trunc(TimeDelta::hours(1)).unwrap();
        let mut tags = HashMap::new();
        // pressure: a broken wire part of the time.
        let mut p = TagStats::default();
        feed(&mut p, start, 30, 1200, |i| json!(if i % 40 == 0 { 32767 } else { 500 + i % 7 }));
        tags.insert(("plc".to_string(), "pressure".to_string()), p);
        // temp: quiet for 9 hours, noisy in the last complete hour.
        let mut t = TagStats::default();
        feed(&mut t, start, 10, 3600, |i| {
            let hour = i / 360;
            let amp = if hour == 9 { 3.0 } else { 0.1 };
            json!(50.0 + if i % 2 == 0 { amp } else { -amp })
        });
        tags.insert(("plc".to_string(), "temp".to_string()), t);
        let out = snap_with(&cfg, &tags, &[], &[]);
        let ids: Vec<&str> = out.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"rails:plc:pressure"), "{ids:?}");
        assert!(ids.contains(&"noise:plc:temp"), "{ids:?}");

        // level: a steady climb over 20 hours.
        let start = (Utc::now() - TimeDelta::hours(20)).duration_trunc(TimeDelta::hours(1)).unwrap();
        let mut l = TagStats::default();
        feed(&mut l, start, 60, 1200, |i| json!(10.0 + i as f64 * 0.05 + if i % 2 == 0 { 0.01 } else { -0.01 }));
        let mut tags = HashMap::new();
        tags.insert(("plc".to_string(), "level".to_string()), l);
        let out = snap_with(&cfg, &tags, &[], &[]);
        assert!(out.iter().any(|i| i.id == "drift:plc:level"), "{out:#?}");
    }

    #[test]
    fn forecast_reaches_alarm() {
        let cfg = plc("  - { name: level, datatype: float32, register: 0 }\n");
        let start = Utc::now() - TimeDelta::minutes(40);
        let mut l = TagStats::default();
        // Rising 0.01 per second from 60: 90 is 50 min away from the last reading.
        feed(&mut l, start, 20, 120, |i| json!(60.0 + i as f64 * 0.2));
        let mut tags = HashMap::new();
        tags.insert(("plc".to_string(), "level".to_string()), l);
        let hi: crate::alarms::AlarmDef = serde_yaml::from_str("id: hi-level\nname: High level\nservice: plc\ntag: level\nkind: high\nlimit: 90").unwrap();
        let out = snap_with(&cfg, &tags, &[hi], &[]);
        let f = out.iter().find(|i| i.id == "forecast:hi-level").expect("forecast");
        assert!(f.title.contains("min"), "{}", f.title);
    }

    #[test]
    fn stuck_step_and_slower_cycle() {
        let cfg = plc("  - { name: filling, datatype: bool, table: coil, register: 0 }\n");
        let start = Utc::now() - TimeDelta::hours(3);
        let mut st = TagStats::default();
        let mut ts = start;
        // 30 cycles of 60 s on / 60 s off, then 10 cycles of 90 s on / 60 s off, then on and stuck.
        for i in 0..40 {
            st.observe(ts, &json!(true), Quality::Good);
            ts += TimeDelta::seconds(if i < 30 { 60 } else { 90 });
            st.observe(ts, &json!(false), Quality::Good);
            ts += TimeDelta::seconds(60);
        }
        st.observe(ts, &json!(true), Quality::Good);
        let mut tags = HashMap::new();
        tags.insert(("plc".to_string(), "filling".to_string()), st);
        let out = snap_with(&cfg, &tags, &[], &[]);
        let ids: Vec<&str> = out.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"stuck:plc:filling"), "{ids:?}");
        assert!(ids.contains(&"slower:plc:filling"), "{ids:?}");
    }

    #[test]
    fn command_without_feedback() {
        let p = Pair { name: "Feed pump".into(), command: "plc/run".into(), feedback: "plc/running".into(), kind: PairKind::Runs, tolerance: 0.0, delay_s: 10 };
        let mut st = PairState::default();
        let t0 = Utc::now() - TimeDelta::minutes(5);
        check_pair(&p, &mut st, t0, &json!(true), &json!(false));
        assert_eq!(st.incidents, 0);
        check_pair(&p, &mut st, t0 + TimeDelta::seconds(5), &json!(true), &json!(false));
        assert_eq!(st.incidents, 0, "within the delay");
        check_pair(&p, &mut st, t0 + TimeDelta::seconds(12), &json!(true), &json!(false));
        check_pair(&p, &mut st, t0 + TimeDelta::seconds(20), &json!(true), &json!(false));
        assert_eq!(st.incidents, 1, "one incident per mismatch");
        check_pair(&p, &mut st, t0 + TimeDelta::seconds(25), &json!(true), &json!(true));
        assert!(st.mismatch_since.is_none());
        let f = Pair { kind: PairKind::Follows, tolerance: 2.0, ..p.clone() };
        let mut fs = PairState::default();
        check_pair(&f, &mut fs, t0, &json!(50), &json!(49));
        check_pair(&f, &mut fs, t0 + TimeDelta::seconds(30), &json!(80), &json!(49));
        check_pair(&f, &mut fs, t0 + TimeDelta::seconds(45), &json!(80), &json!(49));
        assert_eq!(fs.incidents, 1);
        let cfg = Config::default();
        let out = snap_with(&cfg, &HashMap::new(), &[], &[(p, st)]);
        assert!(out.iter().any(|i| i.id == "pair:Feed pump" && i.title.contains("at times")), "{out:#?}");
    }

    #[test]
    fn change_and_its_effect() {
        let cfg = plc("  - { name: temp, datatype: float32, register: 0 }\n");
        let start = (Utc::now() - TimeDelta::hours(8)).duration_trunc(TimeDelta::hours(1)).unwrap();
        let change_at = start + TimeDelta::hours(4) + TimeDelta::minutes(1);
        let mut st = TagStats::default();
        // 50 before the change, 60 after (noise ±0.1).
        feed(&mut st, start, 30, 8 * 120, |i| json!(if i < 4 * 120 { 50.0 } else { 60.0 } + if i % 2 == 0 { 0.1 } else { -0.1 }));
        let mut tags = HashMap::new();
        tags.insert(("plc".to_string(), "temp".to_string()), st);
        let changes = vec![Change { ts: change_at, by: "Cowpoke".into(), service: Some("plc".into()), device: None, what: "setpoint set to 60".into() }];
        let (services, states, names) = (HashMap::new(), HashMap::new(), HashMap::new());
        let s = Snapshot { now: Utc::now(), cfg: &cfg, tags: &tags, services: &services, alarm_log: &[], alarm_states: &states, alarm_names: &names,
            monitors: &[], alarm_defs: &[], pairs: &[], changes: &changes, monitor_events: &[], clocks: &[] };
        let out = analyze(&s, &Tuning::default());
        let e = out.iter().find(|i| i.id.starts_with("effect:")).expect("effect insight");
        assert!(e.evidence[0].contains("50.000 → 60.000"), "{:?}", e.evidence);
    }

    #[test]
    fn writes_at_an_unusual_hour() {
        use chrono::Timelike;
        let now = Utc::now();
        let odd_hour = (now.hour() as usize + 12) % 24;
        let usual = (odd_hour + 6) % 24;
        let mut tk = crate::monitor::Talker::new("10.0.0.9".into(), "10.0.0.20:102".into(), crate::monitor::Proto::S7, now - TimeDelta::days(5));
        tk.action_hours[usual] = 300;
        let ts = (now - TimeDelta::hours(12)).with_minute(5).unwrap();
        let ev = crate::monitor::Event { ts, protocol: crate::monitor::Proto::S7, client: "10.0.0.9".into(), server: "10.0.0.20:102".into(),
            action: crate::monitor::Action::Program, what: "download block".into(), new_talker: false };
        let mons = vec![("mon".to_string(), vec![tk], vec![])];
        let events = vec![ev];
        let cfg = Config::default();
        let (tags, services, states, names) = (HashMap::new(), HashMap::new(), HashMap::new(), HashMap::new());
        let s = Snapshot { now, cfg: &cfg, tags: &tags, services: &services, alarm_log: &[], alarm_states: &states, alarm_names: &names,
            monitors: &mons, alarm_defs: &[], pairs: &[], changes: &[], monitor_events: &events, clocks: &[] };
        let out = analyze(&s, &Tuning::default());
        let o = out.iter().find(|i| i.id.starts_with("offhours:")).expect("off-hours insight");
        assert_eq!(o.level, Level::Warn, "a program download");
    }

    #[test]
    fn data_quality_checks() {
        let cfg = Config { services: vec![
            svc("id: a\nkind: modbus\nmode: client\nsettings: { host: 10.0.0.5, port: 502, poll_ms: 1000 }\ntags:\n  - { name: level, datatype: int16, register: 0, scale: 0.1 }\n  - { name: speed, datatype: uint16, register: 3, units: \"%\" }\n  - { name: temp, datatype: int16, register: 5 }\n"),
            svc("id: b\nkind: modbus\nmode: client\nsettings: { host: 10.0.0.5, port: 502, poll_ms: 1000 }\ntags:\n  - { name: tank_level, datatype: int16, register: 0 }\n"),
        ]};
        let start = (Utc::now() - TimeDelta::hours(3)).duration_trunc(TimeDelta::hours(1)).unwrap();
        let mut tags = HashMap::new();
        let mut a = TagStats::default();
        feed(&mut a, start, 60, 180, |_| json!(42.5));
        tags.insert(("a".to_string(), "level".to_string()), a);
        let mut b = TagStats::default();
        feed(&mut b, start, 60, 180, |_| json!(425));
        tags.insert(("b".to_string(), "tank_level".to_string()), b);
        let mut sp = TagStats::default();
        feed(&mut sp, start, 60, 180, |i| json!(400 + i));
        tags.insert(("a".to_string(), "speed".to_string()), sp);
        // temp stopped updating an hour ago; the rest of service a is fresh.
        let mut tt = TagStats::default();
        feed(&mut tt, Utc::now() - TimeDelta::hours(3), 60, 120, |i| json!(20 + i % 3));
        tags.insert(("a".to_string(), "temp".to_string()), tt);
        let mut fresh = TagStats::default();
        feed(&mut fresh, Utc::now() - TimeDelta::minutes(5), 1, 300, |_| json!(1));
        tags.get_mut(&("a".to_string(), "speed".to_string())).unwrap().last_update = fresh.last_update;
        let out = snap_with(&cfg, &tags, &[], &[]);
        let ids: Vec<&str> = out.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.iter().any(|i| i.starts_with("disagree:")), "{ids:?}");
        assert!(ids.contains(&"units:a:speed"), "{ids:?}");
        assert!(ids.contains(&"gap:a:temp"), "{ids:?}");
    }

    #[test]
    fn resolved_and_digest() {
        let dir = tempfile::tempdir().unwrap();
        let ins = Insights::open(dir.path().join("baselines.json"));
        let mk = |id: &str, level| Insight { id: id.into(), area: Area::Comms, level, title: format!("title {id}"), detail: String::new(),
            evidence: vec![], suggestion: format!("do {id}"), service: Some("plc".into()), tag: None, fix: None };
        let t0 = Utc::now() - TimeDelta::hours(2);
        let first = vec![mk("a", Level::Warn), mk("b", Level::Suggest)];
        ins.track(&first, t0);
        ins.report.lock().insights = first;
        // An hour later "a" is gone and "c" is new.
        let second = vec![mk("b", Level::Suggest), mk("c", Level::Warn)];
        ins.track(&second, t0 + TimeDelta::hours(1));
        ins.report.lock().insights = second;
        let resolved = ins.resolved.lock().clone();
        assert_eq!(resolved.len(), 1);
        assert_eq!((resolved[0].id.as_str(), resolved[0].first_seen), ("a", t0));
        // The digest since just after the first run names only "c".
        let (n, text) = ins.digest_text(Some(t0 + TimeDelta::minutes(30)));
        assert_eq!(n, 1, "{text}");
        assert!(text.contains("[CHECK] title c") && !text.contains("title b"), "{text}");
        assert!(text.starts_with("Cowpoke insights: 2 open (1 to check), 1 new, 1 resolved."), "{text}");
        // Due once at the configured hour, not again the same day.
        use chrono::{Timelike, TimeZone};
        ins.set_config("digest: { hooks: [ops], hour: 7 }").unwrap();
        let at7 = Utc.with_ymd_and_hms(2026, 11, 23, 7, 5, 0).unwrap();
        assert!(ins.digest_due(at7).is_some());
        assert!(ins.digest_due(at7.with_hour(8).unwrap()).is_none());
        ins.mark_digest_sent(at7);
        assert!(ins.digest_due(at7 + TimeDelta::minutes(15)).is_none());
        assert!(ins.set_config("digest: { hooks: [ops], hour: 24 }").is_err());
    }

    #[test]
    fn device_clock_off() {
        let cfg = Config { services: vec![
            svc("id: ua\nkind: opcua\nmode: client\nsettings: { url: \"opc.tcp://x:4840\" }\ntags: []\n"),
            svc("id: sp\nkind: mqtt\nmode: client\nsettings: { host: x, sparkplug: { group_id: G, edge_node: N } }\ntags: []\n"),
        ]};
        let clocks = vec![("ua".to_string(), -90_000.0, 20), ("sp".to_string(), 2_000.0, 20)];
        let (tags, services, states, names) = (HashMap::new(), HashMap::new(), HashMap::new(), HashMap::new());
        let s = Snapshot { now: Utc::now(), cfg: &cfg, tags: &tags, services: &services, alarm_log: &[], alarm_states: &states, alarm_names: &names,
            monitors: &[], alarm_defs: &[], pairs: &[], changes: &[], monitor_events: &[], clocks: &clocks };
        let out = analyze(&s, &Tuning::default());
        let c = out.iter().find(|i| i.id == "clock:ua").expect("clock insight");
        assert!(c.title.contains("2 min behind") || c.title.contains("90 s behind"), "{}", c.title);
        assert_eq!(c.level, Level::Warn);
        assert!(!out.iter().any(|i| i.id == "clock:sp"), "2 s is within tolerance");
    }

    #[test]
    fn rounding() {
        assert_eq!(round_ms(75_000.0), 60_000);
        assert_eq!(round_db(0.1), 0.1);
        assert_eq!(round_db(0.27), 0.2);
        assert_eq!(round_db(4.0), 5.0);
    }
}
