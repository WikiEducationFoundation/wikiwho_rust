//! Translate an in-memory [`Article`] into the on-disk format.
//!
//! Implements the wholesale-rewrite path resolved in STORAGE.md §4
//! Strategy B. Append-log support is a follow-up — for now every
//! `write_article` produces a fresh set of files from scratch.
//!
//! Write order:
//!
//! 1. Build the string-interning table (so token records reference
//!    string ids, not raw strings).
//! 2. Project [`Article::tokens`] into [`StoredToken`] records.
//! 3. Walk each revision once via [`iter_rev_tokens`] to capture its
//!    token sequence.
//! 4. Aggregate paragraph + sentence hash counts.
//! 5. Write `strings.bin`, `tokens.bin`, `revisions.bin`,
//!    `hashtables.bin`, `meta.json` into the article directory.
//!
//! The writer does NOT perform the atomic-rename dance for crash
//! safety yet (STORAGE.md §2.6). That's a follow-up at compaction
//! time. For first-cut single-process tests the direct write is fine.

use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use wikiwho_attribute::structures::{Article, iter_rev_tokens};

use crate::hashtables::{HashBucket, HashTables};
use crate::layout::{
    article_dir, HASHTABLES_FILE, META_FILE, PARAGRAPHS_FILE, REVISIONS_FILE, SENTENCES_FILE,
    STRINGS_FILE, TOKENS_FILE,
};
use crate::meta::Meta;
use crate::paragraphs::{write_paragraphs, StoredOrderedSentence, StoredParagraph};
use crate::rev_id_index::RevIdIndex;
use crate::revisions::{write_revisions, StoredOrderedParagraph, StoredRevision};
use crate::sentences::{write_sentences, StoredSentence};
use crate::strings::write_strings;
use crate::tokens::{write_tokens, StoredToken};
use crate::{Result, StorageError};

/// zstd level for the per-article `.bin` files. Level 3 (the zstd
/// default) hits 100-300× compression on the highly repetitive
/// token-id / paragraph-id streams in `revisions.bin` + `paragraphs.bin`
/// and 2-9× on the other four (`tokens.bin`, `sentences.bin`,
/// `strings.bin`, `hashtables.bin`). See
/// `notes/2026-05-25-storage-compression.md` for the per-file
/// measurements. Higher levels save only a few extra percent.
const ZSTD_LEVEL: i32 = 3;

/// Write `article` to disk under `volume`, sharded by language +
/// page_id (see [`article_dir`]). Returns the directory path that was
/// written.
pub fn write_article(article: &Article, volume: &Path, language: &str) -> Result<PathBuf> {
    let page_id = article.page_id.ok_or_else(|| StorageError::Malformed {
        file: "meta.json",
        detail: "article has no page_id; storage layout requires one".into(),
    })?;
    let dir = article_dir(volume, language, page_id);
    fs::create_dir_all(&dir)?;

    let (strings, tokens) = project_tokens(article);
    let revisions = project_revisions(article);
    let paragraphs = project_paragraphs(article);
    let sentences = project_sentences(article);
    let hashtables = project_hashtables(article);
    let meta = project_meta(article, language);

    write_strings_file(&dir, &strings)?;
    write_tokens_file(&dir, &tokens)?;
    write_revisions_file(&dir, &revisions)?;
    write_paragraphs_file(&dir, &paragraphs)?;
    write_sentences_file(&dir, &sentences)?;
    write_hashtables_file(&dir, &hashtables)?;
    write_meta_file(&dir, &meta)?;

    // Per-language `rev_id_index.bin` sidecar. This is the authoritative
    // map endpoint 1 (`/rev_content/rev_id/{rev_id}/`) reads from.
    // Write order matters: the article's per-article files must exist
    // before we publish its rev_ids to the index, so a concurrent
    // reader chasing a new rev_id always finds the article files on
    // disk. (Reverse order would expose a tiny window where the index
    // points at an article that's still being written.)
    RevIdIndex::update_for_article(volume, language, page_id, &article.ordered_revisions)?;

    Ok(dir)
}

