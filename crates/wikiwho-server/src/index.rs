//! In-memory title→page_id index per language.
//!
//! Built at startup by walking the sharded storage tree and reading
//! each article's `meta.json`. This is fine for the first-cut server
//! (tens of thousands of articles, milliseconds per language); a
//! persistent on-disk index — likely a per-language `title_index.bin`
//! sibling to the shard tree — is a follow-up once corpora grow.
//!
//! Title equivalence here matches what the MW Action API returns:
//! spaces vs underscores are NOT normalized by this layer. Callers
//! (route handlers) normalize URL-decoded titles by replacing spaces
//! with underscores before lookup, since wikiwho's stored titles
//! always use underscores (see `Article::title` populated by
//! `wikiwho-mwclient`).

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use wikiwho_storage::layout::META_FILE;
use wikiwho_storage::meta::Meta;

/// `title -> page_id` lookup keyed on stored titles (underscores).
#[derive(Debug, Default)]
pub struct TitleIndex {
    by_title: HashMap<String, u64>,
}

impl TitleIndex {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Walk `<storage_root>/<language>/` and add every article's
    /// `(title, page_id)` to the index. Silently skips broken
    /// `meta.json` files but logs them via `tracing::warn!`.
    pub fn build(storage_root: &Path, language: &str) -> std::io::Result<Self> {
        let mut index = Self::default();
        let lang_dir = storage_root.join(language);
        if !lang_dir.exists() {
            return Ok(index);
        }
        index.populate_recursive(&lang_dir)?;
        Ok(index)
    }

    fn populate_recursive(&mut self, dir: &Path) -> std::io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if !file_type.is_dir() {
                continue;
            }
            // The article directory contains meta.json. If present,
            // load it; otherwise recurse one level deeper (shard
            // directory).
            let meta_path = path.join(META_FILE);
            if meta_path.exists() {
                match fs::read_to_string(&meta_path).and_then(|s| {
                    Meta::from_json(&s).map_err(|e| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
                    })
                }) {
                    Ok(meta) => {
                        self.by_title.insert(meta.title, meta.page_id);
                    }
                    Err(err) => {
                        tracing::warn!(
                            path = %meta_path.display(),
                            error = %err,
                            "skipping unreadable meta.json"
                        );
                    }
                }
            } else {
                self.populate_recursive(&path)?;
            }
        }
        Ok(())
    }

    /// Insert / overwrite a single mapping. Used by tests + by future
    /// catch-up flows that add an article without restarting the
    /// server.
    pub fn insert(&mut self, title: impl Into<String>, page_id: u64) {
        self.by_title.insert(title.into(), page_id);
    }

    pub fn lookup(&self, title: &str) -> Option<u64> {
        self.by_title.get(title).copied()
    }

    pub fn len(&self) -> usize {
        self.by_title.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_title.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wikiwho_storage::layout::article_dir;

    fn write_meta(volume: &Path, language: &str, page_id: u64, title: &str) {
        let dir = article_dir(volume, language, page_id);
        fs::create_dir_all(&dir).unwrap();
        let meta = Meta::new(page_id, language, title);
        fs::write(dir.join(META_FILE), meta.to_pretty_json().unwrap()).unwrap();
    }

    #[test]
    fn build_indexes_every_article_in_language() {
        let tmp = tempfile::tempdir().unwrap();
        write_meta(tmp.path(), "en", 1, "Foo");
        write_meta(tmp.path(), "en", 1001, "Bar");
        write_meta(tmp.path(), "en", 12345678, "Baz");
        // Different language directory should not bleed in.
        write_meta(tmp.path(), "simple", 2, "OtherWikiArticle");

        let index = TitleIndex::build(tmp.path(), "en").unwrap();
        assert_eq!(index.len(), 3);
        assert_eq!(index.lookup("Foo"), Some(1));
        assert_eq!(index.lookup("Bar"), Some(1001));
        assert_eq!(index.lookup("Baz"), Some(12345678));
        assert_eq!(index.lookup("OtherWikiArticle"), None);
    }

    #[test]
    fn build_returns_empty_when_language_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let index = TitleIndex::build(tmp.path(), "en").unwrap();
        assert!(index.is_empty());
    }

    #[test]
    fn manual_insert_overrides_existing_entry() {
        let mut index = TitleIndex::empty();
        index.insert("Foo", 1);
        assert_eq!(index.lookup("Foo"), Some(1));
        index.insert("Foo", 999);
        assert_eq!(index.lookup("Foo"), Some(999));
    }
}
