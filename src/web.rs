//! The REST API, the live event stream, the web UI and the `/x/<id>/` mock API mount.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, header, request::Parts};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, put};
use axum::{Json, Router};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use chrono::{DateTime, Utc};
use futures::Stream;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::{Auth, Role, User};
use crate::historian::Historian;
use crate::manager::Manager;
use crate::model::{Kind, Mode, ServiceDef};
use crate::services;
use crate::store::{self, ConfigStore};
use crate::tags::TagStore;

const COOKIE: &str = "cowpoke_session";

#[derive(Clone)]
pub struct App {
    pub cfg: Arc<ConfigStore>,
    pub mgr: Arc<Manager>,
    pub tags: TagStore,
    pub auth: Arc<Auth>,
    pub hist: Arc<Historian>,
    pub agents: Arc<crate::agent::hub::Hub>,
    pub data_dir: PathBuf,
}

impl App {
    /// Re-apply the configuration after a change.
    pub async fn reload(&self) {
        let cfg = self.cfg.get();
        self.hist.configure(&cfg);
        self.mgr.apply(&cfg).await;
        self.agents.push_config(&cfg);
    }

    /// Refuse services assigned to an agent that isn't registered.
    fn check_agent(&self, def: &ServiceDef) -> R<()> {
        match &def.agent {
            Some(a) if !self.agents.exists(a) => Err(bad(format!(
                "service `{}`: no agent `{a}`. Create it on the Agents page first.",
                def.id
            ))),
            _ => Ok(()),
        }
    }
}

// ------------------------------------------------------------------ errors

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}
fn not_found(what: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("no {what}"))
}

type R<T> = Result<T, ApiError>;

// ------------------------------------------------------------------ auth

pub struct Me(pub User);

impl FromRequestParts<App> for Me {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        let bearer = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(str::to_string);
        let token = bearer.or_else(|| {
            CookieJar::from_headers(&parts.headers)
                .get(COOKIE)
                .map(|c| c.value().to_string())
        });
        token
            .and_then(|t| app.auth.session(&t))
            .map(Me)
            .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "sign in first".into()))
    }
}

impl Me {
    fn need(&self, role: Role) -> R<()> {
        if self.0.role >= role {
            Ok(())
        } else {
            Err(ApiError(
                StatusCode::FORBIDDEN,
                format!("needs the {role:?} role"),
            ))
        }
    }
}

#[derive(Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

async fn login(State(app): State<App>, jar: CookieJar, Json(b): Json<LoginBody>) -> R<impl IntoResponse> {
    let Some((token, user)) = app.auth.login(&b.username, &b.password) else {
        tokio::time::sleep(Duration::from_millis(600)).await;
        return Err(ApiError(StatusCode::UNAUTHORIZED, "wrong username or password".into()));
    };
    let cookie = Cookie::build((COOKIE, token.clone()))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Strict)
        .build();
    Ok((jar.add(cookie), Json(json!({"token": token, "user": user}))))
}

async fn logout(State(app): State<App>, jar: CookieJar, headers: HeaderMap) -> impl IntoResponse {
    if let Some(c) = jar.get(COOKIE) {
        app.auth.logout(c.value());
    }
    if let Some(t) = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
    {
        app.auth.logout(t);
    }
    (jar.remove(Cookie::from(COOKIE)), StatusCode::NO_CONTENT)
}

/// Version, license and where to get the source (the AGPL asks hosted copies to offer it).
/// Public: no login needed.
async fn about() -> Json<Value> {
    Json(json!({
        "name": "Cowpoke",
        "version": env!("CARGO_PKG_VERSION"),
        "license": env!("CARGO_PKG_LICENSE"),
        "source": std::env::var("COWPOKE_SOURCE_URL").ok().filter(|s| !s.trim().is_empty()),
    }))
}

async fn me(me: Me) -> Json<User> {
    Json(me.0)
}

async fn list_users(State(app): State<App>, me: Me) -> R<Json<Vec<User>>> {
    me.need(Role::Admin)?;
    Ok(Json(app.auth.list()))
}

#[derive(Deserialize)]
struct UserBody {
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    role: Option<Role>,
}