/// Build the string-interning table + projected token records.
///
/// Token strings are deduplicated; the order of first appearance in
/// `article.tokens` determines the string id. This makes string_id
/// stable across re-runs of the writer on the same Article, which is
/// useful for deterministic-output testing.
fn project_tokens<'a>(article: &'a Article) -> (Vec<&'a str>, Vec<StoredToken>) {
    let mut strings: Vec<&'a str> = Vec::new();
    let mut interner: HashMap<&'a str, u32> = HashMap::new();
    let mut tokens = Vec::with_capacity(article.tokens.len());

    for w in &article.tokens {
        let s = w.value.as_str();
        let id = *interner.entry(s).or_insert_with(|| {
            let id = strings.len() as u32;
            strings.push(s);
            id
        });
        tokens.push(StoredToken {
            string_id: id,
            origin_rev_id: w.origin_rev_id,
            last_rev_id: w.last_rev_id,
            inbound: w.inbound.clone(),
            outbound: w.outbound.clone(),
        });
    }

    (strings, tokens)
}

/// Project every stored revision into the on-disk shape, in
/// processing order (`article.ordered_revisions`).
///
/// For each revision we capture both the flat `token_sequence` (the
/// read-hot path) and the per-rev `ordered_paragraphs` list (the
/// resume-from-disk path). The two are derived from the same in-memory
/// state — `Revision::paragraphs` + `Revision::ordered_paragraphs` —
/// but live in different files because they're consumed by different
/// callers.
fn project_revisions(article: &Article) -> Vec<StoredRevision> {
    article
        .ordered_revisions
        .iter()
        .filter_map(|rev_id| {
            article.revisions.get(rev_id).map(|rev| {
                let sequence = iter_rev_tokens(article, rev);
                let ordered_paragraphs = project_rev_ordered_paragraphs(rev);
                StoredRevision {
                    rev_id: rev.id,
                    timestamp: rev.timestamp.clone(),
                    editor: rev.editor.clone(),
                    length: rev.length as u64,
                    original_adds: rev.original_adds,
                    token_sequence: sequence,
                    ordered_paragraphs,
                }
            })
        })
        .collect()
}

/// Walk a revision's `ordered_paragraphs` (hash list) and pair each
/// position with the matching paragraph_id from `revision.paragraphs`.
/// Same disambiguation logic as `iter_rev_tokens`: when a hash appears
/// N>1 times in the ordered list, the N entries in
/// `revision.paragraphs[hash]` correspond 1:1 in order.
fn project_rev_ordered_paragraphs(
    rev: &wikiwho_attribute::structures::Revision,
) -> Vec<StoredOrderedParagraph> {
    let mut seen_count: HashMap<&str, usize> = HashMap::new();
    let mut out = Vec::with_capacity(rev.ordered_paragraphs.len());
    for hash in &rev.ordered_paragraphs {
        let n = seen_count.entry(hash.as_str()).or_insert(0);
        let pid = rev
            .paragraphs
            .get(hash)
            .and_then(|v| v.get(*n).copied())
            .unwrap_or_default();
        *n += 1;
        out.push(StoredOrderedParagraph {
            hash: hash.clone(),
            paragraph_id: pid,
        });
    }
    out
}

/// Project the paragraph arena into [`StoredParagraph`] records. Each
/// paragraph's `sentences` map is flattened into `ordered_sentences`
/// (the in-memory map is derivable from the ordered list at read
/// time).
fn project_paragraphs(article: &Article) -> Vec<StoredParagraph> {
    article
        .paragraphs
        .iter()
        .map(|p| {
            let mut seen_count: HashMap<&str, usize> = HashMap::new();
            let mut ordered_sentences = Vec::with_capacity(p.ordered_sentences.len());
            for hash in &p.ordered_sentences {
                let n = seen_count.entry(hash.as_str()).or_insert(0);
                let sid = p
                    .sentences
                    .get(hash)
                    .and_then(|v| v.get(*n).copied())
                    .unwrap_or_default();
                *n += 1;
                ordered_sentences.push(StoredOrderedSentence {
                    hash: hash.clone(),
                    sentence_id: sid,
                });
            }
            StoredParagraph {
                hash_value: p.hash_value.clone(),
                value: p.value.clone(),
                ordered_sentences,
            }
        })
        .collect()
}

