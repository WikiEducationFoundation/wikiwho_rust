//! Per-revision pipeline: spam checks → cascade → hash-table updates →
//! commit.
//!
//! Port of the per-revision body of `analyse_article`
//! (`wikiwho.py:144-205`). The cascade itself lives in
//! `crate::cascade`; this module is the thin layer that turns a
//! `RevisionInput` into either a committed revision or a vandalism
//! verdict.
//!
//! Not yet implemented:
//! - Inbound / outbound recording (`wikiwho.py:257-305`). Required
//!   before multi-revision parity numbers can move; queued for the
//!   session after Myers diff lands.

use crate::cascade::determine_authorship;
use crate::spam::{hash_matches_known_spam, length_shrink_is_vandalism};
use crate::structures::{Article, Hash, RevId, Revision};
use crate::tokenize::hash_md5;

/// One revision's worth of input, normalized across the JSON / XML
/// paths in `wikiwho.py`. Callers populate this from the MW Action API
/// (`analyse_article`, JSON path) or a dump iterator
/// (`analyse_article_from_xml_dump`) before invoking
/// `Article::analyse_revision`. The `texthidden` / `textmissing` /
/// `revision.deleted` skip cases (`ALGORITHM.md §3.1`) are the
/// caller's responsibility — don't construct a `RevisionInput` for
/// those.
#[derive(Debug, Clone)]
pub struct RevisionInput {
    pub rev_id: RevId,
    pub timestamp: String,
    pub text: String,
    /// MW-provided content hash. The reference uses whatever MW
    /// supplies (SHA-1 in practice) AND falls back to MD5
    /// (`utils.calculate_hash`) when absent — both go into the same
    /// `spam_hashes` set. We mirror that quirk; see `wikiwho.py:79`,
    /// `:155`.
    pub sha1: Option<Hash>,
    /// Edit summary. `None` and `Some("")` are equivalent for the
    /// good-faith-move escape hatch — only a non-empty comment
    /// counts.
    pub comment: Option<String>,
    pub minor: bool,
    /// `Some(0)` for anons (the MW API encodes anon edits as
    /// `userid: 0`), `Some(>0)` for registered users, `None` for
    /// revisions with no editor info. See `ALGORITHM.md §7`.
    pub user_id: Option<u64>,
    pub user_name: Option<String>,
}

