//! Load an on-disk blob back into an in-memory [`Article`].
//!
//! Inverse of [`crate::writer::write_article`]. The reconstructed
//! `Article` has enough state to serve `rev_content` via
//! [`wikiwho_attribute::response::build_rev_content`]:
//!
//! - `tokens` arena (from `tokens.bin` + `strings.bin`).
//! - `revisions` map keyed by rev_id, each with `token_sequence_override`
//!   populated so `iter_rev_tokens` returns the persisted sequence
//!   without walking paragraph/sentence arenas.
//! - `ordered_revisions` in the original processing order (so the
//!   two-rev range form of `rev_content` works).
//! - `paragraphs_ht` and `sentences_ht` populated as hash → empty
//!   `Vec` since we don't yet persist back-references. These are
//!   informational only on the read path; the algorithm-resume path
//!   that needs them is deferred.
//!
//! The paragraph and sentence arenas stay empty — they're not
//! persisted and `iter_rev_tokens` won't be asked to walk them
//! thanks to the override field.

use std::fs;
use std::path::{Path, PathBuf};

use wikiwho_attribute::structures::{Article, Word};

use crate::hashtables::{parse_hashtables_blob, HashTables};
use crate::layout::{
    article_dir, HASHTABLES_FILE, META_FILE, REVISIONS_FILE, STRINGS_FILE, TOKENS_FILE,
};
use crate::meta::Meta;
use crate::revisions::parse_revisions_blob;
use crate::strings::parse_strings_blob;
use crate::tokens::parse_tokens_blob;
use crate::{Result, StorageError, SCHEMA_VERSION};

/// Read one article directory off disk.
///
/// Validates magic + CRC on every binary file and refuses to open a
/// directory whose `schema_version` exceeds what this build understands.
pub struct SnapshotReader {
    pub meta: Meta,
    pub article: Article,
    pub hashtables: HashTables,
    pub dir: PathBuf,
}

impl SnapshotReader {
    pub fn open(volume: &Path, language: &str, page_id: u64) -> Result<Self> {
        let dir = article_dir(volume, language, page_id);
        Self::open_dir(dir)
    }

