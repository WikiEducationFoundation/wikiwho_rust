//! `wikiwho-ingest` binary.
//!
//! Reads:
//! - `WIKIWHO_STORAGE` — storage root (default `./var/storage`).
//! - `WIKIWHO_INGEST_LANGS` — comma-separated language codes
//!   (default `en,simple`).
//! - `WIKIWHO_EVENTSTREAMS_URL` — override the stream URL (default
//!   production).
//!
//! Graceful shutdown on SIGINT / SIGTERM (Unix) or Ctrl-C (any
//! platform). The final checkpoint flush runs before exit so a
//! restart resumes from approximately the same place.

use std::path::PathBuf;

use tracing_subscriber::EnvFilter;
use wikiwho_ingest::{IngestConfig, ShutdownSignal, run_ingest};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let storage_root = std::env::var("WIKIWHO_STORAGE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./var/storage"));
    let languages: Vec<String> = std::env::var("WIKIWHO_INGEST_LANGS")
        .unwrap_or_else(|_| "en,simple".into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut config = IngestConfig::new(storage_root, languages);
    if let Ok(url) = std::env::var("WIKIWHO_EVENTSTREAMS_URL") {
        config.stream_url = url;
    }

    tracing::info!(
        storage = %config.storage_root.display(),
        languages = ?config.languages,
        stream = %config.stream_url,
        "starting wikiwho-ingest"
    );

    let shutdown = ShutdownSignal::new();
    let shutdown_for_signals = shutdown.clone();
    tokio::spawn(async move {
        wait_for_shutdown().await;
        tracing::info!("shutdown signal received, draining");
        shutdown_for_signals.cancel();
    });

    run_ingest(config, shutdown).await
}

#[cfg(unix)]
async fn wait_for_shutdown() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut intr = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = intr.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
