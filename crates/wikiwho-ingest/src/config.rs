//! Ingest configuration.
//!
//! Loaded by the daemon at startup. Defaults map to the production
//! Wikimedia EventStreams endpoint; tests substitute their own.

use std::path::PathBuf;

/// All the knobs the ingest loop needs.
#[derive(Debug, Clone)]
pub struct IngestConfig {
    /// On-disk storage root. Mirrors the server's `WIKIWHO_STORAGE`.
    pub storage_root: PathBuf,
    /// Language codes to ingest (e.g. `["en", "simple"]`). Events for
    /// any other wiki are dropped at the SSE filter.
    pub languages: Vec<String>,
    /// EventStreams base URL. Defaults to the production endpoint
    /// (`https://stream.wikimedia.org/v2/stream/recentchange`); tests
    /// override to a local mock.
    pub stream_url: String,
    /// Flush the checkpoint file every N events. 1 = after every event
    /// (safest); 100 = noticeably less disk churn at the cost of
    /// replaying a few events on restart.
    pub checkpoint_every: usize,
}

impl IngestConfig {
    /// Default config for production use. `storage_root` must still be
    /// set by the caller.
    pub fn new(storage_root: PathBuf, languages: Vec<String>) -> Self {
        Self {
            storage_root,
            languages,
            stream_url: "https://stream.wikimedia.org/v2/stream/recentchange".into(),
            checkpoint_every: 25,
        }
    }
}
