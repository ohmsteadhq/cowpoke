//! API routes for dashboards, alarms and hooks, plus the page script and styles for them.
//! Merged into the main router in `main.rs`.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header, request::Parts};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::cookie::CookieJar;
use futures::Stream;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::alarms::{AlarmConfig, AlarmDef, Engine, HookDef};
use crate::auth::{Role, User};
use crate::dashboards::{Dashboard, Dashboards};
use crate::web::App;

#[derive(Clone)]
pub struct Panels {
    pub app: App,
    pub engine: Arc<Engine>,
    pub dash: Arc<Dashboards>,
}

pub struct ApiErr(StatusCode, String);

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

fn bad(m: impl Into<String>) -> ApiErr {
    ApiErr(StatusCode::BAD_REQUEST, m.into())
}
fn missing(what: &str) -> ApiErr {
    ApiErr(StatusCode::NOT_FOUND, format!("no such {what}"))
}

type R<T> = Result<T, ApiErr>;

pub struct Me(User);

impl FromRequestParts<Panels> for Me {
    type Rejection = ApiErr;
    async fn from_request_parts(parts: &mut Parts, p: &Panels) -> Result<Self, ApiErr> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(str::to_string)
            .or_else(|| CookieJar::from_headers(&parts.headers).get("cowpoke_session").map(|c| c.value().to_string()));
        token
            .and_then(|t| p.app.auth.session(&t))
            .map(Me)
            .ok_or_else(|| ApiErr(StatusCode::UNAUTHORIZED, "sign in first".into()))
    }
}

impl Me {
    fn need(&self, role: Role) -> R<()> {
        if self.0.role >= role {
            Ok(())
        } else {
            Err(ApiErr(StatusCode::FORBIDDEN, format!("needs the {role:?} role")))
        }
    }
}

/// A body as JSON or YAML, by content type.
fn parse<T: DeserializeOwned>(headers: &HeaderMap, body: &Bytes) -> R<T> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|c| c.contains("json"));
    if json {
        serde_json::from_slice(body).map_err(|e| bad(e.to_string()))
    } else {
        let text = std::str::from_utf8(body).map_err(|_| bad("not UTF-8"))?;
        serde_yaml::from_str(text).map_err(|e| bad(e.to_string()))
    }
}

const RESERVED: [&str; 4] = ["log", "events", "config", "ack-all"];

// ------------------------------------------------------------------ alarms

async fn list_alarms(State(p): State<Panels>, _me: Me) -> Json<Value> {
    let cfg = p.engine.store.get();
    let states = p.engine.states();
    Json(Value::Array(
        cfg.alarms
            .iter()
            .map(|a| {
                let st = states.get(&a.id).cloned().unwrap_or_default();
                let mut v = serde_json::to_value(a).unwrap_or_default();
                v["shown"] = json!(st.shown() && a.enabled);
                v["state"] = serde_json::to_value(&st).unwrap_or_default();
                v["current"] = json!(p.app.tags.get(&a.service, &a.tag));
                v
            })
            .collect(),
    ))
}

fn check_alarm(p: &Panels, a: &AlarmDef) -> R<()> {
    if RESERVED.contains(&a.id.as_str()) {
        return Err(bad(format!("`{}` is a reserved word; pick another id", a.id)));
    }
    let svc = p.app.cfg.service(&a.service).ok_or_else(|| bad(format!("no service `{}`", a.service)))?;
    if svc.tag(&a.tag).is_none() {
        return Err(bad(format!("service `{}` has no tag `{}`", a.service, a.tag)));
    }
    Ok(())
}

async fn create_alarm(State(p): State<Panels>, me: Me, headers: HeaderMap, body: Bytes) -> R<(StatusCode, Json<AlarmDef>)> {
    me.need(Role::Admin)?;
    let a: AlarmDef = parse(&headers, &body)?;
    check_alarm(&p, &a)?;
    p.engine
        .store
        .update(|c| {
            if c.alarms.iter().any(|x| x.id == a.id) {
                return Err(format!("an alarm with id `{}` already exists", a.id));
            }
            c.alarms.push(a.clone());
            Ok(())
        })
        .map_err(bad)?;
    Ok((StatusCode::CREATED, Json(a)))
}

