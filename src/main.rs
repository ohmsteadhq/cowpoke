//! Cowpoke: one container, many industrial communication services.
//! Copyright (C) 2026 the Cowpoke contributors. Licensed under the MIT License (see LICENSE).
//!
//! `cowpoke` runs a hub: the web UI, the API and the services.
//! `cowpoke agent` runs an edge agent that dials out to a hub and runs the services assigned to it.

mod agent;
mod alarms;
mod auth;
mod cluster;
mod codec;
mod dashboards;
mod foreman;
mod generate;
mod historian;
mod import_ohmstead;
mod insights;
mod manager;
mod model;
mod monitor;
mod panels;
mod services;
mod store;
mod tags;
mod tsdb;
mod web;

#[cfg(test)]
mod agent_tests;
#[cfg(test)]
mod alarm_tests;
#[cfg(test)]
mod import_ohmstead_tests;
#[cfg(test)]
mod loopback_tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

const USAGE: &str = "\
Usage:
  cowpoke          run a hub (web UI, API and services)
  cowpoke agent    run an edge agent for a hub
  cowpoke health   check that the hub on COWPOKE_BIND answers (exit 0) or not (exit 1)

Hub settings (environment): COWPOKE_DATA, COWPOKE_CONFIG, COWPOKE_BIND, COWPOKE_ADMIN_PASSWORD, COWPOKE_LOG
  Two hubs (active + standby): COWPOKE_PEER (the other hub's URL), COWPOKE_CLUSTER_KEY, COWPOKE_NODE, COWPOKE_FAILOVER_S
Agent settings (environment): COWPOKE_HUB (the hub's URL, or several separated by commas), COWPOKE_AGENT_TOKEN
  (an agent's or a pool's token), COWPOKE_NODE (a pool worker's name; default: the hostname), COWPOKE_DATA, COWPOKE_LOG";

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
        Some("health") => health().await,
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

/// Start the alarm engine and open the dashboards; the router for their pages and API.
pub fn setup_panels(app: &web::App) -> Result<panels::Panels> {
    let store = Arc::new(alarms::AlarmStore::open(&app.data_dir.join("alarms.yaml"))?);
    let engine = alarms::Engine::new(app.clone(), store);
    tokio::spawn(engine.clone().run());
    let insights = insights::Insights::open(app.data_dir.join("baselines.json"));
    tokio::spawn(insights::run(insights.clone(), app.clone(), engine.clone()));
    Ok(panels::Panels {
        app: app.clone(),
        insights,
        engine,
        dash: Arc::new(dashboards::Dashboards::open(&app.data_dir.join("dashboards.json"))?),
    })
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
    // Pools: move services off workers that left, and place ones still waiting.
    let placer = app.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            t.tick().await;
            placer.agents.tick(&placer.cfg.get());
            placer.note_unplaced();
        }
    });
    Ok(app)
}

async fn run_hub(data_dir: PathBuf) -> Result<()> {
    let config_path = std::env::var("COWPOKE_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| data_dir.join("cowpoke.yaml"));
    let bind = std::env::var("COWPOKE_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());

    // Two hubs: wait as the standby until this one should be active.
    let cluster = cluster::Settings::from_env()?;
    if let Some(cl) = &cluster {
        let peer_epoch = match cluster::decide(&data_dir, cl).await {
            cluster::Role::Active => cluster::peer_state(cl).await.map(|p| p.epoch).unwrap_or(0),
            cluster::Role::Standby => {
                let stop = tokio_util::sync::CancellationToken::new();
                let s2 = stop.clone();
                let signal = tokio::spawn(async move {
                    shutdown_signal().await;
                    s2.cancel();
                });
                let promoted = cluster::run_standby(&data_dir, &config_path, cl, &bind, stop).await?;
                signal.abort();
                match promoted {
                    Some(epoch) => epoch,
                    None => return Ok(()), // stopped while standby
                }
            }
        };
        cluster::activate(&data_dir, &config_path, cl, peer_epoch);
    }

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
    let panels = setup_panels(&app)?;
    let demote = tokio_util::sync::CancellationToken::new();
    if cluster.is_some() {
        tokio::spawn(cluster::watch(demote.clone()));
    }
    let cluster_routes = cluster::router(app.clone());
    // Graceful shutdown waits for open connections, and the UI's live event streams never
    // end on their own: give requests in flight a few seconds, then close the rest.
    let stopping = tokio_util::sync::CancellationToken::new();
    let serve = axum::serve(listener, web::router(app).merge(panels::router(panels)).merge(cluster_routes)).with_graceful_shutdown({
        let stopping = stopping.clone();
        let demote = demote.clone();
        async move {
            tokio::select! {
                _ = shutdown_signal() => tracing::info!("shutting down"),
                _ = demote.cancelled() => tracing::warn!("the peer hub is active: stopping to restart as the standby"),
            }
            stopping.cancel();
        }
    });
    tokio::select! {
        r = serve => r?,
        _ = async {
            stopping.cancelled().await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => tracing::info!("closing connections still open after {} s", SHUTDOWN_GRACE.as_secs()),
    }
    mgr.shutdown().await;
    hist.flush();
    if demote.is_cancelled() {
        // Docker's restart policy starts us again; the peer is active, so as the standby.
        std::process::exit(cluster::STEPPED_DOWN);
    }
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

/// `cowpoke health`: GET /healthz on the hub's own port. For container healthchecks
/// in images without a shell.
async fn health() -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let bind = std::env::var("COWPOKE_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let port = bind.rsplit(':').next().unwrap_or("8080");
    let addr = format!("127.0.0.1:{port}");
    let check = async {
        let mut s = tokio::net::TcpStream::connect(&addr).await?;
        s.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await?;
        let mut buf = vec![0u8; 64];
        let n = s.read(&mut buf).await?;
        anyhow::Ok(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"))
    };
    match tokio::time::timeout(std::time::Duration::from_secs(3), check).await {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) => bail!("{addr}/healthz didn't answer 200"),
        Ok(Err(e)) => bail!("can't reach {addr}: {e}"),
        Err(_) => bail!("{addr} didn't answer within 3 s"),
    }
}

/// How long shutdown waits for open web connections (live event streams never finish).
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

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
