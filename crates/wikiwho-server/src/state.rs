//! Shared application state.
//!
//! Holds three slices of cached state, each lazily populated:
//!
//! - Per-language **title -> page_id** indexes (built from on-disk
//!   `meta.json` files; see [`crate::index::TitleIndex`]).
//! - Per-language **rev_id -> page_id** indexes (loaded from
//!   `rev_id_index.bin`; see
//!   [`wikiwho_storage::rev_id_index::RevIdIndex`]).
//! - Per-language **MediaWiki clients** (one
//!   [`wikiwho_mwclient::MwClient`] per wiki; reused across all
//!   cache-miss fetches).
//!
//! It also owns the **in-flight cache-miss set** — `(lang, page_id)`
//! pairs whose background processing task is currently running.
//! Concurrent requests for the same article see the entry and skip
//! re-spawning.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use tokio::task::JoinHandle;
use wikiwho_mwclient::{MwClient, MwError, Revision};
use wikiwho_storage::rev_id_index::RevIdIndex;

use crate::cache_miss::{CacheMissError, process_and_persist};
use crate::index::TitleIndex;

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    storage_root: PathBuf,
    /// `lang -> title index`. Populated lazily on first lookup per
    /// language. Read-mostly so `RwLock` is fine.
    title_indexes: RwLock<HashMap<String, Arc<TitleIndex>>>,
    /// `lang -> rev_id_index.bin` snapshot. Loaded from disk lazily and
    /// cached for the lifetime of the process; refreshed explicitly by
    /// tests + by any future ingest path that updates the index.
    rev_id_indexes: RwLock<HashMap<String, Arc<RevIdIndex>>>,
    /// `lang -> MwClient`. Built once per language and reused for
    /// every cache-miss fetch. `Arc<MwClient>` so the spawned task
    /// can capture a cheap clone without taking a borrow of `self`.
    mw_clients: RwLock<HashMap<String, Arc<MwClient>>>,
    /// `(lang, page_id)` pairs that currently have a cache-miss
    /// background task running. New requests for the same key skip
    /// re-spawning and just return the still-processing envelope.
    in_flight: Mutex<HashSet<(String, u64)>>,
}

