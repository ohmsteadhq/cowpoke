//! Passive monitoring: watch industrial traffic on a mirror (SPAN) port, or replay a
//! capture file, and build an inventory of who talks to whom plus notable events
//! (writes, program transfers, run/stop, logins, errors, new talkers).
//!
//! Runs as a `monitor` service (mode `client`: it only listens). Its counters are
//! ordinary tags, so the historian, alarms and boards work on them unchanged.

pub mod decode;
pub mod packet;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::manager::Ctx;
use crate::model::DataType;
use crate::tags::{Origin, Quality};
pub use decode::{Action, Obs, Proto};
use packet::{Frame, Link, Streams};

/// The tags a monitor service publishes. A monitor's `tags` may list any of these.
pub const COUNTERS: &[(&str, DataType, &str)] = &[
    ("talkers", DataType::Uint32, "Distinct client → device pairs seen"),
    ("devices", DataType::Uint32, "Distinct devices (servers) seen"),
    ("events", DataType::Uint64, "Notable events since start"),
    ("writes", DataType::Uint64, "Write requests seen"),
    ("program_changes", DataType::Uint64, "Program or configuration transfers seen"),
    ("control_actions", DataType::Uint64, "Run/stop, reset and method calls seen"),
    ("logins", DataType::Uint64, "Logins and password attempts seen"),
    ("errors", DataType::Uint64, "Error replies seen"),
    ("new_talkers", DataType::Uint64, "Talkers first seen after the learning period"),
    ("last_event", DataType::String, "The latest notable event, in words"),
];

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Network interface to capture on (a mirror/SPAN port), e.g. `eth1`.
    #[serde(default)]
    pub interface: Option<String>,
    /// Replay a pcap or pcapng file instead of capturing live.
    #[serde(default)]
    pub file: Option<String>,
    /// Extra BPF filter for live capture (the protocol ports are always included).
    #[serde(default)]
    pub filter: Option<String>,
    /// Ports per protocol, when devices use non-standard ones. Defaults: Modbus 502,
    /// EtherNet/IP 44818, S7 102, OPC UA 4840, MQTT 1883 and 8883 is skipped (TLS).
    #[serde(default)]
    pub ports: BTreeMap<Proto, Vec<u16>>,
    /// Minutes to learn the normal set of talkers before new ones raise events.
    #[serde(default = "default_learn")]
    pub learn_minutes: u64,
    /// Report reads as events too (noisy; off by default).
    #[serde(default)]
    pub report_reads: bool,
}

impl<'de> Deserialize<'de> for Proto {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(match s.as_str() {
            "modbus" => Proto::Modbus,
            "enip" | "eip" => Proto::Enip,
            "s7" => Proto::S7,
            "opcua" => Proto::Opcua,
            "mqtt" => Proto::Mqtt,
            o => return Err(serde::de::Error::custom(format!("unknown protocol `{o}` (modbus, eip, s7, opcua, mqtt)"))),
        })
    }
}

fn default_learn() -> u64 {
    60
}

impl Settings {
    fn port_map(&self) -> HashMap<u16, Proto> {
        let mut m = HashMap::new();
        for p in Proto::ALL {
            let ports = self.ports.get(&p).cloned().unwrap_or_else(|| vec![p.default_port()]);
            for port in ports {
                m.insert(port, p);
            }
        }
        m
    }
}

#[derive(Serialize, Clone)]
pub struct Talker {
    pub client: String,
    pub server: String,
    pub protocol: Proto,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub counts: BTreeMap<String, u64>,
    pub last: String,
    /// Payload bytes, client → device and back.
    pub bytes_up: u64,
    pub bytes_down: u64,
    /// TCP connections opened, and those still open.
    pub connections: u64,
    pub open: i64,
    /// Data segments seen, and how many were retransmissions.
    pub segments: u64,
    pub retransmits: u64,
    /// Requests answered, and requests that got no answer within 5 s.
    pub answered: u64,
    pub timeouts: u64,
    /// Response time percentiles over recent requests (Modbus, EtherNet/IP, S7).
    pub latency_p50_ms: Option<f64>,
    pub latency_p95_ms: Option<f64>,
    /// Writes, program and control actions by hour of day (UTC), to learn when they normally happen.
    pub action_hours: Vec<u64>,
    #[serde(skip)]
    latencies: Vec<f64>,
}

