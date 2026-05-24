//! Load an on-disk blob back into an in-memory [`Article`].
//!
//! Inverse of [`crate::writer::write_article`]. The reconstructed
//! `Article` is fully ready for the algorithm to resume from disk —
//! every field that `Article::analyse_revision` reads is populated:
//!
//! - `tokens` arena (from `tokens.bin` + `strings.bin`).
//! - `paragraphs` and `sentences` arenas (from `paragraphs.bin` +
//!   `sentences.bin`).
//! - `paragraphs_ht` and `sentences_ht` with full hash → arena id
//!   buckets (from `hashtables.bin`).
//! - `revisions` map keyed by rev_id, each with:
//!   - `paragraphs` (HashMap<Hash, Vec<ParagraphId>>) +
//!     `ordered_paragraphs` (Vec<Hash>) rebuilt from the per-rev
//!     ordered list in `revisions.bin`.
//!   - `length`, `original_adds`, `editor`, `timestamp`.
//!   - `token_sequence_override` populated as a fast path for
//!     `iter_rev_tokens` — the algorithm-side walk through paragraphs
//!     → sentences → words would still work because the arenas are
//!     present, but the override keeps `rev_content` cheap.
//! - `ordered_revisions`, `last_good_rev_id`, `spam_ids`,
//!   `spam_hashes`, `next_token_id` from `meta.json`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use wikiwho_attribute::structures::{
    Article, Paragraph, ParagraphId, Revision, Sentence, SentenceId, Word,
};

