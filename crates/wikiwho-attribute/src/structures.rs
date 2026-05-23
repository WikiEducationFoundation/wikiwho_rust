//! Data structures the attribution algorithm carries across revisions.
//!
//! Port of `../wikiwho_api/lib/WikiWho/WikiWho/structures.py`. Two
//! intentional departures from the Python:
//!
//! - **No shared `matched` flag.** The Python implementation mutates a
//!   `matched: bool` field on Word / Sentence / Paragraph during the
//!   cascade, then resets every touched node at the end of
//!   `determine_authorship` (`wikiwho.py:273-305`). That reset is the
//!   most bug-prone part of the reference. Per `ALGORITHM.md §4` we
//!   use per-iteration `HashSet<TokenId>` / `HashSet<SentenceId>` /
//!   `HashSet<ParagraphId>` instead, scoped to one revision's
//!   processing. The structs here therefore have no `matched` field.
//!
//! - **Arena allocation by ID.** The Python passes references around
//!   freely. Rust's ownership model makes that painful via
//!   `Rc<RefCell<...>>`. Instead we keep all `Word`s, `Sentence`s, and
//!   `Paragraph`s in flat `Vec`s on the `Article` (the lifetime
//!   container) and use typed integer indices. The arena order is also
//!   the natural storage order for the persistence layer
//!   (`STORAGE.md §2.3 tokens.bin`), so this design double-duties.
//!
//! Hashes are stored as lowercase hex `String` for now to match the
//! reference exactly; if memory becomes a concern (millions of
//! cross-revision hash entries on Obama-class articles) we can switch
//! to `[u8; 16]` in a follow-up. The optimization is invisible to the
//! algorithm.

use std::collections::{HashMap, HashSet};

/// Sequential id of a `Word` in the article's lifetime token list.
/// Stable across revisions; matches `Word.token_id` in the Python
/// reference and the `token_id` field in the wire format
/// (`API.md §1`).
pub type TokenId = u32;

/// Arena index for a `Sentence`.
pub type SentenceId = u32;

/// Arena index for a `Paragraph`.
pub type ParagraphId = u32;

/// Wikipedia revision id. `u64` rather than `u32` to be safe — current
/// max enwiki rev_id is in the low 10⁹, but the format won't fit a
/// breaking change later (`STORAGE.md §2.4` notes the same).
pub type RevId = u64;

/// Lowercase hex MD5 digest. Produced by
/// `tokenize::hash_md5`. Matches the Python reference exactly.
pub type Hash = String;

/// A single token in the article's lifetime. Created once when first
/// introduced, never reassigned — `inbound` and `outbound` grow as
/// subsequent revisions delete and reintroduce the token.
#[derive(Debug, Clone)]
pub struct Word {
    pub token_id: TokenId,
    /// The token string. Already lowercased before storage
    /// (`wikiwho.py:123`).
    pub value: String,
    /// Revision id that first introduced this token.
    pub origin_rev_id: RevId,
    /// Last revision id this token appeared in. Used by the inbound
    /// recorder to detect "previously absent, now reintroduced"
    /// (`wikiwho.py:254-258`).
    pub last_rev_id: RevId,
    /// Revision ids where this token was re-added after a delete.
    pub inbound: Vec<RevId>,
    /// Revision ids where this token was deleted.
    pub outbound: Vec<RevId>,
}

impl Word {
    /// Construct a fresh token introduced by `rev_id`.
    pub fn new(token_id: TokenId, value: String, rev_id: RevId) -> Self {
        Self {
            token_id,
            value,
            origin_rev_id: rev_id,
            last_rev_id: rev_id,
            inbound: Vec::new(),
            outbound: Vec::new(),
        }
    }
}

/// A sentence within a paragraph. Sentences are content-addressed by
/// the hash of their normalized (space-joined-after-tokenize) value;
/// the same physical sentence can appear in multiple revisions because
/// of cross-revision hash matching.
#[derive(Debug, Default, Clone)]
pub struct Sentence {
    pub hash_value: Hash,
    /// Normalized sentence text (tokens joined by single space). The
    /// reference clears this to `""` after first insertion
    /// (`wikiwho.py:322`) once it's no longer needed; we mirror that
    /// for memory parity.
    pub value: String,
    /// Tokens in document order. Indices into `Article::tokens`.
    pub words: Vec<TokenId>,
}

