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

async fn get_service(State(app): State<App>, _me: Me, Path(id): Path<String>) -> R<Json<Value>> {
    let s = find(&app, &id)?;
    let mut v = service_json(&app, &s);
    v["definition"] = serde_json::to_value(&s).unwrap_or_default();
    Ok(Json(v))
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

async fn get_service_yaml(State(app): State<App>, _me: Me, Path(id): Path<String>) -> R<impl IntoResponse> {
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
    let written = match &s.agent {
        Some(agent) => {
            // Check it here so mistakes don't need a round trip; the agent writes it.
            let t = s.tag(&tag).ok_or_else(|| bad(format!("no tag `{tag}`")))?;
            let v = crate::tags::coerce_tag(t, &v).map_err(bad)?;
            if s.mode == Mode::Client && !t.writable {
                return Err(bad(format!("tag `{tag}` is read-only")));
            }
            app.agents.write(agent, &id, &tag, v).await.map_err(bad)?
        }
        None => app.mgr.write(&s, &tag, &v).await.map_err(bad)?,
    };
    Ok(Json(json!({"value": written})))
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
        .route("/api/services/{id}/tags/{tag}", put(write_tag))
        .route("/api/services/{id}/history/{tag}", get(history))
        .route("/api/services/{id}/certs", get(list_certs))
        .route("/api/services/{id}/certs/{file}/{action}", post(cert_action))
        .route("/api/agents", get(list_agents).post(create_agent))
        .route("/api/agents/connect", get(agent_connect))
        .route("/api/agents/{id}", axum::routing::delete(delete_agent))
        .route("/api/agents/{id}/token", post(rotate_agent_token))
        .route("/api/config", get(export_config).put(import_config))
        .route("/api/templates/{kind}/{mode}", get(template))
        .route("/api/events", get(events))
        .route("/x/{id}", any(mock))
        .route("/x/{id}/", any(mock))
        .route("/x/{id}/{*rest}", any(mock))
        .with_state(app)
}
