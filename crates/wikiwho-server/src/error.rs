//! Errors that flow out of handlers. Each variant carries enough to
//! pick the right HTTP status + response body shape per API.md §"Error
//! codes".

use thiserror::Error;

/// Errors that handlers can return. The mapping to HTTP responses
/// lives in the route layer; the variants here are independent of
/// axum so they're easy to construct from non-HTTP contexts (e.g. CLI
/// shadow-replay).
#[derive(Debug, Error)]
pub enum ServerError {
    /// Requested article isn't on disk. We don't yet have a
    /// cache-miss path that would fetch + replay, so the response
    /// envelope says "still processing" (HTTP 200, success=false) —
    /// matches what the current production service returns when the
    /// algorithm is mid-build (`api/views.py:158`).
    #[error("article not found on disk: lang={lang}, page_id={page_id:?}, title={title:?}")]
    ArticleNotFound {
        lang: String,
        page_id: Option<u64>,
        title: Option<String>,
    },

    /// Requested rev_id isn't in the article's stored history. API.md
    /// §1 "Response (200, error)" — status 400 with `{"Error": "..."}`.
    #[error("revision not found in article history: rev_id={rev_id}")]
    RevisionNotFound { rev_id: u64 },

    /// Title given but no entry in the title index for `(lang,
    /// title)`. Same handling as ArticleNotFound (the consumer can't
    /// distinguish; both mean "not on disk yet").
    #[error("title not found: lang={lang}, title={title}")]
    TitleNotFound { lang: String, title: String },

    /// Storage layer threw — corrupt files, missing meta.json, etc.
    /// These are 500s.
    #[error("storage error: {0}")]
    Storage(#[from] wikiwho_storage::StorageError),

    /// Generic IO that isn't a storage-file issue (e.g. failed to
    /// list the language directory during index build).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization failures from the response builder — should
    /// be unreachable in practice; included so we never `unwrap()`.
    #[error("json serialization error: {0}")]
    Json(#[from] serde_json::Error),

    /// Catch-all for unexpected handler-internal failures (e.g. a
    /// background task panicked). 500 at the boundary.
    #[error("internal error: {0}")]
    Internal(String),
}