/// A paragraph within a revision. Each paragraph holds its own
/// `sentences` map keyed by sentence hash — the per-paragraph map
/// is what `analyse_sentences_in_paragraphs` looks up at
/// `wikiwho.py:486` when matching sentences against the previous
/// revision's paragraphs.
#[derive(Debug, Default, Clone)]
pub struct Paragraph {
    pub hash_value: Hash,
    /// Raw paragraph text. The reference clears this to `""` after
    /// first insertion (`wikiwho.py:314`).
    pub value: String,
    /// Sentences in this paragraph keyed by sentence hash. Multiple
    /// sentences can share a hash within one paragraph (rare but
    /// possible), hence `Vec<SentenceId>` rather than a single id.
    pub sentences: HashMap<Hash, Vec<SentenceId>>,
    /// Sentence hashes in document order. Same length as
    /// `sentences.values().map(Vec::len).sum()` — the dict only
    /// distinguishes by hash; this preserves ordering and duplicates.
    pub ordered_sentences: Vec<Hash>,
}

/// A revision of the article: a snapshot of which paragraphs (and
/// transitively, sentences and tokens) it contained.
#[derive(Debug, Default, Clone)]
pub struct Revision {
    pub id: RevId,
    /// Editor identifier per `ALGORITHM.md §7`: `str(user_id)` for
    /// registered, `"0|<name>"` for anons, empty string when missing.
    pub editor: String,
    /// MW API timestamp in `YYYY-MM-DDTHH:MM:SSZ` form (`API.md §1`).
    pub timestamp: String,
    /// Paragraphs by hash, mirroring `Paragraph::sentences`.
    pub paragraphs: HashMap<Hash, Vec<ParagraphId>>,
    pub ordered_paragraphs: Vec<Hash>,
    /// Length of the raw wikitext in characters (not bytes) — matches
    /// `len(text)` in `wikiwho.py:84` / `:160`. Used by the
    /// length-shrink vandalism heuristic.
    pub length: usize,
    /// Tokens originally added in *this* revision. Set by the cascade
    /// (`wikiwho.py:627`, `:673`, `:688`).
    pub original_adds: u32,
}

/// Per-iteration scratch state. Replaces the Python's `matched: bool`
/// flag on each node (see module doc). One of these is constructed at
/// the start of `determine_authorship` and dropped at the end; no
/// reset bookkeeping required.
#[derive(Debug, Default)]
pub struct MatchedSets {
    pub paragraphs: HashSet<ParagraphId>,
    pub sentences: HashSet<SentenceId>,
    pub tokens: HashSet<TokenId>,
}

impl MatchedSets {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Article-level container — the Rust equivalent of the Python
/// `Wikiwho` object (`wikiwho.py:32-53`). Holds the lifetime arenas
/// for tokens / sentences / paragraphs, the cross-revision hash
/// tables, the revision dict, and the spam-tracking sets.
#[derive(Debug, Default)]
pub struct Article {
    pub title: String,
    pub page_id: Option<u64>,

    // ---- arenas ----
    pub tokens: Vec<Word>,
    pub sentences: Vec<Sentence>,
    pub paragraphs: Vec<Paragraph>,

    // ---- revisions ----
    pub revisions: HashMap<RevId, Revision>,
    pub ordered_revisions: Vec<RevId>,
    /// Id of the revision currently being analysed (or last processed).
    pub revision_curr: RevId,
    /// Id of the previous successfully-processed revision; `0` before
    /// the first revision lands. Note: spam-detected revisions do NOT
    /// advance this pointer (`wikiwho.py:96`, `:172`).
    pub revision_prev: RevId,

    // ---- cross-revision hash tables ----
    /// Every paragraph hash ever seen in the article. Lets a paragraph
    /// deleted N revisions ago and now reintroduced inherit its
    /// original token ids.
    pub paragraphs_ht: HashMap<Hash, Vec<ParagraphId>>,
    pub sentences_ht: HashMap<Hash, Vec<SentenceId>>,

