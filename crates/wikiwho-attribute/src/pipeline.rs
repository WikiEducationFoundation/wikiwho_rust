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

use crate::cascade::{determine_authorship, record_inbound_outbound};
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

        // 5. Post-cascade inbound / outbound recording
        // (`wikiwho.py:257-305`). Must run BEFORE the hash-table
        // updates because it touches word state on the same arena
        // entries.
        record_inbound_outbound(self, &cascade_out, revision_prev.id, rev_id);

        // 6. Post-cascade hash-table updates
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

        // 7. Commit.
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

    /// Find the first token in the arena whose value equals `value`.
    /// Helper for tests that want to inspect inbound/outbound history
    /// on a specific token string.
    fn find_token(article: &Article, value: &str) -> Option<usize> {
        article.tokens.iter().position(|w| w.value == value)
    }

    #[test]
    fn analyse_revision_mid_sentence_edit_runs_myers_diff() {
        // Mid-sentence word swap is the canonical Myers path: paragraph
        // and sentence hashes both miss, so the token cascade runs the
        // diff. Verify the kept tokens reuse their ids, the new token
        // is allocated fresh, and the deleted token records outbound.
        let mut article = Article::new("Test");
        article.analyse_revision(make_input(100, "the quick brown fox jumps"));
        let tokens_after_rev1: Vec<String> =
            article.tokens.iter().map(|w| w.value.clone()).collect();
        assert_eq!(tokens_after_rev1, vec!["the", "quick", "brown", "fox", "jumps"]);
        let quick_id = find_token(&article, "quick").unwrap();

        article.analyse_revision(make_input(101, "the slow brown fox jumps"));

        // "the", "brown", "fox", "jumps" reused; "slow" allocated fresh.
        let slow_id = find_token(&article, "slow").unwrap();
        assert_eq!(article.tokens.len(), 6);
        assert_eq!(article.tokens[slow_id].origin_rev_id, 101);
        assert_eq!(article.tokens[slow_id].last_rev_id, 101);

        // "quick" was deleted — outbound has rev 101.
        assert_eq!(article.tokens[quick_id].outbound, vec![101]);
        // "quick" was NOT matched in rev 101, so last_rev_id stays at 100.
        assert_eq!(article.tokens[quick_id].last_rev_id, 100);

        // Kept tokens get last_rev_id bumped to 101 but no inbound
        // (they existed in revision_prev = 100).
        for value in ["the", "brown", "fox", "jumps"] {
            let tid = find_token(&article, value).unwrap();
            assert_eq!(article.tokens[tid].last_rev_id, 101, "{value}");
            assert!(article.tokens[tid].inbound.is_empty(), "{value}");
        }

        // The current revision's sentence wires up all 5 of its words.
        let curr = article.revisions.get(&101).unwrap();
        let curr_para = curr.paragraphs.values().next().unwrap()[0];
        let curr_sent = article.paragraph(curr_para).sentences.values().next().unwrap()[0];
        let curr_words: Vec<&str> = article
            .sentence(curr_sent)
            .words
            .iter()
            .map(|wid| article.tokens[*wid as usize].value.as_str())
            .collect();
        assert_eq!(curr_words, vec!["the", "slow", "brown", "fox", "jumps"]);
    }

    #[test]
    fn analyse_revision_deleted_token_records_outbound() {
        // Pure token deletion: shorten "alpha beta gamma" to "alpha
        // gamma". `beta` should land in outbound for rev 101.
        let mut article = Article::new("Test");
        article.analyse_revision(make_input(100, "alpha beta gamma delta"));
        let beta_id = find_token(&article, "beta").unwrap();

        article.analyse_revision(make_input(101, "alpha gamma delta"));

        assert_eq!(article.tokens[beta_id].outbound, vec![101]);
        // No new token allocated (delete-only).
        assert_eq!(article.tokens.len(), 4);
        // Other tokens have no outbound and bumped last_rev_id.
        for value in ["alpha", "gamma", "delta"] {
            let tid = find_token(&article, value).unwrap();
            assert!(article.tokens[tid].outbound.is_empty(), "{value}");
            assert_eq!(article.tokens[tid].last_rev_id, 101, "{value}");
        }
    }

    #[test]
    fn analyse_revision_inserted_tokens_within_existing_sentence() {
        // Pure insertion within a sentence: add "very fast" inside
        // "the fox" to produce "the very fast fox". Reuses "the" and
        // "fox", allocates "very" and "fast".
        let mut article = Article::new("Test");
        article.analyse_revision(make_input(100, "the fox runs daily"));
        let the_id = find_token(&article, "the").unwrap();
        let fox_id = find_token(&article, "fox").unwrap();

        article.analyse_revision(make_input(101, "the very fast fox runs daily"));

        // 4 + 2 new tokens.
        assert_eq!(article.tokens.len(), 6);
        let very_id = find_token(&article, "very").unwrap();
        let fast_id = find_token(&article, "fast").unwrap();
        assert_eq!(article.tokens[very_id].origin_rev_id, 101);
        assert_eq!(article.tokens[fast_id].origin_rev_id, 101);

        // Reused tokens: last_rev_id bumped, inbound still empty.
        for id in [the_id, fox_id] {
            assert_eq!(article.tokens[id].last_rev_id, 101);
            assert!(article.tokens[id].inbound.is_empty());
            assert!(article.tokens[id].outbound.is_empty());
        }
    }

    #[test]
    fn analyse_revision_reintroduced_sentence_records_inbound() {
        // Rev 100: two paragraphs. Rev 101: drop the second paragraph
        // (its words go outbound). Rev 102: re-add the second
        // paragraph (sentences_ht catches it, words get inbound).
        let mut article = Article::new("Test");
        let p1 = "first paragraph stays put.";
        let p2 = "second paragraph comes and goes.";
        article.analyse_revision(make_input(100, &format!("{p1}\n\n{p2}")));
        let comes_id = find_token(&article, "comes").unwrap();
        assert_eq!(article.tokens[comes_id].origin_rev_id, 100);
        assert_eq!(article.tokens[comes_id].last_rev_id, 100);

        // Rev 101: just p1. p2's sentence becomes unmatched_prev. Its
        // words should get outbound[-1] = 101.
        // Use a non-empty comment to bypass length-shrink.
        let mut input = make_input(101, p1);
        input.comment = Some("trimming".into());
        article.analyse_revision(input);
        assert_eq!(article.tokens[comes_id].outbound, vec![101]);
        // last_rev_id stays at 100 (the word wasn't matched in rev 101).
        assert_eq!(article.tokens[comes_id].last_rev_id, 100);

        // Rev 102: bring p2 back. sentences_ht / paragraphs_ht should
        // catch the reuse — same token id, inbound bumped.
        article.analyse_revision(make_input(102, &format!("{p1}\n\n{p2}")));

        // Confirm we did NOT allocate new tokens for p2's words.
        // The expected arena size: 4 (p1) + 5 (p2: second, paragraph,
        // comes, and, goes) + 1 (.) per paragraph + 1 final "." = let
        // me just count vs the rev-100 size.
        // Easier: token "comes" id should be unchanged, and inbound
        // should now include 102.
        let comes_id_after = find_token(&article, "comes").unwrap();
        assert_eq!(comes_id_after, comes_id, "same token reused");
        assert_eq!(article.tokens[comes_id].inbound, vec![102]);
        assert_eq!(article.tokens[comes_id].outbound, vec![101]);
        assert_eq!(article.tokens[comes_id].last_rev_id, 102);
    }

    #[test]
    fn analyse_revision_reintroduced_token_via_diff_records_inbound() {
        // The trickier path: a TOKEN that disappears and comes back,
        // not via sentence-level reuse but via the Myers diff. This
        // exercises the "Keep-via-Myers and inbound bump because
        // last_rev_id != revision_prev.id" path.
        let mut article = Article::new("Test");
        // Rev 100: contains "wolf".
        article.analyse_revision(make_input(100, "the wolf howls."));
        let wolf_id = find_token(&article, "wolf").unwrap();

        // Rev 101: drop "wolf", different sentence. Outbound bumps wolf.
        // Use a non-trivial comment so length-shrink doesn't trip.
        let mut input = make_input(101, "the cat sleeps.");
        input.comment = Some("rewrite".into());
        article.analyse_revision(input);
        assert_eq!(article.tokens[wolf_id].outbound, vec![101]);
        assert_eq!(article.tokens[wolf_id].last_rev_id, 100);

        // Rev 102: re-add "wolf" inside a new sentence. The paragraph
        // hash doesn't match anything (different surrounding text);
        // sentences_ht doesn't have this exact sentence either; token
        // cascade runs Myers, which sees "wolf" available in text_prev
        // (from the rev-101 sentence... wait, no — rev 101's
        // text_prev_for_us would be revs ≤101's contents). Let me
        // think again.
        //
        // Actually for rev 102, revision_prev = rev 101 (last good).
        // text_prev for the cascade is built from rev 101's unmatched
        // sentences. "the cat sleeps." is in rev 101. If rev 102 is
        // "the wolf howls again.", then rev 101's sentence becomes
        // unmatched_sentences_prev. text_prev = ["the","cat","sleeps","."].
        // text_curr = ["the","wolf","howls","again","."].
        // Myers: keep "the", delete "cat" insert "wolf", delete
        // "sleeps" insert "howls", insert "again", keep ".".
        // The "wolf" in text_curr is a fresh insert per Myers.
        //
        // But the FALLBACK in the cascade allocates new when the curr
        // word isn't matched in the diff. So "wolf" gets a NEW token
        // id, NOT reusing the rev-100 "wolf". That's the right
        // algorithm behavior — Myers doesn't know about rev-100's
        // dropped word.
        //
        // So this test demonstrates that single-token reintroduction
        // through the diff path does NOT find the old token; the
        // hash-table reuse (sentences_ht / paragraphs_ht) is the only
        // mechanism that preserves long-distance identity. Without
        // sentence-level reuse, "wolf" gets a new id.
        article.analyse_revision(make_input(102, "the wolf howls again."));

        // Confirm a NEW wolf token was allocated (not reusing the old).
        let wolf_ids: Vec<usize> = article
            .tokens
            .iter()
            .enumerate()
            .filter(|(_, w)| w.value == "wolf")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(wolf_ids.len(), 2, "two distinct wolf tokens");
        // The new wolf is origin = 102.
        let new_wolf = wolf_ids.iter().find(|&&i| i != wolf_id).copied().unwrap();
        assert_eq!(article.tokens[new_wolf].origin_rev_id, 102);
        // The old wolf still has its outbound from rev 101 only.
        assert_eq!(article.tokens[wolf_id].outbound, vec![101]);
        assert!(article.tokens[wolf_id].inbound.is_empty());
    }

    #[test]
    fn analyse_revision_three_revisions_chain_inbound_outbound_consistently() {
        // Validate that consecutive revisions keep token bookkeeping
        // consistent. Pattern: word "shared" stays across all three
        // revisions and should never accumulate inbound/outbound.
        // Word "extra" appears in rev 1, not in rev 2, then again in
        // rev 3 via sentence-level reuse → outbound=[2], inbound=[3].
        let mut article = Article::new("Test");
        article.analyse_revision(make_input(100, "shared text remains.\n\nextra paragraph here."));
        let shared_id = find_token(&article, "shared").unwrap();
        let extra_id = find_token(&article, "extra").unwrap();

        let mut input = make_input(101, "shared text remains.");
        input.comment = Some("trimming".into());
        article.analyse_revision(input);

        article.analyse_revision(make_input(102, "shared text remains.\n\nextra paragraph here."));

        // shared never disappeared — clean history.
        assert!(article.tokens[shared_id].inbound.is_empty());
        assert!(article.tokens[shared_id].outbound.is_empty());
        assert_eq!(article.tokens[shared_id].last_rev_id, 102);

        // extra: out in 101, back in 102.
        assert_eq!(article.tokens[extra_id].outbound, vec![101]);
        assert_eq!(article.tokens[extra_id].inbound, vec![102]);
        assert_eq!(article.tokens[extra_id].last_rev_id, 102);
    }
}