impl Talker {
    pub fn new(client: String, server: String, protocol: Proto, ts: DateTime<Utc>) -> Self {
        Self {
            client,
            server,
            protocol,
            first_seen: ts,
            last_seen: ts,
            counts: BTreeMap::new(),
            last: String::new(),
            bytes_up: 0,
            bytes_down: 0,
            connections: 0,
            open: 0,
            segments: 0,
            retransmits: 0,
            answered: 0,
            timeouts: 0,
            latency_p50_ms: None,
            latency_p95_ms: None,
            action_hours: vec![0; 24],
            latencies: vec![],
        }
    }

    fn latency(&mut self, ms: f64) {
        if self.latencies.len() >= 512 {
            self.latencies.remove(0);
        }
        self.latencies.push(ms);
        let mut s = self.latencies.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pct = |p: f64| s[((s.len() - 1) as f64 * p).round() as usize];
        self.latency_p50_ms = Some(pct(0.5));
        self.latency_p95_ms = Some(pct(0.95));
    }
}

/// One IPv4 address seen from more than one MAC address.
#[derive(Serialize, Clone, Debug)]
pub struct AddressConflict {
    pub ip: String,
    pub macs: Vec<String>,
    pub first_seen: DateTime<Utc>,
}

#[derive(Serialize, Clone)]
pub struct Event {
    pub ts: DateTime<Utc>,
    pub protocol: Proto,
    pub client: String,
    pub server: String,
    pub action: Action,
    pub what: String,
    /// The pair wasn't seen during the learning period.
    pub new_talker: bool,
}

const MAX_EVENTS: usize = 1000;

/// What a monitor has seen, shared with the web API.
#[derive(Default)]
pub struct State {
    pub talkers: BTreeMap<(String, String, Proto), Talker>,
    pub events: VecDeque<Event>,
    pub totals: BTreeMap<&'static str, u64>,
    pub started: Option<DateTime<Utc>>,
    pub frames: u64,
    /// IPv4 address → MAC addresses seen sending from it (Ethernet captures only).
    pub macs: BTreeMap<std::net::Ipv4Addr, Vec<[u8; 6]>>,
    pub conflicts: Vec<AddressConflict>,
}

/// A running monitor's talkers and address conflicts, for the insights engine.
pub fn health() -> Vec<(String, Vec<Talker>, Vec<AddressConflict>)> {
    MONITORS
        .iter()
        .map(|e| {
            let st = e.value().lock();
            (e.key().clone(), st.talkers.values().cloned().collect(), st.conflicts.clone())
        })
        .collect()
}

/// Every running monitor's recent events (oldest first).
pub fn recent_events() -> Vec<Event> {
    MONITORS.iter().flat_map(|e| e.value().lock().events.iter().cloned().collect::<Vec<_>>()).collect()
}

fn mac_text(m: &[u8; 6]) -> String {
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

static MONITORS: LazyLock<dashmap::DashMap<String, Arc<Mutex<State>>>> = LazyLock::new(Default::default);

/// The inventory and recent events of a running monitor service (for the API).
pub fn snapshot(id: &str) -> Option<Value> {
    let st = MONITORS.get(id)?.clone();
    let st = st.lock();
    Some(json!({
        "started": st.started,
        "frames": st.frames,
        "totals": st.totals,
        "talkers": st.talkers.values().collect::<Vec<_>>(),
        "conflicts": st.conflicts,
        "events": st.events.iter().rev().collect::<Vec<_>>(),
    }))
}

/// Decodes frames and keeps the inventory. Separate from I/O so tests can drive it.
pub struct Monitor {
    ports: HashMap<u16, Proto>,
    streams: Streams,
    /// Streams that aren't the protocol their port suggests.
    ignored: std::collections::HashSet<(SocketAddr, SocketAddr)>,
    learn: Duration,
    report_reads: bool,
    /// When each (talker, event text) was last reported: repeats within a minute are
    /// counted but not listed again (a polling client hitting an error every 100 ms).
    recent: HashMap<(String, String, Proto, String), DateTime<Utc>>,
    /// Requests waiting for an answer, per connection (client, device), oldest first.
    pending: HashMap<(SocketAddr, SocketAddr), VecDeque<(DateTime<Utc>, Option<u64>)>>,
    /// Retransmissions already counted, per stream direction.
    retrans_seen: HashMap<(SocketAddr, SocketAddr, bool), u64>,
    pub state: Arc<Mutex<State>>,
}

/// A request with no answer after this long counts as a timeout.
const ANSWER_TIMEOUT: chrono::TimeDelta = chrono::TimeDelta::seconds(5);

const REPEAT_QUIET: chrono::TimeDelta = chrono::TimeDelta::seconds(60);

fn action_key(a: Action) -> &'static str {
    match a {
        Action::Connect => "connect",
        Action::Login => "login",
        Action::Read => "read",
        Action::Write => "write",
        Action::Program => "program",
        Action::Control => "control",
        Action::Diagnostic => "diagnostic",
        Action::Error => "error",
        Action::Other => "other",
    }
}