use crate::hashtables::{parse_hashtables_blob, HashTables};
use crate::layout::{
    article_dir, HASHTABLES_FILE, META_FILE, PARAGRAPHS_FILE, REVISIONS_FILE, SENTENCES_FILE,
    STRINGS_FILE, TOKENS_FILE,
};
use crate::meta::Meta;
use crate::paragraphs::parse_paragraphs_blob;
use crate::revisions::parse_revisions_blob;
use crate::sentences::parse_sentences_blob;
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
        let paragraphs_bytes = fs::read(dir.join(PARAGRAPHS_FILE))?;
        let sentences_bytes = fs::read(dir.join(SENTENCES_FILE))?;
        let hashtables_bytes = fs::read(dir.join(HASHTABLES_FILE))?;

        let strings = parse_strings_blob(&strings_bytes)?;
        let stored_tokens = parse_tokens_blob(&tokens_bytes)?;
        let stored_revisions = parse_revisions_blob(&revisions_bytes)?;
        let stored_paragraphs = parse_paragraphs_blob(&paragraphs_bytes)?;
        let stored_sentences = parse_sentences_blob(&sentences_bytes)?;
        let hashtables = parse_hashtables_blob(&hashtables_bytes)?;

        // Re-hydrate the Article. Field-for-field:
        let mut article = Article::new(meta.title.clone());
        article.page_id = Some(meta.page_id);
        article.next_token_id = meta.next_token_id;

        // Token arena.
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

        // Sentence arena. The `sentences` map on each `Paragraph` is
        // rebuilt below by grouping the `ordered_sentences` list, not
        // stored explicitly.
        article.sentences.reserve_exact(stored_sentences.len());
        for s in &stored_sentences {
            article.sentences.push(Sentence {
                hash_value: s.hash_value.clone(),
                value: s.value.clone(),
                words: s.words.clone(),
            });
        }

        // Paragraph arena. For each stored paragraph, rebuild the
        // `sentences: HashMap<Hash, Vec<SentenceId>>` map by walking
        // `ordered_sentences` and grouping repeated hashes — the
        // exact inverse of `project_paragraphs` in the writer.
        article.paragraphs.reserve_exact(stored_paragraphs.len());
        for p in &stored_paragraphs {
            let mut sentences_by_hash: HashMap<String, Vec<SentenceId>> = HashMap::new();
            let mut ordered_sentences: Vec<String> =
                Vec::with_capacity(p.ordered_sentences.len());
            for entry in &p.ordered_sentences {
                sentences_by_hash
                    .entry(entry.hash.clone())
                    .or_default()
                    .push(entry.sentence_id);
                ordered_sentences.push(entry.hash.clone());
            }
            article.paragraphs.push(Paragraph {
                hash_value: p.hash_value.clone(),
                value: p.value.clone(),
                sentences: sentences_by_hash,
                ordered_sentences,
            });
        }

        // Cross-revision hash tables: full arena-id buckets.
        for b in &hashtables.paragraph_buckets {
            article
                .paragraphs_ht
                .insert(b.hash.clone(), b.arena_ids.clone());
        }
        for b in &hashtables.sentence_buckets {
            article
                .sentences_ht
                .insert(b.hash.clone(), b.arena_ids.clone());
        }

        // Revisions map + ordered list. Each revision rehydrates with
        // both the paragraph references (the resume path) and the
        // flat token sequence (the read-hot path).
        for rev in &stored_revisions {
            let mut paragraphs_by_hash: HashMap<String, Vec<ParagraphId>> = HashMap::new();
            let mut ordered_paragraphs: Vec<String> =
                Vec::with_capacity(rev.ordered_paragraphs.len());
            for entry in &rev.ordered_paragraphs {
                paragraphs_by_hash
                    .entry(entry.hash.clone())
                    .or_default()
                    .push(entry.paragraph_id);
                ordered_paragraphs.push(entry.hash.clone());
            }
            let r = Revision {
                id: rev.rev_id,
                editor: rev.editor.clone(),
                timestamp: rev.timestamp.clone(),
                length: rev.length as usize,
                original_adds: rev.original_adds,
                paragraphs: paragraphs_by_hash,
                ordered_paragraphs,
                token_sequence_override: Some(rev.token_sequence.clone()),
            };
            article.ordered_revisions.push(rev.rev_id);
            article.revisions.insert(rev.rev_id, r);
        }

        article.last_good_rev_id = meta.last_processed_revid;
        article.spam_ids = meta.spam_revisions.clone();
        article.spam_hashes = meta.spam_hashes.iter().cloned().collect();

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

    /// The reader populates every Article field that
    /// `analyse_revision` reads — so the loaded Article is
    /// resume-from-disk ready, not just rev_content-serving ready.
    #[test]
    fn round_trip_preserves_full_article_state() {
        let article = fixture_article();
        let tmp = tempfile::tempdir().unwrap();
        crate::writer::write_article(&article, tmp.path(), "en").unwrap();
        let reader = SnapshotReader::open(tmp.path(), "en", 7).unwrap();
        let loaded = &reader.article;

        // Arena sizes.
        assert_eq!(loaded.tokens.len(), article.tokens.len());
        assert_eq!(loaded.sentences.len(), article.sentences.len());
        assert_eq!(loaded.paragraphs.len(), article.paragraphs.len());

        // Token arena field-by-field.
        for (i, w) in article.tokens.iter().enumerate() {
            let l = &loaded.tokens[i];
            assert_eq!(l.value, w.value, "token {i} value");
            assert_eq!(l.origin_rev_id, w.origin_rev_id, "token {i} origin");
            assert_eq!(l.last_rev_id, w.last_rev_id, "token {i} last");
            assert_eq!(l.inbound, w.inbound, "token {i} inbound");
            assert_eq!(l.outbound, w.outbound, "token {i} outbound");
        }

        // Sentence arena.
        for (i, s) in article.sentences.iter().enumerate() {
            let l = &loaded.sentences[i];
            assert_eq!(l.hash_value, s.hash_value, "sentence {i} hash");
            assert_eq!(l.value, s.value, "sentence {i} value");
            assert_eq!(l.words, s.words, "sentence {i} words");
        }

        // Paragraph arena. The `sentences` HashMap is reconstructed by
        // grouping `ordered_sentences`; compare by sorted (hash, ids)
        // pairs to avoid relying on HashMap iteration order.
        for (i, p) in article.paragraphs.iter().enumerate() {
            let l = &loaded.paragraphs[i];
            assert_eq!(l.hash_value, p.hash_value, "paragraph {i} hash");
            assert_eq!(l.value, p.value, "paragraph {i} value");
            assert_eq!(l.ordered_sentences, p.ordered_sentences, "paragraph {i} ordered");
            for (h, ids) in &p.sentences {
                assert_eq!(l.sentences.get(h), Some(ids), "paragraph {i} sentences[{h}]");
            }
            assert_eq!(l.sentences.len(), p.sentences.len(), "paragraph {i} sentences map size");
        }

        // Cross-revision hash tables.
        for (h, ids) in &article.paragraphs_ht {
            assert_eq!(loaded.paragraphs_ht.get(h), Some(ids), "paragraphs_ht[{h}]");
        }
        assert_eq!(loaded.paragraphs_ht.len(), article.paragraphs_ht.len());
        for (h, ids) in &article.sentences_ht {
            assert_eq!(loaded.sentences_ht.get(h), Some(ids), "sentences_ht[{h}]");
        }
        assert_eq!(loaded.sentences_ht.len(), article.sentences_ht.len());

        // Per-revision state.
        assert_eq!(loaded.ordered_revisions, article.ordered_revisions);
        for (rev_id, rev) in &article.revisions {
            let l = loaded
                .revisions
                .get(rev_id)
                .unwrap_or_else(|| panic!("missing rev {rev_id}"));
            assert_eq!(l.editor, rev.editor, "rev {rev_id} editor");
            assert_eq!(l.timestamp, rev.timestamp, "rev {rev_id} ts");
            assert_eq!(l.length, rev.length, "rev {rev_id} length");
            assert_eq!(l.original_adds, rev.original_adds, "rev {rev_id} adds");
            assert_eq!(l.ordered_paragraphs, rev.ordered_paragraphs, "rev {rev_id} ordered");
            for (h, ids) in &rev.paragraphs {
                assert_eq!(l.paragraphs.get(h), Some(ids), "rev {rev_id} paragraphs[{h}]");
            }
            assert_eq!(l.paragraphs.len(), rev.paragraphs.len(), "rev {rev_id} paragraphs len");
        }

        // Spam + counters.
        assert_eq!(loaded.last_good_rev_id, article.last_good_rev_id);
        assert_eq!(loaded.next_token_id, article.next_token_id);
        assert_eq!(loaded.spam_ids, article.spam_ids);
        assert_eq!(loaded.spam_hashes, article.spam_hashes);
    }

    #[test]
    fn hashtables_round_trip_hashes() {
        let article = fixture_article();
        let tmp = tempfile::tempdir().unwrap();
        crate::writer::write_article(&article, tmp.path(), "en").unwrap();
        let reader = SnapshotReader::open(tmp.path(), "en", 7).unwrap();
        // Every paragraph hash the algorithm produced should be in the
        // loaded hash table — and the bucket should match arena ids.
        for (h, ids) in &article.paragraphs_ht {
            let bucket = reader
                .hashtables
                .paragraph_buckets
                .iter()
                .find(|b| &b.hash == h)
                .unwrap_or_else(|| panic!("missing paragraph hash {h}"));
            assert_eq!(&bucket.arena_ids, ids, "paragraph bucket mismatch for {h}");
        }
        for (h, ids) in &article.sentences_ht {
            let bucket = reader
                .hashtables
                .sentence_buckets
                .iter()
                .find(|b| &b.hash == h)
                .unwrap_or_else(|| panic!("missing sentence hash {h}"));
            assert_eq!(&bucket.arena_ids, ids, "sentence bucket mismatch for {h}");
        }
    }
}