/// Project the sentence arena into [`StoredSentence`] records.
fn project_sentences(article: &Article) -> Vec<StoredSentence> {
    article
        .sentences
        .iter()
        .map(|s| StoredSentence {
            hash_value: s.hash_value.clone(),
            value: s.value.clone(),
            words: s.words.clone(),
        })
        .collect()
}

/// Pull the cross-revision hash tables out of the article.
///
/// Each bucket carries the **full list of arena ids** that share that
/// hash — the algorithm's resume-from-disk path looks up paragraphs /
/// sentences by hash via these tables and the matching
/// [`StoredParagraph`] / [`StoredSentence`] records. Entries are sorted
/// by hash for deterministic on-disk output.
fn project_hashtables(article: &Article) -> HashTables {
    let mut paragraph_buckets: Vec<HashBucket> = article
        .paragraphs_ht
        .iter()
        .map(|(h, ids)| HashBucket {
            hash: h.clone(),
            arena_ids: ids.clone(),
        })
        .collect();
    paragraph_buckets.sort_unstable_by(|a, b| a.hash.cmp(&b.hash));

    let mut sentence_buckets: Vec<HashBucket> = article
        .sentences_ht
        .iter()
        .map(|(h, ids)| HashBucket {
            hash: h.clone(),
            arena_ids: ids.clone(),
        })
        .collect();
    sentence_buckets.sort_unstable_by(|a, b| a.hash.cmp(&b.hash));

    HashTables {
        paragraph_buckets,
        sentence_buckets,
    }
}

fn project_meta(article: &Article, language: &str) -> Meta {
    let last_revid = article.last_good_rev_id;
    let last_timestamp = article
        .revisions
        .get(&last_revid)
        .map(|r| r.timestamp.clone())
        .unwrap_or_default();
    let mut spam_hashes: Vec<String> = article.spam_hashes.iter().cloned().collect();
    spam_hashes.sort_unstable();
    Meta {
        schema_version: crate::SCHEMA_VERSION,
        page_id: article.page_id.unwrap_or(0),
        language: language.to_string(),
        title: article.title.clone(),
        last_processed_revid: last_revid,
        last_processed_timestamp: last_timestamp,
        rvcontinue: String::new(),
        n_revisions: article.ordered_revisions.len() as u64,
        n_lifetime_tokens: article.tokens.len() as u64,
        n_spam_revisions: article.spam_ids.len() as u64,
        next_token_id: article.next_token_id,
        spam_revisions: article.spam_ids.clone(),
        spam_hashes,
    }
}

fn write_strings_file(dir: &Path, strings: &[&str]) -> Result<()> {
    let path = dir.join(STRINGS_FILE);
    let file = File::create(&path)?;
    let mut enc = zstd::stream::write::Encoder::new(file, ZSTD_LEVEL)?;
    write_strings(&mut enc, strings)?;
    enc.finish()?.sync_all()?;
    Ok(())
}

fn write_tokens_file(dir: &Path, tokens: &[StoredToken]) -> Result<()> {
    let path = dir.join(TOKENS_FILE);
    let file = File::create(&path)?;
    let mut enc = zstd::stream::write::Encoder::new(file, ZSTD_LEVEL)?;
    write_tokens(&mut enc, tokens)?;
    enc.finish()?.sync_all()?;
    Ok(())
}

fn write_revisions_file(dir: &Path, revisions: &[StoredRevision]) -> Result<()> {
    let path = dir.join(REVISIONS_FILE);
    let file = File::create(&path)?;
    let mut enc = zstd::stream::write::Encoder::new(file, ZSTD_LEVEL)?;
    write_revisions(&mut enc, revisions)?;
    enc.finish()?.sync_all()?;
    Ok(())
}

