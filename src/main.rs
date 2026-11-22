//! Cowpoke: one container, many industrial communication services.
//! Copyright (C) 2026 the Cowpoke contributors. Licensed under the GNU AGPL v3 or later (see LICENSE).
//!
//! `cowpoke` runs a hub: the web UI, the API and the services.
//! `cowpoke agent` runs an edge agent that dials out to a hub and runs the services assigned to it.

mod agent;
mod auth;
mod codec;
mod historian;
mod manager;
mod model;
mod services;
mod store;
mod tags;
mod web;

#[cfg(test)]
mod agent_tests;
#[cfg(test)]
mod loopback_tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

const USAGE: &str = "\
Usage:
  cowpoke          run a hub (web UI, API and services)
  cowpoke agent    run an edge agent for a hub

Hub settings (environment): COWPOKE_DATA, COWPOKE_CONFIG, COWPOKE_BIND, COWPOKE_ADMIN_PASSWORD, COWPOKE_LOG
Agent settings (environment): COWPOKE_HUB (the hub's URL), COWPOKE_AGENT_TOKEN, COWPOKE_DATA, COWPOKE_LOG";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("COWPOKE_LOG")
                .unwrap_or_else(|_| "info,opcua=warn,tokio_modbus=warn".into()),
        )
        .init();

    let data_dir = PathBuf::from(std::env::var("COWPOKE_DATA").unwrap_or_else(|_| "./data".into()));
    match std::env::args().nth(1).as_deref() {
        None | Some("hub") => run_hub(data_dir).await,
        Some("agent") => run_agent(data_dir).await,
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some("-V" | "--version") => {
            println!("cowpoke {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => bail!("unknown command `{other}`\n\n{USAGE}"),
    }
}

/// Open a hub's stores, start its services and return the app the web layer serves.
pub async fn setup_hub(data_dir: &Path, config_path: &Path) -> Result<web::App> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("can't create data folder {}", data_dir.display()))?;
    let tags = tags::TagStore::new();
    let hist = historian::Historian::new(data_dir);
    tokio::spawn(hist.clone().run(tags.clone()));
    let app = web::App {
        cfg: Arc::new(store::ConfigStore::open(config_path)?),
        mgr: manager::Manager::new(tags.clone(), data_dir.to_path_buf()),
        tags,
        auth: Arc::new(auth::Auth::open(&data_dir.join("users.json"))?),
        hist,
        agents: agent::hub::Hub::open(&data_dir.join("agents.json"))?,
        data_dir: data_dir.to_path_buf(),
    };
    app.reload().await;
    Ok(app)
}

async fn run_hub(data_dir: PathBuf) -> Result<()> {
    let config_path = std::env::var("COWPOKE_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| data_dir.join("cowpoke.yaml"));
    let bind = std::env::var("COWPOKE_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let app = setup_hub(&data_dir, &config_path).await?;
    tracing::info!(
        "Cowpoke {} with {} services from {}",
        env!("CARGO_PKG_VERSION"),
        app.cfg.get().services.len(),
        config_path.display()
    );

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("can't listen on {bind}"))?;
    tracing::info!("web UI and API on http://{bind}");
    let (mgr, hist) = (app.mgr.clone(), app.hist.clone());
    axum::serve(listener, web::router(app))
        .with_graceful_shutdown(async {
            shutdown_signal().await;
            tracing::info!("shutting down");
        })
        .await?;
    mgr.shutdown().await;
    hist.flush();
    Ok(())
}

async fn run_agent(data_dir: PathBuf) -> Result<()> {
    let hub = std::env::var("COWPOKE_HUB").context("set COWPOKE_HUB to the hub's URL, e.g. https://cowpoke.example.com")?;
    let token = std::env::var("COWPOKE_AGENT_TOKEN")
        .context("set COWPOKE_AGENT_TOKEN to the token the hub showed when you created this agent")?;
    let edge = agent::edge::Edge::new(&data_dir)?;
    let cancel = tokio_util::sync::CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutting down");
        stop.cancel();
    });
    tracing::info!("Cowpoke {} agent for {hub}", env!("CARGO_PKG_VERSION"));
    agent::edge::run(edge, hub, token, cancel).await
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}
