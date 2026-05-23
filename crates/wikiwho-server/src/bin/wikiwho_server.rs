//! `wikiwho-server` binary.
//!
//! Minimal launcher — binds to `WIKIWHO_BIND` (default `127.0.0.1:8088`)
//! and serves the routes in [`wikiwho_server::routes::router`]. Reads
//! the storage root from `WIKIWHO_STORAGE` (default `./var/storage`).
//!
//! This is the scaffold-pass binary: no graceful shutdown, no metrics,
//! no per-language directory bootstrap. Subsequent sessions will add
//! those.

use std::path::PathBuf;

use anyhow::Context;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use wikiwho_server::{AppState, router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let storage_root = std::env::var("WIKIWHO_STORAGE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./var/storage"));
    let bind_addr = std::env::var("WIKIWHO_BIND").unwrap_or_else(|_| "127.0.0.1:8088".into());

    tracing::info!(
        storage = %storage_root.display(),
        bind = %bind_addr,
        "starting wikiwho-server"
    );

    let app = router(AppState::new(storage_root));
    let listener = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("binding {bind_addr}"))?;
    axum::serve(listener, app).await.context("axum::serve")?;
    Ok(())
}
