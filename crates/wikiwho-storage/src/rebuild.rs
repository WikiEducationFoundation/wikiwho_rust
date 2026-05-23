//! Storage-tree maintenance routines.
//!
//! Today only one operation: rebuilding `<lang>/rev_id_index.bin` from
//! the article shard tree. Lives in the library (rather than purely in
//! the admin binary) so tests can exercise it and so the server can
//! call it on startup if a stale or missing index is detected.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::layout::{META_FILE, REVISIONS_FILE};
use crate::meta::Meta;
use crate::rev_id_index::RevIdIndex;
use crate::revisions::RevisionsIndex;

/// Result of rebuilding one language: how many articles were scanned
/// and how many `(rev_id, page_id)` entries the resulting index holds.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct RebuildStats {
    pub articles: usize,
    pub entries: usize,
}

/// Enumerate every immediate child directory of `volume` whose name
/// doesn't start with `.`. These are treated as language roots.
pub fn discover_languages(volume: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(volume).with_context(|| format!("reading {}", volume.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') {
            continue;
        }
        out.push(name.to_string());
    }
    out.sort();
    Ok(out)
}

/// Walk the shard tree for `language` under `volume`, build a fresh
/// `RevIdIndex`, and atomic-write it as `<volume>/<language>/rev_id_index.bin`.
///
/// Fails if a rev_id appears in more than one article's `revisions.bin`
/// — that case shouldn't happen on a healthy tree, and we'd rather
/// surface it than silently let the second occurrence win.
pub fn rebuild_one_language(volume: &Path, language: &str) -> Result<RebuildStats> {
    let lang_dir = volume.join(language);
    if !lang_dir.is_dir() {
        bail!("{} is not a directory", lang_dir.display());
    }

    let mut articles: Vec<(u64, Vec<u64>)> = Vec::new();
    walk_shards(&lang_dir, &mut articles)?;

    let mut seen: BTreeSet<u64> = BTreeSet::new();
    for (page_id, rev_ids) in &articles {
        for r in rev_ids {
            if !seen.insert(*r) {
                bail!(
                    "duplicate rev_id {r} found while rebuilding {language} (offending page_id {page_id})"
                );
            }
        }
    }

    let mut index = RevIdIndex::empty();
    for (page_id, rev_ids) in &articles {
        index
            .replace_article(*page_id, rev_ids)
            .with_context(|| format!("indexing page_id {page_id} in {language}"))?;
    }
    let entries = index.len();
    index
        .save(volume, language)
        .with_context(|| format!("saving rev_id_index.bin for {language}"))?;

    Ok(RebuildStats {
        articles: articles.len(),
        entries,
    })
}

fn walk_shards(dir: &Path, out: &mut Vec<(u64, Vec<u64>)>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path: PathBuf = entry.path();
        let meta_path = path.join(META_FILE);
        if meta_path.exists() {
            let (page_id, rev_ids) = scan_article(&path)
                .with_context(|| format!("scanning {}", path.display()))?;
            out.push((page_id, rev_ids));
        } else {
            walk_shards(&path, out)?;
        }
    }
    Ok(())
}

