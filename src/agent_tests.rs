//! Hub and edge agent together, in one process: the hub serves on a free port and the
//! agent dials it, exactly as across a plant firewall.

use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::agent::edge::{self, Edge};
use crate::manager::State;
use crate::model::ServiceDef;
use crate::tags::{Quality, TagStore};
use crate::web::App;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..300 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

fn has(tags: &TagStore, svc: &str, tag: &str, want: &Value, q: Quality) -> bool {
    tags.get(svc, tag).is_some_and(|s| s.quality == q && (q == Quality::Bad || &s.value == want))
}

#[tokio::test(flavor = "multi_thread")]
async fn hub_and_agent() {
    let hub_dir = tempfile::tempdir().unwrap();
    let agent_dir = tempfile::tempdir().unwrap();
    let app: App = crate::setup_hub(hub_dir.path(), &hub_dir.path().join("cowpoke.yaml")).await.unwrap();
    let token = app.agents.create("plant-1", "Plant 1").unwrap();

    // Both services run on the agent: a simulated PLC, and a client reading it back.
    let mb = free_port();
    let services: Vec<ServiceDef> = serde_yaml::from_str(&format!(
        r#"
- id: srv
  kind: modbus
  mode: server
  agent: plant-1
  settings: {{ bind: 127.0.0.1, port: {mb} }}
  tags:
    - {{ name: level, datatype: float32, register: 0, initial: 42.5 }}
    - {{ name: run, datatype: bool, table: coil, register: 1 }}
- id: cli
  kind: modbus
  mode: client
  agent: plant-1
  settings: {{ host: 127.0.0.1, port: {mb}, poll_ms: 100 }}
  tags:
    - {{ name: level, datatype: float32, register: 0, history: {{ mode: deadband, deadband: 0 }} }}
    - {{ name: run, datatype: bool, table: coil, register: 1 }}
"#
    ))
    .unwrap();
    for s in services {
        app.cfg.upsert(None, s).unwrap();
    }
    app.reload().await;
    assert_eq!(app.mgr.status("srv").detail, "waiting for agent plant-1");

    let web = free_port();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", web)).await.unwrap();
    tokio::spawn(axum::serve(listener, crate::web::router(app.clone())).into_future());
    let hub_url = format!("http://127.0.0.1:{web}");

    // --- connect
    let cancel = CancellationToken::new();
    let agent = Edge::new(agent_dir.path()).unwrap();
    let run = tokio::spawn(edge::run(agent.clone(), hub_url.clone(), token.clone(), cancel.clone()));

    wait_for("hub to see the client's value", || has(&app.tags, "cli", "level", &json!(42.5), Quality::Good)).await;
    wait_for("agent statuses on the hub", || app.mgr.status("srv").state == State::Running).await;
    assert_eq!(app.agents.list(&app.cfg.get())[0]["online"], json!(true));

    // --- a write on the hub reaches the device through the agent
    app.agents.write("plant-1", "cli", "run", json!(true)).await.unwrap();
    wait_for("the write to reach the simulated PLC", || has(&app.tags, "srv", "run", &json!(true), Quality::Good)).await;
    app.agents.write("plant-1", "srv", "level", json!(7.25)).await.unwrap();
    wait_for("the new level at the client", || has(&app.tags, "cli", "level", &json!(7.25), Quality::Good)).await;

    // --- the link drops (new token: the agent can't get back in)
    let token2 = app.agents.rotate("plant-1").unwrap();
    wait_for("tags to go bad on the hub", || has(&app.tags, "cli", "level", &Value::Null, Quality::Bad)).await;
    assert_eq!(app.mgr.status("cli").state, State::Error);
    assert!(app.mgr.status("cli").detail.contains("offline"));

    // The plant keeps running: a value changes while the hub can't see it.
    let def = agent.service("srv").unwrap();
    agent.mgr.write(&def, "level", &json!(99.5)).await.unwrap();
    wait_for("the agent's client to read it", || has(&agent.tags, "cli", "level", &json!(99.5), Quality::Good)).await;
    tokio::time::sleep(Duration::from_millis(1200)).await; // let the agent's historian flush
    assert!(!app.hist.raw("cli", "level", 0, i64::MAX).iter().any(|p| p.1 == 99.5));
    let local = agent.hist.raw("cli", "level", 0, i64::MAX);
    assert!(local.iter().any(|p| p.1 == 99.5), "agent history: {local:?}");

    // --- restart the agent with the new token: it reconnects and backfills
    cancel.cancel();
    run.await.unwrap().unwrap();
    let cancel = CancellationToken::new();
    let agent = Edge::new(agent_dir.path()).unwrap();
    let run = tokio::spawn(edge::run(agent.clone(), hub_url, token2, cancel.clone()));
    wait_for("the backfilled point in the hub's historian", || {
        app.hist.raw("cli", "level", 0, i64::MAX).iter().any(|p| p.1 == 99.5)
    })
    .await;
    wait_for("live values again", || app.tags.get("cli", "level").is_some_and(|s| s.quality == Quality::Good)).await;

    cancel.cancel();
    run.await.unwrap().unwrap();
}