async fn put_user(State(app): State<App>, me: Me, Path(name): Path<String>, Json(b): Json<UserBody>) -> R<StatusCode> {
    let own_password = name == me.0.username && b.role.is_none_or(|r| r == me.0.role);
    if !own_password {
        me.need(Role::Admin)?;
    }
    let role = b
        .role
        .or_else(|| app.auth.list().into_iter().find(|u| u.username == name).map(|u| u.role))
        .unwrap_or(Role::Viewer);
    app.auth
        .put_user(&name, b.password.as_deref(), role)
        .map_err(bad)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_user(State(app): State<App>, me: Me, Path(name): Path<String>) -> R<StatusCode> {
    me.need(Role::Admin)?;
    app.auth.delete_user(&name).map_err(bad)?;
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------ services

fn service_json(app: &App, s: &ServiceDef) -> Value {
    json!({
        "id": s.id, "name": s.name_or_id(), "kind": s.kind, "mode": s.mode,
        "enabled": s.enabled, "tag_count": s.tags.len(), "agent": s.agent,
        "status": app.mgr.status(&s.id),
    })
}

async fn list_services(State(app): State<App>, _me: Me) -> Json<Value> {
    let cfg = app.cfg.get();
    Json(Value::Array(cfg.services.iter().map(|s| service_json(&app, s)).collect()))
}

fn find(app: &App, id: &str) -> R<ServiceDef> {
    app.cfg.service(id).ok_or_else(|| not_found("such service"))
}

async fn get_service(State(app): State<App>, me: Me, Path(id): Path<String>) -> R<Json<Value>> {
    let s = find(&app, &id)?;
    let mut v = service_json(&app, &s);
    let mut def = serde_json::to_value(&s).unwrap_or_default();
    // Credentials are admin-only, like export_config. Others still get the
    // definition (the service page reads e.g. settings.port), minus secrets.
    if me.need(Role::Admin).is_err()
        && let Some(settings) = def.get_mut("settings")
    {
        redact_secrets(settings);
    }
    v["definition"] = def;
    Ok(Json(v))
}

/// What a secret reads as for users who may not see it.
const REDACTED: &str = "********";

/// Blank out credentials anywhere in a service's settings: passwords
/// (MQTT/OPC UA client and broker, OPC UA server `users[]`, Sparkplug broker),
/// tokens and secrets, and every header value (API client requests and their
/// `write` request, which commonly carry `Authorization`).
fn redact_secrets(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                let key = k.to_ascii_lowercase();
                if key == "headers"
                    && let Value::Object(h) = val
                {
                    for hv in h.values_mut() {
                        *hv = Value::String(REDACTED.into());
                    }
                    continue;
                }
                let secret = ["password", "passphrase", "secret", "token", "api_key", "apikey", "private_key"]
                    .iter()
                    .any(|w| key.contains(w));
                if secret && !val.is_null() && !val.is_object() && !val.is_array() {
                    *val = Value::String(REDACTED.into());
                } else {
                    redact_secrets(val);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_secrets),
        _ => {}
    }
}

/// Accept a service as JSON or YAML (by content type).
fn parse_def(headers: &HeaderMap, body: &Bytes) -> R<ServiceDef> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if ct.contains("json") {
        serde_json::from_slice(body).map_err(|e| bad(e.to_string()))
    } else {
        let text = std::str::from_utf8(body).map_err(|_| bad("not UTF-8"))?;
        serde_yaml::from_str(text).map_err(|e| bad(e.to_string()))
    }
}

async fn create_service(State(app): State<App>, me: Me, headers: HeaderMap, body: Bytes) -> R<(StatusCode, Json<Value>)> {
    me.need(Role::Admin)?;
    let def = parse_def(&headers, &body)?;
    app.check_agent(&def)?;
    services::check_settings(&def).map_err(bad)?;
    if app.cfg.service(&def.id).is_some() {
        return Err(ApiError(StatusCode::CONFLICT, format!("a service with id `{}` already exists", def.id)));
    }
    app.cfg.upsert(None, def.clone()).map_err(bad)?;
    app.reload().await;
    Ok((StatusCode::CREATED, Json(service_json(&app, &def))))
}

