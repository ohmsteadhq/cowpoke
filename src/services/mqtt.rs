//! MQTT: a client (subscribes to topics on a broker and picks tag values out of the
//! payloads; writes publish) and a server (an embedded MQTT 3.1.1 broker that publishes
//! every tag on change and takes writes on `<topic>/set`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use parking_lot::Mutex;
use rumqttc::{
    AsyncClient, ConnAck, ConnectReturnCode, Event, MqttOptions, Packet, PubAck, PubComp, PubRec,
    Publish, QoS, SubAck, SubscribeReasonCode, Transport, UnsubAck,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::manager::Ctx;
use crate::model::{Mode, ServiceDef, TagDef};
use crate::tags::{Origin, Quality, coerce_tag};

const MAX_PACKET: usize = 1 << 20;

fn qmin(a: QoS, b: QoS) -> QoS {
    if (a as u8) <= (b as u8) { a } else { b }
}

fn qos(n: u8) -> QoS {
    if n == 0 { QoS::AtMostOnce } else { QoS::AtLeastOnce }
}

/// A tag value as an MQTT payload: strings as plain text, everything else as JSON.
pub fn payload_of(v: &Value) -> Vec<u8> {
    match v {
        Value::String(s) => s.as_bytes().to_vec(),
        other => other.to_string().into_bytes(),
    }
}

/// Read a payload as JSON when it parses, else as text.
pub fn parse_payload(bytes: &[u8]) -> Value {
    let text = String::from_utf8_lossy(bytes);
    serde_json::from_str(text.trim()).unwrap_or_else(|_| Value::String(text.into_owned()))
}

/// The tag's value in a payload, following its `pointer` into JSON.
fn extract(t: &TagDef, payload: &[u8]) -> Result<Value, String> {
    let v = parse_payload(payload);
    let v = match &t.pointer {
        Some(p) if !p.is_empty() => v
            .pointer(p)
            .cloned()
            .ok_or_else(|| format!("no `{p}` in the payload"))?,
        _ => v,
    };
    coerce_tag(t, &v)
}

// ------------------------------------------------------------------ client

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientSettings {
    host: String,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    /// Connect with TLS (port 8883 unless set). Uses the system's roots, or `ca`.
    #[serde(default)]
    tls: bool,
    /// PEM CA certificate for a broker with its own CA.
    #[serde(default)]
    ca: Option<String>,
    /// QoS for subscriptions and writes: 0 or 1.
    #[serde(default)]
    qos: u8,
    /// Publish writes with the retain flag.
    #[serde(default)]
    retain: bool,
    /// Topic writes go to. Placeholders: `{{topic}}` (the tag's topic) and `{{tag}}`.
    #[serde(default = "default_write_topic")]
    write_topic: String,
    #[serde(default = "default_keep_alive")]
    keep_alive_s: u64,
    /// Read one Sparkplug B edge node (or device) instead of plain topics. Tags name a `metric`.
    #[serde(default)]
    sparkplug: Option<super::sparkplug::Config>,
}

fn default_write_topic() -> String {
    "{{topic}}/set".into()
}
fn default_keep_alive() -> u64 {
    30
}

fn write_topic(tpl: &str, t: &TagDef) -> String {
    tpl.replace("{{topic}}", t.topic.as_deref().unwrap_or(&t.name))
        .replace("{{tag}}", &t.name)
}

pub async fn client(ctx: &mut Ctx) -> Result<()> {
    let s: ClientSettings = ctx.settings()?;
    let id = ctx.def.id.clone();
    let tags = ctx.def.tags.clone();
    let port = s.port.unwrap_or(if s.tls { 8883 } else { 1883 });
    let client_id = s.client_id.clone().unwrap_or_else(|| format!("cowpoke-{id}"));
    let mut opts = MqttOptions::new(client_id, s.host.clone(), port);
    opts.set_keep_alive(Duration::from_secs(s.keep_alive_s.max(5)));
    opts.set_max_packet_size(MAX_PACKET, MAX_PACKET);
    if let Some(u) = &s.username {
        opts.set_credentials(u.clone(), s.password.clone().unwrap_or_default());
    }
    if s.tls {
        opts.set_transport(match &s.ca {
            Some(ca) => Transport::tls(ca.as_bytes().to_vec(), None, None),
            None => Transport::tls_with_default_config(),
        });
    }
    let (cli, mut events) = AsyncClient::new(opts, 64);
    let sp = s.sparkplug.clone();
    let mut births = super::sparkplug::Births::default();
    let metric = |t: &TagDef| t.metric.clone().unwrap_or_else(|| t.name.clone());
    let mut topics: Vec<String> = match &sp {
        Some(c) => c.subscriptions(),
        None => tags.iter().filter_map(|t| t.topic.clone()).collect(),
    };
    topics.sort();
    topics.dedup();
    let where_ = format!("{}{}:{port}", if s.tls { "mqtts://" } else { "mqtt://" }, s.host);
    ctx.status.degraded(format!("connecting to {where_}"));

    loop {
        tokio::select! {
            _ = ctx.cancel.cancelled() => {
                let _ = cli.try_disconnect();
                return Ok(());
            }
            ev = events.poll() => match ev {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    for t in &topics {
                        cli.subscribe(t.clone(), qos(s.qos)).await?;
                    }
                    if let Some(c) = sp.as_ref().filter(|c| c.rebirth) {
                        use super::sparkplug as spb;
                        let ts = spb::now_ms();
                        let m = spb::encode_metric(Some("Node Control/Rebirth"), None, spb::BOOLEAN, &Value::Bool(true), ts)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        cli.publish(c.ncmd(), QoS::AtMostOnce, false, spb::encode(ts, None, &[m])).await?;
                    }
                    ctx.status.running(format!("connected to {where_}, {} topic(s)", topics.len()));
                }
                Ok(Event::Incoming(Packet::Publish(p))) if sp.is_some() => {
                    use super::sparkplug::{Message, classify, decode};
                    let c = sp.as_ref().unwrap();
                    match classify(c, &p.topic) {
                        Message::Death => {
                            births.clear();
                            for t in &tags { ctx.tags.set_bad(&id, &t.name); }
                            ctx.status.degraded(format!("{where_}: edge node {} is offline (death certificate)", c.edge_node));
                        }
                        Message::Metrics { birth } => match decode(&p.payload) {
                            Ok(payload) => {
                                if birth {
                                    ctx.status.running(format!("connected to {where_}, Sparkplug {}/{}{}", c.group_id, c.edge_node,
                                        c.device.as_ref().map(|d| format!("/{d}")).unwrap_or_default()));
                                }
                                for (name, v) in births.resolve(&payload, birth) {
                                    for t in tags.iter().filter(|t| metric(t) == name) {
                                        match v.as_ref().map(|v| coerce_tag(t, v)) {
                                            Some(Ok(v)) => ctx.tags.set(&id, &t.name, v, Quality::Good, Origin::Service),
                                            _ => ctx.tags.set_bad(&id, &t.name),
                                        }
                                    }
                                }
                            }
                            Err(e) => tracing::debug!(service = %id, "bad Sparkplug payload on {}: {e}", p.topic),
                        },
                        Message::Other => {}
                    }
                }
                Ok(Event::Incoming(Packet::Publish(p))) => {
                    for t in &tags {
                        let Some(filter) = &t.topic else { continue };
                        if !rumqttc::matches(&p.topic, filter) { continue }
                        match extract(t, &p.payload) {
                            Ok(v) => ctx.tags.set(&id, &t.name, v, Quality::Good, Origin::Service),
                            Err(e) => {
                                tracing::debug!(service = %id, tag = %t.name, "bad payload: {e}");
                                ctx.tags.set_bad(&id, &t.name);
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    for t in &tags { ctx.tags.set_bad(&id, &t.name); }
                    ctx.status.degraded(format!("{where_}: {e}"));
                    // The event loop reconnects on the next poll.
                    tokio::select! {
                        _ = ctx.cancel.cancelled() => return Ok(()),
                        _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                    }
                }
            },
            Some(w) = ctx.writes.recv() => {
                let r = match ctx.def.tag(&w.tag) {
                    None => Err(format!("no tag `{}`", w.tag)),
                    Some(t) => match &sp {
                        None => cli
                            .publish(write_topic(&s.write_topic, t), qos(s.qos), s.retain, payload_of(&w.value))
                            .await
                            .map_err(|e| e.to_string()),
                        Some(c) => {
                            use super::sparkplug as spb;
                            let name = metric(t);
                            // Use the type (and alias) the birth announced; else the tag's own type.
                            let (dt, alias) = births.metrics.get(&name).copied()
                                .unwrap_or((spb::datatype_for(t.datatype), None));
                            let ts = spb::now_ms();
                            match spb::encode_metric(Some(&name), alias, dt, &w.value, ts) {
                                Err(e) => Err(e),
                                // Commands are QoS 0 and never retained (Sparkplug B spec).
                                Ok(m) => cli.publish(c.cmd(), QoS::AtMostOnce, false, spb::encode(ts, None, &[m]))
                                    .await
                                    .map_err(|e| e.to_string()),
                            }
                        }
                    },
                };
                let _ = w.reply.send(r);
            }
        }
    }
}

// ------------------------------------------------------------------ server (broker)

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerSettings {
    #[serde(default = "crate::services::any_host")]
    bind: String,
    #[serde(default = "default_broker_port")]
    port: u16,
    /// Tags publish to `<prefix>/<tag>` unless they set `topic`. Default: the service id.
    #[serde(default)]
    #[allow(dead_code)] // read by `server_topic`, which works from the raw settings
    prefix: Option<String>,
    /// Publish tag values retained, so new subscribers get the last value at once.
    #[serde(default = "yes")]
    retain: bool,
    /// Require these credentials from every client.
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    /// Also be a Sparkplug B edge node (or a device under one) on this broker:
    /// births, data on change, NDEATH, and writes and rebirths through NCMD/DCMD.
    #[serde(default)]
    sparkplug: Option<super::sparkplug::Config>,
}

fn default_broker_port() -> u16 {
    1883
}
fn yes() -> bool {
    true
}

/// The topic a server-mode tag publishes to.
pub fn server_topic(def: &ServiceDef, t: &TagDef) -> String {
    if let Some(topic) = &t.topic {
        return topic.clone();
    }
    let prefix = def
        .settings
        .get("prefix")
        .and_then(|p| p.as_str())
        .unwrap_or(&def.id)
        .trim_end_matches('/');
    format!("{prefix}/{}", t.name)
}

struct Session {
    filters: Vec<(String, QoS)>,
    tx: mpsc::UnboundedSender<Publish>,
}

#[derive(Default)]
struct Broker {
    sessions: Mutex<HashMap<u64, Session>>,
    retained: Mutex<HashMap<String, Publish>>,
}

impl Broker {
    fn route(&self, p: &Publish) {
        if p.retain {
            let mut r = self.retained.lock();
            if p.payload.is_empty() {
                r.remove(&p.topic);
            } else {
                r.insert(p.topic.clone(), p.clone());
            }
        }
        for s in self.sessions.lock().values() {
            let granted = s
                .filters
                .iter()
                .filter(|(f, _)| rumqttc::matches(&p.topic, f))
                .map(|(_, q)| *q)
                .max_by_key(|q| *q as u8);
            if let Some(q) = granted {
                let mut out = p.clone();
                out.qos = qmin(out.qos, q);
                out.retain = false;
                out.dup = false;
                let _ = s.tx.send(out);
            }
        }
    }
}

/// The broker's own Sparkplug B edge node: publishes the service's tags as metrics.
struct EdgeNode {
    cfg: super::sparkplug::Config,
    def: Arc<ServiceDef>,
    store: crate::tags::TagStore,
    broker: Arc<Broker>,
    /// Message sequence number, 0-255, reset by each NBIRTH.
    seq: Mutex<u8>,
    bd_seq: u64,
}

impl EdgeNode {
    fn metric<'a>(t: &'a TagDef) -> &'a str {
        t.metric.as_deref().unwrap_or(&t.name)
    }

    fn alias(&self, t: &TagDef) -> u64 {
        // Alias 0 is left out so a zero default never matches a tag.
        self.def.tags.iter().position(|x| x.name == t.name).unwrap_or(0) as u64 + 1
    }

    fn tag_metric(&self, t: &TagDef, v: &Value, with_name: bool, ts: u64) -> Option<Vec<u8>> {
        use super::sparkplug as spb;
        let dt = spb::datatype_for(t.datatype);
        let name = with_name.then(|| Self::metric(t));
        spb::encode_metric(name, Some(self.alias(t)), dt, v, ts)
            .or_else(|_| spb::encode_metric(name, Some(self.alias(t)), dt, &crate::tags::zero(t.datatype), ts))
            .ok()
    }

    fn publish(&self, kind: &str, device: bool, metrics: &[Vec<u8>], ts: u64, reset: bool) {
        let seq = {
            let mut s = self.seq.lock();
            if reset {
                *s = 0;
            }
            let n = *s;
            *s = s.wrapping_add(1);
            n
        };
        let payload = super::sparkplug::encode(ts, Some(seq as u64), metrics);
        self.broker.route(&Publish::new(self.cfg.topic(kind, device), QoS::AtMostOnce, payload));
    }

    /// NBIRTH (and DBIRTH for a device) with every tag's current value.
    fn births(&self) {
        use super::sparkplug as spb;
        let ts = spb::now_ms();
        let mut node = vec![
            spb::encode_metric(Some("bdSeq"), None, spb::UINT64, &json_u64(self.bd_seq), ts).unwrap(),
            spb::encode_metric(Some("Node Control/Rebirth"), None, spb::BOOLEAN, &Value::Bool(false), ts).unwrap(),
        ];
        let tags: Vec<Vec<u8>> = self
            .def
            .tags
            .iter()
            .filter_map(|t| {
                let v = self.store.value(&self.def.id, &t.name).unwrap_or(Value::Null);
                self.tag_metric(t, &v, true, ts)
            })
            .collect();
        if self.cfg.device.is_none() {
            node.extend(tags);
            self.publish("NBIRTH", false, &node, ts, true);
        } else {
            self.publish("NBIRTH", false, &node, ts, true);
            self.publish("DBIRTH", true, &tags, ts, false);
        }
    }

    fn data(&self, t: &TagDef, v: &Value) {
        let ts = super::sparkplug::now_ms();
        if let Some(m) = self.tag_metric(t, v, false, ts) {
            let device = self.cfg.device.is_some();
            self.publish(if device { "DDATA" } else { "NDATA" }, device, &[m], ts, false);
        }
    }

    fn death(&self) {
        use super::sparkplug as spb;
        let ts = spb::now_ms();
        let m = spb::encode_metric(Some("bdSeq"), None, spb::UINT64, &json_u64(self.bd_seq), ts).unwrap();
        // NDEATH carries no seq.
        let payload = spb::encode(ts, None, &[m]);
        self.broker.route(&Publish::new(self.cfg.topic("NDEATH", false), QoS::AtMostOnce, payload));
    }

    /// An NCMD or DCMD: a rebirth request, or writes to tags. Returns false if the topic isn't ours.
    fn command(&self, topic: &str, payload: &[u8]) -> bool {
        let node_cmd = topic == self.cfg.ncmd();
        if !node_cmd && topic != self.cfg.cmd() {
            return false;
        }
        let p = match super::sparkplug::decode(payload) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("bad Sparkplug command on {topic}: {e}");
                return true;
            }
        };
        for m in &p.metrics {
            if node_cmd && m.name.as_deref() == Some("Node Control/Rebirth") {
                if m.value == Some(Value::Bool(true)) {
                    self.births();
                }
                continue;
            }
            // Tag metrics live on the device when there is one, else on the node.
            if node_cmd != self.cfg.device.is_none() {
                continue;
            }
            let t = self.def.tags.iter().find(|t| match (&m.name, m.alias) {
                (Some(n), _) => Self::metric(t) == n,
                (None, Some(a)) => self.alias(t) == a,
                _ => false,
            });
            let (Some(t), Some(v)) = (t, &m.value) else { continue };
            if !t.writable {
                tracing::debug!(tag = %t.name, "Sparkplug write to a read-only tag ignored");
                continue;
            }
            match coerce_tag(t, v) {
                Ok(v) => self.store.set(&self.def.id, &t.name, v, Quality::Good, Origin::Service),
                Err(e) => tracing::debug!(tag = %t.name, "bad Sparkplug write: {e}"),
            }
        }
        true
    }
}