/// A pool of two workers: services spread over both, and move to the survivor when one stops.
#[tokio::test(flavor = "multi_thread")]
async fn pool_failover() {
    let hub_dir = tempfile::tempdir().unwrap();
    let app: App = crate::setup_hub(hub_dir.path(), &hub_dir.path().join("cowpoke.yaml")).await.unwrap();
    let token = app.agents.create_kind("workers", "Workers", true).unwrap();
    let (p1, p2) = (free_port(), free_port());
    let services: Vec<ServiceDef> = serde_yaml::from_str(&format!(
        r#"
- id: a
  kind: modbus
  mode: server
  agent: workers
  settings: {{ bind: 127.0.0.1, port: {p1} }}
  tags: [{{ name: x, datatype: uint16, register: 0, initial: 1 }}]
- id: b
  kind: modbus
  mode: server
  agent: workers
  settings: {{ bind: 127.0.0.1, port: {p2} }}
  tags: [{{ name: x, datatype: uint16, register: 0, initial: 2 }}]
"#
    ))
    .unwrap();
    for s in services {
        app.cfg.upsert(None, s).unwrap();
    }
    app.reload().await;

    let web = free_port();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", web)).await.unwrap();
    tokio::spawn(axum::serve(listener, crate::web::router(app.clone())).into_future());
    let hub_url = format!("http://127.0.0.1:{web}");

    let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (c1, c2) = (CancellationToken::new(), CancellationToken::new());
    let w1 = Edge::with_node(d1.path(), "w1".into()).unwrap();
    let w2 = Edge::with_node(d2.path(), "w2".into()).unwrap();
    let r1 = tokio::spawn(edge::run(w1.clone(), hub_url.clone(), token.clone(), c1.clone()));
    // A comma list: the first hub is down, the agent moves on to the second.
    let dead = format!("http://127.0.0.1:{}", free_port());
    let r2 = tokio::spawn(edge::run(w2.clone(), format!("{dead},{hub_url}"), token.clone(), c2.clone()));

    // The hub waits out its start-up grace, then spreads the two services over the two workers.
    wait_for_long("both services running", || {
        app.mgr.status("a").state == State::Running && app.mgr.status("b").state == State::Running
    })
    .await;
    let (pa, pb) = (app.agents.placed("a").unwrap(), app.agents.placed("b").unwrap());
    assert_ne!(pa, pb, "one service per worker");
    let pools = app.agents.list(&app.cfg.get());
    assert_eq!(pools[0]["members"].as_array().unwrap().len(), 2);

    // A write is routed to whichever worker runs the service.
    app.agents.write("workers", "a", "x", json!(11)).await.unwrap();
    wait_for("the write on the hub", || has(&app.tags, "a", "x", &json!(11), Quality::Good)).await;

    // Stop the worker running `a`: it moves to the other one.
    let (gone, stay) = if pa == "workers/w1" { (c1.clone(), "workers/w2") } else { (c2.clone(), "workers/w1") };
    gone.cancel();
    wait_for_long("a moved to the other worker", || {
        app.agents.placed("a").as_deref() == Some(stay) && app.mgr.status("a").state == State::Running
    })
    .await;
    assert_eq!(app.agents.placed("b").as_deref(), Some(stay));

    c1.cancel();
    c2.cancel();
    let _ = r1.await;
    let _ = r2.await;
}

async fn wait_for_long(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..600 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}