async fn update_service(State(app): State<App>, me: Me, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> R<Json<Value>> {
    me.need(Role::Admin)?;
    find(&app, &id)?;
    let def = parse_def(&headers, &body)?;
    app.check_agent(&def)?;
    services::check_settings(&def).map_err(bad)?;
    app.cfg.upsert(Some(&id), def.clone()).map_err(bad)?;
    if def.id != id {
        // Renamed: move its folder (certificates) along.
        let base = app.data_dir.join("services");
        let _ = std::fs::rename(base.join(&id), base.join(&def.id));
    }
    app.reload().await;
    Ok(Json(service_json(&app, &def)))
}

#[derive(Deserialize)]
struct CheckQuery {
    /// The id of the service being edited, if any (so its own id and port don't count as clashes).
    editing: Option<String>,
}

/// Parse and check a service without saving it. Turns the form's JSON into YAML and back.
async fn check_service(State(app): State<App>, me: Me, Query(q): Query<CheckQuery>, headers: HeaderMap, body: Bytes) -> R<Json<Value>> {
    me.need(Role::Admin)?;
    let def = match parse_def(&headers, &body) {
        Ok(d) => d,
        Err(ApiError(_, e)) => return Ok(Json(json!({"errors": [e], "warnings": []}))),
    };
    let mut errors = vec![];
    let mut warnings = vec![];
    if let Err(e) = def.validate() {
        errors.push(e);
    }
    if let Err(e) = services::check_settings(&def) {
        errors.push(e);
    }
    if let Err(ApiError(_, e)) = app.check_agent(&def) {
        errors.push(e);
    }
    let cfg = app.cfg.get();
    if def.id != q.editing.clone().unwrap_or_default() && cfg.services.iter().any(|s| s.id == def.id) {
        errors.push(format!("a service with id `{}` already exists", def.id));
    }
    if let Some(port) = services::listen_port(&def) {
        for other in cfg.services.iter().filter(|s| Some(&s.id) != q.editing.as_ref() && s.agent == def.agent) {
            if services::listen_port(other) == Some(port) {
                warnings.push(format!(
                    "port {port} is also used by `{}`{}: only one can listen on it at a time",
                    other.id,
                    if other.enabled { "" } else { " (stopped)" }
                ));
            }
        }
        let web_port = std::env::var("COWPOKE_BIND").ok().and_then(|b| b.rsplit(':').next().and_then(|p| p.parse::<u16>().ok())).unwrap_or(8080);
        if def.agent.is_none() && port == web_port {
            warnings.push(format!("port {port} is Cowpoke's own web port"));
        }
        if def.agent.is_none() {
            warnings.push(format!("in Docker, publish port {port} (`-p {port}:{port}`) or use host networking"));
        }
    }
    if def.tags.is_empty() {
        warnings.push("no tags yet".into());
    }
    if def.kind == crate::model::Kind::Modbus {
        warnings.extend(services::modbus::overlaps(&def.tags));
    }
    Ok(Json(json!({
        "definition": def,
        "yaml": serde_yaml::to_string(&def).unwrap_or_default(),
        "errors": errors,
        "warnings": warnings,
    })))
}

/// Run a service for a few seconds without saving it, and report what happened.
async fn test_service(State(app): State<App>, me: Me, Query(q): Query<CheckQuery>, headers: HeaderMap, body: Bytes) -> R<Json<Value>> {
    me.need(Role::Admin)?;
    let mut def = parse_def(&headers, &body)?;
    def.validate().map_err(bad)?;
    services::check_settings(&def).map_err(bad)?;
    if let Some(a) = &def.agent {
        return Err(bad(format!("this service runs on agent `{a}`; tests run on the hub. Save it and watch its status instead.")));
    }
    if def.mode == crate::model::Mode::Server {
        if let Some(cur) = q.editing.as_deref().and_then(|id| app.cfg.service(id)) {
            if cur.enabled && cur.agent.is_none() && services::listen_port(&cur) == services::listen_port(&def) {
                return Ok(Json(json!({
                    "state": "skipped",
                    "detail": "The saved copy of this service is running on the same port. Save to apply your changes.",
                    "tags": [],
                })));
            }
        }
    }
    def.enabled = true;
    let tags = crate::tags::TagStore::new();
    let mgr = Manager::new(tags.clone(), app.data_dir.clone());
    mgr.apply(&crate::model::Config { services: vec![def.clone()] }).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut status;
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        status = mgr.status(&def.id);
        let read_all = def.tags.iter().all(|t| tags.get(&def.id, &t.name).is_some());
        let done = match status.state {
            crate::manager::State::Error => true,
            crate::manager::State::Running | crate::manager::State::Degraded => {
                def.mode == crate::model::Mode::Server || read_all
            }
            _ => false,
        };
        if done || tokio::time::Instant::now() > deadline {
            break;
        }
    }
    let values: Vec<Value> = def
        .tags
        .iter()
        .map(|t| {
            let st = tags.get(&def.id, &t.name);
            json!({"name": t.name, "value": st.as_ref().map(|s| s.value.clone()), "quality": st.as_ref().map(|s| s.quality)})
        })
        .collect();
    mgr.shutdown().await;
    let detail = if status.state == crate::manager::State::Starting {
        "still connecting after 8 s".to_string()
    } else {
        status.detail.trim_end_matches(" (retrying)").to_string()
    };
    Ok(Json(json!({"state": status.state, "detail": detail, "tags": values})))
}

async fn delete_service(State(app): State<App>, me: Me, Path(id): Path<String>) -> R<StatusCode> {
    me.need(Role::Admin)?;
    if !app.cfg.remove(&id).map_err(|e| bad(e.to_string()))? {
        return Err(not_found("such service"));
    }
    app.reload().await;
    app.tags.remove_service(&id);
    Ok(StatusCode::NO_CONTENT)
}

async fn get_service_yaml(State(app): State<App>, me: Me, Path(id): Path<String>) -> R<impl IntoResponse> {
    // The raw definition carries credentials; admin-only like export_config.
    me.need(Role::Admin)?;
    let s = find(&app, &id)?;
    Ok((
        [(header::CONTENT_TYPE, "text/yaml; charset=utf-8")],
        serde_yaml::to_string(&s).unwrap_or_default(),
    ))
}

async fn set_enabled(app: &App, id: &str, enabled: Option<bool>) -> R<Json<Value>> {
    let mut s = find(app, id)?;
    match enabled {
        Some(e) => {
            s.enabled = e;
            app.cfg.upsert(None, s.clone()).map_err(bad)?;
            app.reload().await;
        }
        None => {
            // Restart: stop, then start again.
            s.enabled = false;
            let mut cfg = app.cfg.get();
            if let Some(slot) = cfg.services.iter_mut().find(|x| x.id == id) {
                slot.enabled = false;
            }
            app.mgr.apply(&cfg).await;
            app.reload().await;
            s.enabled = true;
        }
    }
    Ok(Json(service_json(app, &s)))
}