impl AppState {
    pub fn new(storage_root: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                storage_root: storage_root.into(),
                title_indexes: RwLock::new(HashMap::new()),
                rev_id_indexes: RwLock::new(HashMap::new()),
                mw_clients: RwLock::new(HashMap::new()),
                in_flight: Mutex::new(HashSet::new()),
            }),
        }
    }

    pub fn storage_root(&self) -> &std::path::Path {
        &self.inner.storage_root
    }

    /// Resolve `title -> page_id` for a language. Builds the index on
    /// first call per language; subsequent calls return the cached
    /// instance. Returns `None` if the title isn't on disk.
    pub fn resolve_title(&self, language: &str, title: &str) -> Option<u64> {
        let cached = {
            let guard = self.inner.title_indexes.read().expect("title index poisoned");
            guard.get(language).cloned()
        };
        let index = match cached {
            Some(idx) => idx,
            None => self.build_and_cache_index(language).ok()?,
        };
        index.lookup(title)
    }

    /// Resolve `rev_id -> page_id` for a language via the on-disk
    /// `rev_id_index.bin` sidecar. Loaded lazily on first call per
    /// language; subsequent calls hit the cache. Returns `None` if the
    /// rev_id isn't on disk.
    pub fn resolve_rev_id(&self, language: &str, rev_id: u64) -> Option<u64> {
        let cached = {
            let guard = self
                .inner
                .rev_id_indexes
                .read()
                .expect("rev_id index poisoned");
            guard.get(language).cloned()
        };
        let index = match cached {
            Some(idx) => idx,
            None => self.build_and_cache_rev_id_index(language).ok()?,
        };
        index.lookup(rev_id)
    }

    /// Force-build the title index for `language` and cache it. Used
    /// by tests + by the cache-miss path after a fresh `write_article`.
    pub fn refresh_title_index(&self, language: &str) -> std::io::Result<()> {
        let fresh = TitleIndex::build(&self.inner.storage_root, language)?;
        let mut guard = self.inner.title_indexes.write().expect("title index poisoned");
        guard.insert(language.to_string(), Arc::new(fresh));
        Ok(())
    }

    /// Reload the per-language `rev_id_index.bin` from disk. Called by
    /// tests + by the cache-miss path after a fresh `write_article`.
    pub fn refresh_rev_id_index(&self, language: &str) -> std::io::Result<()> {
        let fresh = load_rev_id_index(&self.inner.storage_root, language)?;
        let mut guard = self
            .inner
            .rev_id_indexes
            .write()
            .expect("rev_id index poisoned");
        guard.insert(language.to_string(), Arc::new(fresh));
        Ok(())
    }

    /// Get or build the [`MwClient`] for `language`. The first call
    /// per language constructs the client (and may fail if reqwest
    /// rejects the URL); subsequent calls return the cached `Arc`.
    pub fn mw_client(&self, language: &str) -> Result<Arc<MwClient>, MwError> {
        {
            let guard = self.inner.mw_clients.read().expect("mw client poisoned");
            if let Some(c) = guard.get(language) {
                return Ok(c.clone());
            }
        }
        let fresh = Arc::new(MwClient::new(language)?);
        let mut guard = self.inner.mw_clients.write().expect("mw client poisoned");
        Ok(guard
            .entry(language.to_string())
            .or_insert_with(|| fresh.clone())
            .clone())
    }

    /// Install a pre-built `MwClient` for tests / shadow setups
    /// (e.g. one whose URL points at a local mock). Replaces any
    /// existing client for `language`.
    pub fn install_mw_client(&self, language: &str, client: MwClient) {
        let mut guard = self.inner.mw_clients.write().expect("mw client poisoned");
        guard.insert(language.to_string(), Arc::new(client));
    }

    /// True if a cache-miss task for `(language, page_id)` is
    /// currently running.
    pub fn is_in_flight(&self, language: &str, page_id: u64) -> bool {
        let guard = self.inner.in_flight.lock().expect("in_flight poisoned");
        guard.contains(&(language.to_string(), page_id))
    }

    /// Atomically claim the `(language, page_id)` slot. Returns `true`
    /// if the caller is now responsible for processing the article;
    /// `false` if someone else already holds it. Either way the
    /// caller should respond with the still-processing envelope.
    pub fn try_claim_in_flight(&self, language: &str, page_id: u64) -> bool {
        let mut guard = self.inner.in_flight.lock().expect("in_flight poisoned");
        guard.insert((language.to_string(), page_id))
    }

    fn release_in_flight(&self, language: &str, page_id: u64) {
        let mut guard = self.inner.in_flight.lock().expect("in_flight poisoned");
        guard.remove(&(language.to_string(), page_id));
    }

    /// Spawn a background tokio task that:
    ///
    /// 1. Awaits `fetcher` to materialize the article's revision
    ///    history (oldest-first).
    /// 2. Calls [`process_and_persist`] to build + write the article.
    /// 3. Refreshes the title and rev_id indexes so the next request
    ///    for the same article serves from disk.
    /// 4. Releases the in-flight slot.
    ///
    /// Returns the `JoinHandle` so tests can `.await` it; production
    /// code typically drops it (fire-and-forget).
    ///
    /// **Precondition:** the caller has already called
    /// [`Self::try_claim_in_flight`] and got `true`. This method
    /// trusts that precondition and will release the slot even if the
    /// background task fails.
    pub fn spawn_cache_miss<F>(
        &self,
        language: String,
        title: String,
        page_id: u64,
        fetcher: F,
    ) -> JoinHandle<()>
    where
        F: std::future::Future<Output = Result<Vec<Revision>, CacheMissError>> + Send + 'static,
    {
        let state = self.clone();
        tokio::spawn(async move {
            let outcome = run_cache_miss(&state, &language, &title, page_id, fetcher).await;
            if let Err(err) = outcome {
                tracing::warn!(
                    lang = %language,
                    page_id = page_id,
                    error = %err,
                    "cache-miss task failed"
                );
            }
            state.release_in_flight(&language, page_id);
        })
    }

    fn build_and_cache_index(&self, language: &str) -> std::io::Result<Arc<TitleIndex>> {
        let fresh = TitleIndex::build(&self.inner.storage_root, language)?;
        let arc = Arc::new(fresh);
        let mut guard = self.inner.title_indexes.write().expect("title index poisoned");
        // Another thread may have populated while we were building;
        // prefer the existing entry to avoid churn.
        let entry = guard
            .entry(language.to_string())
            .or_insert_with(|| arc.clone());
        Ok(entry.clone())
    }

    fn build_and_cache_rev_id_index(&self, language: &str) -> std::io::Result<Arc<RevIdIndex>> {
        let fresh = load_rev_id_index(&self.inner.storage_root, language)?;
        let arc = Arc::new(fresh);
        let mut guard = self
            .inner
            .rev_id_indexes
            .write()
            .expect("rev_id index poisoned");
        let entry = guard
            .entry(language.to_string())
            .or_insert_with(|| arc.clone());
        Ok(entry.clone())
    }
}

