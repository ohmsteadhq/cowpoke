//! End-to-end tests: for each protocol, a Cowpoke client service talks to a Cowpoke
//! server service over real sockets.

use std::time::Duration;

use serde_json::{Value, json};

use crate::manager::{Manager, State};
use crate::model::{Config, ServiceDef};
use crate::tags::{Quality, TagStore};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn svc(yaml: &str) -> ServiceDef {
    let s: ServiceDef = serde_yaml::from_str(yaml).unwrap();
    s.validate().unwrap();
    s
}

struct Rig {
    mgr: std::sync::Arc<Manager>,
    tags: TagStore,
    cfg: Config,
    _dir: tempfile::TempDir,
}

impl Rig {
    async fn start(services: Vec<ServiceDef>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let tags = TagStore::new();
        let mgr = Manager::new(tags.clone(), dir.path().to_path_buf());
        let cfg = Config { services };
        mgr.apply(&cfg).await;
        Self { mgr, tags, cfg, _dir: dir }
    }

    fn def(&self, id: &str) -> ServiceDef {
        self.cfg.services.iter().find(|s| s.id == id).unwrap().clone()
    }

    async fn wait_running(&self, id: &str) {
        // OPC UA servers generate an RSA key on first start: slow in debug builds under load.
        for _ in 0..600 {
            let st = self.mgr.status(id);
            if st.state == State::Running {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{id} never started: {:?}", self.mgr.status(id));
    }

    /// Wait until a tag has a good value equal to `want`.
    async fn wait_value(&self, id: &str, tag: &str, want: Value) {
        for _ in 0..200 {
            if let Some(s) = self.tags.get(id, tag) {
                if s.quality == Quality::Good && s.value == want {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "{id}.{tag}: wanted {want}, have {:?} (status {:?})",
            self.tags.get(id, tag),
            self.mgr.status(id)
        );
    }

    async fn write(&self, id: &str, tag: &str, v: Value) {
        self.mgr.write(&self.def(id), tag, &v).await.unwrap();
    }

    async fn stop(self) {
        self.mgr.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn modbus_client_server() {
    let p = free_port();
    let rig = Rig::start(vec![
        svc(&format!(
            r#"
id: srv
kind: modbus
mode: server
settings: {{ bind: 127.0.0.1, port: {p}, word_order: little }}
tags:
  - {{ name: level, datatype: float32, register: 0, initial: 42.5 }}
  - {{ name: run, datatype: bool, table: coil, register: 3 }}
  - {{ name: temp, datatype: int16, table: input_register, register: 7, scale: 0.1, initial: 21.5, unit: 2 }}
  - {{ name: label, datatype: string, register: 20, length: 4, initial: "Cow1" }}
"#
        )),
        svc(&format!(
            r#"
id: cli
kind: modbus
mode: client
settings: {{ host: 127.0.0.1, port: {p}, poll_ms: 100, word_order: little }}
tags:
  - {{ name: level, datatype: float32, register: 0 }}
  - {{ name: run, datatype: bool, table: coil, register: 3 }}
  - {{ name: temp, datatype: int16, table: input_register, register: 7, scale: 0.1, unit: 2 }}
  - {{ name: label, datatype: string, register: 20, length: 4 }}
"#
        )),
    ])
    .await;
    rig.wait_running("srv").await;
    rig.wait_value("cli", "level", json!(42.5)).await;
    rig.wait_value("cli", "temp", json!(21.5)).await;
    rig.wait_value("cli", "label", json!("Cow1")).await;

    // Client writes reach the server...
    rig.write("cli", "run", json!(true)).await;
    rig.wait_value("srv", "run", json!(true)).await;
    rig.write("cli", "level", json!(7.25)).await;
    rig.wait_value("srv", "level", json!(7.25)).await;
    // ...and values set on the server show up at the client.
    rig.write("srv", "temp", json!(-3.5)).await;
    rig.wait_value("cli", "temp", json!(-3.5)).await;
    rig.stop().await;
}

async fn opcua_pair(policy: &str, mode: &str) {
    let p = free_port();
    let host = opcua::crypto::X509Data::computer_hostnames()
        .into_iter()
        .next()
        .unwrap_or_else(|| "localhost".into());
    let rig = Rig::start(vec![
        svc(&format!(
            r#"
id: srv
kind: opcua
mode: server
settings:
  bind: "::"
  port: {p}
  hostname: {host}
  namespace_uri: "urn:cowpoke:test"
  trust_client_certs: true
  users: [{{ username: op, password: secret-pass }}]
tags:
  - {{ name: Temperature, datatype: float64, initial: 21.5 }}
  - {{ name: Running, datatype: bool }}
  - {{ name: Count, datatype: int32, initial: 7 }}
"#
        )),
    ])
    .await;
    rig.wait_running("srv").await;
    // The server's namespace index goes into the client's node ids.
    let detail = rig.mgr.status("srv").detail;
    let ns = detail
        .split("(ns=")
        .nth(1)
        .and_then(|x| x.split(')').next())
        .unwrap()
        .to_string();
    let creds = if policy == "none" { "" } else { "username: op\n  password: secret-pass" };
    let client = svc(&format!(
        r#"
id: cli
kind: opcua
mode: client
settings:
  url: "opc.tcp://{host}:{p}/"
  security_policy: {policy}
  security_mode: {mode}
  trust_server_certs: true
  poll_ms: 100
  {creds}
tags:
  - {{ name: temp, datatype: float64, node_id: "ns={ns};s=Temperature" }}
  - {{ name: running, datatype: bool, node_id: "ns={ns};s=Running" }}
  - {{ name: count, datatype: int32, node_id: "ns={ns};s=Count" }}
"#
    ));
    let mut cfg = rig.cfg.clone();
    cfg.services.push(client);
    rig.mgr.apply(&cfg).await;
    let rig = Rig { cfg, ..rig };

    rig.wait_value("cli", "temp", json!(21.5)).await;
    rig.wait_value("cli", "count", json!(7)).await;
    rig.write("cli", "running", json!(true)).await;
    rig.wait_value("srv", "Running", json!(true)).await;
    rig.write("srv", "Temperature", json!(30.25)).await;
    rig.wait_value("cli", "temp", json!(30.25)).await;
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn opcua_none() {
    opcua_pair("none", "none").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn opcua_basic256sha256_encrypted() {
    opcua_pair("basic256sha256", "sign_and_encrypt").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn opcua_aes256_signed() {
    opcua_pair("aes256_sha256_rsa_pss", "sign").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn api_client_server() {
    let p = free_port();
    let rig = Rig::start(vec![
        svc(&format!(
            r#"
id: srv
kind: api
mode: server
settings:
  bind: 127.0.0.1
  port: {p}
  routes:
    - method: GET
      path: /v1/current
      body: {{ main: {{ temp: "{{{{tags.temp}}}}" }}, ok: true }}
    - method: POST
      path: /v1/setpoint
      set: {{ setpoint: /value }}
      body: {{ accepted: true }}
tags:
  - {{ name: temp, datatype: float64, initial: 18.5 }}
  - {{ name: setpoint, datatype: float64, initial: 20 }}
"#
        )),
        svc(&format!(
            r#"
id: cli
kind: api
mode: client
settings:
  url: "http://127.0.0.1:{p}/v1/current"
  interval_ms: 100
  write:
    url: "http://127.0.0.1:{p}/v1/setpoint"
    method: POST
    body: {{ value: "{{{{value}}}}" }}
tags:
  - {{ name: temp, datatype: float64, pointer: /main/temp }}
  - {{ name: ok, datatype: bool, pointer: /ok }}
  - {{ name: setpoint, datatype: float64, pointer: /nope }}
"#
        )),
    ])
    .await;
    rig.wait_running("srv").await;
    rig.wait_value("cli", "temp", json!(18.5)).await;
    rig.wait_value("cli", "ok", json!(true)).await;
    rig.write("cli", "setpoint", json!(22.5)).await;
    rig.wait_value("srv", "setpoint", json!(22.5)).await;
    rig.write("srv", "temp", json!(25.0)).await;
    rig.wait_value("cli", "temp", json!(25.0)).await;

    // The tags endpoint too.
    let tags: Value = reqwest::get(format!("http://127.0.0.1:{p}/tags"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(tags["setpoint"], json!(22.5));
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn iolink_client_server() {
    let p = free_port();
    let rig = Rig::start(vec![
        svc(&format!(
            r#"
id: srv
kind: iolink
mode: server
settings:
  bind: 127.0.0.1
  port: {p}
  devices: [{{ port: 1, product_name: TV7105, pdin_bytes: 4 }}]
tags:
  - {{ name: temperature, datatype: int16, port: 1, bit_offset: 16, bit_length: 16, scale: 0.1, initial: 21.5 }}
  - {{ name: out2, datatype: bool, port: 1, bit_offset: 1, initial: true }}
  - {{ name: lamp, datatype: bool, port: 2, pd: out, bit_offset: 0 }}
"#
        )),
        svc(&format!(
            r#"
id: cli
kind: iolink
mode: client
settings: {{ host: 127.0.0.1, port: {p}, poll_ms: 100 }}
tags:
  - {{ name: temperature, datatype: int16, port: 1, bit_offset: 16, bit_length: 16, scale: 0.1 }}
  - {{ name: out2, datatype: bool, port: 1, bit_offset: 1 }}
  - {{ name: lamp, datatype: bool, port: 2, pd: out, bit_offset: 0 }}
"#
        )),
    ])
    .await;
    rig.wait_running("srv").await;
    rig.wait_value("cli", "temperature", json!(21.5)).await;
    rig.wait_value("cli", "out2", json!(true)).await;
    rig.write("cli", "lamp", json!(true)).await;
    rig.wait_value("srv", "lamp", json!(true)).await;
    rig.write("srv", "temperature", json!(-4.5)).await;
    rig.wait_value("cli", "temperature", json!(-4.5)).await;

    // The raw IoT Core answer, as a real client would see it.
    let r: Value = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{p}/"))
        .json(&json!({"code": "request", "cid": 9, "adr": "/iolinkmaster/port[1]/iolinkdevice/pdin/getdata"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r, json!({"cid": 9, "code": 200, "data": {"value": "FFD30002"}}));
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_values_reach_clients() {
    let p = free_port();
    let rig = Rig::start(vec![
        svc(&format!(
            r#"
id: srv
kind: modbus
mode: server
settings: {{ bind: 127.0.0.1, port: {p} }}
tags:
  - {{ name: count, datatype: uint16, register: 0, simulate: {{ kind: counter, min: 0, max: 1000, interval_ms: 100 }} }}
  - {{ name: wave, datatype: float32, register: 1, simulate: {{ kind: sine, min: 10, max: 20, period_ms: 2000, interval_ms: 100 }} }}
"#
        )),
        svc(&format!(
            r#"
id: cli
kind: modbus
mode: client
settings: {{ host: 127.0.0.1, port: {p}, poll_ms: 100 }}
tags:
  - {{ name: count, datatype: uint16, register: 0 }}
  - {{ name: wave, datatype: float32, register: 1 }}
"#
        )),
    ])
    .await;
    rig.wait_running("srv").await;
    let mut counts = std::collections::BTreeSet::new();
    let mut waves = std::collections::BTreeSet::new();
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(v) = rig.tags.value("cli", "count").and_then(|v| v.as_u64()) {
            counts.insert(v);
        }
        if let Some(v) = rig.tags.value("cli", "wave").and_then(|v| v.as_f64()) {
            assert!((10.0..=20.0).contains(&v), "wave out of range: {v}");
            waves.insert((v * 10.0) as i64);
        }
    }
    assert!(counts.len() >= 5, "the counter should climb at the client: {counts:?}");
    assert!(waves.len() >= 5, "the sine should move at the client: {waves:?}");
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mqtt_client_broker() {
    let p = free_port();
    let rig = Rig::start(vec![
        svc(&format!(
            r#"
id: srv
kind: mqtt
mode: server
settings: {{ bind: 127.0.0.1, port: {p}, prefix: plant/line1, username: cow, password: poke }}
tags:
  - {{ name: level, datatype: float64, initial: 42.5 }}
  - {{ name: run, datatype: bool }}
  - {{ name: label, datatype: string, topic: plant/label, initial: "Cow1" }}
  - {{ name: locked, datatype: int32, initial: 7, writable: false }}
"#
        )),
        svc(&format!(
            r#"
id: cli
kind: mqtt
mode: client
settings: {{ host: 127.0.0.1, port: {p}, qos: 1, username: cow, password: poke }}
tags:
  - {{ name: level, datatype: float64, topic: plant/line1/level }}
  - {{ name: run, datatype: bool, topic: plant/line1/run }}
  - {{ name: label, datatype: string, topic: plant/label }}
  - {{ name: locked, datatype: int32, topic: plant/line1/locked }}
  - {{ name: any_level, datatype: float64, topic: "plant/+/level", writable: false }}
"#
        )),
    ])
    .await;
    rig.wait_running("srv").await;
    rig.wait_running("cli").await;
    // Retained values arrive on subscribe...
    rig.wait_value("cli", "level", json!(42.5)).await;
    rig.wait_value("cli", "any_level", json!(42.5)).await;
    rig.wait_value("cli", "label", json!("Cow1")).await;
    // ...client writes reach the broker's tags through <topic>/set...
    rig.write("cli", "run", json!(true)).await;
    rig.wait_value("srv", "run", json!(true)).await;
    rig.wait_value("cli", "run", json!(true)).await;
    rig.write("cli", "label", json!("Cow2")).await;
    rig.wait_value("srv", "label", json!("Cow2")).await;
    // ...and values set on the server are published to the client.
    rig.write("srv", "level", json!(-3.5)).await;
    rig.wait_value("cli", "level", json!(-3.5)).await;
    // Read-only server tags ignore writes.
    rig.write("cli", "locked", json!(9)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.tags.value("srv", "locked"), Some(json!(7)));
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mqtt_bad_password_is_refused() {
    let p = free_port();
    let rig = Rig::start(vec![
        svc(&format!(
            "id: srv\nkind: mqtt\nmode: server\nsettings: {{ bind: 127.0.0.1, port: {p}, username: cow, password: poke }}\ntags: [{{ name: level, datatype: float64, initial: 1.0 }}]\n"
        )),
        svc(&format!(
            "id: cli\nkind: mqtt\nmode: client\nsettings: {{ host: 127.0.0.1, port: {p}, username: cow, password: nope }}\ntags: [{{ name: level, datatype: float64, topic: srv/level }}]\n"
        )),
    ])
    .await;
    rig.wait_running("srv").await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_ne!(rig.mgr.status("cli").state, State::Running);
    assert_ne!(rig.tags.get("cli", "level").map(|s| s.quality), Some(Quality::Good));
    rig.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn eip_client_logix_sim() {
    let p = free_port();
    let rig = Rig::start(vec![
        svc(&format!(
            r#"
id: srv
kind: eip
mode: server
settings: {{ bind: 127.0.0.1, port: {p} }}
tags:
  - {{ name: level, datatype: float32, cip_tag: Program:Main.Level, initial: 42.5 }}
  - {{ name: run, datatype: bool, cip_tag: Pump_Run }}
  - {{ name: count, datatype: int32, initial: -7 }}
  - {{ name: big, datatype: int64, initial: 9000000000 }}
  - {{ name: label, datatype: string, cip_tag: Batch.ID, initial: "B-0001" }}
  - {{ name: locked, datatype: int16, initial: 3, writable: false }}
"#
        )),
        svc(&format!(
            r#"
id: cli
kind: eip
mode: client
settings: {{ host: 127.0.0.1, port: {p}, slot: 2, poll_ms: 100 }}
tags:
  - {{ name: level, datatype: float32, cip_tag: program:main.level }}
  - {{ name: run, datatype: bool, cip_tag: Pump_Run }}
  - {{ name: count, datatype: int32, cip_tag: count }}
  - {{ name: big, datatype: int64, cip_tag: big }}
  - {{ name: label, datatype: string, cip_tag: Batch.ID }}
  - {{ name: locked, datatype: int16, cip_tag: locked }}
"#
        )),
        svc(&format!(
            r#"
id: direct
kind: eip
mode: client
settings: {{ host: 127.0.0.1, port: {p}, direct: true, poll_ms: 100 }}
tags:
  - {{ name: count, datatype: int32, cip_tag: count }}
  - {{ name: missing, datatype: int32, cip_tag: Nope }}
"#
        )),
    ])
    .await;
    rig.wait_running("srv").await;
    rig.wait_value("cli", "level", json!(42.5)).await;
    rig.wait_value("cli", "count", json!(-7)).await;
    rig.wait_value("cli", "big", json!(9_000_000_000i64)).await;
    rig.wait_value("cli", "label", json!("B-0001")).await;
    rig.wait_value("direct", "count", json!(-7)).await;

    rig.write("cli", "run", json!(true)).await;
    rig.wait_value("srv", "run", json!(true)).await;
    rig.write("cli", "level", json!(7.25)).await;
    rig.wait_value("srv", "level", json!(7.25)).await;
    rig.write("cli", "label", json!("B-0002")).await;
    rig.wait_value("srv", "label", json!("B-0002")).await;
    rig.write("srv", "count", json!(123)).await;
    rig.wait_value("cli", "count", json!(123)).await;

    // A read-only tag refuses writes; a missing tag reads bad.
    assert!(rig.mgr.write(&rig.def("cli"), "locked", &json!(4)).await.is_err());
    assert_eq!(rig.tags.value("srv", "locked"), Some(json!(3)));
    assert_ne!(rig.tags.get("direct", "missing").map(|s| s.quality), Some(Quality::Good));
    rig.stop().await;
}

/// A Sparkplug B edge node (played by the test) publishes through Cowpoke's broker;
/// a Cowpoke MQTT client in Sparkplug mode reads its metrics and writes with DCMD.
#[tokio::test(flavor = "multi_thread")]
async fn mqtt_sparkplug_b() {
    use crate::services::sparkplug as spb;
    use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};

    let p = free_port();
    let all = vec![
        svc(&format!(
            "id: broker\nkind: mqtt\nmode: server\nsettings: {{ bind: 127.0.0.1, port: {p}, retain: false }}\ntags: []\n"
        )),
        svc(&format!(
            r#"
id: cli
kind: mqtt
mode: client
settings: {{ host: 127.0.0.1, port: {p}, sparkplug: {{ group_id: Plant, edge_node: Line1, device: Tank1 }} }}
tags:
  - {{ name: level, datatype: float64, metric: Level }}
  - {{ name: temp, datatype: int16, metric: Temp }}
  - {{ name: run, datatype: bool, metric: Pump/Run }}
"#
        )),
    ];
    // Start the broker alone; the client comes once the edge node is listening.
    let mut rig = Rig::start(vec![all[0].clone()]).await;
    rig.wait_running("broker").await;

    // The edge node: answer a rebirth request with births, and record commands.
    let mut opts = MqttOptions::new("edge", "127.0.0.1", p);
    opts.set_keep_alive(Duration::from_secs(5));
    let (node, mut ev) = AsyncClient::new(opts, 32);
    let (cmd_tx, mut cmds) = tokio::sync::mpsc::unbounded_channel::<spb::Payload>();
    let births = {
        let node = node.clone();
        move || {
            let node = node.clone();
            async move {
                let ts = spb::now_ms();
                let nb = spb::encode(ts, Some(0), &[spb::encode_metric(Some("bdSeq"), None, spb::UINT64, &json!(0), ts).unwrap()]);
                node.publish("spBv1.0/Plant/NBIRTH/Line1", QoS::AtMostOnce, false, nb).await.unwrap();
                let db = spb::encode(ts, Some(1), &[
                    spb::encode_metric(Some("Level"), Some(1), spb::FLOAT, &json!(42.5), ts).unwrap(),
                    spb::encode_metric(Some("Temp"), Some(2), spb::INT16, &json!(-12), ts).unwrap(),
                    spb::encode_metric(Some("Pump/Run"), Some(3), spb::BOOLEAN, &json!(false), ts).unwrap(),
                ]);
                node.publish("spBv1.0/Plant/DBIRTH/Line1/Tank1", QoS::AtMostOnce, false, db).await.unwrap();
            }
        }
    };
    tokio::spawn(async move {
        while let Ok(e) = ev.poll().await {
            match e {
                Event::Incoming(Packet::ConnAck(_)) => {}
                Event::Incoming(Packet::Publish(m)) => {
                    let payload = spb::decode(&m.payload).unwrap();
                    if m.topic == "spBv1.0/Plant/NCMD/Line1" {
                        // Rebirth request.
                        births().await;
                    } else {
                        let _ = cmd_tx.send(payload);
                    }
                }
                _ => {}
            }
        }
    });
    node.subscribe("spBv1.0/Plant/NCMD/Line1", QoS::AtMostOnce).await.unwrap();
    node.subscribe("spBv1.0/Plant/DCMD/Line1/Tank1", QoS::AtMostOnce).await.unwrap();
    // Once the edge node is subscribed, start the client: its rebirth request brings the births.
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.cfg = Config { services: all };
    rig.mgr.apply(&rig.cfg).await;
    let rig2 = rig;

    rig2.wait_value("cli", "level", json!(42.5)).await;
    rig2.wait_value("cli", "temp", json!(-12)).await;
    rig2.wait_value("cli", "run", json!(false)).await;

    // DDATA by alias only.
    let ts = spb::now_ms();
    let dd = spb::encode(ts, Some(2), &[spb::encode_metric(None, Some(2), spb::INT16, &json!(7), ts).unwrap()]);
    node.publish("spBv1.0/Plant/DDATA/Line1/Tank1", QoS::AtMostOnce, false, dd).await.unwrap();
    rig2.wait_value("cli", "temp", json!(7)).await;

    // A write goes out as DCMD with the birth's type and alias.
    rig2.write("cli", "run", json!(true)).await;
    let cmd = tokio::time::timeout(Duration::from_secs(5), cmds.recv()).await.unwrap().unwrap();
    assert_eq!(cmd.metrics.len(), 1);
    assert_eq!(cmd.metrics[0].name.as_deref(), Some("Pump/Run"));
    assert_eq!(cmd.metrics[0].alias, Some(3));
    assert_eq!(cmd.metrics[0].value, Some(json!(true)));

    // A death certificate marks the tags bad.
    let death = spb::encode(spb::now_ms(), None, &[spb::encode_metric(Some("bdSeq"), None, spb::UINT64, &json!(0), ts).unwrap()]);
    node.publish("spBv1.0/Plant/NDEATH/Line1", QoS::AtMostOnce, false, death).await.unwrap();
    for _ in 0..100 {
        if rig2.tags.get("cli", "level").map(|s| s.quality) == Some(Quality::Bad) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(rig2.tags.get("cli", "level").map(|s| s.quality), Some(Quality::Bad));
    rig2.stop().await;
}
