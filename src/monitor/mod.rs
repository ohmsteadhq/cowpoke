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
use decode::{Action, Obs, Proto};
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
    pub state: Arc<Mutex<State>>,
}

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
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// Feed one frame; returns the notable events it produced.
    pub fn frame(&mut self, f: &Frame) -> Vec<Event> {
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
        let stream = self.streams.push(&seg);
        let mut found = vec![];
        match decode::decode(proto, to_server, &stream.buf) {
            Some((obs, used)) => {
                stream.consume(used);
                found = obs;
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
        if fin {
            self.streams.close(&seg);
        }
        // A new connection is worth recording even before any PDU decodes.
        if seg.syn && to_server {
            found.insert(0, Obs { action: Action::Other, what: "new TCP connection".into() });
        }
        self.record(f.ts, proto, client, server, found)
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
        let t = st.talkers.entry(tk.clone()).or_insert_with(|| Talker {
            client: tk.0.clone(),
            server: tk.1.clone(),
            protocol: proto,
            first_seen: ts,
            last_seen: ts,
            counts: BTreeMap::new(),
            last: String::new(),
        });
        t.last_seen = ts;
        let mut events = vec![];
        let mut flagged_new = false;
        let mut seen: Vec<Action> = vec![];
        for o in found {
            seen.push(o.action);
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
    let mut filter = format!("({})", ports.join(" or "));
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
        .with_context(|| format!("can't capture on {iface} (in Docker: --cap-add NET_RAW --net host)"))?;
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