fn json_u64(n: u64) -> Value {
    Value::from(n)
}

pub async fn server(ctx: &mut Ctx) -> Result<()> {
    let s: ServerSettings = ctx.settings()?;
    let def = Arc::new(ctx.def.clone());
    let broker = Arc::new(Broker::default());
    let topics: HashMap<String, String> = def
        .tags
        .iter()
        .map(|t| (server_topic(&def, t), t.name.clone()))
        .collect();

    // Subscribe before publishing the first values so no change is missed.
    let mut events = ctx.tags.subscribe();
    for t in &def.tags {
        if let Some(v) = ctx.tags.value(&def.id, &t.name) {
            let mut p = Publish::new(server_topic(&def, t), QoS::AtMostOnce, payload_of(&v));
            p.retain = s.retain;
            broker.route(&p);
        }
    }

    let node = s.sparkplug.clone().map(|cfg| {
        // bdSeq goes up with every start, so hosts can tell births and deaths of each session apart.
        let path = ctx.dir.join("sparkplug-bdseq");
        let bd_seq = std::fs::read_to_string(&path).ok().and_then(|t| t.trim().parse::<u64>().ok()).map_or(0, |n| (n + 1) % 256);
        let _ = std::fs::create_dir_all(&ctx.dir);
        let _ = std::fs::write(&path, bd_seq.to_string());
        Arc::new(EdgeNode { cfg, def: def.clone(), store: ctx.tags.clone(), broker: broker.clone(), seq: Mutex::new(0), bd_seq })
    });
    if let Some(n) = &node {
        n.births();
    }

    let listener = crate::services::listen(&s.bind, s.port).await?;
    ctx.status.running(match &node {
        Some(n) => format!(
            "broker on {}, Sparkplug node {}/{}{}",
            listener.local_addr()?,
            n.cfg.group_id,
            n.cfg.edge_node,
            n.cfg.device.as_ref().map(|d| format!("/{d}")).unwrap_or_default()
        ),
        None => format!("broker on {}", listener.local_addr()?),
    });

    let auth = s.username.clone().map(|u| (u, s.password.clone().unwrap_or_default()));
    let accept = {
        let (broker, def, store, topics, node) = (broker.clone(), def.clone(), ctx.tags.clone(), topics.clone(), node.clone());
        async move {
            let mut next = 0u64;
            loop {
                let (sock, addr) = listener.accept().await?;
                next += 1;
                let conn = Conn {
                    id: next,
                    broker: broker.clone(),
                    def: def.clone(),
                    store: store.clone(),
                    topics: topics.clone(),
                    auth: auth.clone(),
                    node: node.clone(),
                };
                tokio::spawn(async move {
                    if let Err(e) = conn.run(sock).await {
                        tracing::debug!("mqtt client {addr}: {e:#}");
                    }
                });
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        }
    };

    let follow = {
        let (broker, def, node) = (broker.clone(), def.clone(), node.clone());
        let retain = s.retain;
        async move {
            loop {
                match events.recv().await {
                    Ok(e) if e.service == def.id => {
                        if let Some(t) = def.tag(&e.tag) {
                            let mut p = Publish::new(server_topic(&def, t), QoS::AtMostOnce, payload_of(&e.state.value));
                            p.retain = retain;
                            broker.route(&p);
                            if let Some(n) = &node {
                                n.data(t, &e.state.value);
                            }
                        }
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                }
            }
        }
    };

    tokio::select! {
        r = accept => r.context("mqtt broker stopped")?,
        _ = follow => {}
        _ = crate::services::idle_until_cancelled(ctx) => {}
    }
    if let Some(n) = &node {
        n.death();
    }
    broker.sessions.lock().clear();
    Ok(())
}

struct Conn {
    id: u64,
    broker: Arc<Broker>,
    def: Arc<ServiceDef>,
    store: crate::tags::TagStore,
    /// Server topic → tag name.
    topics: HashMap<String, String>,
    auth: Option<(String, String)>,
    node: Option<Arc<EdgeNode>>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.broker.sessions.lock().remove(&self.id);
    }
}

async fn send(sock: &mut tokio::net::TcpStream, p: Packet) -> Result<()> {
    let mut buf = bytes::BytesMut::new();
    p.write(&mut buf, MAX_PACKET)?;
    sock.write_all(&buf).await?;
    Ok(())
}

impl Conn {
    async fn run(self, mut sock: tokio::net::TcpStream) -> Result<()> {
        let mut buf = bytes::BytesMut::with_capacity(4096);
        let (tx, mut rx) = mpsc::unbounded_channel::<Publish>();
        let mut connected = false;
        let mut pkid: u16 = 0;
        loop {
            // Drain every complete packet already buffered.
            loop {
                let pkt = match Packet::read(&mut buf, MAX_PACKET) {
                    Ok(p) => p,
                    Err(rumqttc::Error::InsufficientBytes(_)) => break,
                    Err(e) => bail!("bad packet: {e:?}"),
                };
                match pkt {
                    Packet::Connect(c) => {
                        let ok = match &self.auth {
                            None => true,
                            Some((u, p)) => c.login.as_ref().is_some_and(|l| l.validate(u, p)),
                        };
                        let code = if ok { ConnectReturnCode::Success } else { ConnectReturnCode::BadUserNamePassword };
                        send(&mut sock, Packet::ConnAck(ConnAck::new(code, false))).await?;
                        if !ok {
                            return Ok(());
                        }
                        connected = true;
                        self.broker.sessions.lock().insert(self.id, Session { filters: vec![], tx: tx.clone() });
                    }
                    _ if !connected => bail!("first packet wasn't CONNECT"),
                    Packet::Publish(p) => {
                        match p.qos {
                            QoS::AtMostOnce => {}
                            QoS::AtLeastOnce => send(&mut sock, Packet::PubAck(PubAck::new(p.pkid))).await?,
                            QoS::ExactlyOnce => send(&mut sock, Packet::PubRec(PubRec::new(p.pkid))).await?,
                        }
                        self.incoming(p);
                    }
                    Packet::PubRel(r) => send(&mut sock, Packet::PubComp(PubComp::new(r.pkid))).await?,
                    Packet::Subscribe(sub) => {
                        let mut codes = vec![];
                        let mut retained = vec![];
                        {
                            let mut sessions = self.broker.sessions.lock();
                            let sess = sessions.get_mut(&self.id).expect("session");
                            for f in &sub.filters {
                                if !rumqttc::valid_filter(&f.path) {
                                    codes.push(SubscribeReasonCode::Failure);
                                    continue;
                                }
                                let q = qmin(f.qos, QoS::AtLeastOnce);
                                sess.filters.retain(|(p, _)| p != &f.path);
                                sess.filters.push((f.path.clone(), q));
                                codes.push(SubscribeReasonCode::Success(q));
                                for r in self.broker.retained.lock().values() {
                                    if rumqttc::matches(&r.topic, &f.path) {
                                        let mut r = r.clone();
                                        r.qos = qmin(r.qos, q);
                                        retained.push(r);
                                    }
                                }
                            }
                        }
                        send(&mut sock, Packet::SubAck(SubAck::new(sub.pkid, codes))).await?;
                        for mut r in retained {
                            if r.qos != QoS::AtMostOnce {
                                pkid = pkid % u16::MAX + 1;
                                r.pkid = pkid;
                            }
                            send(&mut sock, Packet::Publish(r)).await?;
                        }
                    }
                    Packet::Unsubscribe(u) => {
                        if let Some(s) = self.broker.sessions.lock().get_mut(&self.id) {
                            s.filters.retain(|(p, _)| !u.topics.contains(p));
                        }
                        send(&mut sock, Packet::UnsubAck(UnsubAck::new(u.pkid))).await?;
                    }
                    Packet::PingReq => send(&mut sock, Packet::PingResp).await?,
                    Packet::Disconnect => return Ok(()),
                    // Acks for what we sent: nothing is retried, so nothing to track.
                    _ => {}
                }
            }
            tokio::select! {
                n = sock.read_buf(&mut buf) => {
                    if n? == 0 { return Ok(()) }
                }
                Some(mut p) = rx.recv() => {
                    if p.qos != QoS::AtMostOnce {
                        pkid = pkid % u16::MAX + 1;
                        p.pkid = pkid;
                    }
                    send(&mut sock, Packet::Publish(p)).await?;
                }
            }
        }
    }

    /// A client published: `<tag topic>/set` writes the tag; everything is forwarded.
    fn incoming(&self, p: Publish) {
        if let Some(n) = &self.node {
            if n.command(&p.topic, &p.payload) {
                // Still forwarded, so other hosts see the command.
                self.broker.route(&p);
                return;
            }
        }
        if let Some(base) = p.topic.strip_suffix("/set") {
            if let Some(t) = self.topics.get(base).and_then(|n| self.def.tag(n)) {
                if !t.writable {
                    tracing::debug!(tag = %t.name, "write to a read-only tag ignored");
                } else {
                    match extract(t, &p.payload) {
                        Ok(v) => self.store.set(&self.def.id, &t.name, v, Quality::Good, Origin::Service),
                        Err(e) => tracing::debug!(tag = %t.name, "bad write: {e}"),
                    }
                }
            }
        }
        // A client publishing on a tag's own topic doesn't change the tag: Cowpoke owns it.
        if self.topics.contains_key(&p.topic) {
            return;
        }
        self.broker.route(&p);
    }
}

/// Check settings without running the service.
pub fn check_settings(mode: Mode, v: &Value) -> Result<(), String> {
    let r = match mode {
        Mode::Server => serde_json::from_value::<ServerSettings>(v.clone()).and_then(|s| match &s.sparkplug {
            Some(c) => c.validate().map_err(serde::de::Error::custom),
            None => Ok(()),
        }),
        Mode::Client => serde_json::from_value::<ClientSettings>(v.clone()).and_then(|s| {
            if s.qos > 1 {
                return Err(serde::de::Error::custom("qos is 0 or 1"));
            }
            if let Some(c) = &s.sparkplug {
                c.validate().map_err(serde::de::Error::custom)?;
            }
            Ok(())
        }),
    };
    r.map_err(|e| format!("settings: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn payloads() {
        assert_eq!(parse_payload(b"42.5"), json!(42.5));
        assert_eq!(parse_payload(b"hello"), json!("hello"));
        assert_eq!(parse_payload(br#"{"t": 1}"#), json!({"t": 1}));
        assert_eq!(payload_of(&json!("hi")), b"hi");
        assert_eq!(payload_of(&json!(true)), b"true");
    }
}
