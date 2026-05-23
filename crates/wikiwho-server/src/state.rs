//! Shared application state: storage root + per-language title index.
//!
//! Title indexes are built lazily on first request per language and
//! cached in a `RwLock`-protected map. This means startup is cheap
//! (we don't walk every shard tree on boot) but the first request
//! after `mkfs`-time pays a one-time scan cost. For development +
//! test sizes that's milliseconds; for production we'll want a
//! persistent index — see `notes/decisions-needed.md` for the queued
//! follow-up.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;

use wikiwho_storage::rev_id_index::RevIdIndex;

use crate::index::TitleIndex;

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    storage_root: PathBuf,
    /// `lang -> title index`. Populated lazily on first lookup per
    /// language. Read-mostly so `RwLock` over `parking_lot` is fine.
    title_indexes: RwLock<HashMap<String, Arc<TitleIndex>>>,
    /// `lang -> rev_id_index.bin` snapshot. Loaded from disk lazily and
    /// cached for the lifetime of the process; refreshed explicitly by
    /// tests + by any future ingest path that updates the index.
    rev_id_indexes: RwLock<HashMap<String, Arc<RevIdIndex>>>,
}

impl AppState {
    pub fn new(storage_root: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                storage_root: storage_root.into(),
                title_indexes: RwLock::new(HashMap::new()),
                rev_id_indexes: RwLock::new(HashMap::new()),
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
    /// rev_id isn't on disk (which is also what the placeholder
    /// rev_content/rev_id/ handler treats as "still processing").
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

    /// Force-build the index for `language` and cache it. Used after
    /// tests write an article so subsequent lookups find it without
    /// waiting on the lazy-build path.
    pub fn refresh_title_index(&self, language: &str) -> std::io::Result<()> {
        let fresh = TitleIndex::build(&self.inner.storage_root, language)?;
        let mut guard = self.inner.title_indexes.write().expect("title index poisoned");
        guard.insert(language.to_string(), Arc::new(fresh));
        Ok(())
    }

    /// Reload the per-language `rev_id_index.bin` from disk. Called by
    /// tests + by future ingest paths that have just updated the
    /// sidecar.
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

/// Wrap the storage-layer load behind an `io::Result` boundary so the
/// surrounding `AppState` methods don't have to expose `StorageError`.
/// Storage-level failures (CRC mismatch, bad magic) get translated into
/// `io::Error::InvalidData`; a missing file is already mapped to an
/// empty index by `RevIdIndex::load`.
fn load_rev_id_index(storage_root: &std::path::Path, language: &str) -> std::io::Result<RevIdIndex> {
    RevIdIndex::load(storage_root, language)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
