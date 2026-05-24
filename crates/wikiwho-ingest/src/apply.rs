//! Load → fetch → analyse → save loop for a single event.
//!
//! Pure(-ish) function — takes a storage root, a configured
//! `MwClient`, and one [`PageEdit`]; performs all I/O internally but
//! the public type signature is straightforward to call from tests
//! that point at a temp dir and a local mock MW.
//!
//! ## What happens
//!
//! 1. **Open snapshot.** If the article isn't on disk, return
//!    [`ApplyOutcome::SnapshotMissing`]. The server's cache-miss path
//!    handles cold builds; ingest only keeps existing articles current.
//! 2. **Check rev order.** If `event.rev_id <= article.last_good_rev_id`
//!    we've already processed this (or a later) revision — return
//!    [`ApplyOutcome::AlreadyAtOrAhead`]. Idempotent at the rev-id
//!    granularity.
//! 3. **Fetch window.** Pull every revision in
//!    `(last_good_rev_id, event.rev_id]` from the MW Action API. For
//!    the typical "single new rev" case the window contains one
//!    revision; gaps (we dropped an event, the SSE feed had a hiccup)
//!    just mean a slightly larger window.
//! 4. **Apply.** Run each fetched revision through
//!    `Article::analyse_revision` in order, skipping text-hidden ones
//!    (matches `wikiwho.py:144`).
//! 5. **Save.** Rewrite the article via `write_article`.
//!
//! ## What's deliberately not here
//!
//! - Page deletion / revision deletion handling. The legacy service
//!   has a parallel SSE listener for `mediawiki.revision-visibility-
//!   change` (see `events_stream.py:88`); we'll add that as a second
//!   stream when we get to it.
//! - Concurrency control across ingest workers. The current scaffold
//!   is single-writer; if we shard by language later we'll add
//!   per-article locks alongside the storage layer's existing
//!   tmp-file + rename pattern.

use std::path::Path;

use wikiwho_attribute::pipeline::RevisionInput;
use wikiwho_mwclient::{MwClient, MwError, Revision};
use wikiwho_storage::reader::SnapshotReader;
use wikiwho_storage::writer::write_article;

use crate::events::PageEdit;

/// Outcomes of [`apply_event`]. Only [`ApplyOutcome::Applied`]
/// touches disk; the other two are skip cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied { applied_revs: usize },
    /// No snapshot for this article on disk yet — cold builds happen
    /// in the server's cache-miss path, not here.
    SnapshotMissing,
    /// The article's `last_good_rev_id` is already at or beyond
    /// `event.rev_id`. Idempotent skip.
    AlreadyAtOrAhead,
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("mediawiki error: {0}")]
    Mw(#[from] MwError),

    #[error("storage error: {0}")]
    Storage(#[from] wikiwho_storage::StorageError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub async fn apply_event(
    storage_root: &Path,
    client: &MwClient,
    event: &PageEdit,
) -> Result<ApplyOutcome, ApplyError> {
    let reader = match SnapshotReader::open(storage_root, &event.language, event.page_id) {
        Ok(r) => r,
        Err(wikiwho_storage::StorageError::Io(err))
            if err.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(ApplyOutcome::SnapshotMissing);
        }
        Err(err) => return Err(err.into()),
    };
    let mut article = reader.article;

    let last_good = article.last_good_rev_id;
    if event.rev_id <= last_good {
        return Ok(ApplyOutcome::AlreadyAtOrAhead);
    }

    // Fetch (last_good, event.rev_id]. For the common "single new rev"
    // case the window contains exactly one revision; for gap recovery
    // it could contain a few.
    let new_revs = fetch_window(client, event.page_id, last_good, event.rev_id).await?;

    let mut applied = 0usize;
    for rev in &new_revs {
        if rev.rev_id <= last_good {
            continue; // safety net — MW occasionally echoes the start rev
        }
        if rev.text_hidden {
            continue;
        }
        article.analyse_revision(RevisionInput {
            rev_id: rev.rev_id,
            timestamp: rev.timestamp.clone(),
            text: rev.text.clone(),
            sha1: rev.sha1.clone(),
            comment: rev.comment.clone(),
            minor: rev.minor,
            user_id: rev.user_id,
            user_name: rev.user_name.clone(),
        });
        applied += 1;
    }

    write_article(&article, storage_root, &event.language)?;
    Ok(ApplyOutcome::Applied {
        applied_revs: applied,
    })
}

/// Fetch revisions `(start_exclusive, end_inclusive]` for a page.
///
/// Uses the existing `RevisionFetcher` (which paginates with
/// `rvendid=end`) and discards any rev_id <= start_exclusive
/// client-side. For the typical 1-rev case there's only one page of
/// results and the discard step throws away the start rev (which MW
/// includes by default when rvdir=newer hits the page boundary).
async fn fetch_window(
    client: &MwClient,
    page_id: u64,
    start_exclusive: u64,
    end_inclusive: u64,
) -> Result<Vec<Revision>, MwError> {
    let mut fetcher = client.fetch_revisions(page_id, end_inclusive);
    let mut out = Vec::new();
    while let Some(batch) = fetcher.next_batch().await? {
        for rev in batch.revisions {
            if rev.rev_id > start_exclusive {
                out.push(rev);
            }
        }
        if batch.saw_end {
            break;
        }
    }
    // The fetcher paginates from page beginning when no rvcontinue is
    // given; for big articles with small windows this is wasteful but
    // correct. The window-optimized path (MW `rvstartid`) is a future
    // perf win — for the scaffold the right move is to keep one code
    // path.
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(language: &str, page_id: u64, rev_id: u64, parent: u64) -> PageEdit {
        PageEdit {
            language: language.into(),
            wiki: format!("{language}wiki"),
            page_id,
            rev_id,
            parent_rev_id: parent,
            title: "T".into(),
            sse_id: None,
        }
    }

    #[tokio::test]
    async fn snapshot_missing_when_article_not_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let client = MwClient::new("en").unwrap();
        let event = ev("en", 1, 2, 1);
        let outcome = apply_event(tmp.path(), &client, &event).await.unwrap();
        assert_eq!(outcome, ApplyOutcome::SnapshotMissing);
    }

    #[tokio::test]
    async fn already_at_or_ahead_when_event_rev_in_past() {
        use wikiwho_attribute::structures::Article;
        let tmp = tempfile::tempdir().unwrap();
        let mut article = Article::new("Demo");
        article.page_id = Some(42);
        article.analyse_revision(RevisionInput {
            rev_id: 100,
            timestamp: "2024-01-01T00:00:00Z".into(),
            text: "Hello world.".into(),
            sha1: None,
            comment: None,
            minor: false,
            user_id: Some(1),
            user_name: Some("alice".into()),
        });
        write_article(&article, tmp.path(), "en").unwrap();

        let client = MwClient::new("en").unwrap();
        // Event claims rev 50, but we're at rev 100 already.
        let event = ev("en", 42, 50, 0);
        let outcome = apply_event(tmp.path(), &client, &event).await.unwrap();
        assert_eq!(outcome, ApplyOutcome::AlreadyAtOrAhead);
        // Even an exact match (rev_id == last_good) is "already at".
        let event = ev("en", 42, 100, 99);
        let outcome = apply_event(tmp.path(), &client, &event).await.unwrap();
        assert_eq!(outcome, ApplyOutcome::AlreadyAtOrAhead);
    }
}
