//! The foreman: diagnoses communication misconfigurations.
//!
//! Cowpoke gathers the evidence itself (each service's status, settings, tag map and live
//! values), compares every client with the device simulator of the same kind (rule checks),
//! and asks a small language model to name the misconfigured service, the cause, and the
//! fix. The model is either built in (mistral.rs loading a GGUF file, Cargo feature
//! `foreman`) or external (any OpenAI-compatible chat endpoint, e.g. llama.cpp's server).
//!
//! The evidence text reproduces, byte for byte, what the fine-tuned models were trained on
//! (bench/foreman/gen.py and bench.py): Python-style JSON spacing, key order and the exact
//! rule-finding sentences. Change it only together with the training data.

use std::time::Instant;

use anyhow::{Result, anyhow};
use serde::Serialize;
use serde_json::{Value, json};

use crate::model::{Mode, ServiceDef};
use crate::tags::TagStore;

pub const SYSTEM_PROMPT: &str = include_str!("system_prompt.txt");


// ------------------------------------------------------------------ evidence

/// Render JSON as Python's `json.dumps` does by default (", " and ": " separators,
/// non-ASCII escaped), keeping object key order as given.
pub fn py_json(v: &Value) -> String {
    let mut out = String::new();
    write_py(v, &mut out);
    out
}

fn write_py(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            // Python prints whole floats as "1.0"; serde_json keeps the number as parsed.
            out.push_str(&n.to_string())
        }
        Value::String(s) => {
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                        let mut buf = [0u16; 2];
                        for u in c.encode_utf16(&mut buf) {
                            out.push_str(&format!("\\u{u:04x}"));
                        }
                    }
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_py(x, out);
            }
            out.push(']');
        }
        Value::Object(o) => {
            out.push('{');
            for (i, (k, x)) in o.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_py(&Value::String(k.clone()), out);
                out.push_str(": ");
                write_py(x, out);
            }
            out.push('}');
        }
    }
}

/// Python's `str()` of a JSON value, as the rule-finding sentences print it.
fn py_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "True" } else { "False" }.into(),
        Some(other) => other.to_string(),
    }
}

/// Python's `repr()` of an optional string.
fn py_repr(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => format!("'{s}'"),
        Some(other) => other.to_string(),
    }
}

/// Equality as Python compares JSON values (1 == 1.0).
fn same(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
        (None, Some(Value::Null)) | (Some(Value::Null), None) => true,
        _ => a == b,
    }
}

/// One service as the foreman sees it.
#[derive(Clone, Debug)]
pub struct Seen {
    pub id: String,
    pub kind: String,
    pub mode: Mode,
    pub state: Value,
    pub detail: Value,
    pub settings: Value,
    /// The tag definitions, as the configuration serializes them.
    pub tags_config: Value,
    /// (name, value, quality, units) per tag.
    pub live: Vec<(String, Value, Value, Value)>,
}

impl Seen {
    pub fn gather(def: &ServiceDef, status: &Value, tags: &TagStore) -> Self {
        let tags_config = serde_json::to_value(&def.tags).unwrap_or(Value::Null);
        let live = def
            .tags
            .iter()
            .map(|t| {
                let st = tags.get(&def.id, &t.name);
                (
                    t.name.clone(),
                    st.as_ref().map(|s| s.value.clone()).unwrap_or(Value::Null),
                    st.as_ref().map(|s| serde_json::to_value(s.quality).unwrap_or(Value::Null)).unwrap_or(Value::Null),
                    t.units.clone().map(Value::String).unwrap_or(Value::Null),
                )
            })
            .collect();
        Self {
            id: def.id.clone(),
            kind: serde_json::to_value(def.kind).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default(),
            mode: def.mode,
            state: status.get("state").cloned().unwrap_or(Value::Null),
            detail: status.get("detail").cloned().unwrap_or(Value::Null),
            settings: def.settings.clone(),
            tags_config,
            live,
        }
    }

    /// One evidence line, in the training data's key order.
    fn line(&self) -> String {
        let live: Vec<String> = self
            .live
            .iter()
            .map(|(n, v, q, u)| py_obj(&[("name", py_json(&json!(n))), ("value", py_json(v)), ("quality", py_json(q)), ("units", py_json(u))]))
            .collect();
        py_obj(&[
            ("id", py_json(&json!(self.id))),
            ("kind", py_json(&json!(self.kind))),
            ("mode", py_json(&json!(if self.mode == Mode::Client { "client" } else { "server" }))),
            ("state", py_json(&self.state)),
            ("detail", py_json(&self.detail)),
            ("settings", py_json(&self.settings)),
            ("tags_config", py_json(&self.tags_config)),
            ("live_tags", format!("[{}]", live.join(", "))),
        ])
    }
}