/// Inner cache-miss body. Separated from `spawn_cache_miss` so the
/// `state.release_in_flight` call in the task body runs even when the
/// `?`-shortcut here exits early.
async fn run_cache_miss<F>(
    state: &AppState,
    language: &str,
    title: &str,
    page_id: u64,
    fetcher: F,
) -> Result<(), CacheMissError>
where
    F: std::future::Future<Output = Result<Vec<Revision>, CacheMissError>> + Send + 'static,
{
    let revisions = fetcher.await?;
    let storage_root = state.inner.storage_root.clone();
    let language_owned = language.to_string();
    let title_owned = title.to_string();
    // The algorithm + write_article are blocking; run on tokio's
    // blocking pool so the runtime stays responsive for other requests.
    let article = tokio::task::spawn_blocking(move || {
        process_and_persist(&storage_root, &language_owned, &title_owned, page_id, &revisions)
    })
    .await
    .map_err(|join_err| {
        CacheMissError::Storage(wikiwho_storage::StorageError::Io(std::io::Error::other(
            format!("cache-miss blocking task panicked: {join_err}"),
        )))
    })??;
    tracing::info!(
        lang = %language,
        page_id = page_id,
        revisions = article.ordered_revisions.len(),
        "cache-miss processed and persisted"
    );

    // Refresh both indexes so the next request hits disk directly.
    if let Err(e) = state.refresh_title_index(language) {
        tracing::warn!(lang = %language, error = %e, "failed to refresh title index");
    }
    if let Err(e) = state.refresh_rev_id_index(language) {
        tracing::warn!(lang = %language, error = %e, "failed to refresh rev_id index");
    }
    Ok(())
}

/// Wrap the storage-layer load behind an `io::Result` boundary so the
/// surrounding `AppState` methods don't have to expose `StorageError`.
/// Storage-level failures (CRC mismatch, bad magic) get translated into
/// `io::Error::InvalidData`; a missing file is already mapped to an
/// empty index by `RevIdIndex::load`.
fn load_rev_id_index(storage_root: &std::path::Path, language: &str) -> std::io::Result<RevIdIndex> {
    RevIdIndex::load(storage_root, language)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wikiwho_mwclient::Revision;

    fn fixture_rev(rev_id: u64, ts: &str, user: u64, text: &str) -> Revision {
        Revision {
            rev_id,
            parent_id: 0,
            timestamp: ts.into(),
            sha1: None,
            comment: None,
            minor: false,
            user_id: Some(user),
            user_name: Some(format!("u{user}")),
            text: text.into(),
            text_hidden: false,
        }
    }

    #[test]
    fn try_claim_in_flight_is_atomic() {
        let tmp = tempfile::tempdir().unwrap();
        let state = AppState::new(tmp.path().to_path_buf());
        assert!(state.try_claim_in_flight("en", 7));
        assert!(!state.try_claim_in_flight("en", 7));
        assert!(state.is_in_flight("en", 7));
        // A different page is independent.
        assert!(state.try_claim_in_flight("en", 8));
        // A different language is independent.
        assert!(state.try_claim_in_flight("simple", 7));
    }

    #[tokio::test]
    async fn spawn_cache_miss_runs_pipeline_and_releases_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let state = AppState::new(tmp.path().to_path_buf());
        assert!(state.try_claim_in_flight("en", 42));

        let revisions = vec![
            fixture_rev(101, "2024-01-01T00:00:00Z", 1, "Hello world."),
            fixture_rev(102, "2024-01-02T00:00:00Z", 2, "Hello there, world."),
        ];
        let fetcher = async move { Ok(revisions) };
        let handle = state.spawn_cache_miss(
            "en".to_string(),
            "Demo".to_string(),
            42,
            fetcher,
        );
        handle.await.unwrap();

        // Slot released.
        assert!(!state.is_in_flight("en", 42));
        // Article persisted: title index resolves Demo to page_id 42.
        assert_eq!(state.resolve_title("en", "Demo"), Some(42));
        // rev_id index resolves rev_id 102 to page_id 42.
        assert_eq!(state.resolve_rev_id("en", 102), Some(42));
    }

    #[tokio::test]
    async fn spawn_cache_miss_releases_slot_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let state = AppState::new(tmp.path().to_path_buf());
        assert!(state.try_claim_in_flight("en", 99));

        // Fetcher returns an MwError — the task should log, release,
        // and not panic.
        let fetcher = async move {
            Err(CacheMissError::Mw(MwError::PageMissing { page_id: 99 }))
        };
        let handle = state.spawn_cache_miss(
            "en".to_string(),
            "Missing".to_string(),
            99,
            fetcher,
        );
        handle.await.unwrap();

        assert!(!state.is_in_flight("en", 99));
        // Title index empty for this language — no article persisted.
        assert_eq!(state.resolve_title("en", "Missing"), None);
    }
}