async fn update_alarm(State(p): State<Panels>, me: Me, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> R<Json<AlarmDef>> {
    me.need(Role::Admin)?;
    let a: AlarmDef = parse(&headers, &body)?;
    check_alarm(&p, &a)?;
    p.engine
        .store
        .update(|c| {
            if a.id != id && c.alarms.iter().any(|x| x.id == a.id) {
                return Err(format!("an alarm with id `{}` already exists", a.id));
            }
            let slot = c.alarms.iter_mut().find(|x| x.id == id).ok_or("no such alarm")?;
            *slot = a.clone();
            Ok(())
        })
        .map_err(bad)?;
    Ok(Json(a))
}

async fn delete_alarm(State(p): State<Panels>, me: Me, Path(id): Path<String>) -> R<StatusCode> {
    me.need(Role::Admin)?;
    p.engine
        .store
        .update(|c| {
            let before = c.alarms.len();
            c.alarms.retain(|a| a.id != id);
            if c.alarms.len() == before { Err("no such alarm".into()) } else { Ok(()) }
        })
        .map_err(|e| if e == "no such alarm" { missing("alarm") } else { bad(e) })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn ack(State(p): State<Panels>, me: Me, Path(id): Path<String>) -> R<StatusCode> {
    me.need(Role::Operator)?;
    if p.engine.ack(&id, &me.0.username) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(bad("nothing to acknowledge"))
    }
}

async fn ack_all(State(p): State<Panels>, me: Me) -> R<Json<Value>> {
    me.need(Role::Operator)?;
    let ids: Vec<String> = p.engine.states().into_iter().filter(|(_, s)| s.shown() && !s.acked).map(|(id, _)| id).collect();
    let n = ids.iter().filter(|id| p.engine.ack(id, &me.0.username)).count();
    Ok(Json(json!({"acknowledged": n})))
}

#[derive(Deserialize)]
struct LogQuery {
    limit: Option<usize>,
}

async fn alarm_log(State(p): State<Panels>, _me: Me, Query(q): Query<LogQuery>) -> Json<Value> {
    let e = p.engine.clone();
    let limit = q.limit.unwrap_or(200).min(5000);
    let log = tokio::task::spawn_blocking(move || e.log_tail(limit)).await.unwrap_or_default();
    Json(json!(log))
}

async fn alarm_events(State(p): State<Panels>, _me: Me) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = p.engine.subscribe();
    let (tx, out) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        loop {
            let ev = match rx.recv().await {
                Ok(n) => Event::default().event("alarm").json_data(n),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            };
            if let Ok(ev) = ev {
                if tx.send(Ok(ev)).await.is_err() {
                    return;
                }
            }
        }
    });
    Sse::new(tokio_stream::wrappers::ReceiverStream::new(out)).keep_alive(KeepAlive::default())
}

async fn export_alarms(State(p): State<Panels>, me: Me) -> R<impl IntoResponse> {
    me.need(Role::Admin)?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/yaml; charset=utf-8"),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"alarms.yaml\""),
        ],
        p.engine.store.to_yaml(),
    ))
}