async fn start_service(State(app): State<App>, me: Me, Path(id): Path<String>) -> R<Json<Value>> {
    me.need(Role::Operator)?;
    set_enabled(&app, &id, Some(true)).await
}
async fn stop_service(State(app): State<App>, me: Me, Path(id): Path<String>) -> R<Json<Value>> {
    me.need(Role::Operator)?;
    set_enabled(&app, &id, Some(false)).await
}
async fn restart_service(State(app): State<App>, me: Me, Path(id): Path<String>) -> R<Json<Value>> {
    me.need(Role::Operator)?;
    set_enabled(&app, &id, None).await
}

// ------------------------------------------------------------------ tags

#[derive(serde::Deserialize)]
struct DiagnoseReq {
    /// Services to look at; or `prefix` for every service whose id starts with it.
    #[serde(default)]
    services: Vec<String>,
    #[serde(default)]
    prefix: Option<String>,
    /// What the operator noticed, in their words.
    #[serde(default)]
    symptom: Option<String>,
    /// Include Cowpoke's rule checks in the evidence.
    #[serde(default = "yes_default")]
    rules: bool,
    /// Return the evidence and rule findings without asking a model.
    #[serde(default)]
    dry_run: bool,
}

fn yes_default() -> bool {
    true
}

/// Ask the foreman why something is wrong: Cowpoke gathers the evidence, runs its rule
/// checks, and a small model names the misconfigured service, the cause and the fix.
async fn foreman_diagnose(State(app): State<App>, _me: Me, Json(req): Json<DiagnoseReq>) -> R<Json<Value>> {
    let cfg = app.cfg.get();
    let defs: Vec<&ServiceDef> = cfg
        .services
        .iter()
        .filter(|s| req.services.contains(&s.id) || req.prefix.as_deref().is_some_and(|p| s.id.starts_with(p)))
        .collect();
    if defs.is_empty() {
        return Err(bad("name the services to look at (`services` or `prefix`)"));
    }
    let seen: Vec<crate::foreman::Seen> = defs
        .iter()
        .map(|d| {
            let status = serde_json::to_value(app.mgr.status(&d.id)).unwrap_or_default();
            crate::foreman::Seen::gather(d, &status, &app.tags)
        })
        .collect();
    let symptom = req.symptom.as_deref().unwrap_or("Please check these services.");
    if req.dry_run {
        let (evidence, findings) = crate::foreman::evidence(symptom, &seen, req.rules);
        return Ok(Json(json!({"evidence": evidence, "rule_findings": findings})));
    }
    let engine = crate::foreman::engine().await.map_err(|e| bad(format!("{e:#}")))?;
    let report = crate::foreman::diagnose(engine, symptom, &seen, req.rules)
        .await
        .map_err(|e| bad(format!("the model failed: {e:#}")))?;
    Ok(Json(serde_json::to_value(report).unwrap_or_default()))
}

/// A monitor service's inventory of talkers and its recent events.
async fn monitor_state(State(app): State<App>, _me: Me, Path(id): Path<String>) -> R<Json<Value>> {
    let s = find(&app, &id)?;
    if s.kind != crate::model::Kind::Monitor {
        return Err(bad(format!("`{id}` isn't a monitor service")));
    }
    Ok(Json(crate::monitor::snapshot(&id).unwrap_or_else(|| json!({"talkers": [], "events": [], "totals": {}}))))
}

async fn list_tags(State(app): State<App>, _me: Me, Path(id): Path<String>) -> R<Json<Value>> {
    let s = find(&app, &id)?;
    let out: Vec<Value> = s
        .tags
        .iter()
        .map(|t| {
            let st = app.tags.get(&id, &t.name);
            json!({
                "name": t.name, "datatype": t.datatype, "writable": t.writable,
                "units": t.units, "description": t.description,
                "historized": t.history.is_some(),
                "value": st.as_ref().map(|x| x.value.clone()),
                "quality": st.as_ref().map(|x| x.quality),
                "ts": st.as_ref().map(|x| x.ts),
            })
        })
        .collect();
    Ok(Json(Value::Array(out)))
}

async fn write_tag(State(app): State<App>, me: Me, Path((id, tag)): Path<(String, String)>, Json(body): Json<Value>) -> R<Json<Value>> {
    me.need(Role::Operator)?;
    let s = find(&app, &id)?;
    let v = match body {
        Value::Object(o) if o.contains_key("value") => o["value"].clone(),
        other => other,
    };
    let written = do_write(&app, &s, &tag, &v).await.map_err(bad)?;
    Ok(Json(json!({"value": written})))
}

/// Writes one tag of a service, locally or through its agent. Returns the value written.
async fn do_write(app: &App, s: &ServiceDef, tag: &str, v: &Value) -> Result<Value, String> {
    match &s.agent {
        Some(agent) => {
            // Check it here so mistakes don't need a round trip; the agent writes it.
            let t = s.tag(tag).ok_or_else(|| format!("no tag `{tag}`"))?;
            let v = crate::tags::coerce_tag(t, v)?;
            if s.mode == Mode::Client && !t.writable {
                return Err(format!("tag `{tag}` is read-only"));
            }
            app.agents.write(agent, &s.id, tag, v).await
        }
        None => app.mgr.write(s, tag, v).await,
    }
}