impl Monitor {
    pub fn new(s: &Settings) -> Self {
        Self {
            ports: s.port_map(),
            streams: Streams::default(),
            ignored: Default::default(),
            learn: Duration::from_secs(s.learn_minutes * 60),
            report_reads: s.report_reads,
            recent: HashMap::new(),
            pending: HashMap::new(),
            retrans_seen: HashMap::new(),
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// Feed one frame; returns the notable events it produced.
    pub fn frame(&mut self, f: &Frame) -> Vec<Event> {
        let mut out = vec![];
        if let Some((mac, ip)) = packet::ether_sender(f.link, &f.data) {
            if let Some(e) = self.note_mac(f.ts, mac, ip) {
                out.push(e);
            }
        }
        out.extend(self.tcp_frame(f));
        out
    }

    /// Remember which MAC sends from which IPv4 address; a second MAC is a conflict.
    fn note_mac(&mut self, ts: DateTime<Utc>, mac: [u8; 6], ip: std::net::Ipv4Addr) -> Option<Event> {
        let mut st = self.state.lock();
        let macs = st.macs.entry(ip).or_default();
        if macs.contains(&mac) {
            return None;
        }
        macs.push(mac);
        if macs.len() < 2 {
            return None;
        }
        let list: Vec<String> = macs.iter().map(mac_text).collect();
        let what = format!("address {ip} used by {} network cards: {}", list.len(), list.join(", "));
        match st.conflicts.iter_mut().find(|c| c.ip == ip.to_string()) {
            Some(c) => c.macs = list,
            None => st.conflicts.push(AddressConflict { ip: ip.to_string(), macs: list, first_seen: ts }),
        }
        // Not a protocol's event: it names the address, with no device.
        let e = Event { ts, protocol: Proto::Modbus, client: ip.to_string(), server: String::new(), action: Action::Error, what, new_talker: false };
        drop(st);
        Some(self.push_event(e))
    }

    fn push_event(&mut self, e: Event) -> Event {
        let mut st = self.state.lock();
        if st.events.len() >= MAX_EVENTS {
            st.events.pop_front();
        }
        st.events.push_back(e.clone());
        *st.totals.entry("events").or_default() += 1;
        *st.totals.entry("errors").or_default() += 1;
        e
    }

    fn tcp_frame(&mut self, f: &Frame) -> Vec<Event> {
        let Some(seg) = packet::tcp_segment(f.link, &f.data) else { return vec![] };
        let (proto, to_server) = match (self.ports.get(&seg.dst.port()), self.ports.get(&seg.src.port())) {
            (Some(p), _) => (*p, true),
            (None, Some(p)) => (*p, false),
            _ => return vec![],
        };
        let key = (seg.src, seg.dst);
        {
            let mut st = self.state.lock();
            st.frames += 1;
            st.started.get_or_insert(f.ts);
        }
        if self.ignored.contains(&key) {
            return vec![];
        }
        let (client, server) = if to_server { (seg.src, seg.dst) } else { (seg.dst, seg.src) };
        let fin = seg.fin_or_rst;
        let payload = seg.payload.len() as u64;
        let stream = self.streams.push(&seg);
        let mut found = vec![];
        let mut pdus = vec![];
        match decode::decode_counted(proto, to_server, &stream.buf) {
            Some((obs, used, n)) => {
                stream.consume(used);
                found = obs;
                pdus = n;
            }
            None => {
                // Not this protocol (or we joined mid-PDU): drop the buffer; if it never
                // decoded, stop looking at this stream.
                if stream.resynced {
                    stream.buf.clear();
                } else {
                    self.ignored.insert(key);
                }
            }
        }
        let (segs, retrans) = (stream.segments, stream.retransmits);
        if fin {
            self.streams.close(&seg);
        }
        let _ = segs;
        // A new connection is worth recording even before any PDU decodes.
        if seg.syn && to_server {
            found.insert(0, Obs { action: Action::Other, what: "new TCP connection".into() });
        }
        // Record first: it decides whether this talker is new.
        let events = self.record(f.ts, proto, client, server, found);
        self.wire(f.ts, proto, client, server, to_server, payload, pdus, seg.syn && to_server, fin, retrans);
        events
    }

    /// Bytes, connections, retransmissions and response times per talker.
    #[allow(clippy::too_many_arguments)]
    fn wire(&mut self, ts: DateTime<Utc>, proto: Proto, client: SocketAddr, server: SocketAddr, to_server: bool,
            payload: u64, pdus: Vec<Option<u64>>, syn: bool, fin: bool, retrans: u64) {
        let conn = (client, server);
        // The stream counts retransmissions cumulatively; take what's new.
        let dir = (client, server, to_server);
        let prev = self.retrans_seen.insert(dir, retrans).unwrap_or(0);
        let new_retrans = retrans.saturating_sub(prev);
        if fin {
            self.retrans_seen.remove(&dir);
        }
        let mut answered: Vec<f64> = vec![];
        let mut timeouts = 0u64;
        if proto.request_response() {
            let q = self.pending.entry(conn).or_default();
            let before = q.len();
            q.retain(|(t, _)| ts - *t <= ANSWER_TIMEOUT);
            timeouts += (before - q.len()) as u64;
            for pid in pdus {
                if to_server {
                    if q.len() < 256 {
                        q.push_back((ts, pid));
                    }
                    continue;
                }
                // The oldest request with the same id; without ids, the oldest request.
                let hit = match pid {
                    Some(_) => q.iter().position(|(_, r)| *r == pid),
                    None => (!q.is_empty()).then_some(0),
                };
                if let Some((sent, _)) = hit.and_then(|i| q.remove(i)) {
                    answered.push((ts - sent).num_microseconds().unwrap_or(0) as f64 / 1000.0);
                }
            }
            if fin {
                self.pending.remove(&conn);
            }
        }
        let mut st = self.state.lock();
        let tk = (client.ip().to_string(), server.to_string(), proto);
        let t = st.talkers.entry(tk.clone()).or_insert_with(|| Talker::new(tk.0.clone(), tk.1.clone(), proto, ts));
        t.last_seen = ts;
        if to_server { t.bytes_up += payload } else { t.bytes_down += payload }
        if syn {
            t.connections += 1;
            t.open += 1;
        }
        if fin && to_server {
            t.open = (t.open - 1).max(0);
        }
        // The stream's own counters only grow; take this segment's share.
        if payload > 0 {
            t.segments += 1;
        }
        t.retransmits += new_retrans;
        t.timeouts += timeouts;
        for ms in answered {
            t.answered += 1;
            t.latency(ms);
        }
    }

    fn record(&mut self, ts: DateTime<Utc>, proto: Proto, client: SocketAddr, server: SocketAddr, found: Vec<Obs>) -> Vec<Event> {
        if found.is_empty() {
            return vec![];
        }
        let mut st = self.state.lock();
        let started = *st.started.get_or_insert(ts);
        // Talkers are client host → server address: client ports change per connection.
        let tk = (client.ip().to_string(), server.to_string(), proto);
        let is_new = !st.talkers.contains_key(&tk);
        let learned = (ts - started).to_std().unwrap_or_default() >= self.learn;
        let new_talker = is_new && learned;
        let t = st.talkers.entry(tk.clone()).or_insert_with(|| Talker::new(tk.0.clone(), tk.1.clone(), proto, ts));
        t.last_seen = ts;
        let mut events = vec![];
        let mut flagged_new = false;
        let mut seen: Vec<Action> = vec![];
        for o in found {
            seen.push(o.action);
            if matches!(o.action, Action::Write | Action::Program | Action::Control) {
                use chrono::Timelike;
                t.action_hours[ts.hour() as usize] += 1;
            }
            *t.counts.entry(action_key(o.action).into()).or_default() += 1;
            t.last = o.what.clone();
            let notable = match o.action {
                Action::Write | Action::Program | Action::Control | Action::Login | Action::Error | Action::Diagnostic
                | Action::Connect => true,
                Action::Read => self.report_reads,
                Action::Other => new_talker && !flagged_new,
            } || (new_talker && !flagged_new);
            let key = (tk.0.clone(), tk.1.clone(), proto, o.what.clone());
            let repeat = self.recent.get(&key).is_some_and(|&last| ts - last < REPEAT_QUIET);
            if notable && !repeat {
                self.recent.insert(key, ts);
                if self.recent.len() > 10_000 {
                    self.recent.retain(|_, t| ts - *t < REPEAT_QUIET);
                }
                flagged_new |= new_talker;
                events.push(Event {
                    ts,
                    protocol: proto,
                    client: client.ip().to_string(),
                    server: server.to_string(),
                    action: o.action,
                    what: o.what,
                    new_talker,
                });
            }
        }
        let bump = |st: &mut State, k: &'static str, n: u64| *st.totals.entry(k).or_default() += n;
        for a in &seen {
            match a {
                Action::Write => bump(&mut st, "writes", 1),
                Action::Program => bump(&mut st, "program_changes", 1),
                Action::Control => bump(&mut st, "control_actions", 1),
                Action::Login => bump(&mut st, "logins", 1),
                Action::Error => bump(&mut st, "errors", 1),
                _ => {}
            }
        }
        if new_talker {
            bump(&mut st, "new_talkers", 1);
        }
        bump(&mut st, "events", events.len() as u64);
        for e in &events {
            if st.events.len() >= MAX_EVENTS {
                st.events.pop_front();
            }
            st.events.push_back(e.clone());
        }
        events
    }

    /// Counter values for the service's tags.
    pub fn counters(&self) -> Vec<(&'static str, Value)> {
        let st = self.state.lock();
        let devices: std::collections::HashSet<&str> = st.talkers.values().map(|t| t.server.as_str()).collect();
        let last = st.events.back().map(|e| format!("{} → {} {}: {}", e.client, e.server, proto_name(e.protocol), e.what));
        let mut v = vec![
            ("talkers", json!(st.talkers.len())),
            ("devices", json!(devices.len())),
            ("last_event", json!(last.unwrap_or_default())),
        ];
        for k in ["events", "writes", "program_changes", "control_actions", "logins", "errors", "new_talkers"] {
            v.push((k, json!(st.totals.get(k).copied().unwrap_or(0))));
        }
        v
    }
}

pub fn proto_name(p: Proto) -> &'static str {
    match p {
        Proto::Modbus => "Modbus",
        Proto::Enip => "EtherNet/IP",
        Proto::S7 => "S7",
        Proto::Opcua => "OPC UA",
        Proto::Mqtt => "MQTT",
    }
}

fn publish(ctx: &Ctx, m: &Monitor) {
    let wanted: Vec<&str> = if ctx.def.tags.is_empty() {
        COUNTERS.iter().map(|c| c.0).collect()
    } else {
        ctx.def.tags.iter().map(|t| t.name.as_str()).collect()
    };
    for (k, v) in m.counters() {
        if wanted.contains(&k) {
            let old = ctx.tags.value(&ctx.def.id, k);
            if old.as_ref() != Some(&v) {
                ctx.tags.set(&ctx.def.id, k, v, Quality::Good, Origin::Service);
            }
        }
    }
}

pub async fn run(ctx: &mut Ctx) -> Result<()> {
    let s: Settings = ctx.settings()?;
    let mut m = Monitor::new(&s);
    MONITORS.insert(ctx.def.id.clone(), m.state.clone());
    let id = ctx.def.id.clone();
    let _cleanup = scopeguard(move || {
        MONITORS.remove(&id);
    });
    publish(ctx, &m);

    if let Some(file) = &s.file {
        let path = std::path::Path::new(file);
        let path = if path.is_relative() { ctx.dir.join(path) } else { path.to_path_buf() };
        let frames = tokio::task::spawn_blocking(move || packet::read_file(&path)).await??;
        let n = frames.len();
        for f in &frames {
            m.frame(f);
        }
        publish(ctx, &m);
        let st = m.state.lock();
        ctx.status.running(format!(
            "replayed {n} frames from {file}: {} talkers, {} events",
            st.talkers.len(),
            st.events.len()
        ));
        drop(st);
        crate::services::idle_until_cancelled(ctx).await;
        return Ok(());
    }

    let Some(iface) = s.interface.clone() else {
        bail!("give the monitor an `interface` to capture on, or a capture `file` to replay");
    };
    live(ctx, &mut m, &s, &iface).await
}

#[cfg(feature = "live-capture")]
async fn live(ctx: &mut Ctx, m: &mut Monitor, s: &Settings, iface: &str) -> Result<()> {
    use anyhow::Context as _;
    let ports: Vec<String> = s.port_map().keys().map(|p| format!("tcp port {p}")).collect();
    // ARP too: it shows which network card answers for which address.
    let mut filter = format!("({} or arp)", ports.join(" or "));
    if let Some(extra) = &s.filter {
        filter = format!("{filter} and ({extra})");
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(4096);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (iface2, stop2) = (iface.to_string(), stop.clone());
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
    std::thread::spawn(move || {
        let cap = pcap::Capture::from_device(iface2.as_str())
            .and_then(|c| c.promisc(true).snaplen(65535).timeout(500).immediate_mode(true).open());
        let mut cap = match cap {
            Ok(c) => c,
            Err(e) => {
                let _ = ready_tx.send(Err(e.to_string()));
                return;
            }
        };
        if let Err(e) = cap.filter(&filter, true) {
            let _ = ready_tx.send(Err(format!("filter `{filter}`: {e}")));
            return;
        }
        let Some(link) = Link::from_dlt(cap.get_datalink().0) else {
            let _ = ready_tx.send(Err(format!("unsupported link type {:?}", cap.get_datalink())));
            return;
        };
        let _ = ready_tx.send(Ok(()));
        while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
            match cap.next_packet() {
                Ok(p) => {
                    let ts = DateTime::from_timestamp(p.header.ts.tv_sec as i64, p.header.ts.tv_usec as u32 * 1000)
                        .unwrap_or_else(Utc::now);
                    if tx.blocking_send(Frame { ts, link, data: p.data.to_vec() }).is_err() {
                        return;
                    }
                }
                Err(pcap::Error::TimeoutExpired) => {}
                Err(_) => return,
            }
        }
    });
    ready_rx
        .await
        .map_err(|_| anyhow::anyhow!("capture thread stopped"))?
        .map_err(|e| anyhow::anyhow!(e))
        .with_context(|| format!("can't capture on {iface} (in Docker: --user 0 --cap-add NET_RAW --cap-add NET_ADMIN --network host)"))?;
    ctx.status.running(format!("watching {iface}"));
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let result = loop {
        tokio::select! {
            _ = ctx.cancel.cancelled() => break Ok(()),
            f = rx.recv() => match f {
                Some(f) => { m.frame(&f); }
                None => break Err(anyhow::anyhow!("capture on {iface} stopped")),
            },
            _ = tick.tick() => publish(ctx, m),
            Some(w) = ctx.writes.recv() => {
                let _ = w.reply.send(Err("monitor counters are read-only".into()));
            }
        }
    };
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    result
}

#[cfg(not(feature = "live-capture"))]
async fn live(_ctx: &mut Ctx, _m: &mut Monitor, _s: &Settings, iface: &str) -> Result<()> {
    bail!("this build has no live capture (built without `live-capture`): can't watch {iface}; replay a `file` instead")
}

struct Guard<F: FnMut()>(F);
impl<F: FnMut()> Drop for Guard<F> {
    fn drop(&mut self) {
        (self.0)()
    }
}
fn scopeguard<F: FnMut()>(f: F) -> Guard<F> {
    Guard(f)
}

/// Check settings without running the service.
pub fn check_settings(mode: crate::model::Mode, v: &Value) -> Result<(), String> {
    if mode != crate::model::Mode::Client {
        return Err("a monitor only listens: use mode `client`".into());
    }
    let s: Settings = serde_json::from_value(v.clone()).map_err(|e| format!("settings: {e}"))?;
    if s.interface.is_none() && s.file.is_none() {
        return Err("settings: give an `interface` to capture on, or a capture `file` to replay".into());
    }
    if s.interface.is_some() && s.file.is_some() {
        return Err("settings: use `interface` or `file`, not both".into());
    }
    Ok(())
}

// ------------------------------------------------------------------ recording proxy

/// A TCP proxy that writes everything passing through it to a pcap file as raw-IP
/// frames. It records real protocol traffic without packet-capture rights (tests).
#[cfg(test)]
pub mod tap {
    use super::packet::ipv4_tcp_frame;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub struct Recorder {
        frames: parking_lot::Mutex<Vec<(std::time::Duration, Vec<u8>)>>,
    }

