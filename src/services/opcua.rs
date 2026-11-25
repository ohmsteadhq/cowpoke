//! OPC UA: a server (tags as variables in Cowpoke's own namespace) and a client
//! (reads and writes a real server's nodes). Supports None, Basic256Sha256,
//! Aes128Sha256RsaOaep and Aes256Sha256RsaPss, with a per-service trust list.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use opcua::client::{ClientBuilder, IdentityToken};
use opcua::crypto::{CertificateStore, SecurityPolicy};
use opcua::server::address_space::VariableBuilder;
use opcua::server::diagnostics::NamespaceMetadata;
use opcua::server::node_manager::memory::{SimpleNodeManager, simple_node_manager};
use opcua::server::{ANONYMOUS_USER_TOKEN_ID, ServerBuilder, ServerUserToken};
use opcua::types::{
    AttributeId, DataTypeId, DataValue, DateTime, MessageSecurityMode, NodeId, NumericRange,
    ObjectId, ReadValueId, StatusCode, TimestampsToReturn, Variant, VariableTypeId, WriteValue,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::manager::Ctx;
use crate::model::{DataType, TagDef};
use crate::tags::{Origin, Quality, as_f64};

// ------------------------------------------------------------------ security

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    None,
    #[serde(rename = "basic256sha256")]
    Basic256Sha256,
    Aes128Sha256RsaOaep,
    Aes256Sha256RsaPss,
}