// ------------------------------------------------------------------ every tag at once

/// `*` matches any run of characters; everything else matches itself.
fn glob(pat: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), text.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] != '*' && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

fn matches(pats: &[String], name: &str) -> bool {
    pats.is_empty() || pats.iter().any(|p| glob(p, name))
}

/// Splits `service.tag` at the first dot (service ids have none).
fn split_name(name: &str) -> Option<(&str, &str)> {
    name.split_once('.').filter(|(s, t)| !s.is_empty() && !t.is_empty())
}

fn tag_snapshot(app: &App, pats: &[String]) -> Vec<Value> {
    let cfg = app.cfg.get();
    let mut out = Vec::new();
    for s in &cfg.services {
        for t in &s.tags {
            let name = format!("{}.{}", s.id, t.name);
            if !matches(pats, &name) {
                continue;
            }
            let st = app.tags.get(&s.id, &t.name);
            out.push(json!({
                "name": name, "service": s.id, "tag": t.name,
                "datatype": t.datatype, "writable": t.writable || s.mode == Mode::Server,
                "units": t.units,
                "value": st.as_ref().map(|x| x.value.clone()),
                "quality": st.as_ref().map(|x| x.quality),
                "ts": st.as_ref().map(|x| x.ts),
            }));
        }
    }
    out
}

#[derive(Deserialize)]
struct TagsQuery {
    /// Comma-separated `service.tag` patterns; `*` is a wildcard.
    #[serde(rename = "match")]
    pattern: Option<String>,
}

fn patterns(q: &Option<String>) -> Vec<String> {
    q.as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect()
}

async fn all_tags(State(app): State<App>, _me: Me, Query(q): Query<TagsQuery>) -> Json<Value> {
    Json(Value::Array(tag_snapshot(&app, &patterns(&q.pattern))))
}

/// Writes several tags: `{"plc1.speed": 50, "plc1.run": true}`. Each is reported on its own.
async fn write_many(app: &App, writes: &serde_json::Map<String, Value>) -> Value {
    let mut out = serde_json::Map::new();
    for (name, v) in writes {
        let r = match split_name(name) {
            None => Err(format!("`{name}` is not service.tag")),
            Some((sid, tag)) => match app.cfg.service(sid) {
                None => Err(format!("no service `{sid}`")),
                Some(s) => do_write(app, &s, tag, v).await,
            },
        };
        out.insert(
            name.clone(),
            match r {
                Ok(v) => json!({"ok": true, "value": v}),
                Err(e) => json!({"ok": false, "error": e}),
            },
        );
    }
    Value::Object(out)
}

async fn write_tags(State(app): State<App>, me: Me, Json(body): Json<Value>) -> R<Json<Value>> {
    me.need(Role::Operator)?;
    let Value::Object(w) = body else {
        return Err(bad("send an object of {\"service.tag\": value}"));
    };
    Ok(Json(write_many(&app, &w).await))
}

/// A WebSocket for scripts: a snapshot, then every change, and writes on the same socket.
///
/// Client → server: `{"subscribe": ["plc1.*"]}` (replaces the set; empty = all),
/// `{"write": {"plc1.speed": 50}, "id": 7}`, `{"read": ["plc1.*"], "id": 8}`.
/// Server → client: `{"type":"snapshot","tags":[…]}`, `{"type":"tag",…}`,
/// `{"type":"written","id":7,"results":{…}}`, `{"type":"values","id":8,"tags":[…]}`, `{"type":"error",…}`.
async fn tags_ws(State(app): State<App>, me: Me, Query(q): Query<TagsQuery>, ws: axum::extract::ws::WebSocketUpgrade) -> Response {
    let pats = patterns(&q.pattern);
    ws.on_upgrade(move |socket| tags_socket(app, me, pats, socket))
}

