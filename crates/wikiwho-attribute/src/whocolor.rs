//! WhoColor response data builder (API.md §7–8).
//!
//! Mirrors `wikiwho_simple.get_whocolor_data` (`wikiwho_api/wikiwho/
//! wikiwho_simple.py:362-414`). Produces the per-revision data the
//! WhoColor endpoint needs:
//!
//! - One [`WhoColorToken`] per token in the requested revision,
//!   including its `conflict_score` and `age_seconds`.
//! - A revisions dict mapping each processed `rev_id` to
//!   `(timestamp, parent_rev_id, editor)` — where `parent_rev_id`
//!   is the previously-processed revision (spam-skipped revs don't
//!   appear in the chain).
//! - The `biggest_conflict_score` across all tokens (used by
//!   consumers to scale conflict-intensity colors).
//!
//! Pure algorithm data — no MW client deps and no HTML rendering.
//! The server-side handler (`wikiwho-server::handlers::whocolor`)
//! takes this output, resolves editor user_ids to names via MW,
//! computes class names (md5-hashing anons), and injects HTML
//! spans into the Parsoid-rendered article. See PLAN.md §4.6.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::structures::{Article, RevId, iter_rev_tokens};

/// One token's worth of WhoColor data, before MW editor-name
/// resolution. Mirrors `wikiwho_simple.py:390-398`. The handler adds
/// `class_name` (a derived field) on top of `editor` before serializing
/// to the wire format.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WhoColorToken {
    /// Token text (lowercased per `wikiwho.py:123`).
    pub str: String,
    /// Origin revision id — the rev that first introduced this token.
    pub o_rev_id: RevId,
    /// Revisions where the token was reintroduced after a delete.
    #[serde(rename = "in")]
    pub inbound: Vec<RevId>,
    /// Revisions where the token was deleted.
    #[serde(rename = "out")]
    pub outbound: Vec<RevId>,
    /// Raw `editor` string from `Article::revisions[o_rev_id].editor`.
    /// `str(user_id)` for registered users, `0|<name>` for anons,
    /// empty for missing.
    pub editor: String,
    /// Count of editor-vs-editor conflicts on this token. Algorithm
    /// from `wikiwho_simple.py:373-389`.
    pub conflict_score: u32,
    /// Seconds elapsed between the origin revision's timestamp and
    /// the "now" reference time supplied to [`get_whocolor_data`].
    pub age_seconds: f64,
}

/// One entry in the revisions dict — `(timestamp, parent_rev_id,
/// editor)`. The handler appends `editor_name` (a fourth element) once
/// MW user resolution is done.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WhoColorRevision {
    pub timestamp: String,
    pub parent_rev_id: RevId,
    pub editor: String,
}

/// Output of [`get_whocolor_data`]. The handler turns this into the
/// API.md §7 wire format by adding editor-name resolution and
/// class-name hashing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WhoColorData {
    pub tokens: Vec<WhoColorToken>,
    /// `(rev_id, revision_entry)` pairs in `ordered_revisions` order.
    /// Stored as a vec rather than a map because Rust's stdlib `HashMap`
    /// doesn't preserve insertion order; the JSON serializer in the
    /// server crate emits this as a single-key-object dict.
    pub revisions: Vec<(RevId, WhoColorRevision)>,
    pub biggest_conflict_score: u32,
}

/// Errors `get_whocolor_data` can surface.
#[derive(Debug, thiserror::Error)]
pub enum WhoColorError {
    /// The requested rev_id isn't in `article.revisions` — caller
    /// should serve the "still processing" envelope (API.md §7
    /// "Response (200, in progress)") for an unknown rev_id or the
    /// vandalism envelope for a spam-flagged one.
    #[error("revision {0} is not present in article (deleted or spam)")]
    UnknownRevision(RevId),
    /// A token's `origin_rev_id` doesn't match any revision in the
    /// article — would indicate corrupt on-disk state. The handler
    /// should treat this as an internal error.
    #[error("token's origin revision {0} is missing from article.revisions")]
    OrphanOriginRevision(RevId),
}