fn write_paragraphs_file(dir: &Path, paragraphs: &[StoredParagraph]) -> Result<()> {
    let path = dir.join(PARAGRAPHS_FILE);
    let file = File::create(&path)?;
    let mut enc = zstd::stream::write::Encoder::new(file, ZSTD_LEVEL)?;
    write_paragraphs(&mut enc, paragraphs)?;
    enc.finish()?.sync_all()?;
    Ok(())
}

fn write_sentences_file(dir: &Path, sentences: &[StoredSentence]) -> Result<()> {
    let path = dir.join(SENTENCES_FILE);
    let file = File::create(&path)?;
    let mut enc = zstd::stream::write::Encoder::new(file, ZSTD_LEVEL)?;
    write_sentences(&mut enc, sentences)?;
    enc.finish()?.sync_all()?;
    Ok(())
}

fn write_hashtables_file(dir: &Path, tables: &HashTables) -> Result<()> {
    let path = dir.join(HASHTABLES_FILE);
    let file = File::create(&path)?;
    let mut enc = zstd::stream::write::Encoder::new(file, ZSTD_LEVEL)?;
    crate::hashtables::write_hashtables(&mut enc, tables)?;
    enc.finish()?.sync_all()?;
    Ok(())
}

fn write_meta_file(dir: &Path, meta: &Meta) -> Result<()> {
    let path = dir.join(META_FILE);
    let json = meta.to_pretty_json()?;
    fs::write(&path, json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wikiwho_attribute::pipeline::{RevisionInput, RevisionOutcome};

    fn fixture_article() -> Article {
        let mut article = Article::new("Demo");
        article.page_id = Some(7);
        let mut feed = |rev_id: u64, user_id: u64, ts: &str, text: &str| {
            article.analyse_revision(RevisionInput {
                rev_id,
                timestamp: ts.into(),
                user_id: Some(user_id),
                user_name: Some(format!("u{user_id}")),
                comment: None,
                minor: false,
                sha1: None,
                text: text.into(),
            })
        };
        assert_eq!(
            feed(101, 11, "2024-01-01T00:00:00Z", "Hello there friend."),
            RevisionOutcome::Stored
        );
        assert_eq!(
            feed(102, 22, "2024-01-02T00:00:00Z", "Hello dear friend."),
            RevisionOutcome::Stored
        );
        article
    }

    #[test]
    fn projection_dedupes_strings() {
        // Two tokens with the same string ("hello") should map to the
        // same string_id.
        let article = fixture_article();
        let (strings, tokens) = project_tokens(&article);
        // Spot-check: the string list is deduplicated.
        let unique: std::collections::HashSet<&&str> = strings.iter().collect();
        assert_eq!(unique.len(), strings.len(), "dedup failed");
        // Every token references a valid string id.
        for t in &tokens {
            assert!((t.string_id as usize) < strings.len());
        }
    }

    #[test]
    fn projection_preserves_token_id_order() {
        let article = fixture_article();
        let (_strings, tokens) = project_tokens(&article);
        assert_eq!(tokens.len(), article.tokens.len());
        // Spot-check: origin_rev_id matches Word.origin_rev_id by index.
        for (i, w) in article.tokens.iter().enumerate() {
            assert_eq!(tokens[i].origin_rev_id, w.origin_rev_id);
            assert_eq!(tokens[i].last_rev_id, w.last_rev_id);
            assert_eq!(tokens[i].inbound, w.inbound);
            assert_eq!(tokens[i].outbound, w.outbound);
        }
    }

    #[test]
    fn projection_walks_revisions_in_processing_order() {
        let article = fixture_article();
        let revs = project_revisions(&article);
        assert_eq!(revs.len(), 2);
        assert_eq!(revs[0].rev_id, 101);
        assert_eq!(revs[1].rev_id, 102);
        assert!(!revs[0].token_sequence.is_empty());
        assert!(!revs[1].token_sequence.is_empty());
        assert_eq!(revs[0].editor, "11");
        assert_eq!(revs[1].editor, "22");
    }

    #[test]
    fn write_article_produces_all_files() {
        let article = fixture_article();
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_article(&article, tmp.path(), "en").unwrap();
        for f in [
            STRINGS_FILE,
            TOKENS_FILE,
            REVISIONS_FILE,
            HASHTABLES_FILE,
            META_FILE,
        ] {
            assert!(dir.join(f).exists(), "{f} missing");
        }
        // Verify sharding: en/0/0/7
        assert!(dir.ends_with("en/0/0/7"), "actual: {dir:?}");
    }

    #[test]
    fn write_article_updates_rev_id_index() {
        let article = fixture_article();
        let tmp = tempfile::tempdir().unwrap();
        write_article(&article, tmp.path(), "en").unwrap();

        let index = RevIdIndex::load(tmp.path(), "en").unwrap();
        // Fixture has two revisions: 101 and 102, both for page_id 7.
        assert_eq!(index.len(), 2);
        assert_eq!(index.lookup(101), Some(7));
        assert_eq!(index.lookup(102), Some(7));
        assert_eq!(index.lookup(999), None);
    }

    #[test]
    fn write_article_for_two_articles_merges_into_one_index() {
        let tmp = tempfile::tempdir().unwrap();

        // First article: page_id 7, revs 101 + 102.
        let a1 = fixture_article();
        write_article(&a1, tmp.path(), "en").unwrap();

        // Second article: different page_id, different rev_ids.
        let mut a2 = Article::new("Other");
        a2.page_id = Some(42);
        a2.analyse_revision(RevisionInput {
            rev_id: 9001,
            timestamp: "2024-02-01T00:00:00Z".into(),
            user_id: Some(33),
            user_name: Some("u33".into()),
            comment: None,
            minor: false,
            sha1: None,
            text: "Independent article content.".into(),
        });
        write_article(&a2, tmp.path(), "en").unwrap();

        let index = RevIdIndex::load(tmp.path(), "en").unwrap();
        assert_eq!(index.len(), 3);
        assert_eq!(index.lookup(101), Some(7));
        assert_eq!(index.lookup(102), Some(7));
        assert_eq!(index.lookup(9001), Some(42));
    }

    #[test]
    fn rewriting_article_replaces_its_rev_ids_in_index() {
        let tmp = tempfile::tempdir().unwrap();

        // Initial write: two revs.
        let a1 = fixture_article();
        write_article(&a1, tmp.path(), "en").unwrap();
        assert_eq!(RevIdIndex::load(tmp.path(), "en").unwrap().len(), 2);

        // Pretend the article picked up a third revision: rewrite from
        // scratch with one extra rev. The old rev_ids should be replaced,
        // not duplicated.
        let mut a2 = fixture_article();
        a2.analyse_revision(RevisionInput {
            rev_id: 103,
            timestamp: "2024-01-03T00:00:00Z".into(),
            user_id: Some(33),
            user_name: Some("u33".into()),
            comment: None,
            minor: false,
            sha1: None,
            text: "Hello there, dear friend.".into(),
        });
        write_article(&a2, tmp.path(), "en").unwrap();

        let index = RevIdIndex::load(tmp.path(), "en").unwrap();
        assert_eq!(index.len(), 3, "no duplicates expected, just the 3 revs");
        assert_eq!(index.lookup(101), Some(7));
        assert_eq!(index.lookup(102), Some(7));
        assert_eq!(index.lookup(103), Some(7));
    }

    #[test]
    fn write_article_fails_without_page_id() {
        let mut article = Article::new("Demo");
        article.analyse_revision(RevisionInput {
            rev_id: 1,
            timestamp: "2024-01-01T00:00:00Z".into(),
            user_id: Some(11),
            user_name: Some("u11".into()),
            comment: None,
            minor: false,
            sha1: None,
            text: "hi".into(),
        });
        let tmp = tempfile::tempdir().unwrap();
        let err = write_article(&article, tmp.path(), "en").unwrap_err();
        assert!(matches!(err, StorageError::Malformed { .. }));
    }
}