/// A JSON object from already-rendered values, in the given key order.
fn py_obj(pairs: &[(&str, String)]) -> String {
    let body: Vec<String> = pairs.iter().map(|(k, v)| format!("{}: {v}", py_json(&json!(k)))).collect();
    format!("{{{}}}", body.join(", "))
}

/// The port a client connects to (or a server listens on): `port`, or the port in `url`.
fn port_of(settings: &Value) -> Option<i64> {
    if let Some(p) = settings.get("port").and_then(|p| p.as_i64()) {
        if p != 0 {
            return Some(p);
        }
    }
    let url = settings.get("url")?.as_str()?;
    let rest = url.split("://").nth(1)?;
    let host_port = rest.split('/').next()?;
    host_port.rsplit_once(':')?.1.parse().ok()
}

const TAG_FIELDS: &[&str] = &[
    "register", "table", "unit", "length", "scale", "offset", "port", "pd", "bit_offset", "bit_length", "node_id",
    "datatype",
];

fn tag_default(k: &str) -> Option<Value> {
    Some(match k {
        "unit" => json!(1),
        "table" => json!("holding_register"),
        "scale" => json!(1),
        "offset" => json!(0),
        "bit_offset" => json!(0),
        "pd" => json!("in"),
        _ => return None,
    })
}

