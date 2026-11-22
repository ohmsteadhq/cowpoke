//! The four service kinds, each with a client and a server mode.

pub mod api;
pub mod iolink;
pub mod modbus;
pub mod opcua;

use anyhow::{Context as _, Result};

use crate::manager::Ctx;
use crate::model::{Kind, Mode};

pub async fn run(ctx: &mut Ctx) -> Result<()> {
    match (ctx.def.kind, ctx.def.mode) {
        (Kind::Modbus, Mode::Server) => modbus::server(ctx).await,
        (Kind::Modbus, Mode::Client) => modbus::client(ctx).await,
        (Kind::Opcua, Mode::Server) => opcua::server(ctx).await,
        (Kind::Opcua, Mode::Client) => opcua::client(ctx).await,
        (Kind::Api, Mode::Server) => api::server(ctx).await,
        (Kind::Api, Mode::Client) => api::client(ctx).await,
        (Kind::Iolink, Mode::Server) => iolink::server(ctx).await,
        (Kind::Iolink, Mode::Client) => iolink::client(ctx).await,
    }
}

/// Answer writes a server-mode service never expects (the manager handles them),
/// and wait until the service is cancelled.
pub async fn idle_until_cancelled(ctx: &mut Ctx) {
    loop {
        tokio::select! {
            _ = ctx.cancel.cancelled() => return,
            Some(w) = ctx.writes.recv() => {
                let _ = w.reply.send(Err("server-mode values are written directly".into()));
            }
        }
    }
}

/// Starter YAML for a new service of each kind and mode, shown in the web UI.
pub fn template(kind: Kind, mode: Mode) -> &'static str {
    match (kind, mode) {
        (Kind::Modbus, Mode::Server) => include_str!("../../templates/modbus-server.yaml"),
        (Kind::Modbus, Mode::Client) => include_str!("../../templates/modbus-client.yaml"),
        (Kind::Opcua, Mode::Server) => include_str!("../../templates/opcua-server.yaml"),
        (Kind::Opcua, Mode::Client) => include_str!("../../templates/opcua-client.yaml"),
        (Kind::Api, Mode::Server) => include_str!("../../templates/api-server.yaml"),
        (Kind::Api, Mode::Client) => include_str!("../../templates/api-client.yaml"),
        (Kind::Iolink, Mode::Server) => include_str!("../../templates/iolink-server.yaml"),
        (Kind::Iolink, Mode::Client) => include_str!("../../templates/iolink-client.yaml"),
    }
}

/// Default `bind` for server-mode services: both IPv6 and IPv4 where the host allows it.
pub fn any_host() -> String {
    String::new()
}

/// Listen on `bind:port`. An empty `bind` tries dual-stack `[::]` first, then `0.0.0.0`
/// (for hosts and containers with IPv6 turned off).
pub async fn listen(bind: &str, port: u16) -> Result<tokio::net::TcpListener> {
    if bind.is_empty() {
        if let Ok(l) = tokio::net::TcpListener::bind(("::", port)).await {
            return Ok(l);
        }
        return tokio::net::TcpListener::bind(("0.0.0.0", port))
            .await
            .with_context(|| format!("can't listen on port {port}"));
    }
    tokio::net::TcpListener::bind((bind, port))
        .await
        .with_context(|| format!("can't listen on {bind}:{port}"))
}
