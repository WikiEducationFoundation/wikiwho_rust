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
}

impl AppState {
    pub fn new(storage_root: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                storage_root: storage_root.into(),
                title_indexes: RwLock::new(HashMap::new()),
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

    /// Force-build the index for `language` and cache it. Used after
    /// tests write an article so subsequent lookups find it without
    /// waiting on the lazy-build path.
    pub fn refresh_title_index(&self, language: &str) -> std::io::Result<()> {
        let fresh = TitleIndex::build(&self.inner.storage_root, language)?;
        let mut guard = self.inner.title_indexes.write().expect("title index poisoned");
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
}
