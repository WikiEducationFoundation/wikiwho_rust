//! Cache-miss processing pipeline.
//!
//! When a request lands for an article that isn't on disk, the server
//! does this work in a background task (PLAN.md §280-287):
//!
//! 1. Fetch the article's revision history via `wikiwho-mwclient`.
//! 2. Feed each non-hidden revision to
//!    [`wikiwho_attribute::structures::Article::analyse_revision`].
//! 3. Persist via [`wikiwho_storage::writer::write_article`].
//! 4. Refresh the in-memory title + rev_id indexes so the next
//!    request for the same article serves from disk.
//!
//! The handler that triggered the cache-miss returns the "still
//! processing" envelope (HTTP 408 per API.md §1) immediately; the
//! consumer's existing retry path picks up the persisted article on
//! a subsequent request.
//!
//! This module exposes the **pure** part of the pipeline (step 2 +
//! step 3) as [`process_and_persist`]. The spawn / orchestration
//! lives on [`crate::state::AppState`] — it owns the in-flight map.

use std::path::Path;

use wikiwho_attribute::pipeline::RevisionInput;
use wikiwho_attribute::structures::Article;
use wikiwho_mwclient::Revision;
use wikiwho_storage::writer::write_article;

/// Errors that can surface from [`process_and_persist`].
#[derive(Debug, thiserror::Error)]
pub enum CacheMissError {
    #[error("mediawiki error: {0}")]
    Mw(#[from] wikiwho_mwclient::MwError),

    #[error("storage error: {0}")]
    Storage(#[from] wikiwho_storage::StorageError),
}

/// Build a fresh [`Article`] by feeding `revisions` through
/// `analyse_revision` in order. Pure (no I/O, no async, no clock); the
/// `revisions` iterable must already be sorted oldest-first the way
/// the MW Action API delivers them (`rvdir=newer`).
///
/// `text_hidden` revisions are skipped — mirrors
/// `wikiwho.py:144` / `capture_history.py` semantics.
pub fn build_article_from_revisions<'a, I>(title: &str, page_id: u64, revisions: I) -> Article
where
    I: IntoIterator<Item = &'a Revision>,
{
    let mut article = Article::new(title);
    article.page_id = Some(page_id);
    for rev in revisions {
        if rev.text_hidden {
            continue;
        }
        article.analyse_revision(RevisionInput {
            rev_id: rev.rev_id,
            timestamp: rev.timestamp.clone(),
            sha1: rev.sha1.clone(),
            comment: rev.comment.clone(),
            minor: rev.minor,
            user_id: rev.user_id,
            user_name: rev.user_name.clone(),
            text: rev.text.clone(),
        });
    }
    article
}

/// Run the algorithm over `revisions` and persist the resulting
/// article to disk via `write_article`. Returns the final on-disk
/// [`Article`] so callers can refresh in-memory caches without a
/// re-read.
pub fn process_and_persist(
    storage_root: &Path,
    language: &str,
    title: &str,
    page_id: u64,
    revisions: &[Revision],
) -> Result<Article, CacheMissError> {
    let article = build_article_from_revisions(title, page_id, revisions.iter());
    write_article(&article, storage_root, language)?;
    Ok(article)
}

/// Collect every revision from a [`wikiwho_mwclient::RevisionFetcher`]
/// into a `Vec`, draining pages in order. Helper used by the
/// production cache-miss flow; tests typically build the `Vec`
/// themselves from a fixture's `history.jsonl`.
pub async fn collect_all_revisions(
    mut fetcher: wikiwho_mwclient::RevisionFetcher<'_>,
) -> Result<Vec<Revision>, wikiwho_mwclient::MwError> {
    let mut out = Vec::new();
    while let Some(batch) = fetcher.next_batch().await? {
        out.extend(batch.revisions);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wikiwho_attribute::response::{ResponseParameters, build_rev_content};
    use wikiwho_storage::reader::SnapshotReader;
    use wikiwho_storage::rev_id_index::RevIdIndex;

    fn rev(rev_id: u64, ts: &str, user: u64, text: &str) -> Revision {
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
    fn build_article_skips_text_hidden_revisions() {
        let revs = [
            rev(1, "2024-01-01T00:00:00Z", 10, "Hello world."),
            Revision {
                rev_id: 2,
                parent_id: 1,
                timestamp: "2024-01-02T00:00:00Z".into(),
                sha1: None,
                comment: None,
                minor: false,
                user_id: Some(11),
                user_name: Some("u11".into()),
                text: String::new(),
                text_hidden: true,
            },
            rev(3, "2024-01-03T00:00:00Z", 12, "Hello dear world."),
        ];
        let article = build_article_from_revisions("Demo", 7, revs.iter());
        // Hidden rev 2 should NOT appear in ordered_revisions.
        assert_eq!(article.ordered_revisions, vec![1, 3]);
        assert_eq!(article.page_id, Some(7));
        assert_eq!(article.title, "Demo");
    }

    #[test]
    fn process_and_persist_writes_all_storage_files_and_index() {
        let tmp = tempfile::tempdir().unwrap();
        let revs = vec![
            rev(101, "2024-01-01T00:00:00Z", 10, "Hello world."),
            rev(102, "2024-01-02T00:00:00Z", 11, "Hello there, world."),
        ];
        let article = process_and_persist(tmp.path(), "en", "Demo", 7, &revs).unwrap();
        assert_eq!(article.ordered_revisions, vec![101, 102]);

        // Storage round-trip works.
        let reader = SnapshotReader::open(tmp.path(), "en", 7).unwrap();
        assert_eq!(reader.article.ordered_revisions, vec![101, 102]);
        let resp = build_rev_content(&reader.article, &[102], ResponseParameters::ALL).unwrap();
        // The persisted article serves a response — the actual
        // byte-for-byte parity to build_rev_content on the in-memory
        // article is covered by storage's round_trip_history tests.
        assert_eq!(resp.revisions.len(), 1);

        // rev_id_index sidecar is populated.
        let idx = RevIdIndex::load(tmp.path(), "en").unwrap();
        assert_eq!(idx.lookup(101), Some(7));
        assert_eq!(idx.lookup(102), Some(7));
    }

    #[test]
    fn process_and_persist_empty_history_writes_empty_article() {
        // Edge case: an article whose every revision is text_hidden, or
        // a page that exists but has no fetchable revisions. We persist
        // an empty article rather than fail loudly — the response
        // builder will surface "no revisions" via its own error path.
        let tmp = tempfile::tempdir().unwrap();
        let article = process_and_persist(tmp.path(), "en", "Empty", 99, &[]).unwrap();
        assert!(article.ordered_revisions.is_empty());
        // meta.json + index sidecar both end up on disk.
        let idx = RevIdIndex::load(tmp.path(), "en").unwrap();
        assert!(idx.is_empty());
    }
}