fn scan_article(dir: &Path) -> Result<(u64, Vec<u64>)> {
    let meta_json = fs::read_to_string(dir.join(META_FILE))
        .with_context(|| format!("reading meta.json in {}", dir.display()))?;
    let meta = Meta::from_json(&meta_json)
        .with_context(|| format!("parsing meta.json in {}", dir.display()))?;

    let revisions_bytes = fs::read(dir.join(REVISIONS_FILE))
        .with_context(|| format!("reading revisions.bin in {}", dir.display()))?;
    let index = RevisionsIndex::new(&revisions_bytes)
        .with_context(|| format!("parsing revisions.bin header in {}", dir.display()))?;
    let rev_ids = index.rev_ids_sorted();
    Ok((meta.page_id, rev_ids))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::write_article;
    use wikiwho_attribute::pipeline::RevisionInput;
    use wikiwho_attribute::structures::Article;

    fn fixture(page_id: u64, title: &str, revs: &[(u64, &str)]) -> Article {
        let mut a = Article::new(title);
        a.page_id = Some(page_id);
        for (rev_id, text) in revs {
            a.analyse_revision(RevisionInput {
                rev_id: *rev_id,
                timestamp: format!("2024-01-{:02}T00:00:00Z", rev_id % 28 + 1),
                user_id: Some(1),
                user_name: Some("u1".into()),
                comment: None,
                minor: false,
                sha1: None,
                text: (*text).into(),
            });
        }
        a
    }

    #[test]
    fn rebuild_indexes_every_article_in_language() {
        let tmp = tempfile::tempdir().unwrap();
        write_article(&fixture(7, "A", &[(100, "Hello world.")]), tmp.path(), "en").unwrap();
        write_article(
            &fixture(42, "B", &[(200, "Goodbye world."), (201, "Goodbye, cruel world.")]),
            tmp.path(),
            "en",
        )
        .unwrap();

        // Sanity: the writer's sidecar already contains both articles.
        // Delete it and rebuild from scratch to exercise the rebuild path.
        let idx_path = crate::rev_id_index::index_path(tmp.path(), "en");
        fs::remove_file(&idx_path).unwrap();

        let stats = rebuild_one_language(tmp.path(), "en").unwrap();
        assert_eq!(stats.articles, 2);
        assert_eq!(stats.entries, 3);

        let idx = RevIdIndex::load(tmp.path(), "en").unwrap();
        assert_eq!(idx.lookup(100), Some(7));
        assert_eq!(idx.lookup(200), Some(42));
        assert_eq!(idx.lookup(201), Some(42));
    }

    #[test]
    fn discover_languages_lists_immediate_dirs_only() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("en")).unwrap();
        fs::create_dir_all(tmp.path().join("simple")).unwrap();
        fs::create_dir_all(tmp.path().join(".git")).unwrap();
        fs::write(tmp.path().join("README.md"), "not a lang").unwrap();
        let langs = discover_languages(tmp.path()).unwrap();
        assert_eq!(langs, vec!["en".to_string(), "simple".to_string()]);
    }

    #[test]
    fn rebuild_on_empty_language_dir_produces_empty_index() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("en")).unwrap();
        let stats = rebuild_one_language(tmp.path(), "en").unwrap();
        assert_eq!(stats.articles, 0);
        assert_eq!(stats.entries, 0);
        let idx = RevIdIndex::load(tmp.path(), "en").unwrap();
        assert!(idx.is_empty());
    }

    #[test]
    fn rebuild_errors_on_missing_language_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let err = rebuild_one_language(tmp.path(), "missing").unwrap_err();
        assert!(err.to_string().contains("not a directory"));
    }

    #[test]
    fn rebuild_detects_cross_article_rev_id_duplicate() {
        // Two articles whose history we hand-author to share a rev_id —
        // wouldn't happen on a healthy tree, but if a corrupt one lands
        // somehow, the rebuilder should refuse to silently let one win.
        // We construct that corrupt state by deleting the sidecar
        // between writes so the in-writer collision check doesn't fire.
        let tmp = tempfile::tempdir().unwrap();
        let idx_path = crate::rev_id_index::index_path(tmp.path(), "en");

        write_article(
            &fixture(7, "A", &[(500, "Hello world.")]),
            tmp.path(),
            "en",
        )
        .unwrap();
        fs::remove_file(&idx_path).unwrap();

        write_article(
            &fixture(8, "B", &[(500, "Different content here.")]),
            tmp.path(),
            "en",
        )
        .unwrap();
        // Sidecar now records 500 → page 8 only, but both article dirs
        // live on disk. Rebuild should refuse.
        fs::remove_file(&idx_path).unwrap();
        let err = rebuild_one_language(tmp.path(), "en").unwrap_err();
        assert!(
            err.to_string().contains("duplicate rev_id"),
            "expected duplicate-rev_id error, got: {err}"
        );
    }
}