/// Compare each client with each simulator of the same kind, field by field.
/// Secrets are compared, never shown.
pub fn rule_checks(seen: &[Seen]) -> Vec<String> {
    let mut out = vec![];
    for c in seen.iter().filter(|s| s.mode == Mode::Client) {
        for sv in seen.iter().filter(|s| s.mode == Mode::Server && s.kind == c.kind) {
            let (cs, ss) = (&c.settings, &sv.settings);
            if let (Some(a), Some(b)) = (port_of(cs), port_of(ss)) {
                if a != b {
                    out.push(format!("{} connects to port {a}, but {} listens on port {b}.", c.id, sv.id));
                }
            }
            let order = |s: &Value| s.get("word_order").cloned().unwrap_or(json!("big"));
            let (a, b) = (order(cs), order(ss));
            if a != b {
                out.push(format!("{} uses word_order {}, but {} uses {}.", c.id, py_str(Some(&a)), sv.id, py_str(Some(&b))));
            }
            for k in ["username", "password"] {
                let want = ss.get(k).filter(|v| !v.is_null());
                if want.is_some() && !same(cs.get(k), want) {
                    out.push(format!("{}'s {k} does not match the one {} requires.", c.id, sv.id));
                }
            }
            let csp = cs.get("sparkplug").filter(|v| v.is_object());
            let ssp = ss.get("sparkplug").filter(|v| v.is_object());
            if csp.is_some() || ssp.is_some() {
                for k in ["group_id", "edge_node", "device"] {
                    let (a, b) = (csp.and_then(|m| m.get(k)), ssp.and_then(|m| m.get(k)));
                    if !same(a, b) {
                        out.push(format!(
                            "Sparkplug {k}: {} expects {}, {} publishes {}.",
                            c.id, py_repr(a), sv.id, py_repr(b)
                        ));
                    }
                }
            }
            let stags = sv.tags_config.as_array().cloned().unwrap_or_default();
            for t in c.tags_config.as_array().into_iter().flatten() {
                let name = t.get("name").and_then(|n| n.as_str()).unwrap_or_default();
                let Some(st) = stags.iter().find(|x| x.get("name").and_then(|n| n.as_str()) == Some(name)) else {
                    continue;
                };
                for k in TAG_FIELDS {
                    let a = t.get(*k).cloned().or_else(|| tag_default(k));
                    let b = st.get(*k).cloned().or_else(|| tag_default(k));
                    if !same(a.as_ref(), b.as_ref()) {
                        out.push(format!(
                            "Tag {name}: {} has {k} {}, but {} has {}.",
                            c.id, py_str(a.as_ref()), sv.id, py_str(b.as_ref())
                        ));
                    }
                }
                let s = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
                match c.kind.as_str() {
                    "eip" => {
                        let a = s(t, "cip_tag").unwrap_or_else(|| name.to_string());
                        let b = s(st, "cip_tag").unwrap_or_else(|| name.to_string());
                        if a.to_lowercase() != b.to_lowercase() {
                            out.push(format!("Tag {name}: {} reads Logix tag {a}, but {} has {b}.", c.id, sv.id));
                        }
                    }
                    "mqtt" if csp.is_none() => {
                        if let Some(topic) = s(t, "topic") {
                            let prefix = s(ss, "prefix").unwrap_or_else(|| sv.id.clone());
                            let want = s(st, "topic").unwrap_or_else(|| format!("{}/{name}", prefix.trim_end_matches('/')));
                            if topic != want {
                                out.push(format!(
                                    "Tag {name}: {} subscribes to {topic}, but {} publishes it on {want}.",
                                    c.id, sv.id
                                ));
                            }
                        }
                    }
                    "mqtt" => {
                        let a = s(t, "metric").unwrap_or_else(|| name.to_string());
                        let b = s(st, "metric").unwrap_or_else(|| name.to_string());
                        if a != b {
                            out.push(format!("Tag {name}: {} reads metric {a}, but {} publishes {b}.", c.id, sv.id));
                        }
                    }
                    "api" => {
                        let a = s(t, "pointer").unwrap_or_else(|| format!("/{name}"));
                        let b = format!("/{name}");
                        if a != b {
                            out.push(format!(
                                "Tag {name}: {} reads JSON pointer {a}, but {} serves it at {b}.",
                                c.id, sv.id
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

/// The user turn: the operator's report, the services, and (optionally) the rule findings.
pub fn evidence(symptom: &str, seen: &[Seen], rules: bool) -> (String, Vec<String>) {
    let mut text = format!(
        "Operator report: {symptom}\n\nServices:\n{}",
        seen.iter().map(|s| s.line()).collect::<Vec<_>>().join("\n")
    );
    let found = rule_checks(seen);
    if rules {
        text.push_str("\n\nRule checks (client compared with the device's simulator):\n");
        if found.is_empty() {
            text.push_str("- No differences between the client and the device settings.");
        } else {
            text.push_str(&found.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n"));
        }
    }
    (text, found)
}

/// The first JSON object in a reply (after any <think> block).
pub fn first_json(text: &str) -> Option<Value> {
    let text = match (text.find("<think>"), text.find("</think>")) {
        (Some(_), Some(e)) => &text[e + 8..],
        _ => text,
    };
    let b = text.as_bytes();
    let mut start = text.find('{')?;
    loop {
        let mut depth = 0i32;
        let mut in_str = false;
        let mut esc = false;
        for i in start..b.len() {
            let c = b[i];
            if in_str {
                match (esc, c) {
                    (true, _) => esc = false,
                    (false, b'\\') => esc = true,
                    (false, b'"') => in_str = false,
                    _ => {}
                }
                continue;
            }
            match c {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        if let Ok(v) = serde_json::from_str::<Value>(&text[start..=i]) {
                            return Some(v);
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
        start = start + 1 + text[start + 1..].find('{')?;
    }
}

// ------------------------------------------------------------------ engines

#[derive(Serialize, Clone, Debug)]
pub struct Diagnosis {
    pub service: Option<String>,
    pub cause: Option<String>,
    pub explanation: Option<String>,
    pub fix: Option<String>,
    /// The model's raw reply, when it couldn't be read as a diagnosis.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

impl Diagnosis {
    fn from_reply(reply: &str) -> Self {
        match first_json(reply) {
            Some(v) => {
                let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
                Self { service: s("service"), cause: s("cause"), explanation: s("explanation"), fix: s("fix"), raw: None }
            }
            None => Self { service: None, cause: None, explanation: None, fix: None, raw: Some(reply.chars().take(2000).collect()) },
        }
    }
}

/// Where the model runs.
pub enum Engine {
    /// Any OpenAI-compatible chat endpoint (llama.cpp's server, mlx_lm.server, Ollama, the hub).
    Http { url: String, model: String, http: reqwest::Client },
    /// Built in: a GGUF file loaded with mistral.rs.
    #[cfg(feature = "foreman")]
    Builtin(mistralrs::Model),
}

impl Engine {
    /// From the environment: `COWPOKE_FOREMAN_MODEL` (a .gguf file, built-in engine) or
    /// `COWPOKE_FOREMAN_URL` (+ optional `COWPOKE_FOREMAN_MODEL_NAME`).
    pub async fn from_env() -> Result<Self> {
        if let Ok(path) = std::env::var("COWPOKE_FOREMAN_MODEL") {
            return Self::builtin(&path).await;
        }
        if let Ok(url) = std::env::var("COWPOKE_FOREMAN_URL") {
            let model = std::env::var("COWPOKE_FOREMAN_MODEL_NAME").unwrap_or_else(|_| "default_model".into());
            return Ok(Self::Http { url, model, http: reqwest::Client::new() });
        }
        Err(anyhow!("no foreman model: set COWPOKE_FOREMAN_MODEL (a .gguf file) or COWPOKE_FOREMAN_URL"))
    }

    #[cfg(feature = "foreman")]
    pub async fn builtin(path: &str) -> Result<Self> {
        let p = std::path::Path::new(path);
        let dir = p.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_else(|| ".".into());
        let file = p.file_name().ok_or_else(|| anyhow!("{path}: not a file"))?.to_string_lossy().into_owned();
        let model = mistralrs::GgufModelBuilder::new(dir, vec![file]).build().await?;
        Ok(Self::Builtin(model))
    }

    #[cfg(not(feature = "foreman"))]
    pub async fn builtin(path: &str) -> Result<Self> {
        Err(anyhow!("this build has no built-in foreman engine (build with --features foreman); can't load {path}. Set COWPOKE_FOREMAN_URL instead"))
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Http { url, .. } => format!("external model at {url}"),
            #[cfg(feature = "foreman")]
            Self::Builtin(_) => "built-in model (mistral.rs)".into(),
        }
    }

    /// Ask the model; returns its reply text.
    pub async fn ask(&self, user: &str) -> Result<String> {
        match self {
            Self::Http { url, model, http } => {
                let body = json!({
                    "model": model, "temperature": 0, "max_tokens": 400,
                    "messages": [{"role": "system", "content": SYSTEM_PROMPT}, {"role": "user", "content": user}],
                    "chat_template_kwargs": {"enable_thinking": false},
                });
                let r: Value = http
                    .post(format!("{}/v1/chat/completions", url.trim_end_matches('/')))
                    .json(&body)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                Ok(r["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_string())
            }
            #[cfg(feature = "foreman")]
            Self::Builtin(m) => {
                use mistralrs::{RequestBuilder, TextMessageRole};
                let req = RequestBuilder::new()
                    .add_message(TextMessageRole::System, SYSTEM_PROMPT)
                    .add_message(TextMessageRole::User, user)
                    .set_deterministic_sampler()
                    .set_sampler_max_len(400)
                    .enable_thinking(false);
                let r = m.send_chat_request(req).await?;
                Ok(r.choices.first().and_then(|c| c.message.content.clone()).unwrap_or_default())
            }
        }
    }
}

/// A finished diagnosis with what it was based on.
#[derive(Serialize)]
pub struct Report {
    pub diagnosis: Diagnosis,
    pub rule_findings: Vec<String>,
    pub services: Vec<String>,
    pub engine: String,
    pub seconds: f64,
}

static ENGINE: tokio::sync::OnceCell<Engine> = tokio::sync::OnceCell::const_new();

/// The engine, loaded on first use from the environment.
pub async fn engine() -> Result<&'static Engine> {
    ENGINE.get_or_try_init(Engine::from_env).await
}

pub async fn diagnose(engine: &Engine, symptom: &str, seen: &[Seen], rules: bool) -> Result<Report> {
    let t0 = Instant::now();
    let (text, found) = evidence(symptom, seen, rules);
    let reply = engine.ask(&text).await?;
    Ok(Report {
        diagnosis: Diagnosis::from_reply(&reply),
        rule_findings: found,
        services: seen.iter().map(|s| s.id.clone()).collect(),
        engine: engine.describe(),
        seconds: (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_json_style() {
        let v: Value = serde_json::from_str(r#"{"b": [1, 2.5, "x"], "a": {"c": null, "d": true}}"#).unwrap();
        // serde_json sorts keys (as the training data's settings were); spacing matches Python.
        assert_eq!(py_json(&v), r#"{"a": {"c": null, "d": true}, "b": [1, 2.5, "x"]}"#);
        assert_eq!(py_json(&json!("\u{e9}\"")), r#""\u00e9\"""#);
    }

    #[test]
    fn reads_the_first_json_object() {
        assert_eq!(first_json("<think>\n\n</think>\n\n{\"service\": \"a\"} trailing").unwrap()["service"], "a");
        assert_eq!(first_json("text {not json} then {\"cause\": \"x}\"}").unwrap()["cause"], "x}");
        assert!(first_json("no json").is_none());
    }
}
