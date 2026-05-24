//! `meta.json` — small JSON-encoded per-article header.
//!
//! Kept tiny and human-readable so `find` + `jq` can survey the corpus
//! cheaply. Per STORAGE.md §2.1 this is required for every article;
//! the binary files won't be touched if a tool only needs to know
//! "what's the latest rev_id for this article."

use serde::{Deserialize, Serialize};

/// On-disk metadata stamp for one article.
///
/// Fields the spec lists as `appendlog_*` and `*_checksum` are omitted
/// until the append-log lands. Adding them later is purely additive
/// from a JSON-shape perspective.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// The on-disk schema version this stamp was written against.
    /// Bumped when a binary file's layout changes.
    pub schema_version: u16,
    pub page_id: u64,
    pub language: String,
    pub title: String,

    /// Most recent rev_id that was processed by the algorithm (spam
    /// revisions do NOT advance this — matches `last_good_rev_id` in
    /// the in-memory `Article`).
    pub last_processed_revid: u64,

    /// Same as the timestamp string the MW API returns
    /// (`YYYY-MM-DDTHH:MM:SSZ`); empty before any revision has been
    /// processed.
    pub last_processed_timestamp: String,

    /// Continuation token from the MW Action API, used by the lazy
    /// ingest path to resume from where we left off. Empty when the
    /// article is fully caught up.
    #[serde(default)]
    pub rvcontinue: String,

    pub n_revisions: u64,
    pub n_lifetime_tokens: u64,
    pub n_spam_revisions: u64,

    /// `Article::next_token_id` at the time of write. Lets a future
    /// resume-from-disk path keep token-id assignment monotonic
    /// across restarts.
    pub next_token_id: u32,

    /// Spam revision ids in detection order. Mirrors
    /// `Article::spam_ids`; needed so the resume-from-disk path can
    /// match `--show-spam-ids` output byte-for-byte and so a new
    /// revision's spam-detection cascade has the same `spam_ids`
    /// snapshot the in-memory algorithm would.
    #[serde(default)]
    pub spam_revisions: Vec<u64>,

    /// Revision sha1 hashes flagged as spam. Mirrors
    /// `Article::spam_hashes` — the first sanity check on a newly
    /// arriving revision is "is this sha1 in spam_hashes?".
    /// `wikiwho.py:80-82` / `:156-158`.
    #[serde(default)]
    pub spam_hashes: Vec<String>,
}

impl Meta {
    pub fn new(
        page_id: u64,
        language: impl Into<String>,
        title: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: crate::SCHEMA_VERSION,
            page_id,
            language: language.into(),
            title: title.into(),
            last_processed_revid: 0,
            last_processed_timestamp: String::new(),
            rvcontinue: String::new(),
            n_revisions: 0,
            n_lifetime_tokens: 0,
            n_spam_revisions: 0,
            next_token_id: 0,
            spam_revisions: Vec::new(),
            spam_hashes: Vec::new(),
        }
    }

    pub fn to_pretty_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json(s: &str) -> serde_json::Result<Self> {
        serde_json::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let m = Meta {
            schema_version: 2,
            page_id: 534366,
            language: "en".into(),
            title: "Barack_Obama".into(),
            last_processed_revid: 1212345678,
            last_processed_timestamp: "2024-03-14T15:09:26Z".into(),
            rvcontinue: "20240314150926|1212345678".into(),
            n_revisions: 56789,
            n_lifetime_tokens: 412345,
            n_spam_revisions: 1023,
            next_token_id: 412345,
            spam_revisions: vec![123, 456, 789],
            spam_hashes: vec!["sha1_a".into(), "sha1_b".into()],
        };
        let s = m.to_pretty_json().unwrap();
        let back: Meta = Meta::from_json(&s).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn default_constructor_uses_schema_version_constant() {
        let m = Meta::new(7, "en", "Demo");
        assert_eq!(m.schema_version, crate::SCHEMA_VERSION);
        assert_eq!(m.title, "Demo");
        assert_eq!(m.language, "en");
        assert_eq!(m.page_id, 7);
        assert_eq!(m.last_processed_revid, 0);
        assert!(m.rvcontinue.is_empty());
    }
}
