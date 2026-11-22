//! IO-Link through an ifm IoT Core master (JSON over HTTP).
//!
//! Client: reads each port's process data in (`pdin`) from a real master and decodes the
//! tags by IODD bit offset; writes go to `pdout`.
//! Server: simulates a master, answering the same JSON requests from the tag values.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use axum::body::Bytes;
use axum::http::{Method, Uri};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::codec::{decode_pd, encode_pd};
use crate::manager::Ctx;
use crate::model::{ProcessData, ServiceDef, TagDef};
use crate::tags::{Origin, Quality, TagStore};

fn pd_path(port: u8, pd: ProcessData) -> String {
    let dir = match pd {
        ProcessData::In => "pdin",
        ProcessData::Out => "pdout",
    };
    format!("/iolinkmaster/port[{port}]/iolinkdevice/{dir}")
}

/// Bytes needed to hold every tag of one port and direction.
fn pd_len(tags: &[&TagDef]) -> usize {
    tags.iter()
        .map(|t| {
            let len = t.bit_length.unwrap_or_else(|| t.datatype.bits()) as usize;
            (t.bit_offset.unwrap_or(0) as usize + len).div_ceil(8)
        })
        .max()
        .unwrap_or(0)
}

fn port_tags(tags: &[TagDef], port: u8, pd: ProcessData) -> Vec<&TagDef> {
    tags.iter()
        .filter(|t| t.port == Some(port) && t.pd() == pd)
        .collect()
}

// ------------------------------------------------------------------ client

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientSettings {
    host: String,
    #[serde(default = "http_port")]
    port: u16,
    #[serde(default = "default_poll")]
    poll_ms: u64,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

fn http_port() -> u16 {
    80
}
fn default_poll() -> u64 {
    1000
}
fn default_timeout() -> u64 {
    3000
}

struct Master {
    http: reqwest::Client,
    url: String,
    cid: i64,
}

impl Master {
    async fn request(&mut self, adr: &str, data: Option<Value>) -> Result<Value> {
        self.cid = (self.cid + 1) % 1_000_000;
        let mut body = json!({"code": "request", "cid": self.cid, "adr": adr});
        if let Some(d) = data {
            body["data"] = d;
        }
        let resp: Value = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("{} unreachable", self.url))?
            .json()
            .await
            .context("master answered with something other than JSON")?;
        match resp.get("code").and_then(|c| c.as_i64()) {
            Some(200) => Ok(resp.get("data").cloned().unwrap_or(Value::Null)),
            Some(c) => Err(anyhow!("master answered {c} for {adr}")),
            None => Err(anyhow!("master answer has no code")),
        }
    }

    async fn get_pd(&mut self, port: u8, pd: ProcessData) -> Result<Vec<u8>> {
        let d = self.request(&format!("{}/getdata", pd_path(port, pd)), None).await?;
        let hex_str = d.get("value").and_then(|v| v.as_str()).unwrap_or("");
        hex::decode(hex_str).map_err(|_| anyhow!("bad process data `{hex_str}`"))
    }
}

pub async fn client(ctx: &mut Ctx) -> Result<()> {
    let s: ClientSettings = ctx.settings()?;
    let mut m = Master {
        http: reqwest::Client::builder()
            .timeout(Duration::from_millis(s.timeout_ms))
            .build()?,
        url: format!("http://{}:{}/", s.host, s.port),
        cid: 0,
    };
    // Check the master answers before calling it running.
    let name = m
        .request("/deviceinfo/productname/getdata", None)
        .await
        .map(|d| d.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string())
        .with_context(|| format!("no IoT Core master at {}", m.url))?;
    ctx.status.running(format!("{name} at {}", m.url));

    let tags = ctx.def.tags.clone();
    let ports: BTreeSet<u8> = tags
        .iter()
        .filter(|t| t.pd() == ProcessData::In)
        .filter_map(|t| t.port)
        .collect();
    let id = ctx.def.id.clone();
    let mut tick = tokio::time::interval(Duration::from_millis(s.poll_ms.max(50)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ctx.cancel.cancelled() => return Ok(()),
            _ = tick.tick() => {
                let mut transport_err = None;
                for &p in &ports {
                    match m.get_pd(p, ProcessData::In).await {
                        Ok(pd) => for t in port_tags(&tags, p, ProcessData::In) {
                            ctx.tags.set(&id, &t.name, decode_pd(t, &pd), Quality::Good, Origin::Service);
                        },
                        Err(e) => {
                            for t in port_tags(&tags, p, ProcessData::In) { ctx.tags.set_bad(&id, &t.name); }
                            if e.to_string().contains("unreachable") { transport_err = Some(e); break; }
                        }
                    }
                }
                if let Some(e) = transport_err { return Err(e); }
            }
            Some(w) = ctx.writes.recv() => {
                let r = async {
                    let t = tags.iter().find(|t| t.name == w.tag).ok_or_else(|| anyhow!("no tag `{}`", w.tag))?;
                    if t.pd() != ProcessData::Out {
                        bail!("`{}` is process data in (from the device); only pdout tags can be written", t.name);
                    }
                    let port = t.port.unwrap_or(1);
                    let mut pd = m.get_pd(port, ProcessData::Out).await.unwrap_or_default();
                    let need = pd_len(&port_tags(&tags, port, ProcessData::Out));
                    if pd.len() < need { pd.resize(need, 0); }
                    encode_pd(t, &w.value, &mut pd).map_err(|e| anyhow!(e))?;
                    m.request(&format!("{}/setdata", pd_path(port, ProcessData::Out)),
                        Some(json!({"newvalue": hex::encode_upper(&pd)}))).await?;
                    Ok(())
                }.await;
                if r.is_ok() {
                    ctx.tags.set(&id, &w.tag, w.value.clone(), Quality::Good, Origin::Service);
                }
                let _ = w.reply.send(r.map_err(|e| format!("{e:#}")));
            }
        }
    }
}