async fn import_alarms(State(p): State<Panels>, me: Me, headers: HeaderMap, body: Bytes) -> R<StatusCode> {
    me.need(Role::Admin)?;
    let new: AlarmConfig = parse(&headers, &body)?;
    for a in &new.alarms {
        check_alarm(&p, a)?;
    }
    p.engine.store.update(|c| {
        *c = new;
        Ok(())
    })
    .map_err(bad)?;
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------ hooks

async fn list_hooks(State(p): State<Panels>, me: Me) -> R<Json<Value>> {
    me.need(Role::Admin)?;
    let results = p.engine.hook_results();
    let cfg = p.engine.store.get();
    Ok(Json(Value::Array(
        cfg.hooks
            .iter()
            .map(|h| {
                let mut v = serde_json::to_value(h).unwrap_or_default();
                v["last"] = json!(results.get(&h.id));
                v["used_by"] = json!(cfg.alarms.iter().filter(|a| a.hooks.contains(&h.id)).map(|a| &a.id).collect::<Vec<_>>());
                v
            })
            .collect(),
    )))
}

async fn create_hook(State(p): State<Panels>, me: Me, headers: HeaderMap, body: Bytes) -> R<(StatusCode, Json<HookDef>)> {
    me.need(Role::Admin)?;
    let h: HookDef = parse(&headers, &body)?;
    p.engine
        .store
        .update(|c| {
            if c.hooks.iter().any(|x| x.id == h.id) {
                return Err(format!("a hook with id `{}` already exists", h.id));
            }
            c.hooks.push(h.clone());
            Ok(())
        })
        .map_err(bad)?;
    Ok((StatusCode::CREATED, Json(h)))
}

async fn update_hook(State(p): State<Panels>, me: Me, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> R<Json<HookDef>> {
    me.need(Role::Admin)?;
    let h: HookDef = parse(&headers, &body)?;
    p.engine
        .store
        .update(|c| {
            if h.id != id {
                if c.hooks.iter().any(|x| x.id == h.id) {
                    return Err(format!("a hook with id `{}` already exists", h.id));
                }
                // Keep alarms pointing at it.
                for a in c.alarms.iter_mut() {
                    for x in a.hooks.iter_mut().filter(|x| **x == id) {
                        *x = h.id.clone();
                    }
                }
            }
            let slot = c.hooks.iter_mut().find(|x| x.id == id).ok_or("no such hook")?;
            *slot = h.clone();
            Ok(())
        })
        .map_err(bad)?;
    Ok(Json(h))
}

async fn delete_hook(State(p): State<Panels>, me: Me, Path(id): Path<String>) -> R<StatusCode> {
    me.need(Role::Admin)?;
    p.engine
        .store
        .update(|c| {
            let users: Vec<&str> = c.alarms.iter().filter(|a| a.hooks.contains(&id)).map(|a| a.id.as_str()).collect();
            if !users.is_empty() {
                return Err(format!("remove it from these alarms first: {}", users.join(", ")));
            }
            let before = c.hooks.len();
            c.hooks.retain(|h| h.id != id);
            if c.hooks.len() == before { Err("no such hook".into()) } else { Ok(()) }
        })
        .map_err(bad)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Run a hook now with a sample event. A body, if sent, is tried instead of the saved hook.
async fn test_hook(State(p): State<Panels>, me: Me, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> R<Json<Value>> {
    me.need(Role::Admin)?;
    let h: HookDef = if body.is_empty() {
        p.engine.store.get().hooks.into_iter().find(|h| h.id == id).ok_or_else(|| missing("hook"))?
    } else {
        parse(&headers, &body)?
    };
    let r = tokio::time::timeout(Duration::from_secs(40), p.engine.test_hook(&h, &me.0.username))
        .await
        .map_err(|_| bad("the hook didn't finish within 40 s"))?;
    Ok(Json(json!(r)))
}

// ------------------------------------------------------------------ dashboards

async fn list_dashboards(State(p): State<Panels>, _me: Me) -> Json<Value> {
    Json(json!(p.dash.list().into_iter().map(|d| json!({
        "id": d.id, "name": if d.name.is_empty() { d.id.clone() } else { d.name.clone() },
        "widgets": d.widgets.len(), "updated": d.updated, "updated_by": d.updated_by,
    })).collect::<Vec<_>>()))
}

async fn get_dashboard(State(p): State<Panels>, _me: Me, Path(id): Path<String>) -> R<Json<Dashboard>> {
    p.dash.get(&id).map(Json).ok_or_else(|| missing("dashboard"))
}

async fn create_dashboard(State(p): State<Panels>, me: Me, headers: HeaderMap, body: Bytes) -> R<(StatusCode, Json<Dashboard>)> {
    me.need(Role::Operator)?;
    let d: Dashboard = parse(&headers, &body)?;
    let d = p.dash.put(None, d, &me.0.username).map_err(bad)?;
    Ok((StatusCode::CREATED, Json(d)))
}

async fn update_dashboard(State(p): State<Panels>, me: Me, Path(id): Path<String>, headers: HeaderMap, body: Bytes) -> R<Json<Dashboard>> {
    me.need(Role::Operator)?;
    let d: Dashboard = parse(&headers, &body)?;
    p.dash.put(Some(&id), d, &me.0.username).map(Json).map_err(bad)
}

async fn delete_dashboard(State(p): State<Panels>, me: Me, Path(id): Path<String>) -> R<StatusCode> {
    me.need(Role::Operator)?;
    p.dash.delete(&id).map_err(bad)?;
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------ router

const JS: &str = include_str!("../ui/panels.js");
const CSS: &str = include_str!("../ui/panels.css");

pub fn router(p: Panels) -> Router {
    Router::new()
        .route("/panels.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], JS) }))
        .route("/panels.css", get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], CSS) }))
        .route("/api/alarms", get(list_alarms).post(create_alarm))
        .route("/api/alarms/log", get(alarm_log))
        .route("/api/alarms/events", get(alarm_events))
        .route("/api/alarms/config", get(export_alarms).put(import_alarms))
        .route("/api/alarms/ack-all", post(ack_all))
        .route("/api/alarms/{id}", axum::routing::put(update_alarm).delete(delete_alarm))
        .route("/api/alarms/{id}/ack", post(ack))
        .route("/api/hooks", get(list_hooks).post(create_hook))
        .route("/api/hooks/{id}", axum::routing::put(update_hook).delete(delete_hook))
        .route("/api/hooks/{id}/test", post(test_hook))
        .route("/api/dashboards", get(list_dashboards).post(create_dashboard))
        .route("/api/dashboards/{id}", get(get_dashboard).put(update_dashboard).delete(delete_dashboard))
        .with_state(p)
}