/// Outcome of processing a single revision via
/// `Article::analyse_revision`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevisionOutcome {
    /// Cascade ran cleanly; tokens, paragraphs and sentences are in
    /// their arenas, hash tables updated, the revision is stored.
    Stored,
    /// Revision flagged as vandalism. Not stored. Scratch allocations
    /// from the partial cascade run remain in the arenas (see the
    /// note in `ALGORITHM.md §10` on acceptable divergences — orphan
    /// arena entries are inert and don't affect correctness).
    Vandalism(VandalismReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VandalismReason {
    /// `rev_hash` matched a hash already on the spam list
    /// (`wikiwho.py:80-82` / `:156-158`).
    HashMatch,
    /// Length-shrink heuristic — big prev rev became tiny curr rev
    /// (`wikiwho.py:85-92` / `:161-168`).
    LengthShrink,
    /// Token-density check inside the cascade
    /// (`wikiwho.py:608-611`).
    TokenDensity,
}

/// Compose the Python `editor` derivation (`wikiwho.py:184-188`).
fn derive_editor(user_id: Option<u64>, user_name: Option<&str>) -> String {
    match user_id {
        Some(0) => format!("0|{}", user_name.unwrap_or("")),
        Some(id) => id.to_string(),
        None => String::new(),
    }
}

impl Article {
    /// Process one revision. Mirrors the per-revision body of
    /// `analyse_article` (`wikiwho.py:144-205`):
    ///
    /// 1. Hash-spam membership check.
    /// 2. Length-shrink heuristic.
    /// 3. Build the in-flight `Revision`.
    /// 4. Run the cascade on lowercased text.
    /// 5. If the cascade flagged vandalism: roll back.
    /// 6. Otherwise: update hash tables, advance `last_good_rev_id`,
    ///    store the revision.
    pub fn analyse_revision(&mut self, input: RevisionInput) -> RevisionOutcome {
        let rev_id = input.rev_id;
        let rev_hash = input
            .sha1
            .clone()
            .unwrap_or_else(|| hash_md5(&input.text));

        // 1. Hash-spam check.
        if hash_matches_known_spam(&rev_hash, &self.spam_hashes) {
            self.spam_ids.push(rev_id);
            return RevisionOutcome::Vandalism(VandalismReason::HashMatch);
        }

        // Resolve previous revision (clone so the cascade can hold an
        // immutable reference while mutating other parts of self). For
        // typical articles `revision_prev.paragraphs` is a few hundred
        // entries — small enough to clone per revision without
        // measurable overhead. Documented as a perf hot-path candidate
        // when Obama-class processing comes online.
        let revision_prev = self
            .revisions
            .get(&self.last_good_rev_id)
            .cloned()
            .unwrap_or_default();

        // Length in Unicode codepoints to match Python's `len(text)`
        // (`wikiwho.py:84`, `:160`).
        let text_len = input.text.chars().count();

        // 2. Length-shrink heuristic.
        let comment_present = input.comment.as_deref().is_some_and(|c| !c.is_empty());
        if length_shrink_is_vandalism(revision_prev.length, text_len, comment_present, input.minor)
        {
            self.spam_ids.push(rev_id);
            self.spam_hashes.insert(rev_hash);
            return RevisionOutcome::Vandalism(VandalismReason::LengthShrink);
        }

        // 3. Build the in-flight current revision.
        let mut revision_curr = Revision {
            id: rev_id,
            editor: derive_editor(input.user_id, input.user_name.as_deref()),
            timestamp: input.timestamp,
            length: text_len,
            ..Default::default()
        };

        // 4. Run the cascade on the lowercased text
        // (`wikiwho.py:123`, `:191`).
        let lowered = input.text.to_lowercase();
        let cascade_out = determine_authorship(self, &lowered, &revision_prev, &mut revision_curr);

        if cascade_out.vandalism {
            self.spam_ids.push(rev_id);
            self.spam_hashes.insert(rev_hash);
            return RevisionOutcome::Vandalism(VandalismReason::TokenDensity);
        }

        // 5. Post-cascade hash-table updates
        // (`wikiwho.py:308-323`). Newly-allocated paragraphs and
        // sentences go into the global hash tables; their `value`
        // fields are cleared (the hash is enough for future matching,
        // and the raw text isn't needed once the sentence is split
        // into words).
        for &pid in &cascade_out.unmatched_paragraphs_curr {
            let hash = self.paragraph(pid).hash_value.clone();
            self.paragraphs_ht.entry(hash).or_default().push(pid);
            self.paragraph_mut(pid).value.clear();
        }
        for &sid in &cascade_out.unmatched_sentences_curr {
            let hash = self.sentence(sid).hash_value.clone();
            self.sentences_ht.entry(hash).or_default().push(sid);
            self.sentence_mut(sid).value.clear();
        }

        // TODO(next session): inbound / outbound recording on words
        // (`wikiwho.py:257-305`). Single-revision parity is not
        // affected; multi-rev parity is, and the Myers diff session
        // will land this alongside the diff.

        // 6. Commit.
        self.last_good_rev_id = rev_id;
        self.ordered_revisions.push(rev_id);
        self.revisions.insert(rev_id, revision_curr);
        RevisionOutcome::Stored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokenize::tokenize_revision;

    fn make_input(rev_id: RevId, text: &str) -> RevisionInput {
        RevisionInput {
            rev_id,
            timestamp: "2024-01-01T00:00:00Z".into(),
            text: text.into(),
            sha1: None,
            comment: None,
            minor: false,
            user_id: Some(1),
            user_name: Some("alice".into()),
        }
    }

    #[test]
    fn derive_editor_cases() {
        assert_eq!(derive_editor(Some(0), Some("1.2.3.4")), "0|1.2.3.4");
        assert_eq!(derive_editor(Some(0), None), "0|");
        assert_eq!(derive_editor(Some(42), Some("ignored")), "42");
        assert_eq!(derive_editor(None, Some("alice")), "");
        assert_eq!(derive_editor(None, None), "");
    }

    #[test]
    fn analyse_revision_first_revision_commits() {
        let mut article = Article::new("Test");
        let text = "Hello world.\n\nSecond paragraph here.";
        let outcome = article.analyse_revision(make_input(100, text));
        assert_eq!(outcome, RevisionOutcome::Stored);
        assert_eq!(article.last_good_rev_id, 100);
        assert_eq!(article.ordered_revisions, vec![100]);
        assert!(article.revisions.contains_key(&100));

        // The cascade's token stream should equal the plain tokenizer's
        // output, lowercased and walked in document order.
        let direct = tokenize_revision(&text.to_lowercase());
        let arena: Vec<_> = article.tokens.iter().map(|w| w.value.clone()).collect();
        assert_eq!(arena, direct);
        // Every token was original to rev 100.
        for w in &article.tokens {
            assert_eq!(w.origin_rev_id, 100);
            assert_eq!(w.last_rev_id, 100);
            assert!(w.inbound.is_empty());
            assert!(w.outbound.is_empty());
        }
        // Hash tables populated.
        assert!(!article.paragraphs_ht.is_empty());
        assert!(!article.sentences_ht.is_empty());
        // Paragraph / sentence values are cleared post-insertion to
        // mirror the reference.
        for p in &article.paragraphs {
            assert!(p.value.is_empty());
        }
        for s in &article.sentences {
            assert!(s.value.is_empty());
        }
    }

    #[test]
    fn analyse_revision_identical_text_reuses_paragraphs() {
        let mut article = Article::new("Test");
        let text = "Hello world.\n\nSecond paragraph here.";
        article.analyse_revision(make_input(100, text));
        let tokens_after_first = article.tokens.len();
        let paragraphs_after_first = article.paragraphs.len();
        let sentences_after_first = article.sentences.len();

        let outcome = article.analyse_revision(make_input(101, text));
        assert_eq!(outcome, RevisionOutcome::Stored);
        // Identical text → no fresh allocations.
        assert_eq!(article.tokens.len(), tokens_after_first);
        assert_eq!(article.paragraphs.len(), paragraphs_after_first);
        assert_eq!(article.sentences.len(), sentences_after_first);
        // Revision pointer advanced.
        assert_eq!(article.last_good_rev_id, 101);
        assert_eq!(article.ordered_revisions, vec![100, 101]);
    }

    #[test]
    fn analyse_revision_added_paragraph_only_grows_arena_for_new_content() {
        let mut article = Article::new("Test");
        let r1 = "Original paragraph.";
        let r2 = "Original paragraph.\n\nA brand new paragraph.";
        article.analyse_revision(make_input(100, r1));
        let toks_before = article.tokens.len();
        let paras_before = article.paragraphs.len();

        article.analyse_revision(make_input(101, r2));
        // The original paragraph matched (paragraph-level reuse), so
        // only the new paragraph's tokens / sentence / paragraph were
        // allocated.
        let new_tokens: Vec<_> = article.tokens[toks_before..]
            .iter()
            .map(|w| w.value.clone())
            .collect();
        assert_eq!(
            new_tokens,
            crate::tokenize::tokenize_revision("a brand new paragraph.")
        );
        // One extra paragraph; sentences likely also +1.
        assert_eq!(article.paragraphs.len(), paras_before + 1);
        // All freshly-allocated words belong to rev 101.
        for w in &article.tokens[toks_before..] {
            assert_eq!(w.origin_rev_id, 101);
        }
    }

    #[test]
    fn analyse_revision_hash_match_marks_vandalism_and_skips_processing() {
        let mut article = Article::new("Test");
        // Pre-seed a spam hash.
        let spam_hash = hash_md5("doesn't matter");
        article.spam_hashes.insert(spam_hash.clone());

        let input = RevisionInput {
            rev_id: 999,
            timestamp: "2024-01-01T00:00:00Z".into(),
            text: "irrelevant content".into(),
            sha1: Some(spam_hash),
            comment: None,
            minor: false,
            user_id: Some(1),
            user_name: Some("bob".into()),
        };
        let outcome = article.analyse_revision(input);
        assert_eq!(outcome, RevisionOutcome::Vandalism(VandalismReason::HashMatch));
        assert!(article.revisions.is_empty());
        assert_eq!(article.spam_ids, vec![999]);
        // Hash-match path doesn't touch the cascade — arenas stay empty.
        assert!(article.tokens.is_empty());
        assert!(article.paragraphs.is_empty());
    }

    /// Build a long string of distinct tokens. The repetition is what
    /// makes `Lorem ipsum`-style fixtures dangerous as cascade inputs
    /// — the token-density gate fires on duplicate-heavy paragraphs,
    /// which is the right algorithm behavior but the wrong thing for
    /// length-shrink unit tests.
    fn varied_text(token_count: usize) -> String {
        (0..token_count)
            .map(|i| format!("uniqueword{i} "))
            .collect()
    }

    #[test]
    fn analyse_revision_length_shrink_flags_vandalism() {
        let mut article = Article::new("Test");
        // 200 unique tokens × ~13 chars/token ≈ 2600 chars. Density
        // stays at 1.0 (every token distinct) so the cascade commits.
        let big = varied_text(200);
        let r = article.analyse_revision(make_input(100, &big));
        assert_eq!(r, RevisionOutcome::Stored);
        assert_eq!(article.last_good_rev_id, 100);

        // 50 'x's tokenizes to a single token — length 50 (< 1000),
        // density 1.0 (won't trip the density gate). The
        // length-shrink heuristic should be the only thing tripping.
        let small = "x".repeat(50);
        let outcome = article.analyse_revision(make_input(101, &small));
        assert_eq!(
            outcome,
            RevisionOutcome::Vandalism(VandalismReason::LengthShrink)
        );
        assert!(article.spam_hashes.contains(&hash_md5(&small)));
        assert_eq!(article.last_good_rev_id, 100); // unchanged
        assert!(!article.revisions.contains_key(&101));
    }

    #[test]
    fn analyse_revision_good_faith_move_bypasses_length_shrink() {
        // Hand-craft a previous revision with length > 1000 but no
        // paragraph content. This isolates the comment+minor escape
        // hatch from the diff (multi-rev with real prev sentences
        // requires Myers, which lands next session — see ALGORITHM.md
        // §6). With no prev paragraphs / sentences, `text_prev` is
        // empty and the cascade hits the insertion-only path.
        let mut article = Article::new("Test");
        article.revisions.insert(
            100,
            Revision {
                id: 100,
                length: 1500,
                ..Default::default()
            },
        );
        article.last_good_rev_id = 100;

        let small = "x".repeat(50);
        let input = RevisionInput {
            rev_id: 101,
            timestamp: "2024-01-01T01:00:00Z".into(),
            text: small,
            sha1: None,
            // comment + minor signals a content move — length-shrink
            // heuristic should NOT fire.
            comment: Some("moved content to subarticle".into()),
            minor: true,
            user_id: Some(2),
            user_name: Some("carol".into()),
        };
        let outcome = article.analyse_revision(input);
        assert_eq!(outcome, RevisionOutcome::Stored);
        assert_eq!(article.last_good_rev_id, 101);
    }

    #[test]
    fn analyse_revision_token_density_vandalism_flags_repeated_paste() {
        // The token-density gate catches copy-paste vandalism: a
        // first revision where the same handful of tokens recur over
        // and over. "lorem ipsum dolor sit amet" repeated many times
        // is exactly the pattern the gate exists to catch (every
        // paragraph is unmatched → possible_vandalism, then density
        // > 20 confirms).
        let mut article = Article::new("Test");
        let spammy = "lorem ipsum dolor sit amet. ".repeat(60);
        let outcome = article.analyse_revision(make_input(100, &spammy));
        assert_eq!(
            outcome,
            RevisionOutcome::Vandalism(VandalismReason::TokenDensity)
        );
        assert_eq!(article.spam_ids, vec![100]);
        assert!(article.revisions.is_empty());
        assert_eq!(article.last_good_rev_id, 0);
    }
}
