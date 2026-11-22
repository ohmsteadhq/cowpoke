//! Cowpoke: one container, many industrial communication services.
//! Copyright (C) 2026 the Cowpoke contributors. Licensed under the GNU AGPL v3 or later (see LICENSE).

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
mod loopback_tests;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("COWPOKE_LOG")
                .unwrap_or_else(|_| "info,opcua=warn,tokio_modbus=warn".into()),
        )
        .init();

    let data_dir = PathBuf::from(std::env::var("COWPOKE_DATA").unwrap_or_else(|_| "./data".into()));
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("can't create data folder {}", data_dir.display()))?;
    let config_path = std::env::var("COWPOKE_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| data_dir.join("cowpoke.yaml"));
    let bind = std::env::var("COWPOKE_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());

    let cfg = Arc::new(store::ConfigStore::open(&config_path)?);
    let auth = Arc::new(auth::Auth::open(&data_dir.join("users.json"))?);
    let tags = tags::TagStore::new();
    let mgr = manager::Manager::new(tags.clone(), data_dir.clone());
    let hist = historian::Historian::new(&data_dir);
    tokio::spawn(hist.clone().run(tags.clone()));

    let app = web::App {
        cfg,
        mgr: mgr.clone(),
        tags,
        auth,
        hist: hist.clone(),
        data_dir,
    };
    app.reload().await;
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
    axum::serve(listener, web::router(app))
        .with_graceful_shutdown(async {
            let ctrl_c = tokio::signal::ctrl_c();
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
                tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
            }
            #[cfg(not(unix))]
            let _ = ctrl_c.await;
            tracing::info!("shutting down");
        })
        .await?;
    mgr.shutdown().await;
    hist.flush();
    Ok(())
}
