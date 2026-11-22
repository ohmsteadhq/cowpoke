//! API request: a client (calls an HTTP endpoint on a timer and picks values out
//! of the JSON) and a server (mock routes with canned responses, plus the tags as JSON).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::manager::Ctx;
use crate::model::ServiceDef;
use crate::tags::{Origin, Quality, TagStore, coerce_tag};

// ------------------------------------------------------------------ templates

/// Replace `{{tags.NAME}}`, `{{value}}`, `{{tag}}` and `{{now}}` in text.
fn fill_text(tpl: &str, vars: &dyn Fn(&str) -> Option<Value>) -> String {
    let mut out = String::with_capacity(tpl.len());
    let mut rest = tpl;
    while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        match after.find("}}") {
            Some(j) => {
                let key = after[..j].trim();
                match vars(key) {
                    Some(Value::String(s)) => out.push_str(&s),
                    Some(v) => out.push_str(&v.to_string()),
                    None => out.push_str(&rest[i..i + 2 + j + 2]),
                }
                rest = &after[j + 2..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Fill a JSON body: a string leaf that is exactly one placeholder becomes the typed value.
fn fill_json(v: &Value, vars: &dyn Fn(&str) -> Option<Value>) -> Value {
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
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), fill_json(x, vars)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn tag_vars<'a>(
    service: &'a str,
    store: &'a TagStore,
    extra: Vec<(&'static str, Value)>,
) -> impl Fn(&str) -> Option<Value> + 'a {
    move |key: &str| {
        if let Some(name) = key.strip_prefix("tags.") {
            return store.value(service, name);
        }
        if key == "now" {
            return Some(json!(chrono::Utc::now().to_rfc3339()));
        }
        extra.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone())
    }
}

// ------------------------------------------------------------------ client

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    url: String,
    #[serde(default = "get")]
    method: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    /// Text or JSON. Placeholders: `{{tags.NAME}}`, `{{value}}`, `{{tag}}`, `{{now}}`.
    #[serde(default)]
    body: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientSettings {
    #[serde(flatten)]
    request: Request,
    #[serde(default = "default_interval")]
    interval_ms: u64,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
    /// The request to send when a tag is written. Without it, tags are read-only.
    #[serde(default)]
    write: Option<Request>,
}

fn get() -> String {
    "GET".into()
}
fn default_interval() -> u64 {
    5000
}
fn default_timeout() -> u64 {
    10_000
}

fn build(
    http: &reqwest::Client,
    r: &Request,
    vars: &dyn Fn(&str) -> Option<Value>,
) -> Result<reqwest::RequestBuilder> {
    let method = Method::from_bytes(r.method.to_uppercase().as_bytes())
        .map_err(|_| anyhow!("bad method `{}`", r.method))?;
    let url = fill_text(&r.url, vars);
    let mut rb = http.request(method, &url);
    for (k, v) in &r.headers {
        rb = rb.header(k, fill_text(v, vars));
    }
    match &r.body {
        None => {}
        Some(Value::String(s)) => rb = rb.body(fill_text(s, vars)),
        Some(v) => rb = rb.json(&fill_json(v, vars)),
    }
    Ok(rb)
}

pub async fn client(ctx: &mut Ctx) -> Result<()> {
    let s: ClientSettings = ctx.settings()?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_millis(s.timeout_ms))
        .user_agent(concat!("cowpoke/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let id = ctx.def.id.clone();
    let tags = ctx.def.tags.clone();
    let mut tick = tokio::time::interval(Duration::from_millis(s.interval_ms.max(100)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut failures = 0u32;
    ctx.status.running(format!("{} {}", s.request.method.to_uppercase(), s.request.url));

    loop {
        tokio::select! {
            _ = ctx.cancel.cancelled() => return Ok(()),
            _ = tick.tick() => {
                let vars = tag_vars(&id, &ctx.tags, vec![]);
                let res: Result<Value> = async {
                    let resp = build(&http, &s.request, &vars)?.send().await?;
                    let status = resp.status();
                    let text = resp.text().await?;
                    if !status.is_success() {
                        bail!("HTTP {status}: {}", text.chars().take(200).collect::<String>());
                    }
                    serde_json::from_str(&text).context("response isn't JSON")
                }.await;
                match res {
                    Ok(body) => {
                        if failures > 0 {
                            ctx.status.running(format!("{} {}", s.request.method.to_uppercase(), s.request.url));
                        }
                        failures = 0;
                        for t in &tags {
                            let ptr = t.pointer.clone().unwrap_or_else(|| format!("/{}", t.name));
                            match body.pointer(&ptr).map(|v| coerce_tag(t, v)) {
                                Some(Ok(v)) => ctx.tags.set(&id, &t.name, v, Quality::Good, Origin::Service),
                                _ => ctx.tags.set_bad(&id, &t.name),
                            }
                        }
                    }
                    Err(e) => {
                        failures += 1;
                        for t in &tags { ctx.tags.set_bad(&id, &t.name); }
                        // Keep polling; only the status shows the failure.
                        tracing::debug!(service = %id, "request failed: {e:#}");
                        ctx.status.degraded(format!("last request failed: {e:#}"));
                    }
                }
            }
            Some(w) = ctx.writes.recv() => {
                let r = match &s.write {
                    None => Err("this service has no `write` request, so its tags are read-only".to_string()),
                    Some(req) => {
                        let vars = tag_vars(&id, &ctx.tags, vec![("value", w.value.clone()), ("tag", json!(w.tag))]);
                        async {
                            let resp = build(&http, req, &vars).map_err(|e| e.to_string())?
                                .send().await.map_err(|e| e.to_string())?;
                            if resp.status().is_success() { Ok(()) } else { Err(format!("HTTP {}", resp.status())) }
                        }.await
                    }
                };
                if r.is_ok() {
                    ctx.tags.set(&id, &w.tag, w.value.clone(), Quality::Good, Origin::Service);
                }
                let _ = w.reply.send(r);
            }
        }
    }
}

// ------------------------------------------------------------------ server

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Route {
    #[serde(default = "get")]
    pub method: String,
    /// `/pumps/:id` style; `:name` matches one path segment.
    pub path: String,
    #[serde(default = "ok")]
    pub status: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Response body: text or JSON, with `{{tags.NAME}}`, `{{params.NAME}}`, `{{now}}` placeholders.
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default)]
    pub delay_ms: u64,
    /// Set tags from the request's JSON body: tag name → JSON pointer.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
}

fn ok() -> u16 {
    200
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ServerSettings {
    /// Own port, serving routes at `/`. Without it, routes are served by Cowpoke's
    /// web port under `/x/<service id>/`.
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default = "crate::services::any_host")]
    pub bind: String,
    /// Serve `GET /tags`, `GET /tags/NAME` and `PUT /tags/NAME`.
    #[serde(default = "yes")]
    pub expose_tags: bool,
    #[serde(default)]
    pub routes: Vec<Route>,
}

fn yes() -> bool {
    true
}

fn match_path(pattern: &str, path: &str) -> Option<Vec<(String, String)>> {
    let p: Vec<&str> = pattern.trim_matches('/').split('/').collect();
    let a: Vec<&str> = path.trim_matches('/').split('/').collect();
    if p.len() != a.len() {
        return None;
    }
    let mut params = vec![];
    for (x, y) in p.iter().zip(&a) {
        if let Some(name) = x.strip_prefix(':') {
            params.push((name.to_string(), y.to_string()));
        } else if x != y {
            return None;
        }
    }
    Some(params)
}

fn json_resp(status: StatusCode, v: Value) -> Response {
    (status, axum::Json(v)).into_response()
}

/// Answer one request to an API server-mode service. `path` is relative to the service's base.
pub async fn handle(
    def: &ServiceDef,
    store: &TagStore,
    method: &Method,
    path: &str,
    body: &Bytes,
) -> Response {
    let s: ServerSettings = match serde_json::from_value(def.settings.clone()) {
        Ok(s) => s,
        Err(e) => return json_resp(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
    };
    let path = if path.is_empty() { "/" } else { path };

    for r in &s.routes {
        if !r.method.eq_ignore_ascii_case(method.as_str()) {
            continue;
        }
        let Some(params) = match_path(&r.path, path) else {
            continue;
        };
        if !r.set.is_empty() {
            let Ok(input) = serde_json::from_slice::<Value>(body) else {
                return json_resp(StatusCode::BAD_REQUEST, json!({"error": "expected a JSON body"}));
            };
            for (tag, ptr) in &r.set {
                let (Some(t), Some(v)) = (def.tag(tag), input.pointer(ptr)) else {
                    continue;
                };
                match coerce_tag(t, v) {
                    Ok(v) => store.set(&def.id, tag, v, Quality::Good, Origin::Service),
                    Err(e) => return json_resp(StatusCode::BAD_REQUEST, json!({"error": format!("{tag}: {e}")})),
                }
            }
        }
        if r.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(r.delay_ms.min(60_000))).await;
        }
        let params2 = params.clone();
        let base = tag_vars(&def.id, store, vec![]);
        let vars = move |k: &str| {
            if let Some(n) = k.strip_prefix("params.") {
                return params2.iter().find(|(p, _)| p == n).map(|(_, v)| json!(v));
            }
            base(k)
        };
        let status = StatusCode::from_u16(r.status).unwrap_or(StatusCode::OK);
        let mut headers = HeaderMap::new();
        for (k, v) in &r.headers {
            if let (Ok(k), Ok(v)) = (
                HeaderName::from_bytes(k.as_bytes()),
                HeaderValue::from_str(&fill_text(v, &vars)),
            ) {
                headers.insert(k, v);
            }
        }
        return match &r.body {
            None => (status, headers).into_response(),
            Some(Value::String(t)) => (status, headers, fill_text(t, &vars)).into_response(),
            Some(v) => (status, headers, axum::Json(fill_json(v, &vars))).into_response(),
        };
    }

    if s.expose_tags {
        let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
        match (method.clone(), segs.as_slice()) {
            (Method::GET, ["tags"]) => {
                let m: Map<String, Value> = store
                    .service_tags(&def.id)
                    .into_iter()
                    .map(|(k, s)| (k, s.value))
                    .collect();
                return json_resp(StatusCode::OK, Value::Object(m));
            }
            (Method::GET, ["tags", name]) => {
                return match store.get(&def.id, name) {
                    Some(st) => json_resp(StatusCode::OK, json!({"value": st.value, "ts": st.ts})),
                    None => json_resp(StatusCode::NOT_FOUND, json!({"error": "no such tag"})),
                };
            }
            (Method::PUT | Method::POST, ["tags", name]) => {
                let Some(t) = def.tag(name) else {
                    return json_resp(StatusCode::NOT_FOUND, json!({"error": "no such tag"}));
                };
                if !t.writable {
                    return json_resp(StatusCode::FORBIDDEN, json!({"error": "tag is read-only"}));
                }
                let input: Value = match serde_json::from_slice(body) {
                    Ok(Value::Object(o)) if o.contains_key("value") => o["value"].clone(),
                    Ok(v) => v,
                    Err(_) => Value::String(String::from_utf8_lossy(body).into_owned()),
                };
                return match coerce_tag(t, &input) {
                    Ok(v) => {
                        store.set(&def.id, name, v.clone(), Quality::Good, Origin::Service);
                        json_resp(StatusCode::OK, json!({"value": v}))
                    }
                    Err(e) => json_resp(StatusCode::BAD_REQUEST, json!({"error": e})),
                };
            }
            _ => {}
        }
    }
    json_resp(StatusCode::NOT_FOUND, json!({"error": "no route", "method": method.as_str(), "path": path}))
}

pub async fn server(ctx: &mut Ctx) -> Result<()> {
    let s: ServerSettings = ctx.settings()?;
    for r in &s.routes {
        Method::from_bytes(r.method.to_uppercase().as_bytes())
            .map_err(|_| anyhow!("route {}: bad method `{}`", r.path, r.method))?;
        if !r.path.starts_with('/') {
            bail!("route paths start with / (`{}`)", r.path);
        }
    }
    let Some(port) = s.port else {
        ctx.status.running(format!("serving at /x/{}/ on Cowpoke's web port", ctx.def.id));
        crate::services::idle_until_cancelled(ctx).await;
        return Ok(());
    };
    let def = Arc::new(ctx.def.clone());
    let store = ctx.tags.clone();
    let app = axum::Router::new().fallback(move |method: Method, uri: Uri, body: Bytes| {
        let (def, store) = (def.clone(), store.clone());
        async move { handle(&def, &store, &method, uri.path(), &body).await }
    });
    let listener = crate::services::listen(&s.bind, port).await?;
    ctx.status.running(format!("listening on {}", listener.local_addr()?));
    let cancel = ctx.cancel.clone();
    tokio::select! {
        r = axum::serve(listener, app).with_graceful_shutdown(async move { cancel.cancelled().await }) => r?,
        _ = crate::services::idle_until_cancelled(ctx) => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates() {
        let vars = |k: &str| match k {
            "tags.temp" => Some(json!(21.5)),
            "tags.name" => Some(json!("pump")),
            _ => None,
        };
        assert_eq!(fill_text("t={{tags.temp}} n={{ tags.name }} x={{nope}}", &vars), "t=21.5 n=pump x={{nope}}");
        assert_eq!(
            fill_json(&json!({"t": "{{tags.temp}}", "s": "id-{{tags.name}}"}), &vars),
            json!({"t": 21.5, "s": "id-pump"})
        );
    }

    #[test]
    fn paths() {
        assert_eq!(match_path("/pumps/:id", "/pumps/7"), Some(vec![("id".into(), "7".into())]));
        assert_eq!(match_path("/pumps/:id", "/pumps"), None);
        assert_eq!(match_path("/", "/"), Some(vec![]));
    }
}

/// Check settings without running the service (used by the form and `/api/services/check`).
pub fn check_settings(mode: crate::model::Mode, v: &Value) -> Result<(), String> {
    let r = match mode {
        crate::model::Mode::Server => serde_json::from_value::<ServerSettings>(v.clone()).map(|_| ()),
        crate::model::Mode::Client => serde_json::from_value::<ClientSettings>(v.clone()).map(|_| ()),
    };
    r.map_err(|e| format!("settings: {e}"))
}