// ------------------------------------------------------------------ server

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct PortDevice {
    port: u8,
    #[serde(default)]
    product_name: Option<String>,
    #[serde(default)]
    vendor_id: Option<u32>,
    #[serde(default)]
    device_id: Option<u32>,
    #[serde(default)]
    serial_number: Option<String>,
    /// Process data in length in bytes (default: what the tags need).
    #[serde(default)]
    pdin_bytes: Option<usize>,
    #[serde(default)]
    pdout_bytes: Option<usize>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct ServerSettings {
    #[serde(default = "crate::services::any_host")]
    bind: String,
    #[serde(default = "sim_port")]
    port: u16,
    #[serde(default = "master_name")]
    product_name: String,
    #[serde(default = "port_count")]
    ports: u8,
    #[serde(default)]
    devices: Vec<PortDevice>,
}

fn sim_port() -> u16 {
    8080
}
fn master_name() -> String {
    "Cowpoke IO-Link master".into()
}
fn port_count() -> u8 {
    8
}

struct Sim {
    def: ServiceDef,
    s: ServerSettings,
    store: TagStore,
}

impl Sim {
    fn device(&self, port: u8) -> Option<PortDevice> {
        self.s.devices.iter().find(|d| d.port == port).cloned().or_else(|| {
            self.def
                .tags
                .iter()
                .any(|t| t.port == Some(port))
                .then(|| PortDevice {
                    port,
                    product_name: None,
                    vendor_id: None,
                    device_id: None,
                    serial_number: None,
                    pdin_bytes: None,
                    pdout_bytes: None,
                })
        })
    }

    fn pd_bytes(&self, port: u8, pd: ProcessData) -> Vec<u8> {
        let tags = port_tags(&self.def.tags, port, pd);
        let dev = self.device(port);
        let len = dev
            .and_then(|d| match pd {
                ProcessData::In => d.pdin_bytes,
                ProcessData::Out => d.pdout_bytes,
            })
            .unwrap_or_else(|| pd_len(&tags));
        let mut buf = vec![0u8; len];
        for t in tags {
            if let Some(v) = self.store.value(&self.def.id, &t.name) {
                let _ = encode_pd(t, &v, &mut buf);
            }
        }
        buf
    }

    /// Answer one IoT Core request. Returns (code, data).
    fn answer(&self, adr: &str, data: Option<&Value>) -> (u16, Value) {
        let adr = adr.trim_end_matches('/');
        if adr == "/getdatamulti" {
            let mut out = serde_json::Map::new();
            let list = data
                .and_then(|d| d.get("datatosend"))
                .and_then(|l| l.as_array())
                .cloned()
                .unwrap_or_default();
            for item in list {
                let Some(a) = item.as_str() else { continue };
                let (code, d) = self.answer(&format!("{a}/getdata"), None);
                let entry = if code == 200 {
                    json!({"code": 200, "data": d.get("value").cloned().unwrap_or(d)})
                } else {
                    json!({"code": code})
                };
                out.insert(a.to_string(), entry);
            }
            return (200, Value::Object(out));
        }
        let (path, op) = match adr.rsplit_once('/') {
            Some(x) => x,
            None => return (400, Value::Null),
        };
        match (path, op) {
            ("/deviceinfo/productname", "getdata") => (200, json!({"value": self.s.product_name})),
            ("/deviceinfo/vendor", "getdata") => (200, json!({"value": "Cowpoke"})),
            ("/deviceinfo/serialnumber", "getdata") => (200, json!({"value": format!("CP-{}", self.def.id)})),
            ("/iolinkmaster/numberofports", "getdata") => (200, json!({"value": self.s.ports})),
            _ => self.port_answer(path, op, data),
        }
    }

    fn port_answer(&self, path: &str, op: &str, data: Option<&Value>) -> (u16, Value) {
        let Some(rest) = path.strip_prefix("/iolinkmaster/port[") else {
            return (404, Value::Null);
        };
        let Some((n, rest)) = rest.split_once(']') else {
            return (400, Value::Null);
        };
        let Ok(port) = n.parse::<u8>() else {
            return (400, Value::Null);
        };
        if port == 0 || port > self.s.ports {
            return (404, Value::Null);
        }
        let dev = self.device(port);
        match (rest, op) {
            // 0 = deactivated, 2 = operate (a device is connected).
            ("/mode", "getdata") => (200, json!({"value": if dev.is_some() { 3 } else { 0 }})),
            ("/iolinkdevice/status", "getdata") => (200, json!({"value": if dev.is_some() { 2 } else { 0 }})),
            (_, _) if dev.is_none() && rest.starts_with("/iolinkdevice") => (503, Value::Null),
            ("/iolinkdevice/productname", "getdata") => (200, json!({"value": dev.unwrap().product_name.unwrap_or_else(|| format!("Simulated device {port}"))})),
            ("/iolinkdevice/vendorid", "getdata") => (200, json!({"value": dev.unwrap().vendor_id.unwrap_or(0)})),
            ("/iolinkdevice/deviceid", "getdata") => (200, json!({"value": dev.unwrap().device_id.unwrap_or(0)})),
            ("/iolinkdevice/serial", "getdata") => (200, json!({"value": dev.unwrap().serial_number.unwrap_or_default()})),
            ("/iolinkdevice/pdin", "getdata") => (200, json!({"value": hex::encode_upper(self.pd_bytes(port, ProcessData::In))})),
            ("/iolinkdevice/pdout", "getdata") => (200, json!({"value": hex::encode_upper(self.pd_bytes(port, ProcessData::Out))})),
            ("/iolinkdevice/pdout", "setdata") => {
                let Some(hexv) = data.and_then(|d| d.get("newvalue")).and_then(|v| v.as_str()) else {
                    return (400, Value::Null);
                };
                let Ok(bytes) = hex::decode(hexv) else {
                    return (400, Value::Null);
                };
                for t in port_tags(&self.def.tags, port, ProcessData::Out) {
                    self.store.set(&self.def.id, &t.name, decode_pd(t, &bytes), Quality::Good, Origin::Service);
                }
                (200, Value::Null)
            }
            _ => (404, Value::Null),
        }
    }
}

pub async fn server(ctx: &mut Ctx) -> Result<()> {
    let s: ServerSettings = ctx.settings()?;
    if let Some(t) = ctx.def.tags.iter().find(|t| t.port.is_some_and(|p| p == 0 || p > s.ports)) {
        bail!("tag `{}` is on port {}, but the master has {} ports", t.name, t.port.unwrap_or(0), s.ports);
    }
    let sim = Arc::new(Sim {
        def: ctx.def.clone(),
        s: s.clone(),
        store: ctx.tags.clone(),
    });
    let app = axum::Router::new().fallback(move |method: Method, uri: Uri, body: Bytes| {
        let sim = sim.clone();
        async move {
            // POST / with {"code":"request","cid":..,"adr":..}, or GET /<adr> as a shortcut.
            let (cid, adr, data) = if method == Method::POST {
                let Ok(req) = serde_json::from_slice::<Value>(&body) else {
                    return axum::Json(json!({"cid": -1, "code": 400})).into_response();
                };
                (
                    req.get("cid").cloned().unwrap_or(json!(-1)),
                    req.get("adr").and_then(|a| a.as_str()).unwrap_or("").to_string(),
                    req.get("data").cloned(),
                )
            } else {
                (json!(-1), uri.path().to_string(), None)
            };
            let (code, d) = sim.answer(&adr, data.as_ref());
            let mut out = json!({"cid": cid, "code": code});
            if !d.is_null() {
                out["data"] = d;
            }
            axum::Json(out).into_response()
        }
    });
    let listener = crate::services::listen(&s.bind, s.port).await?;
    let used: BTreeMap<u8, ()> = ctx.def.tags.iter().filter_map(|t| t.port).map(|p| (p, ())).collect();
    ctx.status.running(format!(
        "IoT Core on {}, ports in use: {}",
        listener.local_addr()?,
        used.keys().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
    ));
    let cancel = ctx.cancel.clone();
    tokio::select! {
        r = axum::serve(listener, app).with_graceful_shutdown(async move { cancel.cancelled().await }) => r?,
        _ = crate::services::idle_until_cancelled(ctx) => {}
    }
    Ok(())
}
