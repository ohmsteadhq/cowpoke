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
    #[serde(skip)]
    last_value: Option<Value>,
}

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
        if let Some(x) = as_f64(v).filter(|x| x.is_finite()) {
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
}

/// Run every analyzer.
pub fn analyze(s: &Snapshot, t: &Tuning) -> Vec<Insight> {
    let mut out = vec![];
    network(s, &mut out);
    comms(s, t, &mut out);
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
}

#[derive(Serialize, Clone, Default)]
pub struct Report {
    pub generated: Option<DateTime<Utc>>,
    pub watching_since: Option<DateTime<Utc>>,
    pub tags_watched: usize,
    pub insights: Vec<Insight>,
    pub dismissed: usize,
}

pub struct Insights {
    path: PathBuf,
    tags: Mutex<HashMap<(String, String), TagStats>>,
    services: Mutex<HashMap<String, ServiceStats>>,
    dismissed: Mutex<HashSet<String>>,
    report: Mutex<Report>,
    pub tuning: Tuning,
}

impl Insights {
    pub fn open(path: PathBuf) -> Arc<Self> {
        let saved: Saved = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Arc::new(Self {
            path,
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

    pub fn recompute(&self, cfg: &Config, alarm_log: &[AlarmEvent], alarm_states: &HashMap<String, AlarmState>, alarm_names: &HashMap<String, String>) {
        let tags = self.tags.lock().clone();
        let services = self.services.lock().clone();
        let monitors = crate::monitor::health();
        let snap = Snapshot { now: Utc::now(), cfg, tags: &tags, services: &services, alarm_log, alarm_states, alarm_names, monitors: &monitors };
        let all = analyze(&snap, &self.tuning);
        let dismissed = self.dismissed.lock().clone();
        let mut r = self.report.lock();
        r.generated = Some(snap.now);
        r.watching_since = tags.values().filter_map(|t| t.first).min();
        r.tags_watched = tags.len();
        r.dismissed = all.iter().filter(|i| dismissed.contains(&i.id)).count();
        r.insights = all;
    }

    /// The report; dismissed insights are flagged rather than dropped.
    pub fn report(&self) -> Value {
        let r = self.report.lock().clone();
        let d = self.dismissed.lock().clone();
        let mut v = serde_json::to_value(&r).unwrap_or_default();
        if let Some(list) = v.get_mut("insights").and_then(|x| x.as_array_mut()) {
            for i in list {
                let dis = i["id"].as_str().is_some_and(|id| d.contains(id));
                i["dismissed"] = Value::Bool(dis);
            }
        }
        v
    }

    pub fn save(&self) {
        let saved = Saved {
            tags: self.tags.lock().iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            services: self.services.lock().clone(),
            dismissed: self.dismissed.lock().clone(),
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
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return,
            },
            _ = status.tick() => {
                for (id, st) in app.mgr.all_statuses() {
                    ins.sample_status(&id, st.state, &st.detail);
                }
            }
            _ = compute.tick() => recompute_now(&ins, &app, &engine),
            _ = save.tick() => ins.save(),
        }
    }
}

pub fn recompute_now(ins: &Insights, app: &crate::web::App, engine: &crate::alarms::Engine) {
    let cfg = app.cfg.get();
    let log = engine.log_tail(20_000);
    let names: HashMap<String, String> = engine
        .store
        .get()
        .alarms
        .iter()
        .map(|a| (a.id.clone(), if a.name.is_empty() { a.id.clone() } else { a.name.clone() }))
        .collect();
    ins.recompute(&cfg, &log, &engine.states(), &names);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HistoryCfg, TagDef};
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
        let s = Snapshot { now: Utc::now(), cfg, tags, services: &services, alarm_log: log, alarm_states: &states, alarm_names: &names, monitors: &[] };
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

    #[test]
    fn rounding() {
        assert_eq!(round_ms(75_000.0), 60_000);
        assert_eq!(round_db(0.1), 0.1);
        assert_eq!(round_db(0.27), 0.2);
        assert_eq!(round_db(4.0), 5.0);
    }
}