    impl Recorder {
        fn add(&self, f: Vec<u8>) {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            self.frames.lock().push((now, f));
        }

        /// Write the recording as a pcap file with raw-IP link type.
        pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
            use pcap_file::pcap::{PcapHeader, PcapPacket, PcapWriter};
            let header = PcapHeader { datalink: pcap_file::DataLink::RAW, ..Default::default() };
            let mut w = PcapWriter::with_header(std::fs::File::create(path)?, header)?;
            for (ts, f) in self.frames.lock().iter() {
                w.write_packet(&PcapPacket::new(*ts, f.len() as u32, &f[..])?)?;
            }
            Ok(())
        }
    }

    /// Listen on `listen` and forward to `target`, recording both directions as if the
    /// client were `client_ip` and the server `server` (addresses only label the capture).
    pub async fn start(target: SocketAddr, server: SocketAddr, client_ip: [u8; 4]) -> anyhow::Result<(SocketAddr, Arc<Recorder>)> {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = l.local_addr()?;
        let rec = Arc::new(Recorder { frames: Default::default() });
        let r2 = rec.clone();
        tokio::spawn(async move {
            let mut port = 40000u16;
            while let Ok((mut c, _)) = l.accept().await {
                port += 1;
                let client = SocketAddr::from((client_ip, port));
                let Ok(mut s) = tokio::net::TcpStream::connect(target).await else { continue };
                let rec = r2.clone();
                tokio::spawn(async move {
                    let (mut cseq, mut sseq) = (1000u32, 9000u32);
                    rec.add(ipv4_tcp_frame(client, server, cseq - 1, 0x02, &[]));
                    rec.add(ipv4_tcp_frame(server, client, sseq - 1, 0x12, &[]));
                    let (mut cb, mut sb) = (vec![0u8; 65536], vec![0u8; 65536]);
                    loop {
                        tokio::select! {
                            n = c.read(&mut cb) => {
                                let Ok(n) = n else { break };
                                if n == 0 { break }
                                rec.add(ipv4_tcp_frame(client, server, cseq, 0x18, &cb[..n]));
                                cseq = cseq.wrapping_add(n as u32);
                                if s.write_all(&cb[..n]).await.is_err() { break }
                            }
                            n = s.read(&mut sb) => {
                                let Ok(n) = n else { break };
                                if n == 0 { break }
                                rec.add(ipv4_tcp_frame(server, client, sseq, 0x18, &sb[..n]));
                                sseq = sseq.wrapping_add(n as u32);
                                if c.write_all(&sb[..n]).await.is_err() { break }
                            }
                        }
                    }
                    rec.add(ipv4_tcp_frame(client, server, cseq, 0x11, &[]));
                });
            }
        });
        Ok((addr, rec))
    }
}