    /// Open a snapshot at a specific directory, bypassing the
    /// volume-sharded layout. Useful for tests and for tooling that
    /// knows the path directly.
    pub fn open_dir(dir: PathBuf) -> Result<Self> {
        let meta_json = fs::read_to_string(dir.join(META_FILE))?;
        let meta = Meta::from_json(&meta_json)?;
        if meta.schema_version > SCHEMA_VERSION {
            return Err(StorageError::UnsupportedVersion {
                file: "meta.json",
                got: meta.schema_version,
                max: SCHEMA_VERSION,
            });
        }

        let strings_bytes = fs::read(dir.join(STRINGS_FILE))?;
        let tokens_bytes = fs::read(dir.join(TOKENS_FILE))?;
        let revisions_bytes = fs::read(dir.join(REVISIONS_FILE))?;
        let hashtables_bytes = fs::read(dir.join(HASHTABLES_FILE))?;

        let strings = parse_strings_blob(&strings_bytes)?;
        let stored_tokens = parse_tokens_blob(&tokens_bytes)?;
        let stored_revisions = parse_revisions_blob(&revisions_bytes)?;
        let hashtables = parse_hashtables_blob(&hashtables_bytes)?;

        // Re-hydrate the Article. Field-for-field:
        let mut article = Article::new(meta.title.clone());
        article.page_id = Some(meta.page_id);
        article.next_token_id = meta.next_token_id;

        // Token arena (paragraph/sentence arenas stay empty).
        article.tokens.reserve_exact(stored_tokens.len());
        for (id, st) in stored_tokens.iter().enumerate() {
            let value = strings
                .get(st.string_id as usize)
                .ok_or_else(|| StorageError::Malformed {
                    file: "tokens.bin",
                    detail: format!(
                        "token {id} references string_id {} but only {} strings present",
                        st.string_id,
                        strings.len()
                    ),
                })?
                .clone();
            article.tokens.push(Word {
                token_id: id as u32,
                value,
                origin_rev_id: st.origin_rev_id,
                last_rev_id: st.last_rev_id,
                inbound: st.inbound.clone(),
                outbound: st.outbound.clone(),
            });
        }

        // Revisions map + ordered list. revisions.bin returns
        // processing order; that's what we want.
        for rev in &stored_revisions {
            let r = wikiwho_attribute::structures::Revision {
                id: rev.rev_id,
                editor: rev.editor.clone(),
                timestamp: rev.timestamp.clone(),
                token_sequence_override: Some(rev.token_sequence.clone()),
                ..Default::default()
            };
            article.ordered_revisions.push(rev.rev_id);
            article.revisions.insert(rev.rev_id, r);
        }

        // last_good_rev_id from meta (mirrors what the writer recorded).
        article.last_good_rev_id = meta.last_processed_revid;

        // Cross-revision hash tables: hash set membership only, no
        // arena back-references yet (see writer + hashtables.rs).
        for (h, _count) in &hashtables.paragraph_hashes {
            article.paragraphs_ht.insert(h.clone(), Vec::new());
        }
        for (h, _count) in &hashtables.sentence_hashes {
            article.sentences_ht.insert(h.clone(), Vec::new());
        }

        Ok(Self {
            meta,
            article,
            hashtables,
            dir,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wikiwho_attribute::pipeline::{RevisionInput, RevisionOutcome};
    use wikiwho_attribute::response::{build_rev_content, ResponseParameters};

    fn fixture_article() -> Article {
        let mut article = Article::new("Demo");
        article.page_id = Some(7);
        let revs = [
            (101u64, 11u64, "2024-01-01T00:00:00Z", "Hello there friend."),
            (102, 22, "2024-01-02T00:00:00Z", "Hello dear friend."),
            (103, 33, "2024-01-03T00:00:00Z", "Hello dear best friend."),
        ];
        for (rev_id, user_id, ts, text) in revs {
            assert_eq!(
                article.analyse_revision(RevisionInput {
                    rev_id,
                    timestamp: ts.into(),
                    user_id: Some(user_id),
                    user_name: Some(format!("u{user_id}")),
                    comment: None,
                    minor: false,
                    sha1: None,
                    text: text.into(),
                }),
                RevisionOutcome::Stored,
                "rev {rev_id}"
            );
        }
        article
    }

    #[test]
    fn round_trip_yields_byte_identical_rev_content() {
        let article = fixture_article();

        // Capture the in-memory response shape.
        let before = build_rev_content(&article, &[102], ResponseParameters::ALL).unwrap();
        let before_json = serde_json::to_string(&before).unwrap();

        // Persist + reload.
        let tmp = tempfile::tempdir().unwrap();
        crate::writer::write_article(&article, tmp.path(), "en").unwrap();
        let reader = SnapshotReader::open(tmp.path(), "en", 7).unwrap();

        // The loaded Article serves the same response.
        let after =
            build_rev_content(&reader.article, &[102], ResponseParameters::ALL).unwrap();
        let after_json = serde_json::to_string(&after).unwrap();
        assert_eq!(before_json, after_json);
    }

    #[test]
    fn round_trip_handles_two_rev_id_range() {
        let article = fixture_article();
        let before = build_rev_content(&article, &[101, 103], ResponseParameters::ALL).unwrap();
        let before_json = serde_json::to_string(&before).unwrap();

        let tmp = tempfile::tempdir().unwrap();
        crate::writer::write_article(&article, tmp.path(), "en").unwrap();
        let reader = SnapshotReader::open(tmp.path(), "en", 7).unwrap();

        let after =
            build_rev_content(&reader.article, &[101, 103], ResponseParameters::ALL).unwrap();
        let after_json = serde_json::to_string(&after).unwrap();
        assert_eq!(before_json, after_json);
    }

    #[test]
    fn round_trip_with_no_optional_fields() {
        let article = fixture_article();
        let before = build_rev_content(&article, &[102], ResponseParameters::NONE).unwrap();
        let before_json = serde_json::to_string(&before).unwrap();

        let tmp = tempfile::tempdir().unwrap();
        crate::writer::write_article(&article, tmp.path(), "en").unwrap();
        let reader = SnapshotReader::open(tmp.path(), "en", 7).unwrap();

        let after =
            build_rev_content(&reader.article, &[102], ResponseParameters::NONE).unwrap();
        let after_json = serde_json::to_string(&after).unwrap();
        assert_eq!(before_json, after_json);
    }

    #[test]
    fn meta_round_trips() {
        let article = fixture_article();
        let tmp = tempfile::tempdir().unwrap();
        crate::writer::write_article(&article, tmp.path(), "en").unwrap();
        let reader = SnapshotReader::open(tmp.path(), "en", 7).unwrap();
        assert_eq!(reader.meta.page_id, 7);
        assert_eq!(reader.meta.language, "en");
        assert_eq!(reader.meta.title, "Demo");
        assert_eq!(reader.meta.n_revisions, 3);
        assert_eq!(reader.meta.last_processed_revid, 103);
        assert_eq!(reader.meta.last_processed_timestamp, "2024-01-03T00:00:00Z");
        assert_eq!(reader.meta.n_lifetime_tokens, article.tokens.len() as u64);
    }

    #[test]
    fn hashtables_round_trip_hashes() {
        let article = fixture_article();
        let tmp = tempfile::tempdir().unwrap();
        crate::writer::write_article(&article, tmp.path(), "en").unwrap();
        let reader = SnapshotReader::open(tmp.path(), "en", 7).unwrap();
        // Every paragraph hash the algorithm produced should be in the
        // loaded hash table.
        for h in article.paragraphs_ht.keys() {
            assert!(
                reader
                    .hashtables
                    .paragraph_hashes
                    .iter()
                    .any(|(s, _)| s == h),
                "missing paragraph hash {h}"
            );
        }
        for h in article.sentences_ht.keys() {
            assert!(
                reader
                    .hashtables
                    .sentence_hashes
                    .iter()
                    .any(|(s, _)| s == h),
                "missing sentence hash {h}"
            );
        }
    }
}