async fn tags_socket(app: App, me: Me, mut pats: Vec<String>, mut socket: axum::extract::ws::WebSocket) {
    use axum::extract::ws::Message;
    let mut rx = app.tags.subscribe();
    let send = |v: Value| Message::Text(v.to_string().into());
    if socket.send(send(json!({"type": "snapshot", "tags": tag_snapshot(&app, &pats)}))).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            r = rx.recv() => match r {
                Ok(e) => {
                    let name = format!("{}.{}", e.service, e.tag);
                    if !matches(&pats, &name) { continue; }
                    let m = json!({"type": "tag", "name": name, "service": e.service, "tag": e.tag,
                        "value": e.state.value, "quality": e.state.quality, "ts": e.state.ts});
                    if socket.send(send(m)).await.is_err() { return; }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    // Too slow to keep up: say so, and resend where everything stands.
                    let m = json!({"type": "snapshot", "lagged": n, "tags": tag_snapshot(&app, &pats)});
                    if socket.send(send(m)).await.is_err() { return; }
                }
                Err(_) => return,
            },
            m = socket.recv() => {
                let Some(Ok(m)) = m else { return };
                let text = match m {
                    Message::Text(t) => t.to_string(),
                    Message::Close(_) => return,
                    _ => continue,
                };
                let reply = match serde_json::from_str::<Value>(&text) {
                    Err(e) => json!({"type": "error", "error": format!("not JSON: {e}")}),
                    Ok(cmd) => {
                        let id = cmd.get("id").cloned().unwrap_or(Value::Null);
                        let list = |v: &Value| -> Vec<String> {
                            v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
                        };
                        if let Some(sub) = cmd.get("subscribe") {
                            pats = list(sub);
                            json!({"type": "snapshot", "id": id, "tags": tag_snapshot(&app, &pats)})
                        } else if let Some(r) = cmd.get("read") {
                            json!({"type": "values", "id": id, "tags": tag_snapshot(&app, &list(r))})
                        } else if let Some(Value::Object(w)) = cmd.get("write") {
                            if me.0.role < Role::Operator {
                                json!({"type": "error", "id": id, "error": "writing needs the Operator role"})
                            } else {
                                json!({"type": "written", "id": id, "results": write_many(&app, w).await})
                            }
                        } else {
                            json!({"type": "error", "id": id, "error": "expected subscribe, read or write"})
                        }
                    }
                };
                if socket.send(send(reply)).await.is_err() { return; }
            }
        }
    }
}

#[derive(Deserialize)]
struct HistQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    points: Option<usize>,
}

async fn history(State(app): State<App>, _me: Me, Path((id, tag)): Path<(String, String)>, Query(q): Query<HistQuery>) -> R<Json<Value>> {
    let s = find(&app, &id)?;
    s.tag(&tag).ok_or_else(|| not_found("such tag"))?;
    let to = q.to.unwrap_or_else(Utc::now);
    let from = q.from.unwrap_or(to - chrono::Duration::hours(1));
    if from >= to {
        return Err(bad("`from` must be before `to`"));
    }
    if (to - from).num_days() > 366 {
        return Err(bad("ask for at most a year at a time"));
    }
    let hist = app.hist.clone();
    let points = q.points.unwrap_or(600);
    let pts = tokio::task::spawn_blocking(move || hist.query(&id, &tag, from, to, points))
        .await
        .map_err(|e| bad(e.to_string()))?;
    Ok(Json(json!({"from": from, "to": to, "points": pts})))
}

// ------------------------------------------------------------------ certificates

async fn list_certs(State(app): State<App>, me: Me, Path(id): Path<String>) -> R<Json<Value>> {
    me.need(Role::Admin)?;
    let s = find(&app, &id)?;
    if s.kind != Kind::Opcua {
        return Err(bad("only OPC UA services have certificates"));
    }
    if let Some(agent) = &s.agent {
        let op = crate::agent::Op::CertList { service: id.clone() };
        return app.agents.request(agent, op).await.map(Json).map_err(bad);
    }
    let dir = app.data_dir.join("services").join(&id);
    let own = services::opcua::pki_dir(&dir).join("own").join("cert.der");
    let own_tp = services::opcua::crypto_thumbprint(&own);
    Ok(Json(json!({
        "own_thumbprint": own_tp,
        "certificates": services::opcua::list_certs(&dir),
    })))
}

async fn cert_action(State(app): State<App>, me: Me, Path((id, file, action)): Path<(String, String, String)>) -> R<StatusCode> {
    me.need(Role::Admin)?;
    let s = find(&app, &id)?;
    if let Some(agent) = &s.agent {
        let op = crate::agent::Op::CertAction { service: id.clone(), file, action };
        app.agents.request(agent, op).await.map_err(bad)?;
        return Ok(StatusCode::NO_CONTENT);
    }
    let dir = app.data_dir.join("services").join(&id);
    services::opcua::set_cert(&dir, &file, &action).map_err(bad)?;
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------ agents

async fn list_agents(State(app): State<App>, _me: Me) -> Json<Value> {
    Json(Value::Array(app.agents.list(&app.cfg.get())))
}

#[derive(Deserialize)]
struct AgentBody {
    id: String,
    #[serde(default)]
    name: String,
}

/// The token is in this answer only. Save it on the agent as `COWPOKE_AGENT_TOKEN`.
async fn create_agent(State(app): State<App>, me: Me, Json(b): Json<AgentBody>) -> R<(StatusCode, Json<Value>)> {
    me.need(Role::Admin)?;
    let token = app.agents.create(&b.id, &b.name).map_err(bad)?;
    Ok((StatusCode::CREATED, Json(json!({"id": b.id, "token": token}))))
}

async fn rotate_agent_token(State(app): State<App>, me: Me, Path(id): Path<String>) -> R<Json<Value>> {
    me.need(Role::Admin)?;
    let token = app.agents.rotate(&id).map_err(bad)?;
    Ok(Json(json!({"id": id, "token": token})))
}

async fn delete_agent(State(app): State<App>, me: Me, Path(id): Path<String>) -> R<StatusCode> {
    me.need(Role::Admin)?;
    app.agents.delete(&id, &app.cfg.get()).map_err(bad)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The agent's WebSocket. Authorized by the agent token, not a user session.
async fn agent_connect(State(app): State<App>, headers: HeaderMap, ws: axum::extract::ws::WebSocketUpgrade) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    let Some(agent) = app.agents.authenticate(token) else {
        tokio::time::sleep(Duration::from_millis(600)).await;
        return ApiError(StatusCode::UNAUTHORIZED, "unknown agent token".into()).into_response();
    };
    ws.on_upgrade(move |socket| crate::agent::hub::serve(app, agent, socket))
}

// ------------------------------------------------------------------ whole config

async fn export_config(State(app): State<App>, me: Me) -> R<impl IntoResponse> {
    me.need(Role::Admin)?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/yaml; charset=utf-8"),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"cowpoke.yaml\""),
        ],
        app.cfg.to_yaml(),
    ))
}

