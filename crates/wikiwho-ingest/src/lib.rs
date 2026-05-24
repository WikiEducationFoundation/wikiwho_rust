//! EventStreams listener + load-apply-save loop.
//!
//! Wikimedia's [EventStreams][] service emits `recentchange` SSE events
//! whenever a wiki page is edited. This crate consumes that feed,
//! filters down to article-namespace edits on wikis we host, and for
//! each event:
//!
//! 1. Resolves the article on disk via
//!    [`wikiwho_storage::reader::SnapshotReader`]. If the article isn't
//!    on disk we skip — cold builds happen via the server's cache-miss
//!    path, not here.
//! 2. Fetches the new revision (or window of new revisions, if our
//!    snapshot is behind) from the MW Action API via
//!    [`wikiwho_mwclient`].
//! 3. Calls [`wikiwho_attribute::structures::Article::analyse_revision`]
//!    on each fetched revision in order.
//! 4. Persists the updated article via
//!    [`wikiwho_storage::writer::write_article`].
//!
//! Periodically the listener persists the last-seen SSE event id to a
//! checkpoint file so a restart can resume close to where it left off
//! via the `Last-Event-ID` header.
//!
//! Public surface:
//!
//! - [`IngestConfig`] — what to ingest and where to put it.
//! - [`run_ingest`] — async entry point used by the binary; loops
//!   until the cancellation signal fires.
//!
//! The crate is structured so each phase is independently testable:
//!
//! - [`events`] parses SSE bytes into [`events::PageEdit`] events.
//! - [`apply`] runs one load-apply-save cycle as a pure function.
//! - [`checkpoint`] handles on-disk resume state.
//!
//! [EventStreams]: https://wikitech.wikimedia.org/wiki/EventStreams

pub mod apply;
pub mod checkpoint;
pub mod config;
pub mod events;

pub use apply::{ApplyError, ApplyOutcome, apply_event};
pub use checkpoint::Checkpoint;
pub use config::IngestConfig;
pub use events::{EventStreamError, PageEdit};

use std::collections::HashSet;
use std::sync::Arc;

use futures_util::StreamExt;
use tokio::sync::Mutex;
use wikiwho_mwclient::MwClient;

/// User-Agent header for the SSE connection. Wikimedia policy
/// requires a contact address.
pub const USER_AGENT: &str = concat!(
    "wikiwho_rust-ingest/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/WikiEducationFoundation; sage@wikiedu.org)"
);

/// Tiny replacement for tokio-util's CancellationToken so we don't
/// pull in another dep just for SIGINT handling.
mod cancel {
    use std::sync::Arc;
    use tokio::sync::Notify;

    #[derive(Clone, Default)]
    pub struct CancellationToken {
        notify: Arc<Notify>,
        flag: Arc<std::sync::atomic::AtomicBool>,
    }

    impl CancellationToken {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn cancel(&self) {
            self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
            self.notify.notify_waiters();
        }

        pub fn is_cancelled(&self) -> bool {
            self.flag.load(std::sync::atomic::Ordering::SeqCst)
        }

        pub async fn cancelled(&self) {
            if self.is_cancelled() {
                return;
            }
            self.notify.notified().await;
        }
    }
}

pub use cancel::CancellationToken as ShutdownSignal;

/// Run the ingest loop until `shutdown` fires or an unrecoverable
/// error occurs. Reconnects to EventStreams transparently on network
/// errors; the only fatal errors are configuration problems (bad
/// storage root, unreadable checkpoint).
///
/// One `MwClient` is built per configured language and reused across
/// every event for that language — the same pattern as the server's
/// `AppState`.
pub async fn run_ingest(config: IngestConfig, shutdown: ShutdownSignal) -> anyhow::Result<()> {
    let storage_root = config.storage_root.clone();
    let mut clients: std::collections::HashMap<String, MwClient> = std::collections::HashMap::new();
    for lang in &config.languages {
        clients.insert(lang.clone(), MwClient::new(lang)?);
    }
    let clients = Arc::new(clients);

    let wikis: HashSet<String> = config.languages.iter().map(|l| events::lang_to_wiki(l)).collect();

    let checkpoint = Arc::new(Mutex::new(Checkpoint::load_or_init(
        &config.storage_root,
        &config.languages,
    )?));

    let resume = checkpoint.lock().await.last_event_id_header();

    let stream = events::recentchange_stream_filtered(
        config.stream_url.clone(),
        resume,
        wikis,
        shutdown.clone(),
    );
    tokio::pin!(stream);

    while let Some(item) = stream.next().await {
        if shutdown.is_cancelled() {
            break;
        }
        let event = match item {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(error = %err, "event stream error; reconnect handled internally");
                continue;
            }
        };

        let Some(client) = clients.get(&event.language) else {
            continue;
        };

        match apply::apply_event(&storage_root, client, &event).await {
            Ok(ApplyOutcome::Applied { applied_revs }) => {
                tracing::info!(
                    lang = %event.language,
                    page = event.page_id,
                    rev = event.rev_id,
                    applied = applied_revs,
                    "applied"
                );
            }
            Ok(ApplyOutcome::SnapshotMissing) => {
                tracing::debug!(
                    lang = %event.language,
                    page = event.page_id,
                    "snapshot not on disk, skipping"
                );
            }
            Ok(ApplyOutcome::AlreadyAtOrAhead) => {
                tracing::debug!(
                    lang = %event.language,
                    page = event.page_id,
                    rev = event.rev_id,
                    "snapshot already at-or-ahead of event"
                );
            }
            Err(err) => {
                tracing::warn!(
                    lang = %event.language,
                    page = event.page_id,
                    rev = event.rev_id,
                    error = %err,
                    "apply failed"
                );
            }
        }

        if let Some(id) = event.sse_id.as_ref() {
            let mut cp = checkpoint.lock().await;
            cp.advance(&event.language, id);
            if cp.dirty_count() >= config.checkpoint_every {
                if let Err(err) = cp.flush(&config.storage_root) {
                    tracing::warn!(error = %err, "checkpoint flush failed");
                }
            }
        }
    }

    // Final flush on shutdown.
    let mut cp = checkpoint.lock().await;
    if let Err(err) = cp.flush(&config.storage_root) {
        tracing::warn!(error = %err, "final checkpoint flush failed");
    }
    Ok(())
}