#[cfg(test)]
mod tests {
    use super::packet::{Frame, Link, ipv4_tcp_frame};
    use super::*;
    use chrono::TimeDelta;

    fn eth(src_mac: [u8; 6], ip_frame: &[u8]) -> Vec<u8> {
        let mut f = vec![0xff; 6];
        f.extend_from_slice(&src_mac);
        f.extend_from_slice(&[0x08, 0x00]);
        f.extend_from_slice(ip_frame);
        f
    }

    #[test]
    fn network_health_from_traffic() {
        let s: Settings = serde_json::from_value(json!({"file": "x", "learn_minutes": 0})).unwrap();
        let mut m = Monitor::new(&s);
        let hmi: SocketAddr = "10.0.0.5:50000".parse().unwrap();
        let plc: SocketAddr = "10.0.0.10:502".parse().unwrap();
        let mac_a = [2, 0, 0, 0, 0, 1];
        let t0 = Utc::now() - TimeDelta::minutes(30);
        let frame = |ts, mac, data: Vec<u8>| Frame { ts, link: Link::Ethernet, data: eth(mac, &data) };
        let req = |tid: u16| { let t = tid.to_be_bytes(); [t[0], t[1], 0, 0, 0, 6, 1, 3, 0, 0, 0, 2] };
        let resp = |tid: u16| { let t = tid.to_be_bytes(); [t[0], t[1], 0, 0, 0, 7, 1, 3, 4, 0, 1, 0, 2] };
        let (mut cseq, mut sseq) = (1000u32, 5000u32);
        m.frame(&frame(t0, mac_a, ipv4_tcp_frame(hmi, plc, cseq - 1, 0x02, &[])));
        let mut ts = t0;
        for i in 0..120 {
            ts += TimeDelta::seconds(1);
            let q = req(i as u16);
            m.frame(&frame(ts, mac_a, ipv4_tcp_frame(hmi, plc, cseq, 0x18, &q)));
            // Every 10th request is sent twice (a retransmission).
            if i % 10 == 0 {
                m.frame(&frame(ts, mac_a, ipv4_tcp_frame(hmi, plc, cseq, 0x18, &q)));
            }
            cseq += q.len() as u32;
            // Every 20th gets no answer; the rest are answered after 300 ms.
            if i % 20 != 19 {
                let r = resp(i as u16);
                m.frame(&frame(ts + TimeDelta::milliseconds(300), [2, 0, 0, 0, 0, 2], ipv4_tcp_frame(plc, hmi, sseq, 0x18, &r)));
                sseq += r.len() as u32;
            }
        }
        // The same IP from another network card.
        m.frame(&frame(ts, [2, 0, 0, 0, 0, 9], ipv4_tcp_frame("10.0.0.5:50001".parse().unwrap(), plc, 1, 0x02, &[])));

        let st = m.state.lock();
        let t = st.talkers.values().next().unwrap();
        assert_eq!(t.answered, 114, "{:?}", (t.answered, t.timeouts));
        assert!(t.timeouts >= 5, "{}", t.timeouts);
        assert_eq!(t.retransmits, 12);
        assert!((t.latency_p50_ms.unwrap() - 300.0).abs() < 1.0);
        assert_eq!(st.conflicts.len(), 1, "{:?}", st.conflicts);
        assert_eq!(st.conflicts[0].ip, "10.0.0.5");
        let mons = vec![("mon".to_string(), st.talkers.values().cloned().collect::<Vec<_>>(), st.conflicts.clone())];
        drop(st);

        let cfg = crate::model::Config::default();
        let (tags, svcs, states, names) = (HashMap::new(), HashMap::new(), HashMap::new(), HashMap::new());
        let snap = crate::insights::Snapshot {
            now: Utc::now(), cfg: &cfg, tags: &tags, services: &svcs, alarm_log: &[], alarm_states: &states,
            alarm_names: &names, monitors: &mons, alarm_defs: &[], pairs: &[], changes: &[], monitor_events: &[],
        };
        let out = crate::insights::analyze(&snap, &crate::insights::Tuning::default());
        let ids: Vec<&str> = out.iter().map(|i| i.id.as_str()).collect();
        for want in ["dupip:mon:10.0.0.5", "timeouts:mon:10.0.0.5>10.0.0.10:502", "slow:mon:10.0.0.5>10.0.0.10:502", "retrans:mon:10.0.0.5>10.0.0.10:502"] {
            assert!(ids.contains(&want), "{want}: {ids:?}");
        }
    }
}