    // ---- spam tracking ----
    /// Spam revision ids in order of detection. Useful for debugging
    /// and matches `Wikiwho.spam_ids` in the reference; not used
    /// hot-path.
    pub spam_ids: Vec<RevId>,
    /// Revision SHA-1 hashes flagged as spam. The first check in
    /// `analyse_article` (`wikiwho.py:80-82`, `:156-158`) is whether
    /// the new revision's hash matches one we've seen before;
    /// HashSet membership is the right shape.
    pub spam_hashes: HashSet<Hash>,

    /// Monotonic counter for the next `TokenId` to assign. Equivalent
    /// to `Wikiwho.token_id` (`wikiwho.py:46`).
    pub next_token_id: TokenId,
}

impl Article {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    /// Push a fresh `Word` into the arena, assigning its id from
    /// `next_token_id`. Returns the assigned id.
    pub fn alloc_word(&mut self, value: String, origin_rev: RevId) -> TokenId {
        let id = self.next_token_id;
        let w = Word::new(id, value, origin_rev);
        self.tokens.push(w);
        self.next_token_id += 1;
        id
    }

    /// Push a fresh `Sentence` into the arena and return its id.
    pub fn alloc_sentence(&mut self, hash: Hash, value: String) -> SentenceId {
        let id = self.sentences.len() as SentenceId;
        self.sentences.push(Sentence {
            hash_value: hash,
            value,
            words: Vec::new(),
        });
        id
    }

    /// Push a fresh `Paragraph` into the arena and return its id.
    pub fn alloc_paragraph(&mut self, hash: Hash, value: String) -> ParagraphId {
        let id = self.paragraphs.len() as ParagraphId;
        self.paragraphs.push(Paragraph {
            hash_value: hash,
            value,
            sentences: HashMap::new(),
            ordered_sentences: Vec::new(),
        });
        id
    }

    pub fn word(&self, id: TokenId) -> &Word {
        &self.tokens[id as usize]
    }

    pub fn word_mut(&mut self, id: TokenId) -> &mut Word {
        &mut self.tokens[id as usize]
    }

    pub fn sentence(&self, id: SentenceId) -> &Sentence {
        &self.sentences[id as usize]
    }

    pub fn sentence_mut(&mut self, id: SentenceId) -> &mut Sentence {
        &mut self.sentences[id as usize]
    }

    pub fn paragraph(&self, id: ParagraphId) -> &Paragraph {
        &self.paragraphs[id as usize]
    }

    pub fn paragraph_mut(&mut self, id: ParagraphId) -> &mut Paragraph {
        &mut self.paragraphs[id as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn article_alloc_word_assigns_sequential_ids() {
        let mut a = Article::new("Test");
        let id0 = a.alloc_word("foo".into(), 100);
        let id1 = a.alloc_word("bar".into(), 100);
        let id2 = a.alloc_word("baz".into(), 200);
        assert_eq!(id0, 0);
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(a.tokens.len(), 3);
        assert_eq!(a.next_token_id, 3);

        // Spot-check that the Word has its origin set.
        assert_eq!(a.word(id0).value, "foo");
        assert_eq!(a.word(id0).origin_rev_id, 100);
        assert_eq!(a.word(id0).last_rev_id, 100);
        assert!(a.word(id0).inbound.is_empty());
        assert!(a.word(id0).outbound.is_empty());
    }

    #[test]
    fn article_alloc_sentence_paragraph() {
        let mut a = Article::new("Test");
        let s0 = a.alloc_sentence("abc".into(), "value".into());
        let p0 = a.alloc_paragraph("def".into(), "para text".into());
        assert_eq!(s0, 0);
        assert_eq!(p0, 0);
        assert_eq!(a.sentence(s0).hash_value, "abc");
        assert_eq!(a.paragraph(p0).value, "para text");
    }

    #[test]
    fn matched_sets_default_empty() {
        let m = MatchedSets::new();
        assert!(m.paragraphs.is_empty());
        assert!(m.sentences.is_empty());
        assert!(m.tokens.is_empty());
    }

    #[test]
    fn word_new_initializes_lists_empty() {
        let w = Word::new(42, "value".into(), 1000);
        assert_eq!(w.token_id, 42);
        assert_eq!(w.origin_rev_id, 1000);
        assert_eq!(w.last_rev_id, 1000);
        assert!(w.inbound.is_empty());
        assert!(w.outbound.is_empty());
    }
}