impl Policy {
    fn ua(self) -> SecurityPolicy {
        match self {
            Policy::None => SecurityPolicy::None,
            Policy::Basic256Sha256 => SecurityPolicy::Basic256Sha256,
            Policy::Aes128Sha256RsaOaep => SecurityPolicy::Aes128Sha256RsaOaep,
            Policy::Aes256Sha256RsaPss => SecurityPolicy::Aes256Sha256RsaPss,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecMode {
    None,
    Sign,
    SignAndEncrypt,
}

impl SecMode {
    fn ua(self) -> MessageSecurityMode {
        match self {
            SecMode::None => MessageSecurityMode::None,
            SecMode::Sign => MessageSecurityMode::Sign,
            SecMode::SignAndEncrypt => MessageSecurityMode::SignAndEncrypt,
        }
    }
}

fn all_policies() -> Vec<Policy> {
    vec![
        Policy::None,
        Policy::Basic256Sha256,
        Policy::Aes128Sha256RsaOaep,
        Policy::Aes256Sha256RsaPss,
    ]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserPass {
    username: String,
    password: String,
}

// ------------------------------------------------------------------ types

fn ua_type(t: &TagDef) -> DataTypeId {
    if t.is_scaled() && t.datatype != DataType::Bool && t.datatype != DataType::String {
        return DataTypeId::Double;
    }
    match t.datatype {
        DataType::Bool => DataTypeId::Boolean,
        DataType::Int16 => DataTypeId::Int16,
        DataType::Uint16 => DataTypeId::UInt16,
        DataType::Int32 => DataTypeId::Int32,
        DataType::Uint32 => DataTypeId::UInt32,
        DataType::Int64 => DataTypeId::Int64,
        DataType::Uint64 => DataTypeId::UInt64,
        DataType::Float32 => DataTypeId::Float,
        DataType::Float64 => DataTypeId::Double,
        DataType::String => DataTypeId::String,
    }
}

/// A tag value as the Variant type the tag declares.
pub fn to_variant(t: &TagDef, v: &Value) -> Variant {
    let n = as_f64(v)
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0.0);
    match ua_type(t) {
        DataTypeId::Boolean => Variant::from(crate::codec::truthy(v)),
        DataTypeId::Int16 => Variant::from(n.round() as i16),
        DataTypeId::UInt16 => Variant::from(n.round() as u16),
        DataTypeId::Int32 => Variant::from(n.round() as i32),
        DataTypeId::UInt32 => Variant::from(n.round() as u32),
        DataTypeId::Int64 => Variant::from(n.round() as i64),
        DataTypeId::UInt64 => Variant::from(n.round() as u64),
        DataTypeId::Float => Variant::from(n as f32),
        DataTypeId::String => Variant::from(match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }),
        _ => Variant::from(n),
    }
}

pub fn from_variant(t: &TagDef, v: &Variant) -> Option<Value> {
    let v = v.clone();
    Some(match t.datatype {
        DataType::Bool => json!(v.try_cast_to::<bool>().ok()?),
        DataType::String => json!(v.try_cast_to::<String>().ok()?),
        DataType::Float32 if !t.is_scaled() => {
            let f = v.try_cast_to::<f32>().ok()?;
            json!(f.to_string().parse::<f64>().unwrap_or(f as f64))
        }
        _ if t.is_scaled() || matches!(t.datatype, DataType::Float32 | DataType::Float64) => {
            json!(v.try_cast_to::<f64>().ok()?)
        }
        DataType::Uint16 | DataType::Uint32 | DataType::Uint64 => {
            json!(v.try_cast_to::<u64>().ok()?)
        }
        _ => json!(v.try_cast_to::<i64>().ok()?),
    })
}

// ------------------------------------------------------------------ server

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerSettings {
    #[serde(default = "crate::services::any_host")]
    bind: String,
    #[serde(default = "default_port")]
    port: u16,
    /// Host name clients use to reach this server (goes into the endpoint URL and certificate).
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    namespace_uri: Option<String>,
    /// Security policies to offer. Each secured policy gets Sign and SignAndEncrypt endpoints.
    #[serde(default = "all_policies")]
    security_policies: Vec<Policy>,
    #[serde(default = "yes")]
    allow_anonymous: bool,
    #[serde(default)]
    users: Vec<UserPass>,
    /// Trust every client certificate (otherwise new ones land in the trust list's rejected folder).
    #[serde(default)]
    trust_client_certs: bool,
}

fn default_port() -> u16 {
    4840
}
fn yes() -> bool {
    true
}

pub fn pki_dir(service_dir: &Path) -> PathBuf {
    service_dir.join("pki")
}

fn hostname(explicit: &Option<String>) -> String {
    explicit
        .clone()
        .or_else(|| opcua::crypto::X509Data::computer_hostnames().into_iter().next())
        .unwrap_or_else(|| "localhost".into())
}

pub async fn server(ctx: &mut Ctx) -> Result<()> {
    let s: ServerSettings = ctx.settings()?;
    let host = hostname(&s.hostname);
    let ns_uri = s
        .namespace_uri
        .clone()
        .unwrap_or_else(|| format!("urn:cowpoke:{}", ctx.def.id));
    let app_uri = format!("urn:cowpoke:{}:server", ctx.def.id);

    // Built in its own scope: ServerBuilder is not Send, so it must not live across an await.
    let (server, handle, endpoints) = {
        let mut token_ids: Vec<String> = vec![];
        if s.allow_anonymous {
            token_ids.push(ANONYMOUS_USER_TOKEN_ID.to_string());
        }
        let mut builder = ServerBuilder::new()
            .application_name(format!("Cowpoke {}", ctx.def.name_or_id()))
            .application_uri(&app_uri)
            .product_uri("urn:cowpoke")
            // The advertised host; the listener below binds `bind` separately.
            .host(&host)
            .port(s.port)
            .discovery_urls(vec![format!("opc.tcp://{host}:{}/", s.port)])
            .pki_dir(pki_dir(&ctx.dir))
            .create_sample_keypair(true)
            .trust_client_certs(s.trust_client_certs)
            .token(ctx.cancel.child_token());
        for (i, u) in s.users.iter().enumerate() {
            let id = format!("user{i}");
            builder = builder.add_user_token(&id, ServerUserToken::user_pass(u.username.clone(), u.password.clone()));
            token_ids.push(id);
        }
        if token_ids.is_empty() {
            bail!("allow anonymous access or add at least one user");
        }
        let ids: Vec<&str> = token_ids.iter().map(|s| s.as_str()).collect();
        let mut endpoints = vec![];
        for p in &s.security_policies {
            let modes: &[(SecMode, &str)] = if *p == Policy::None {
                &[(SecMode::None, "")]
            } else {
                &[(SecMode::Sign, "_sign"), (SecMode::SignAndEncrypt, "_sign_encrypt")]
            };
            for (m, suffix) in modes {
                let name = format!("{}{suffix}", serde_json::to_value(p)?.as_str().unwrap_or("x"));
                builder = builder.add_endpoint(&name, ("/", p.ua(), m.ua(), &ids as &[&str]));
                endpoints.push(name);
            }
        }
        if endpoints.is_empty() {
            bail!("security_policies is empty");
        }
        builder = builder.with_node_manager(simple_node_manager(
            NamespaceMetadata {
                namespace_uri: ns_uri.clone(),
                ..Default::default()
            },
            "cowpoke",
        ));
        let (server, handle) = builder.build().map_err(|e| anyhow!("{e}"))?;
        (server, handle, endpoints)
    };
    let nm: Arc<SimpleNodeManager> = handle
        .node_managers()
        .get_of_type::<SimpleNodeManager>()
        .ok_or_else(|| anyhow!("node manager missing"))?;
    let ns = handle
        .get_namespace_index(&ns_uri)
        .ok_or_else(|| anyhow!("namespace missing"))?;

    // Subscribe before reading initial values so nothing is missed.
    let mut events = ctx.tags.subscribe();
    let (wtx, mut wrx) = tokio::sync::mpsc::unbounded_channel::<(String, NodeId, DataValue)>();
    let folder = NodeId::new(ns, ctx.def.id.clone());
    let mut node_of: Vec<(String, NodeId)> = vec![];
    {
        let mut space = nm.address_space().write();
        space.add_folder(
            &folder,
            ctx.def.name_or_id(),
            ctx.def.name_or_id(),
            &ObjectId::ObjectsFolder.into(),
        );
        for t in &ctx.def.tags {
            let ident = t
                .node_id
                .clone()
                .map(|n| n.trim_start_matches("s=").to_string())
                .unwrap_or_else(|| t.name.clone());
            let node = NodeId::new(ns, ident);
            let v = ctx
                .tags
                .value(&ctx.def.id, &t.name)
                .unwrap_or(Value::Null);
            let mut vb = VariableBuilder::new(&node, &t.name, &t.name)
                .data_type(ua_type(t))
                .value(to_variant(t, &v))
                .has_type_definition(VariableTypeId::BaseDataVariableType)
                .organized_by(folder.clone());
            if let Some(d) = &t.description {
                vb = vb.description(d.as_str());
            }
            if t.writable {
                vb = vb.writable();
            }
            vb.insert(&mut *space);
            node_of.push((t.name.clone(), node));
        }
    }
    for (name, node) in &node_of {
        let tx = wtx.clone();
        let (name, n2) = (name.clone(), node.clone());
        nm.inner().add_write_callback(node.clone(), move |dv: DataValue, _r: &NumericRange| {
            // Never touch the node manager here: its lock is held.
            let _ = tx.send((name.clone(), n2.clone(), dv));
            StatusCode::Good
        });
    }

    let listener = crate::services::listen(&s.bind, s.port).await?;
    ctx.status.running(format!(
        "opc.tcp://{host}:{}/  namespace {ns_uri} (ns={ns}), {} endpoints",
        s.port,
        endpoints.len()
    ));

    let run = server.run_with(listener);
    tokio::pin!(run);
    let tags = ctx.def.tags.clone();
    let id = ctx.def.id.clone();
    loop {
        tokio::select! {
            r = &mut run => {
                r.map_err(|e| anyhow!("{e}"))?;
                return Ok(());
            }
            Some((name, node, dv)) = wrx.recv() => {
                // A client wrote: store it in the node (the callback doesn't) and in Cowpoke.
                let _ = nm.set_value(handle.subscriptions(), &node, None, DataValue::new_now(dv.value.clone().unwrap_or_default()));
                if let (Some(t), Some(v)) = (tags.iter().find(|t| t.name == name), dv.value.as_ref()) {
                    if let Some(j) = from_variant(t, v) {
                        ctx.tags.set(&id, &name, j, Quality::Good, Origin::Service);
                    }
                }
            }
            ev = events.recv() => match ev {
                Ok(e) if e.service == id && e.origin != Origin::Service => {
                    if let (Some(t), Some((_, node))) = (tags.iter().find(|t| t.name == e.tag), node_of.iter().find(|(n, _)| *n == e.tag)) {
                        let _ = nm.set_value(handle.subscriptions(), node, None, DataValue::new_now(to_variant(t, &e.state.value)));
                    }
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => {}
            },
            _ = ctx.cancel.cancelled() => {
                handle.cancel();
                let _ = tokio::time::timeout(Duration::from_secs(3), &mut run).await;
                return Ok(());
            }
            Some(w) = ctx.writes.recv() => {
                let _ = w.reply.send(Err("server-mode values are written directly".into()));
            }
        }
    }
}

// ------------------------------------------------------------------ client

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientSettings {
    /// `opc.tcp://host:4840/path`
    url: String,
    #[serde(default = "policy_none")]
    security_policy: Policy,
    #[serde(default = "mode_none")]
    security_mode: SecMode,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    /// Trust the server's certificate automatically (otherwise trust it from the service's certificate list).
    #[serde(default)]
    trust_server_certs: bool,
    /// `poll` reads every tag every `poll_ms`; `subscribe` asks the server to send changes.
    #[serde(default)]
    read: ReadMode,
    /// Poll period, or in subscribe mode the publishing interval.
    #[serde(default = "default_poll")]
    poll_ms: u64,
    /// Subscribe mode: how often the server samples each node (default: the publishing interval).
    #[serde(default)]
    sampling_ms: Option<u64>,
    /// Subscribe mode: absolute deadband; numeric changes smaller than this aren't sent.
    #[serde(default)]
    deadband: Option<f64>,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
enum ReadMode {
    #[default]
    Poll,
    Subscribe,
}

fn policy_none() -> Policy {
    Policy::None
}
fn mode_none() -> SecMode {
    SecMode::None
}
fn default_poll() -> u64 {
    1000
}
fn default_timeout() -> u64 {
    10_000
}

pub async fn client(ctx: &mut Ctx) -> Result<()> {
    let s: ClientSettings = ctx.settings()?;
    if (s.security_policy == Policy::None) != (s.security_mode == SecMode::None) {
        bail!("security_policy none goes with security_mode none, and only with it");
    }
    let mut client = ClientBuilder::new()
        .application_name(format!("Cowpoke {}", ctx.def.name_or_id()))
        .application_uri(format!("urn:cowpoke:{}:client", ctx.def.id))
        .product_uri("urn:cowpoke")
        .pki_dir(pki_dir(&ctx.dir))
        .create_sample_keypair(true)
        .trust_server_certs(s.trust_server_certs)
        .session_retry_limit(0)
        .client()
        .map_err(|e| anyhow!("{}", e.join(", ")))?;
    let identity = match (&s.username, &s.password) {
        (Some(u), Some(p)) => IdentityToken::new_user_name(u.clone(), p.clone()),
        (None, None) => IdentityToken::Anonymous,
        _ => bail!("set both username and password, or neither"),
    };
    let timeout = Duration::from_millis(s.timeout_ms);
    let (session, event_loop) = tokio::time::timeout(
        timeout,
        client.connect_to_matching_endpoint(
            (
                s.url.as_str(),
                s.security_policy.ua().to_str(),
                s.security_mode.ua(),
            ),
            identity,
        ),
    )
    .await
    .map_err(|_| anyhow!("timed out connecting to {}", s.url))?
    .map_err(|e| anyhow!("connecting to {}: {e}", s.url))?;
    let mut lp = event_loop.spawn();
    if !tokio::time::timeout(timeout, session.wait_for_connection())
        .await
        .unwrap_or(false)
    {
        lp.abort();
        bail!("couldn't open a session with {}", s.url);
    }
    let tags = ctx.def.tags.clone();
    let nodes: Vec<NodeId> = tags
        .iter()
        .map(|t| {
            t.node_id
                .as_deref()
                .unwrap_or_default()
                .parse::<NodeId>()
                .map_err(|_| anyhow!("tag `{}`: bad node_id", t.name))
        })
        .collect::<Result<_>>()?;
    let mut to_read: Vec<ReadValueId> = nodes.iter().map(ReadValueId::from).collect();
    // Server/ServerStatus/CurrentTime, read with the tags: the server's clock, for drift checks.
    let has_tags = !to_read.is_empty();
    to_read.push(ReadValueId::from(&NodeId::new(0, 2258u32)));
    let id = ctx.def.id.clone();
    let subscribe = s.read == ReadMode::Subscribe;
    // Subscribe mode: changes come in on `changes`; the timer only reads the server's clock.
    let (ctx_tx, mut changes) = tokio::sync::mpsc::unbounded_channel::<(usize, DataValue)>();
    if subscribe && has_tags {
        match subscribe_tags(&session, &s, &nodes, ctx_tx, timeout).await {
            Ok(refused) => {
                for (i, code) in &refused {
                    ctx.tags.set_bad(&id, &tags[*i].name);
                    tracing::warn!("{id}: server refused to monitor `{}`: {code}", tags[*i].name);
                }
                let note = if refused.is_empty() {
                    String::new()
                } else {
                    let names: Vec<&str> = refused.iter().map(|(i, _)| tags[*i].name.as_str()).collect();
                    format!(", refused: {}", names.join(", "))
                };
                ctx.status.running(format!("connected to {}, subscribed to {} tags{note}", s.url, tags.len() - refused.len()));
            }
            Err(e) => {
                let _ = tokio::time::timeout(Duration::from_secs(2), session.disconnect()).await;
                lp.abort();
                return Err(e);
            }
        }
    } else {
        ctx.status.running(format!("connected to {}", s.url));
    }
    // In subscribe mode the clock check alone runs on the timer, at most every 5 s.
    let period = if subscribe { s.poll_ms.max(5000) } else { s.poll_ms.max(50) };
    let mut tick = tokio::time::interval(Duration::from_millis(period));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let clock_only = [ReadValueId::from(&NodeId::new(0, 2258u32))];

    let result: Result<()> = loop {
        tokio::select! {
            _ = ctx.cancel.cancelled() => break Ok(()),
            r = &mut lp => break Err(anyhow!("session ended: {}", r.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string()))),
            Some((i, dv)) = changes.recv() => {
                let t = &tags[i];
                match dv.value.as_ref().and_then(|v| from_variant(t, v)) {
                    Some(j) if dv.status().is_good() => ctx.tags.set(&id, &t.name, j, Quality::Good, Origin::Service),
                    Some(j) if !dv.status().is_bad() => ctx.tags.set(&id, &t.name, j, Quality::Uncertain, Origin::Service),
                    _ => ctx.tags.set_bad(&id, &t.name),
                }
            }
            _ = tick.tick() => {
                if subscribe {
                    let sent = chrono::Utc::now();
                    match tokio::time::timeout(timeout, session.read(&clock_only, TimestampsToReturn::Neither, 0.0)).await {
                        Err(_) => break Err(anyhow!("timed out reading the server's clock")),
                        Ok(Err(e)) => break Err(anyhow!("reading the server's clock: {e}")),
                        Ok(Ok(mut v)) => {
                            if let Some(Variant::DateTime(st)) = v.pop().and_then(|dv| dv.value) {
                                let mid = sent + (chrono::Utc::now() - sent) / 2;
                                crate::insights::note_clock_offset(&id, (st.as_chrono() - mid).num_milliseconds() as f64);
                            }
                        }
                    }
                    continue;
                }
                if !has_tags { continue; }
                let sent = chrono::Utc::now();
                let mut vals = match tokio::time::timeout(timeout, session.read(&to_read, TimestampsToReturn::Neither, 0.0)).await {
                    Err(_) => break Err(anyhow!("timed out reading")),
                    Ok(Err(e)) => break Err(anyhow!("reading: {e}")),
                    Ok(Ok(v)) => v,
                };
                // The server's clock against ours, at the middle of the round trip.
                if let Some(Variant::DateTime(st)) = vals.pop().and_then(|dv| dv.value) {
                    let mid = sent + (chrono::Utc::now() - sent) / 2;
                    crate::insights::note_clock_offset(&id, (st.as_chrono() - mid).num_milliseconds() as f64);
                }
                for (t, dv) in tags.iter().zip(vals) {
                    match dv.value.as_ref().and_then(|v| from_variant(t, v)) {
                        Some(j) if dv.status().is_good() => ctx.tags.set(&id, &t.name, j, Quality::Good, Origin::Service),
                        Some(j) => ctx.tags.set(&id, &t.name, j, Quality::Uncertain, Origin::Service),
                        None => ctx.tags.set_bad(&id, &t.name),
                    }
                }
            }
            Some(w) = ctx.writes.recv() => {
                let Some(i) = tags.iter().position(|t| t.name == w.tag) else {
                    let _ = w.reply.send(Err(format!("no tag `{}`", w.tag)));
                    continue;
                };
                let wv = WriteValue {
                    node_id: nodes[i].clone(),
                    attribute_id: AttributeId::Value as u32,
                    index_range: NumericRange::None,
                    value: DataValue {
                        value: Some(to_variant(&tags[i], &w.value)),
                        source_timestamp: Some(DateTime::now()),
                        ..Default::default()
                    },
                };
                let r = match tokio::time::timeout(timeout, session.write(&[wv])).await {
                    Err(_) => Err("timed out writing".to_string()),
                    Ok(Err(e)) => Err(e.to_string()),
                    Ok(Ok(codes)) => match codes.first() {
                        Some(c) if c.is_good() => Ok(()),
                        Some(c) => Err(format!("server refused: {c}")),
                        None => Err("no result".into()),
                    },
                };
                if r.is_ok() {
                    ctx.tags.set(&id, &w.tag, w.value.clone(), Quality::Good, Origin::Service);
                }
                let _ = w.reply.send(r);
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(2), session.disconnect()).await;
    lp.abort();
    result
}

/// Creates one subscription with a monitored item per tag. Each data change goes to `tx`
/// as (tag index, value). Returns the tags the server refused to monitor, with its reason.
async fn subscribe_tags(
    session: &Arc<opcua::client::Session>,
    s: &ClientSettings,
    nodes: &[NodeId],
    tx: tokio::sync::mpsc::UnboundedSender<(usize, DataValue)>,
    timeout: Duration,
) -> Result<Vec<(usize, StatusCode)>> {
    use opcua::client::DataChangeCallback;
    use opcua::types::{
        DataChangeFilter, DataChangeTrigger, DeadbandType, ExtensionObject, MonitoredItemCreateRequest,
        MonitoringMode, MonitoringParameters,
    };
    let publish = Duration::from_millis(s.poll_ms.max(50));
    let sub_id = tokio::time::timeout(
        timeout,
        session.create_subscription(
            publish,
            // Lifetime and keep-alive counts, in publishing intervals: the server keeps the
            // subscription for 60 intervals without a publish request and sends an empty
            // keep-alive at least every 10, so a quiet server still proves it's there.
            60,
            10,
            0,
            0,
            true,
            DataChangeCallback::new(move |dv, item| {
                // Client handles are tag index + 1 (0 means "pick one for me").
                let _ = tx.send((item.client_handle() as usize - 1, dv));
            }),
        ),
    )
    .await
    .map_err(|_| anyhow!("timed out creating the subscription"))?
    .map_err(|e| anyhow!("creating the subscription: {e}"))?;

    let sampling = s.sampling_ms.unwrap_or(s.poll_ms) as f64;
    let filter = match s.deadband {
        Some(d) if d > 0.0 => ExtensionObject::from_message(DataChangeFilter {
            trigger: DataChangeTrigger::StatusValue,
            deadband_type: DeadbandType::Absolute as u32,
            deadband_value: d,
        }),
        _ => ExtensionObject::null(),
    };
    let items: Vec<MonitoredItemCreateRequest> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            MonitoredItemCreateRequest::new(
                n.into(),
                MonitoringMode::Reporting,
                MonitoringParameters {
                    client_handle: i as u32 + 1,
                    sampling_interval: sampling,
                    filter: filter.clone(),
                    queue_size: 1,
                    discard_oldest: true,
                },
            )
        })
        .collect();
    let created = tokio::time::timeout(timeout, session.create_monitored_items(sub_id, TimestampsToReturn::Source, items))
        .await
        .map_err(|_| anyhow!("timed out creating monitored items"))?
        .map_err(|e| anyhow!("creating monitored items: {e}"))?;
    let mut refused = vec![];
    for (i, r) in created.iter().enumerate() {
        if !r.result.status_code.is_good() {
            refused.push((i, r.result.status_code));
        }
    }
    // A deadband on a node that isn't numeric is refused with BadFilterNotAllowed:
    // monitor those again without a filter.
    if s.deadband.is_some() {
        let retry: Vec<usize> = refused
            .iter()
            .filter(|(_, c)| *c == StatusCode::BadFilterNotAllowed || *c == StatusCode::BadMonitoredItemFilterUnsupported)
            .map(|(i, _)| *i)
            .collect();
        if !retry.is_empty() {
            let items = retry
                .iter()
                .map(|&i| {
                    MonitoredItemCreateRequest::new(
                        (&nodes[i]).into(),
                        MonitoringMode::Reporting,
                        MonitoringParameters {
                            client_handle: i as u32 + 1,
                            sampling_interval: sampling,
                            filter: ExtensionObject::null(),
                            queue_size: 1,
                            discard_oldest: true,
                        },
                    )
                })
                .collect();
            let again = tokio::time::timeout(timeout, session.create_monitored_items(sub_id, TimestampsToReturn::Source, items))
                .await
                .map_err(|_| anyhow!("timed out creating monitored items"))?
                .map_err(|e| anyhow!("creating monitored items: {e}"))?;
            for (&i, r) in retry.iter().zip(again.iter()) {
                if r.result.status_code.is_good() {
                    refused.retain(|(j, _)| *j != i);
                }
            }
        }
    }
    Ok(refused)
}

// ------------------------------------------------------------------ trust list

#[derive(Serialize)]
pub struct CertInfo {
    pub file: String,
    pub common_name: String,
    pub thumbprint: String,
    pub trusted: bool,
}

/// Certificates in a service's trust list (`trusted/` and `rejected/`).
pub fn list_certs(service_dir: &Path) -> Vec<CertInfo> {
    let pki = pki_dir(service_dir);
    let mut out = vec![];
    for (sub, trusted) in [("trusted", true), ("rejected", false)] {
        let Ok(rd) = std::fs::read_dir(pki.join(sub)) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("der") {
                continue;
            }
            let (cn, tp) = match CertificateStore::read_cert(&p) {
                Ok(c) => (
                    c.common_name().unwrap_or_default(),
                    c.thumbprint().as_hex_string(),
                ),
                Err(_) => (String::new(), String::new()),
            };
            out.push(CertInfo {
                file: e.file_name().to_string_lossy().into_owned(),
                common_name: cn,
                thumbprint: tp,
                trusted,
            });
        }
    }
    out.sort_by(|a, b| a.file.cmp(&b.file));
    out
}

/// Move a certificate between `rejected/` and `trusted/`, or delete it.
pub fn set_cert(service_dir: &Path, file: &str, action: &str) -> Result<(), String> {
    if file.contains('/') || file.contains('\\') || file.contains("..") || !file.ends_with(".der") {
        return Err("bad file name".into());
    }
    let pki = pki_dir(service_dir);
    let (rej, tru) = (pki.join("rejected").join(file), pki.join("trusted").join(file));
    let r = match action {
        "trust" => {
            std::fs::create_dir_all(pki.join("trusted")).ok();
            std::fs::rename(&rej, &tru)
        }
        "reject" => {
            std::fs::create_dir_all(pki.join("rejected")).ok();
            std::fs::rename(&tru, &rej)
        }
        "delete" => std::fs::remove_file(&rej).or_else(|_| std::fs::remove_file(&tru)),
        _ => return Err("action is trust, reject or delete".into()),
    };
    r.map_err(|e| e.to_string())
}

/// Thumbprint of a certificate file, if it can be read.
pub fn crypto_thumbprint(path: &Path) -> Option<String> {
    CertificateStore::read_cert(path)
        .ok()
        .map(|c| c.thumbprint().as_hex_string())
}

/// Check settings without running the service (used by the form and `/api/services/check`).
pub fn check_settings(mode: crate::model::Mode, v: &Value) -> Result<(), String> {
    let r = match mode {
        crate::model::Mode::Server => serde_json::from_value::<ServerSettings>(v.clone()).map(|_| ()),
        crate::model::Mode::Client => serde_json::from_value::<ClientSettings>(v.clone()).map(|_| ()),
    };
    r.map_err(|e| format!("settings: {e}"))
}