/// Compute the WhoColor data for `rev_id` against `article`.
///
/// `now_unix_seconds` is the reference timestamp for the `age` field.
/// Production passes [`SystemTime::now()`] via [`now_unix_seconds`];
/// tests pass a fixed value so assertions are deterministic.
pub fn get_whocolor_data(
    article: &Article,
    rev_id: RevId,
    now_unix_seconds: i64,
) -> Result<WhoColorData, WhoColorError> {
    let revision = article
        .revisions
        .get(&rev_id)
        .ok_or(WhoColorError::UnknownRevision(rev_id))?;

    // Per-token loop: walk tokens in document order; compute
    // conflict_score and age_seconds for each.
    let mut tokens: Vec<WhoColorToken> = Vec::new();
    let mut biggest_conflict_score: u32 = 0;
    for token_id in iter_rev_tokens(article, revision) {
        let word = article.word(token_id);
        let origin = article
            .revisions
            .get(&word.origin_rev_id)
            .ok_or(WhoColorError::OrphanOriginRevision(word.origin_rev_id))?;
        let origin_unix = parse_mw_timestamp(&origin.timestamp).unwrap_or(now_unix_seconds);
        let age_seconds = (now_unix_seconds - origin_unix) as f64;
        let conflict_score = compute_conflict_score(article, &word.inbound, &word.outbound);
        if conflict_score > biggest_conflict_score {
            biggest_conflict_score = conflict_score;
        }
        tokens.push(WhoColorToken {
            str: word.value.clone(),
            o_rev_id: word.origin_rev_id,
            inbound: word.inbound.clone(),
            outbound: word.outbound.clone(),
            editor: origin.editor.clone(),
            conflict_score,
            age_seconds,
        });
    }

    // Revisions dict: each rev_id maps to (timestamp, parent_rev_id,
    // editor) where parent is the previously-processed rev_id (so
    // spam-skipped revs don't appear). First rev's parent is 0.
    let mut revisions: Vec<(RevId, WhoColorRevision)> =
        Vec::with_capacity(article.ordered_revisions.len());
    for (idx, rid) in article.ordered_revisions.iter().enumerate() {
        let r = article
            .revisions
            .get(rid)
            .expect("ordered_revisions points at known rev");
        let parent = if idx == 0 {
            0
        } else {
            article.ordered_revisions[idx - 1]
        };
        revisions.push((
            *rid,
            WhoColorRevision {
                timestamp: r.timestamp.clone(),
                parent_rev_id: parent,
                editor: r.editor.clone(),
            },
        ));
    }

    Ok(WhoColorData {
        tokens,
        revisions,
        biggest_conflict_score,
    })
}

/// Convenience helper: same as [`get_whocolor_data`] but with
/// `now_unix_seconds` filled in from [`SystemTime::now()`].
pub fn get_whocolor_data_now(
    article: &Article,
    rev_id: RevId,
) -> Result<WhoColorData, WhoColorError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    get_whocolor_data(article, rev_id, now)
}

/// Per-token conflict score (`wikiwho_simple.py:373-389`).
///
/// Walks paired `(out, in)` deletes-and-reintroduces. A "conflict" is
/// either: (a) the previous reintroducer differs from the current
/// deleter, or (b) the deleter differs from the reintroducer. Self-
/// reverts (same editor both sides) do not count.
fn compute_conflict_score(article: &Article, inbound: &[RevId], outbound: &[RevId]) -> u32 {
    let mut conflict_score: u32 = 0;
    let mut editor_in_prev: Option<&str> = None;
    for (i, out_rev) in outbound.iter().enumerate() {
        let editor_out = article
            .revisions
            .get(out_rev)
            .map(|r| r.editor.as_str())
            .unwrap_or("");
        if let Some(prev) = editor_in_prev {
            if prev != editor_out {
                conflict_score += 1;
            }
        }
        if let Some(in_rev) = inbound.get(i) {
            let editor_in = article
                .revisions
                .get(in_rev)
                .map(|r| r.editor.as_str())
                .unwrap_or("");
            if editor_out != editor_in {
                conflict_score += 1;
            }
            editor_in_prev = Some(editor_in);
        }
    }
    conflict_score
}