async fn import_config(State(app): State<App>, me: Me, body: String) -> R<StatusCode> {
    me.need(Role::Admin)?;
    let cfg = store::parse(&body).map_err(|e| bad(e.to_string()))?;
    for s in &cfg.services {
        app.check_agent(s)?;
    }
    app.cfg.replace_all(cfg).map_err(bad)?;
    app.reload().await;
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------ import from Ohmstead

#[derive(Deserialize)]
struct OhmsteadSource {
    #[serde(default)]
    url: String,
    token: String,
    project_id: String,
}

#[derive(Deserialize)]
struct OhmsteadImport {
    /// An existing service to import into.
    service: Option<String>,
    /// Or an unsaved service (the builder's form): the merged definition comes back, nothing is saved.
    definition: Option<ServiceDef>,
    /// The ohmstead-io JSON, as an object or pasted text.
    io: Option<Value>,
    /// Or fetch it from an Ohmstead MCP endpoint.
    source: Option<OhmsteadSource>,
    #[serde(default)]
    options: crate::import_ohmstead::Options,
    /// Save into `service`. Without it, only preview.
    #[serde(default)]
    apply: bool,
}

/// Map an Ohmstead project's I/O points to tags on a service. Previews unless `apply`.
async fn import_ohmstead(State(app): State<App>, me: Me, Json(b): Json<OhmsteadImport>) -> R<Json<Value>> {
    use crate::import_ohmstead as io;
    me.need(Role::Admin)?;
    let ex = match (b.io, b.source) {
        (Some(Value::String(text)), _) => io::parse(&text).map_err(bad)?,
        (Some(v), _) => io::from_value(v).map_err(bad)?,
        (None, Some(s)) => io::fetch(&s.url, &s.token, &s.project_id).await.map_err(bad)?,
        (None, None) => return Err(bad("send `io` (the Ohmstead I/O JSON) or `source` (url, token, project_id)")),
    };
    let mut def = match (&b.service, b.definition) {
        (Some(id), _) => find(&app, id)?,
        (None, Some(d)) => d,
        (None, None) => return Err(bad("name a `service`, or send its `definition`")),
    };
    if b.apply && b.service.is_none() {
        return Err(bad("`apply` saves into an existing `service`; save the new service first"));
    }
    let preview = io::apply(&mut def, &ex, &b.options);
    if b.apply {
        def.validate().map_err(bad)?;
        let id = def.id.clone();
        app.cfg.upsert(Some(&id), def.clone()).map_err(bad)?;
        app.reload().await;
    }
    Ok(Json(json!({ "preview": preview, "definition": def, "saved": b.apply, "points": ex.points.len() })))
}

async fn template(_me: Me, Path((kind, mode)): Path<(String, String)>) -> R<impl IntoResponse> {
    let kind: Kind = serde_json::from_value(json!(kind)).map_err(|_| bad("unknown kind"))?;
    let mode: Mode = serde_json::from_value(json!(mode)).map_err(|_| bad("unknown mode"))?;
    Ok((
        [(header::CONTENT_TYPE, "text/yaml; charset=utf-8")],
        services::template(kind, mode),
    ))
}

// ------------------------------------------------------------------ live events

async fn events(State(app): State<App>, _me: Me) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = app.tags.subscribe();
    let stream = async_stream(move |tx| async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            let ev = tokio::select! {
                r = rx.recv() => match r {
                    Ok(e) => Event::default().event("tag").json_data(json!({
                        "service": e.service, "tag": e.tag, "value": e.state.value,
                        "quality": e.state.quality, "ts": e.state.ts,
                    })),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                },
                _ = tick.tick() => {
                    let cfg = app.cfg.get();
                    let st: serde_json::Map<String, Value> = cfg.services.iter()
                        .map(|s| (s.id.clone(), json!(app.mgr.status(&s.id))))
                        .collect();
                    Event::default().event("status").json_data(Value::Object(st))
                }
            };
            let Ok(ev) = ev else { continue };
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// A small channel-backed stream, so the SSE loop can be plain async code.
fn async_stream<F, Fut>(f: F) -> impl Stream<Item = Result<Event, Infallible>>
where
    F: FnOnce(tokio::sync::mpsc::Sender<Result<Event, Infallible>>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    tokio::spawn(f(tx));
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

// ------------------------------------------------------------------ mock API mount

async fn mock(State(app): State<App>, method: Method, Path(params): Path<Vec<(String, String)>>, body: Bytes) -> Response {
    let id = params.iter().find(|(k, _)| k == "id").map(|(_, v)| v.clone()).unwrap_or_default();
    let rest = params.iter().find(|(k, _)| k == "rest").map(|(_, v)| format!("/{v}")).unwrap_or_else(|| "/".into());
    let Some(def) = app.cfg.service(&id) else {
        return ApiError(StatusCode::NOT_FOUND, "no such service".into()).into_response();
    };
    if def.kind != Kind::Api || def.mode != Mode::Server {
        return ApiError(StatusCode::NOT_FOUND, "not an API server service".into()).into_response();
    }
    if !def.enabled {
        return ApiError(StatusCode::SERVICE_UNAVAILABLE, "service is stopped".into()).into_response();
    }
    services::api::handle(&def, &app.tags, &method, &rest, &body).await
}

// ------------------------------------------------------------------ UI

const INDEX: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const BUILDER_JS: &str = include_str!("../ui/builder.js");
const STYLE: &str = include_str!("../ui/style.css");

fn asset(ct: &'static str, body: &'static str) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "no-cache")], body)
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/", get(|| async { asset("text/html; charset=utf-8", INDEX) }))
        .route("/app.js", get(|| async { asset("text/javascript; charset=utf-8", APP_JS) }))
        .route("/style.css", get(|| async { asset("text/css; charset=utf-8", STYLE) }))
        .route("/builder.js", get(|| async { asset("text/javascript; charset=utf-8", BUILDER_JS) }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/about", get(about))
        .route("/api/users", get(list_users))
        .route("/api/users/{name}", put(put_user).delete(delete_user))
        .route("/api/services", get(list_services).post(create_service))
        .route("/api/services/check", post(check_service))
        .route("/api/services/test", post(test_service))
        .route("/api/services/{id}", get(get_service).put(update_service).delete(delete_service))
        .route("/api/services/{id}/yaml", get(get_service_yaml))
        .route("/api/services/{id}/start", post(start_service))
        .route("/api/services/{id}/stop", post(stop_service))
        .route("/api/services/{id}/restart", post(restart_service))
        .route("/api/services/{id}/tags", get(list_tags))
        .route("/api/tags", get(all_tags).post(write_tags))
        .route("/api/tags/ws", get(tags_ws))
        .route("/api/services/{id}/monitor", get(monitor_state))
        .route("/api/foreman/diagnose", post(foreman_diagnose))
        .route("/api/services/{id}/tags/{tag}", put(write_tag))
        .route("/api/services/{id}/history/{tag}", get(history))
        .route("/api/services/{id}/certs", get(list_certs))
        .route("/api/services/{id}/certs/{file}/{action}", post(cert_action))
        .route("/api/agents", get(list_agents).post(create_agent))
        .route("/api/agents/connect", get(agent_connect))
        .route("/api/agents/{id}", axum::routing::delete(delete_agent))
        .route("/api/agents/{id}/token", post(rotate_agent_token))
        .route("/api/config", get(export_config).put(import_config))
        .route("/api/import/ohmstead", post(import_ohmstead))
        .route("/api/templates/{kind}/{mode}", get(template))
        .route("/api/events", get(events))
        .route("/x/{id}", any(mock))
        .route("/x/{id}/", any(mock))
        .route("/x/{id}/{*rest}", any(mock))
        .with_state(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_secrets_blanks_credentials_but_keeps_the_rest() {
        let mut v = json!({
            "url": "https://example.test/api",
            "port": 8080,
            "username": "ops",
            "password": "hunter2",
            "headers": { "Authorization": "Bearer prod-token", "Accept": "application/json" },
            "write": { "url": "https://example.test/w", "headers": { "X-Api-Key": "k" } },
            "users": [{ "username": "a", "password": "p1" }, { "username": "b", "password": "p2" }],
            "sparkplug": { "broker": { "host": "mq", "password": "bp" } },
            "allow_anonymous": false,
        });
        redact_secrets(&mut v);
        assert_eq!(v["password"], REDACTED);
        assert_eq!(v["headers"]["Authorization"], REDACTED);
        assert_eq!(v["headers"]["Accept"], REDACTED);
        assert_eq!(v["write"]["headers"]["X-Api-Key"], REDACTED);
        assert_eq!(v["users"][0]["password"], REDACTED);
        assert_eq!(v["users"][1]["password"], REDACTED);
        assert_eq!(v["sparkplug"]["broker"]["password"], REDACTED);
        // Non-secret settings the UI reads are untouched.
        assert_eq!(v["port"], 8080);
        assert_eq!(v["url"], "https://example.test/api");
        assert_eq!(v["username"], "ops");
        assert_eq!(v["users"][1]["username"], "b");
        assert_eq!(v["sparkplug"]["broker"]["host"], "mq");
        assert_eq!(v["allow_anonymous"], false);
        assert!(!v.to_string().contains("hunter2") && !v.to_string().contains("prod-token"));
    }
}
