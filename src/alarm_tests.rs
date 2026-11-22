//! The alarm engine end to end: a tag crosses a limit, hooks run, an operator acknowledges.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::alarms::{AlarmConfig, EventKind};
use crate::model::ServiceDef;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..200 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn raise_hooks_ack_clear() {
    let dir = tempfile::tempdir().unwrap();
    let app = crate::setup_hub(dir.path(), &dir.path().join("cowpoke.yaml")).await.unwrap();
    let svc: ServiceDef = serde_yaml::from_str(&format!(
        "id: tank\nkind: modbus\nmode: server\nsettings: {{ bind: 127.0.0.1, port: {} }}\ntags:\n  - {{ name: level, datatype: float64, register: 0, initial: 10, units: '%' }}\n  - {{ name: horn, datatype: bool, table: coil, register: 0 }}\n",
        free_port()
    ))
    .unwrap();
    app.cfg.upsert(None, svc).unwrap();
    app.reload().await;

    // A webhook receiver.
    let got: Arc<Mutex<Vec<Value>>> = Default::default();
    let sink = got.clone();
    let hook_port = free_port();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", hook_port)).await.unwrap();
    let recv = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |axum::Json(v): axum::Json<Value>| {
            let sink = sink.clone();
            async move {
                sink.lock().push(v);
                "ok"
            }
        }),
    );
    tokio::spawn(axum::serve(listener, recv).into_future());

    let panels = crate::setup_panels(&app).unwrap();
    let engine = panels.engine.clone();
    let cfg: AlarmConfig = serde_yaml::from_str(&format!(
        r#"
alarms:
  - {{ id: tank-high, name: Tank high, service: tank, tag: level, kind: high, limit: 50, deadband: 5, severity: high, hooks: [horn, notify] }}
hooks:
  - {{ id: horn, kind: write, service: tank, tag: horn, value: true, clear_value: false }}
  - id: notify
    kind: webhook
    url: "http://127.0.0.1:{hook_port}/hook"
    body: {{ text: "{{{{message}}}}", event: "{{{{event}}}}", value: "{{{{value}}}}" }}
    on: [raise, clear, ack]
"#
    ))
    .unwrap();
    engine.store.update(|c| { *c = cfg; Ok(()) }).unwrap();
    let def = app.cfg.service("tank").unwrap();

    // Raise.
    app.mgr.write(&def, "level", &json!(60)).await.unwrap();
    wait_for("the alarm to raise", || engine.states().get("tank-high").is_some_and(|s| s.active)).await;
    wait_for("the horn", || app.tags.value("tank", "horn") == Some(json!(true))).await;
    wait_for("the webhook", || !got.lock().is_empty()).await;
    assert_eq!(got.lock()[0], json!({"text": "Tank high: 60 % above 50 %", "event": "raise", "value": 60.0}));

    // Inside the deadband it stays raised.
    app.mgr.write(&def, "level", &json!(47)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(engine.states()["tank-high"].active);

    // Acknowledge, then clear: it leaves the list.
    assert!(engine.ack("tank-high", "ann"));
    assert!(!engine.ack("tank-high", "ann"));
    app.mgr.write(&def, "level", &json!(40)).await.unwrap();
    wait_for("the alarm to clear", || !engine.states()["tank-high"].shown()).await;
    wait_for("the horn off", || app.tags.value("tank", "horn") == Some(json!(false))).await;
    wait_for("three webhook calls", || got.lock().len() == 3).await;
    let events: Vec<Value> = got.lock().iter().map(|v| v["event"].clone()).collect();
    assert_eq!(events, vec![json!("raise"), json!("ack"), json!("clear")]);

    let log: Vec<EventKind> = engine.log_tail(10).into_iter().rev().map(|e| e.event).collect();
    assert_eq!(log, vec![EventKind::Raise, EventKind::Ack, EventKind::Clear]);
    let hook_results = engine.hook_results();
    assert!(hook_results["notify"].ok && hook_results["horn"].ok);
    app.mgr.shutdown().await;
}