/// Parse a MediaWiki revision timestamp (`YYYY-MM-DDTHH:MM:SSZ`,
/// UTC, second precision) to Unix epoch seconds. Returns `None` on
/// any parse failure — caller substitutes a reasonable default.
///
/// We roll a tiny parser rather than pull in `chrono` or `time` for
/// what's a strictly-formatted string. The MW API guarantees this
/// shape for `prop=revisions&rvprop=timestamp` output.
pub(crate) fn parse_mw_timestamp(s: &str) -> Option<i64> {
    let s = s.as_bytes();
    if s.len() < 20 {
        return None;
    }
    // Cheap validation: positions of separators.
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return None;
    }
    let to_int = |a: usize, b: usize| -> Option<i64> {
        let mut v: i64 = 0;
        for &c in &s[a..b] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as i64;
        }
        Some(v)
    };
    let year = to_int(0, 4)?;
    let month = to_int(5, 7)?;
    let day = to_int(8, 10)?;
    let hour = to_int(11, 13)?;
    let min = to_int(14, 16)?;
    let sec = to_int(17, 19)?;
    Some(days_from_civil(year as i32, month as u32, day as u32) as i64 * 86400
        + hour * 3600
        + min * 60
        + sec)
}

/// Compute days since the Unix epoch (1970-01-01) for a civil
/// `(year, month, day)`. Adapted from H. S. Hinnant's
/// `days_from_civil` algorithm — exact and constant-time, no leap-
/// year branches.
fn days_from_civil(y: i32, m: u32, d: u32) -> i32 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
    let yoe = (y - era * 400) as u32;
    let doy = (153 * if m > 2 { m - 3 } else { m + 9 } + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i32 - 719468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::RevisionInput;

    fn analyse(article: &mut Article, rev_id: RevId, ts: &str, editor: &str, text: &str) {
        article.analyse_revision(RevisionInput {
            rev_id,
            timestamp: ts.into(),
            sha1: None,
            comment: None,
            minor: false,
            user_id: editor.parse::<u64>().ok(),
            user_name: if editor.parse::<u64>().is_ok() {
                None
            } else {
                Some(editor.into())
            },
            text: text.into(),
        });
    }

    #[test]
    fn parse_timestamp_works_for_iso_z() {
        // 2000-01-01T00:00:00Z = 946684800 (well-known Unix value).
        assert_eq!(parse_mw_timestamp("2000-01-01T00:00:00Z"), Some(946684800));
        // 1970-01-01T00:00:00Z = 0.
        assert_eq!(parse_mw_timestamp("1970-01-01T00:00:00Z"), Some(0));
        // Negative isn't returned because we don't accept BC dates, but
        // any year ≥ 1970 should give a non-negative result.
        assert!(parse_mw_timestamp("2026-05-23T13:00:00Z").unwrap() > 0);
    }

    #[test]
    fn parse_timestamp_rejects_bad_shape() {
        assert!(parse_mw_timestamp("").is_none());
        assert!(parse_mw_timestamp("not-a-timestamp").is_none());
        // Wrong separators.
        assert!(parse_mw_timestamp("2026/05/23T13:00:00Z").is_none());
        // Truncated.
        assert!(parse_mw_timestamp("2026-05-23T13:00").is_none());
    }

    #[test]
    fn whocolor_returns_data_for_known_revision() {
        let mut article = Article::new("Sample");
        article.page_id = Some(42);
        analyse(&mut article, 101, "2024-01-01T00:00:00Z", "1", "Hello world.");
        analyse(&mut article, 102, "2024-01-02T00:00:00Z", "2", "Hello there world.");

        // 2026-01-01T00:00:00Z is far enough out that all tokens have
        // a strictly positive age.
        let now = parse_mw_timestamp("2026-01-01T00:00:00Z").unwrap();
        let data = get_whocolor_data(&article, 102, now).unwrap();

        assert_eq!(data.revisions.len(), 2);
        let (rid0, rev0) = &data.revisions[0];
        assert_eq!(*rid0, 101);
        assert_eq!(rev0.parent_rev_id, 0);
        assert_eq!(rev0.editor, "1");

        let (rid1, rev1) = &data.revisions[1];
        assert_eq!(*rid1, 102);
        assert_eq!(rev1.parent_rev_id, 101);
        assert_eq!(rev1.editor, "2");

        // Every token in rev 102 has age relative to its origin's
        // timestamp; all origins are at most 2024-01-02, so age must
        // be at least ~63 million seconds (2 years).
        for t in &data.tokens {
            assert!(t.age_seconds > 60_000_000.0);
        }
        // No conflicts on a clean 2-rev add-only history.
        assert_eq!(data.biggest_conflict_score, 0);
    }

    #[test]
    fn whocolor_errors_on_unknown_revision() {
        let mut article = Article::new("Sample");
        analyse(&mut article, 101, "2024-01-01T00:00:00Z", "1", "Hello.");
        let err = get_whocolor_data(&article, 999, 0).unwrap_err();
        assert!(matches!(err, WhoColorError::UnknownRevision(999)));
    }

    #[test]
    fn conflict_score_pure_function_zero_when_no_history() {
        // No outbound / no inbound = 0.
        let article = Article::new("X");
        assert_eq!(compute_conflict_score(&article, &[], &[]), 0);
    }

    #[test]
    fn conflict_score_counts_deleter_vs_reintroducer() {
        // Build an article with three revisions and use synthetic
        // inbound/outbound lists rather than going through the
        // algorithm — the function under test is the pure conflict
        // computation, not the cascade.
        let mut article = Article::new("X");
        article.revisions.insert(
            10,
            crate::structures::Revision {
                id: 10,
                editor: "alice".into(),
                timestamp: "2024-01-01T00:00:00Z".into(),
                ..Default::default()
            },
        );
        article.revisions.insert(
            20,
            crate::structures::Revision {
                id: 20,
                editor: "bob".into(),
                timestamp: "2024-01-02T00:00:00Z".into(),
                ..Default::default()
            },
        );
        article.revisions.insert(
            30,
            crate::structures::Revision {
                id: 30,
                editor: "alice".into(),
                timestamp: "2024-01-03T00:00:00Z".into(),
                ..Default::default()
            },
        );

        // Token deleted by bob (20), then re-added by alice (30).
        // bob != alice → conflict_score = 1.
        assert_eq!(compute_conflict_score(&article, &[30], &[20]), 1);

        // Self-revert: deleted by alice, re-added by alice. Same
        // editor on both ends → 0.
        article.revisions.insert(
            40,
            crate::structures::Revision {
                id: 40,
                editor: "alice".into(),
                timestamp: "2024-01-04T00:00:00Z".into(),
                ..Default::default()
            },
        );
        assert_eq!(compute_conflict_score(&article, &[40], &[20]), 1);
        // Note: still 1 because the outbound editor "bob" differs from
        // the inbound "alice". The "self-revert" exemption is about
        // the *deleter* matching the *reintroducer*, not the original
        // author.
    }

    #[test]
    fn conflict_score_counts_chained_reverts() {
        // Three-cycle: deleter1 != reintroducer1, then
        // reintroducer1 (now editor_in_prev) != deleter2 → +1 per pair.
        let mut article = Article::new("X");
        for (id, ed) in [(10, "alice"), (20, "bob"), (30, "carol"), (40, "dave")] {
            article.revisions.insert(
                id,
                crate::structures::Revision {
                    id,
                    editor: ed.into(),
                    timestamp: "2024-01-01T00:00:00Z".into(),
                    ..Default::default()
                },
            );
        }
        // Pair 0: out=20(bob), in=30(carol). editor_in_prev=None
        //         → +1 (bob != carol). editor_in_prev := carol.
        // Pair 1: out=40(dave), in=?    . editor_in_prev=carol != dave
        //         → +1. No matching in → loop ends.
        // Total: 2.
        assert_eq!(compute_conflict_score(&article, &[30], &[20, 40]), 2);
    }
}
