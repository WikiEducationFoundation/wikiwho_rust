//! Matching cascade — paragraphs → sentences → tokens.
//!
//! Port of `../wikiwho_api/lib/WikiWho/WikiWho/wikiwho.py:207-691`. The
//! cascade is the heart of the attribution algorithm: it determines
//! which tokens in the current revision are reused from the past and
//! which are freshly added. Spec lives in `ALGORITHM.md §4`.
//!
//! State of the port:
//!
//! - `analyse_paragraphs_in_revision` — full port. Hashes each
//!   current-revision paragraph; tries to match against
//!   `revision_prev`, then the global `paragraphs_ht`.
//! - `analyse_sentences_in_paragraphs` — full port for the
//!   unmatched-paragraph subset.
//! - `analyse_words_in_sentences` — full port. Insertion-only path
//!   (`text_prev` empty), deletion-only short-circuit, the token-
//!   density vandalism gate, and the general case driven by a Rust
//!   port of Python's `difflib.Differ` (see `differ.rs`). The earlier
//!   Myers implementation in `diff.rs` is kept around for a future
//!   revisit — see `notes/diff-algorithm-revisit.md`.
//! - `determine_authorship` — orchestrator that composes the three
//!   levels.
//!
//! The reference uses a shared `matched: bool` flag on every Word /
//! Sentence / Paragraph and resets it at the end of every revision.
//! That reset is the most bug-prone part of the original (per
//! `ALGORITHM.md §4`). Our port uses a per-revision `MatchedSets`
//! scratchpad instead — when it falls out of scope the "reset" is
//! automatic.

use crate::differ;
use crate::spam::{TOKEN_DENSITY_LIMIT, UNMATCHED_PARAGRAPH};
use crate::structures::{Article, Hash, MatchedSets, ParagraphId, Revision, SentenceId, TokenId};
use crate::tokenize::{avg_word_freq, hash_md5, split_paragraphs, split_sentences, split_tokens};
use std::collections::HashMap;

/// Output of the paragraph level of the cascade. The three sets feed
/// the sentence level + the post-cascade inbound/outbound recorder.
#[derive(Debug, Default)]
pub struct ParagraphAnalysis {
    /// Paragraphs allocated this revision (no match anywhere).
    pub unmatched_paragraphs_curr: Vec<ParagraphId>,
    /// Paragraphs in `revision_prev` that didn't match in `revision_curr`.
    pub unmatched_paragraphs_prev: Vec<ParagraphId>,
    /// Paragraphs from `revision_prev` or `paragraphs_ht` that matched.
    /// Includes both full-matches and "all words already matched"
    /// consumptions — both flavours need to be reachable post-cascade
    /// for inbound/outbound recording.
    pub matched_paragraphs_prev: Vec<ParagraphId>,
}

/// Output of the sentence level. Same shape as `ParagraphAnalysis` plus
/// `total_sentences` (used by no one yet, but the reference returns it
/// so we keep parity).
#[derive(Debug, Default)]
pub struct SentenceAnalysis {
    pub unmatched_sentences_curr: Vec<SentenceId>,
    pub unmatched_sentences_prev: Vec<SentenceId>,
    pub matched_sentences_prev: Vec<SentenceId>,
    pub total_sentences: u32,
}

/// Aggregated cascade output. `determine_authorship` returns this so
/// callers can walk the matched/unmatched sets for the post-cascade
/// inbound/outbound recorder (`Article::record_inbound_outbound`).
#[derive(Debug, Default)]
pub struct CascadeOutput {
    pub matched_paragraphs_prev: Vec<ParagraphId>,
    pub unmatched_paragraphs_prev: Vec<ParagraphId>,
    pub matched_sentences_prev: Vec<SentenceId>,
    pub unmatched_sentences_prev: Vec<SentenceId>,
    pub matched_words_prev: Vec<TokenId>,
    pub unmatched_paragraphs_curr: Vec<ParagraphId>,
    pub unmatched_sentences_curr: Vec<SentenceId>,
    /// The set of token IDs that were matched somewhere in the cascade
    /// (paragraph, sentence, or word level). Equivalent to the Python's
    /// `word_prev.matched == True` predicate after the cascade
    /// completes; the recorder consults this when deciding whether a
    /// prev sentence's words went "outbound" or stayed in the matched
    /// tracker.
    pub matched_token_ids: std::collections::HashSet<TokenId>,
    /// True if the token-density vandalism gate fired
    /// (`wikiwho.py:608-611`). The caller rolls back when this is set.
    pub vandalism: bool,
}

/// Top-level cascade entry. Port of `wikiwho.py:207-256` (the
/// orchestrator body; the post-cascade inbound/outbound recording at
/// lines 257-323 lives in the caller and isn't ported yet).
pub fn determine_authorship(
    article: &mut Article,
    text_curr_lower: &str,
    revision_prev: &Revision,
    revision_curr: &mut Revision,
) -> CascadeOutput {
    let mut matched = MatchedSets::new();
    let mut out = CascadeOutput::default();

    let pa = analyse_paragraphs_in_revision(
        article,
        text_curr_lower,
        revision_prev,
        revision_curr,
        &mut matched,
    );
    out.unmatched_paragraphs_curr = pa.unmatched_paragraphs_curr;
    out.unmatched_paragraphs_prev = pa.unmatched_paragraphs_prev;
    out.matched_paragraphs_prev = pa.matched_paragraphs_prev;

    if out.unmatched_paragraphs_curr.is_empty() {
        // Every curr paragraph matched at the paragraph level — no
        // sentence or token cascade work to do, but we still need to
        // surface the matched token ids the recorder will use to bump
        // inbound/last_rev_id on paragraph-matched words.
        out.matched_token_ids = matched.tokens;
        return out;
    }

    let sa = analyse_sentences_in_paragraphs(
        article,
        &out.unmatched_paragraphs_curr,
        &out.unmatched_paragraphs_prev,
        &mut matched,
    );
    out.unmatched_sentences_curr = sa.unmatched_sentences_curr;
    out.unmatched_sentences_prev = sa.unmatched_sentences_prev;
    out.matched_sentences_prev = sa.matched_sentences_prev;

    // Copy-paste vandalism gate (`wikiwho.py:228-230`). If MORE than
    // UNMATCHED_PARAGRAPH (0.0) of current paragraphs failed to match,
    // possible_vandalism gets set and feeds into the token-density
    // check below. With the threshold pinned at 0.0, this fires as
    // soon as ANY paragraph is unmatched.
    let curr_para_count = revision_curr.ordered_paragraphs.len();
    let possible_vandalism = curr_para_count > 0
        && (out.unmatched_paragraphs_curr.len() as f64 / curr_para_count as f64)
            > UNMATCHED_PARAGRAPH;

    if !out.unmatched_sentences_curr.is_empty() {
        let (matched_words, vandalism) = analyse_words_in_sentences(
            article,
            &out.unmatched_sentences_curr,
            &out.unmatched_sentences_prev,
            possible_vandalism,
            revision_curr,
            &mut matched,
        );
        out.matched_words_prev = matched_words;
        out.vandalism = vandalism;
    }

    // Capture the cascade's matched-token set before it falls out of
    // scope. The recorder needs it to distinguish words that survived
    // the cascade (kept, get inbound/last_rev_id bumps) from words
    // that didn't (outbound).
    out.matched_token_ids = matched.tokens;
    out
}

/// Paragraph level. Port of `wikiwho.py:327-459`.
pub fn analyse_paragraphs_in_revision(
    article: &mut Article,
    text_curr: &str,
    revision_prev: &Revision,
    revision_curr: &mut Revision,
    matched: &mut MatchedSets,
) -> ParagraphAnalysis {
    let mut analysis = ParagraphAnalysis::default();

    for raw in split_paragraphs(text_curr) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            // wikiwho.py:340 — empty paragraphs are dropped, not hashed.
            continue;
        }
        let hash_curr = hash_md5(trimmed);
        let mut matched_curr = false;

        // 1) Try to match against revision_prev's paragraphs.
        //
        // The bucket entries are arena ParagraphIds, so cloning is just
        // copying a small Vec<u32>. Cloning detaches us from the borrow
        // of `revision_prev.paragraphs` so we can mutate
        // `revision_curr` and `article` inside the loop.
        if let Some(bucket) = revision_prev.paragraphs.get(&hash_curr).cloned() {
            for pid in bucket {
                if matched.paragraphs.contains(&pid) {
                    continue;
                }
                let (matched_one, matched_all) = check_paragraph_words_status(article, pid, matched);
                if !matched_one {
                    record_full_paragraph_match(article, pid, matched, &mut analysis);
                    add_paragraph_to_revision(revision_curr, &hash_curr, pid);
                    matched_curr = true;
                    break;
                } else if matched_all {
                    // All words already matched by another paragraph in
                    // this rev — mark this paragraph "consumed" so the
                    // tail loop doesn't pick it up, but DON'T add to
                    // revision_curr. Mirrors wikiwho.py:382-388.
                    matched.paragraphs.insert(pid);
                    analysis.matched_paragraphs_prev.push(pid);
                }
                // else: matched_one and not matched_all — paragraph is
                // partially consumed. Leave it for the next attempt in
                // this bucket (or the tail loop). wikiwho.py implicitly
                // does the same by not setting `matched`.
            }
        }

        // 2) Fall back to the global paragraphs_ht (wikiwho.py:392-431).
        // This is what lets a deleted-and-reintroduced paragraph inherit
        // its original token ids.
        if !matched_curr {
            if let Some(bucket) = article.paragraphs_ht.get(&hash_curr).cloned() {
                for pid in bucket {
                    if matched.paragraphs.contains(&pid) {
                        continue;
                    }
                    let (matched_one, matched_all) =
                        check_paragraph_words_status(article, pid, matched);
                    if !matched_one {
                        record_full_paragraph_match(article, pid, matched, &mut analysis);
                        add_paragraph_to_revision(revision_curr, &hash_curr, pid);
                        matched_curr = true;
                        break;
                    } else if matched_all {
                        matched.paragraphs.insert(pid);
                        analysis.matched_paragraphs_prev.push(pid);
                    }
                }
            }
        }

        // 3) Allocate a fresh paragraph (wikiwho.py:435-445).
        if !matched_curr {
            let pid = article.alloc_paragraph(hash_curr.clone(), trimmed.to_string());
            add_paragraph_to_revision(revision_curr, &hash_curr, pid);
            analysis.unmatched_paragraphs_curr.push(pid);
        }
    }

    // Tail loop (wikiwho.py:447-457): every paragraph in revision_prev
    // not yet marked is unmatched_prev. The reference uses a string-keyed
    // counter (`self.temp`) to disambiguate duplicate hashes within
    // `revision_prev.ordered_paragraphs`; we use a per-hash counter,
    // which `ALGORITHM.md §4` notes is cleaner with identical semantics.
    let mut hash_counts: HashMap<&Hash, usize> = HashMap::new();
    for hash in &revision_prev.ordered_paragraphs {
        let entry = hash_counts.entry(hash).or_insert(0);
        let idx = *entry;
        *entry += 1;
        let Some(bucket) = revision_prev.paragraphs.get(hash) else {
            continue;
        };
        let Some(&pid) = bucket.get(idx) else {
            continue;
        };
        if !matched.paragraphs.contains(&pid) {
            analysis.unmatched_paragraphs_prev.push(pid);
        }
    }

    analysis
}

/// Sentence level. Port of `wikiwho.py:461-582`.
///
/// For each unmatched current paragraph, split into sentences, normalize
/// (`split_into_tokens` then `' '.join(...)`), hash, and try to match
/// against the previous unmatched paragraphs' sentence buckets, then
/// `sentences_ht`. Anything that doesn't match is allocated fresh.
pub fn analyse_sentences_in_paragraphs(
    article: &mut Article,
    unmatched_paragraphs_curr: &[ParagraphId],
    unmatched_paragraphs_prev: &[ParagraphId],
    matched: &mut MatchedSets,
) -> SentenceAnalysis {
    let mut analysis = SentenceAnalysis::default();

    for &pid_curr in unmatched_paragraphs_curr {
        // Clone the paragraph value to release the borrow on `article`
        // for the inner cascade (which may allocate sentences, mutate
        // `revision_curr`, etc.).
        let value = article.paragraph(pid_curr).value.clone();

        for raw_sentence in split_sentences(&value) {
            let trimmed = raw_sentence.trim();
            if trimmed.is_empty() {
                // wikiwho.py:476.
                continue;
            }
            // wikiwho.py:479. Tokenize and rejoin — this is what
            // normalizes whitespace before hashing.
            let normalized = split_tokens(trimmed).join(" ");
            let hash_curr = hash_md5(&normalized);
            analysis.total_sentences += 1;

            let mut matched_via: Option<SentenceId> = None;

            // 1) Match against unmatched_paragraphs_prev.
            'outer: for &pid_prev in unmatched_paragraphs_prev {
                let Some(bucket) =
                    article.paragraph(pid_prev).sentences.get(&hash_curr).cloned()
                else {
                    continue;
                };
                for sid in bucket {
                    if matched.sentences.contains(&sid) {
                        continue;
                    }
                    let (matched_one, matched_all) =
                        check_sentence_words_status(article, sid, matched);
                    if !matched_one {
                        record_full_sentence_match(article, sid, matched, &mut analysis);
                        matched_via = Some(sid);
                        break 'outer;
                    } else if matched_all {
                        matched.sentences.insert(sid);
                        analysis.matched_sentences_prev.push(sid);
                    }
                }
            }

            // 2) Fall back to sentences_ht.
            if matched_via.is_none() {
                if let Some(bucket) = article.sentences_ht.get(&hash_curr).cloned() {
                    for sid in bucket {
                        if matched.sentences.contains(&sid) {
                            continue;
                        }
                        let (matched_one, matched_all) =
                            check_sentence_words_status(article, sid, matched);
                        if !matched_one {
                            record_full_sentence_match(article, sid, matched, &mut analysis);
                            matched_via = Some(sid);
                            break;
                        } else if matched_all {
                            matched.sentences.insert(sid);
                            analysis.matched_sentences_prev.push(sid);
                        }
                    }
                }
            }

            // 3) Allocate fresh if still unmatched.
            let sid = match matched_via {
                Some(s) => s,
                None => {
                    let s = article.alloc_sentence(hash_curr.clone(), normalized);
                    analysis.unmatched_sentences_curr.push(s);
                    s
                }
            };

            // Attach to the current paragraph (mirrors wikiwho.py:505-510,
            // :541-545, :559-563 — all three branches do the same
            // attach).
            let para = article.paragraph_mut(pid_curr);
            para.sentences.entry(hash_curr.clone()).or_default().push(sid);
            para.ordered_sentences.push(hash_curr);
        }
    }

    // Tail loop (wikiwho.py:567-580): unmatched sentences in unmatched
    // prev paragraphs. Each unmatched sentence is ALSO added to
    // matched_sentences_prev and inserted into the matched set —
    // wikiwho.py:578-580 explains the reason ("to reset matched words
    // in analyse_words_in_sentences").
    for &pid_prev in unmatched_paragraphs_prev {
        let paragraph_prev = article.paragraph(pid_prev);
        let mut hash_counts: HashMap<&Hash, usize> = HashMap::new();
        for hash in &paragraph_prev.ordered_sentences {
            let entry = hash_counts.entry(hash).or_insert(0);
            let idx = *entry;
            *entry += 1;
            let Some(bucket) = paragraph_prev.sentences.get(hash) else {
                continue;
            };
            let Some(&sid) = bucket.get(idx) else {
                continue;
            };
            if !matched.sentences.contains(&sid) {
                analysis.unmatched_sentences_prev.push(sid);
                matched.sentences.insert(sid);
                analysis.matched_sentences_prev.push(sid);
            }
        }
    }

    analysis
}

/// Token level. Full port of `wikiwho.py:584-691`.
///
/// Three paths:
/// 1. **Deletion-only** (`text_curr` empty): nothing to match, return
///    early. Outbound recording lives in the post-cascade recorder.
/// 2. **Insertion-only** (`text_prev` empty): every curr token is
///    fresh — allocate them all in document order.
/// 3. **General case**: run our Differ port (`differ::differ_compare`,
///    Python's Ratcliff/Obershelp matcher) over `text_prev` ×
///    `text_curr`, then walk the curr sentences consuming the
///    transcript per `wikiwho.py:631-691`. The DELETE-branch quirk
///    (where a curr token's value coincides with a deleted prev
///    token's value, so we consume the prev word AND keep scanning the
///    diff for the curr word) is ported verbatim — it's load-bearing
///    per `ALGORITHM.md §4.3`.
pub fn analyse_words_in_sentences(
    article: &mut Article,
    unmatched_sentences_curr: &[SentenceId],
    unmatched_sentences_prev: &[SentenceId],
    possible_vandalism: bool,
    revision_curr: &mut Revision,
    matched: &mut MatchedSets,
) -> (Vec<TokenId>, bool) {
    let mut matched_words_prev: Vec<TokenId> = Vec::new();

    // Parallel arrays for the unmatched prev words: `unmatched_words_prev`
    // holds the TokenId, `text_prev` holds the value (a cloned string,
    // since the `Word::value` is also stored in `article` which we'll
    // be mutating later). The two are indexed in lockstep and the
    // "matched" status is read from `matched.tokens` — the Python's
    // `word_prev.matched` flag.
    let mut unmatched_words_prev: Vec<TokenId> = Vec::new();
    let mut text_prev: Vec<String> = Vec::new();
    for &sid in unmatched_sentences_prev {
        let sentence = article.sentence(sid);
        for &wid in &sentence.words {
            if !matched.tokens.contains(&wid) {
                text_prev.push(article.word(wid).value.clone());
                unmatched_words_prev.push(wid);
            }
        }
    }

    // Build text_curr: tokens from current unmatched sentences. The
    // sentence value is already the space-joined token list
    // (wikiwho.py:479 / our analyse_sentences_in_paragraphs), so
    // split on ' ' reconstructs the tokens.
    let mut text_curr: Vec<String> = Vec::new();
    let mut sentence_words: Vec<(SentenceId, Vec<String>)> =
        Vec::with_capacity(unmatched_sentences_curr.len());
    for &sid in unmatched_sentences_curr {
        let sentence = article.sentence(sid);
        let words: Vec<String> = sentence.value.split(' ').map(String::from).collect();
        text_curr.extend(words.iter().cloned());
        sentence_words.push((sid, words));
    }

    // Deletion-only (wikiwho.py:604-605): every paragraph in curr
    // matched, and any leftover prev sentences are pure deletions.
    // Outbound recording happens in the caller.
    if text_curr.is_empty() {
        return (matched_words_prev, false);
    }

    // Token-density vandalism gate (wikiwho.py:608-613). Fires only
    // when the paragraph-level gate already raised possible_vandalism
    // (i.e., > 0% of curr paragraphs are unmatched).
    let mut possible_vandalism = possible_vandalism;
    if possible_vandalism {
        let density = avg_word_freq(&text_curr);
        if density > TOKEN_DENSITY_LIMIT {
            return (matched_words_prev, true);
        }
        possible_vandalism = false;
    }

    // Insertion-only path (wikiwho.py:616-629).
    if text_prev.is_empty() {
        for (sid, words) in sentence_words {
            for word in words {
                let wid = article.alloc_word(word, revision_curr.id);
                article.sentence_mut(sid).words.push(wid);
                revision_curr.original_adds += 1;
            }
        }
        return (matched_words_prev, possible_vandalism);
    }

    // General case: Differ (wikiwho.py:631-691). Port of Python's
    // `difflib.Differ().compare(text_prev, text_curr)` — see
    // `differ.rs`'s module doc for why we match Python's
    // Ratcliff/Obershelp matcher exactly rather than using a true-LCS
    // matcher like Myers (kept in `diff.rs` for a future revisit).
    let ops = differ::differ_compare(&text_prev, &text_curr);

    // The matching loop consults the transcript by looking up entries
    // by their token VALUE (matching the Python `if word == word_diff[2:]`).
    // We need a mutable working copy so we can mark entries consumed.
    let mut diff_entries: Vec<DiffEntry> = ops
        .into_iter()
        .map(|op| {
            let (kind, value) = match op {
                differ::DiffOp::Keep(v) => (DiffKind::Keep, v),
                differ::DiffOp::Delete(v) => (DiffKind::Delete, v),
                differ::DiffOp::Insert(v) => (DiffKind::Insert, v),
            };
            DiffEntry {
                kind,
                value,
                consumed: false,
            }
        })
        .collect();

    for (sid, words) in sentence_words {
        for word in words {
            let mut curr_matched = false;

            // Walk the diff looking for an entry whose value equals
            // `word` (and isn't already consumed). The first such entry
            // determines what we do — keep / delete / insert — exactly
            // mirroring the Python `while pos < diff_len` loop. The
            // delete branch keeps scanning (Python comment: "but don't
            // set curr_matched"); the keep/insert branches exit the
            // scan immediately.
            let mut pos = 0;
            while pos < diff_entries.len() {
                let entry = &diff_entries[pos];
                if !entry.consumed && entry.value == word {
                    match entry.kind {
                        DiffKind::Keep => {
                            if let Some(prev_idx) = find_unmatched_prev(
                                &unmatched_words_prev,
                                &text_prev,
                                &word,
                                matched,
                            ) {
                                let wid_prev = unmatched_words_prev[prev_idx];
                                matched.tokens.insert(wid_prev);
                                curr_matched = true;
                                article.sentence_mut(sid).words.push(wid_prev);
                                matched_words_prev.push(wid_prev);
                                diff_entries[pos].consumed = true;
                                break;
                            }
                        }
                        DiffKind::Delete => {
                            if let Some(prev_idx) = find_unmatched_prev(
                                &unmatched_words_prev,
                                &text_prev,
                                &word,
                                matched,
                            ) {
                                let wid_prev = unmatched_words_prev[prev_idx];
                                matched.tokens.insert(wid_prev);
                                article.word_mut(wid_prev).outbound.push(revision_curr.id);
                                matched_words_prev.push(wid_prev);
                                diff_entries[pos].consumed = true;
                                // Do NOT set curr_matched — the curr
                                // word still needs to be matched (or
                                // allocated fresh). The Python's `break`
                                // here only exits the inner `for
                                // word_prev` loop, not the diff scan.
                            }
                        }
                        DiffKind::Insert => {
                            curr_matched = true;
                            let wid = article.alloc_word(word.clone(), revision_curr.id);
                            article.sentence_mut(sid).words.push(wid);
                            revision_curr.original_adds += 1;
                            diff_entries[pos].consumed = true;
                            break;
                        }
                    }
                }
                pos += 1;
            }

            // Fallback (wikiwho.py:679-689): no diff entry matched →
            // allocate fresh. This catches duplicate curr tokens past
            // the first occurrence, which the diff only listed once.
            if !curr_matched {
                let wid = article.alloc_word(word, revision_curr.id);
                article.sentence_mut(sid).words.push(wid);
                revision_curr.original_adds += 1;
            }
        }
    }

    (matched_words_prev, possible_vandalism)
}

#[derive(Debug, Clone, Copy)]
enum DiffKind {
    Keep,
    Delete,
    Insert,
}

#[derive(Debug, Clone)]
struct DiffEntry {
    kind: DiffKind,
    value: String,
    consumed: bool,
}

/// Find the first index in `unmatched_words_prev` whose value equals
/// `word` and which is not yet in `matched.tokens`. Returns `None` if
/// no such word exists. Mirrors the Python inner loop
/// `for word_prev in unmatched_words_prev: if not word_prev.matched and
/// word_prev.value == word`.
fn find_unmatched_prev(
    unmatched_words_prev: &[TokenId],
    text_prev: &[String],
    word: &str,
    matched: &MatchedSets,
) -> Option<usize> {
    unmatched_words_prev
        .iter()
        .enumerate()
        .find(|(i, wid)| !matched.tokens.contains(wid) && text_prev[*i] == word)
        .map(|(i, _)| i)
}

// ---- helpers ----

/// Are any / all of this paragraph's words already in `matched.tokens`?
/// Port of the `matched_one` / `matched_all` block at
/// `wikiwho.py:352-361`.
fn check_paragraph_words_status(
    article: &Article,
    paragraph_pid: ParagraphId,
    matched: &MatchedSets,
) -> (bool, bool) {
    let mut matched_one = false;
    let mut matched_all = true;
    let paragraph = article.paragraph(paragraph_pid);
    for sentence_bucket in paragraph.sentences.values() {
        for &sid in sentence_bucket {
            let sentence = article.sentence(sid);
            for &wid in &sentence.words {
                if matched.tokens.contains(&wid) {
                    matched_one = true;
                } else {
                    matched_all = false;
                }
            }
        }
    }
    (matched_one, matched_all)
}

/// Same question, scoped to a single sentence
/// (`wikiwho.py:488-494`).
fn check_sentence_words_status(
    article: &Article,
    sid: SentenceId,
    matched: &MatchedSets,
) -> (bool, bool) {
    let mut matched_one = false;
    let mut matched_all = true;
    let sentence = article.sentence(sid);
    for &wid in &sentence.words {
        if matched.tokens.contains(&wid) {
            matched_one = true;
        } else {
            matched_all = false;
        }
    }
    (matched_one, matched_all)
}

/// Mark a previous paragraph and every sentence/word in it as matched.
/// Used when a current paragraph fully matches an unmatched previous
/// paragraph (the `not matched_one` branch at `wikiwho.py:362-373`).
///
/// `matched_sentences_prev` is NOT populated here: the post-cascade
/// recorder reaches those sentences through `matched_paragraphs_prev`
/// (wikiwho.py:273-286). Adding them here would double-count.
fn record_full_paragraph_match(
    article: &Article,
    pid: ParagraphId,
    matched: &mut MatchedSets,
    analysis: &mut ParagraphAnalysis,
) {
    matched.paragraphs.insert(pid);
    analysis.matched_paragraphs_prev.push(pid);
    let paragraph = article.paragraph(pid);
    for sentence_bucket in paragraph.sentences.values() {
        for &sid in sentence_bucket {
            matched.sentences.insert(sid);
            let sentence = article.sentence(sid);
            for &wid in &sentence.words {
                matched.tokens.insert(wid);
            }
        }
    }
}

/// Same idea at the sentence level (`wikiwho.py:496-511`).
fn record_full_sentence_match(
    article: &Article,
    sid: SentenceId,
    matched: &mut MatchedSets,
    analysis: &mut SentenceAnalysis,
) {
    matched.sentences.insert(sid);
    analysis.matched_sentences_prev.push(sid);
    let sentence = article.sentence(sid);
    for &wid in &sentence.words {
        matched.tokens.insert(wid);
    }
}

/// Push a paragraph reference into the current revision's index +
/// ordered list. Used for both matched-prev paragraphs (reuse) and
/// freshly-allocated ones (`wikiwho.py:376-380` / `:440-444`).
fn add_paragraph_to_revision(revision_curr: &mut Revision, hash: &Hash, pid: ParagraphId) {
    revision_curr
        .paragraphs
        .entry(hash.clone())
        .or_default()
        .push(pid);
    revision_curr.ordered_paragraphs.push(hash.clone());
}

/// Post-cascade recorder. Walks the matched/unmatched sets returned by
/// `determine_authorship` to bump `outbound` on deleted words and
/// `inbound` / `last_rev_id` on words that survived (full port of
/// `wikiwho.py:257-305`).
///
/// Two important quirks of the reference are preserved verbatim:
///
/// - The `if not unmatched_sentences_prev:` second outbound pass at
///   `wikiwho.py:263-270` only fires when sentence-cascade didn't
///   produce any unmatched-prev sentences (which is exactly the
///   "all curr paragraphs matched" case, since the tail loop in
///   `analyse_sentences_in_paragraphs` always populates the list when
///   sentence-cascade does run on unmatched_paragraphs_prev). It
///   catches the words inside paragraphs the sentence cascade never
///   visited.
/// - The Python `for matched_word in matched_words_prev` loop at
///   :298-305 references a stale `word_prev` variable from the prior
///   `matched_sentences_prev` loop — this is a latent Python bug.
///   For diff-matched Keep-branch words, the inbound/last_rev_id
///   update is already performed via `matched_sentences_prev`
///   (because the tail loop in `analyse_sentences_in_paragraphs`
///   places those sentences in both unmatched AND matched lists).
///   For Delete-branch words the outbound update happened in the
///   cascade itself. Our port skips the buggy loop entirely; the
///   functional behaviour matches.
pub fn record_inbound_outbound(
    article: &mut Article,
    out: &CascadeOutput,
    revision_prev_id: crate::structures::RevId,
    revision_curr_id: crate::structures::RevId,
) {
    if out.vandalism {
        return;
    }

    // --- Outbound recording (wikiwho.py:257-270) ---
    //
    // Every word in an unmatched-prev sentence that didn't survive
    // anywhere in the cascade is being deleted in this revision; mark
    // it.
    for &sid in &out.unmatched_sentences_prev {
        // Clone the words list to release the immutable borrow on
        // article before we start mutating words.
        let words: Vec<TokenId> = article.sentence(sid).words.clone();
        for wid in words {
            if !out.matched_token_ids.contains(&wid) {
                article.word_mut(wid).outbound.push(revision_curr_id);
            }
        }
    }

    // The "if no sentence-level pass ran" case: walk unmatched prev
    // paragraphs and capture outbound for any unmatched words.
    if out.unmatched_sentences_prev.is_empty() {
        for &pid in &out.unmatched_paragraphs_prev {
            let sentence_ids: Vec<SentenceId> = article
                .paragraph(pid)
                .sentences
                .values()
                .flat_map(|bucket| bucket.iter().copied())
                .collect();
            for sid in sentence_ids {
                let words: Vec<TokenId> = article.sentence(sid).words.clone();
                for wid in words {
                    if !out.matched_token_ids.contains(&wid) {
                        article.word_mut(wid).outbound.push(revision_curr_id);
                    }
                }
            }
        }
    }

    // --- Inbound / last_rev_id recording (wikiwho.py:272-297) ---
    //
    // For paragraph-level full-matches: every word inherits a touch.
    // For sentence-level matches (including the tail-loop additions
    // that overlap unmatched_sentences_prev), iterate words and
    // update only those that survived AND weren't deleted in this rev.
    //
    // A word can be reachable from BOTH `matched_paragraphs_prev`'s
    // sentence walk AND `matched_sentences_prev`'s direct word walk
    // — e.g., when a curr paragraph matches via `paragraphs_ht` (so
    // its words are added to `matched.tokens` AND its old sentence is
    // listed in matched_paragraphs_prev) while a different
    // unmatched-prev paragraph's sentence ALSO contains the same
    // words (via prior Myers-keep matching) and ends up in
    // matched_sentences_prev via the analyse_sentences_in_paragraphs
    // tail loop. Python avoids double-counting via the `matched`
    // flag reset (set to False after the first walk visits the word);
    // we use an explicit `processed` set.
    let mut processed: std::collections::HashSet<TokenId> = std::collections::HashSet::new();

    for &pid in &out.matched_paragraphs_prev {
        let sentence_ids: Vec<SentenceId> = article
            .paragraph(pid)
            .sentences
            .values()
            .flat_map(|bucket| bucket.iter().copied())
            .collect();
        for sid in sentence_ids {
            let words: Vec<TokenId> = article.sentence(sid).words.clone();
            for wid in words {
                update_inbound_and_last_rev_id(
                    article,
                    wid,
                    &out.matched_token_ids,
                    &mut processed,
                    revision_prev_id,
                    revision_curr_id,
                );
            }
        }
    }

    for &sid in &out.matched_sentences_prev {
        let words: Vec<TokenId> = article.sentence(sid).words.clone();
        for wid in words {
            update_inbound_and_last_rev_id(
                article,
                wid,
                &out.matched_token_ids,
                &mut processed,
                revision_prev_id,
                revision_curr_id,
            );
        }
    }
}

/// Apply the `wikiwho.py:280-284` update rule to a single word, and
/// add it to `processed` so subsequent walks skip it. This is the
/// equivalent of the Python `word_prev.matched = False` reset after
/// the update: a word can appear in multiple matched lists (via the
/// sentence-cascade tail loop double-listing or paragraph-vs-sentence
/// overlap), and the update should apply exactly once.
fn update_inbound_and_last_rev_id(
    article: &mut Article,
    wid: TokenId,
    matched_token_ids: &std::collections::HashSet<TokenId>,
    processed: &mut std::collections::HashSet<TokenId>,
    revision_prev_id: crate::structures::RevId,
    revision_curr_id: crate::structures::RevId,
) {
    if !matched_token_ids.contains(&wid) {
        return;
    }
    if !processed.insert(wid) {
        // Already touched this rev — Python's `if word_prev.matched`
        // would also short-circuit here.
        return;
    }
    let word = article.word_mut(wid);
    // Skip if outbound already includes revision_curr_id — that means
    // the diff loop already flagged this word as deleted in THIS rev
    // (the elif '-' branch). The Python check is identical:
    //   if not word_prev.outbound or word_prev.outbound[-1] != self.revision_curr.id
    if word.outbound.last() == Some(&revision_curr_id) {
        return;
    }
    if word.last_rev_id != revision_prev_id {
        word.inbound.push(revision_curr_id);
    }
    word.last_rev_id = revision_curr_id;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structures::Article;

    fn run_paragraph_cascade(
        article: &mut Article,
        text: &str,
        revision_prev: &Revision,
        revision_curr: &mut Revision,
    ) -> (ParagraphAnalysis, MatchedSets) {
        let mut matched = MatchedSets::new();
        let analysis = analyse_paragraphs_in_revision(
            article,
            text,
            revision_prev,
            revision_curr,
            &mut matched,
        );
        (analysis, matched)
    }

    #[test]
    fn paragraph_cascade_first_revision_allocates_everything() {
        // Two paragraphs separated by a blank line. With no
        // revision_prev, both should land in unmatched_paragraphs_curr
        // and the arena should contain two fresh paragraphs.
        let mut article = Article::new("Test");
        let revision_prev = Revision::default();
        let mut revision_curr = Revision {
            id: 100,
            ..Default::default()
        };
        let text = "first paragraph here\n\nsecond paragraph here";
        let (analysis, matched) =
            run_paragraph_cascade(&mut article, text, &revision_prev, &mut revision_curr);

        assert_eq!(analysis.unmatched_paragraphs_curr.len(), 2);
        assert_eq!(analysis.matched_paragraphs_prev.len(), 0);
        assert_eq!(analysis.unmatched_paragraphs_prev.len(), 0);
        assert_eq!(article.paragraphs.len(), 2);
        assert_eq!(revision_curr.ordered_paragraphs.len(), 2);
        // No matches recorded — sets stay empty.
        assert!(matched.paragraphs.is_empty());
        assert!(matched.sentences.is_empty());
        assert!(matched.tokens.is_empty());
    }

    #[test]
    fn paragraph_cascade_drops_empty_paragraphs() {
        // Double blank line between two paragraphs creates an empty
        // middle paragraph that should be skipped (wikiwho.py:340).
        let mut article = Article::new("Test");
        let revision_prev = Revision::default();
        let mut revision_curr = Revision {
            id: 100,
            ..Default::default()
        };
        let text = "first\n\n\n\nsecond";
        let (analysis, _) =
            run_paragraph_cascade(&mut article, text, &revision_prev, &mut revision_curr);

        assert_eq!(analysis.unmatched_paragraphs_curr.len(), 2);
        assert_eq!(revision_curr.ordered_paragraphs.len(), 2);
    }

    #[test]
    fn paragraph_cascade_identical_text_matches_prev_with_words() {
        // Set up a "previous revision" with one paragraph + sentence +
        // word allocated, then process the SAME text and expect a
        // full match.
        let mut article = Article::new("Test");
        let text = "para1 sentence";

        // Build prev by hand. Mirror what the cascade WILL do for the
        // first revision (without actually running the full cascade —
        // those are tested at the analyse_revision integration level).
        let hash = hash_md5(text);
        let pid = article.alloc_paragraph(hash.clone(), text.to_string());
        let sentence_norm = split_tokens(text).join(" ");
        let s_hash = hash_md5(&sentence_norm);
        let sid = article.alloc_sentence(s_hash.clone(), sentence_norm);
        let wid_a = article.alloc_word("para1".into(), 100);
        let wid_b = article.alloc_word("sentence".into(), 100);
        article.sentence_mut(sid).words = vec![wid_a, wid_b];
        article
            .paragraph_mut(pid)
            .sentences
            .insert(s_hash.clone(), vec![sid]);
        article.paragraph_mut(pid).ordered_sentences.push(s_hash);
        article.paragraphs_ht.insert(hash.clone(), vec![pid]);

        let mut revision_prev = Revision {
            id: 100,
            ..Default::default()
        };
        revision_prev.paragraphs.insert(hash.clone(), vec![pid]);
        revision_prev.ordered_paragraphs.push(hash.clone());

        // Process the same text again, now as revision 101.
        let mut revision_curr = Revision {
            id: 101,
            ..Default::default()
        };
        let (analysis, matched) =
            run_paragraph_cascade(&mut article, text, &revision_prev, &mut revision_curr);

        // The paragraph fully matched — its sentences and words are all
        // marked.
        assert_eq!(analysis.matched_paragraphs_prev, vec![pid]);
        assert!(analysis.unmatched_paragraphs_curr.is_empty());
        assert!(analysis.unmatched_paragraphs_prev.is_empty());
        assert!(matched.paragraphs.contains(&pid));
        assert!(matched.sentences.contains(&sid));
        assert!(matched.tokens.contains(&wid_a));
        assert!(matched.tokens.contains(&wid_b));

        // Current revision references the SAME paragraph id.
        assert_eq!(
            revision_curr.paragraphs.values().next().unwrap(),
            &vec![pid]
        );
        // Arena didn't grow.
        assert_eq!(article.paragraphs.len(), 1);
    }

    #[test]
    fn paragraph_cascade_added_paragraph_is_unmatched_curr() {
        // Prev has one paragraph; curr has the same paragraph plus a
        // new one. The shared one should match; the new one should be
        // allocated fresh.
        let mut article = Article::new("Test");
        let p1_text = "shared paragraph";
        let p2_text = "newly added paragraph";

        let p1_hash = hash_md5(p1_text);
        let pid1 = article.alloc_paragraph(p1_hash.clone(), p1_text.to_string());
        article.paragraphs_ht.insert(p1_hash.clone(), vec![pid1]);
        // Empty sentence list — that's fine for this test; we just need
        // matched_one to evaluate to False so it full-matches.
        let mut revision_prev = Revision {
            id: 100,
            ..Default::default()
        };
        revision_prev
            .paragraphs
            .insert(p1_hash.clone(), vec![pid1]);
        revision_prev.ordered_paragraphs.push(p1_hash.clone());

        let mut revision_curr = Revision {
            id: 101,
            ..Default::default()
        };
        let combined = format!("{p1_text}\n\n{p2_text}");
        let (analysis, _) =
            run_paragraph_cascade(&mut article, &combined, &revision_prev, &mut revision_curr);

        assert_eq!(analysis.matched_paragraphs_prev, vec![pid1]);
        assert_eq!(analysis.unmatched_paragraphs_curr.len(), 1);
        let pid2 = analysis.unmatched_paragraphs_curr[0];
        assert_ne!(pid2, pid1);
        assert_eq!(article.paragraph(pid2).value, p2_text);
        // curr has both — pid1 first, pid2 second.
        assert_eq!(
            revision_curr.ordered_paragraphs,
            vec![p1_hash.clone(), hash_md5(p2_text)]
        );
    }

    #[test]
    fn paragraph_cascade_removed_paragraph_is_unmatched_prev() {
        // Prev has two paragraphs; curr keeps only the second. The
        // first should land in unmatched_paragraphs_prev.
        let mut article = Article::new("Test");
        let p1_text = "removed paragraph";
        let p2_text = "kept paragraph";
        let p1_hash = hash_md5(p1_text);
        let p2_hash = hash_md5(p2_text);
        let pid1 = article.alloc_paragraph(p1_hash.clone(), p1_text.to_string());
        let pid2 = article.alloc_paragraph(p2_hash.clone(), p2_text.to_string());
        article.paragraphs_ht.insert(p1_hash.clone(), vec![pid1]);
        article.paragraphs_ht.insert(p2_hash.clone(), vec![pid2]);
        let mut revision_prev = Revision {
            id: 100,
            ..Default::default()
        };
        revision_prev
            .paragraphs
            .insert(p1_hash.clone(), vec![pid1]);
        revision_prev
            .paragraphs
            .insert(p2_hash.clone(), vec![pid2]);
        revision_prev.ordered_paragraphs.push(p1_hash);
        revision_prev.ordered_paragraphs.push(p2_hash);

        let mut revision_curr = Revision {
            id: 101,
            ..Default::default()
        };
        let (analysis, _) = run_paragraph_cascade(
            &mut article,
            p2_text,
            &revision_prev,
            &mut revision_curr,
        );

        assert_eq!(analysis.matched_paragraphs_prev, vec![pid2]);
        assert_eq!(analysis.unmatched_paragraphs_prev, vec![pid1]);
        assert!(analysis.unmatched_paragraphs_curr.is_empty());
    }

    #[test]
    fn paragraph_cascade_global_ht_fallback_for_reintroduction() {
        // Prev does not contain a paragraph hash, but the global
        // paragraphs_ht does (the paragraph was in some older
        // revision). The curr text reintroduces it.
        let mut article = Article::new("Test");
        let text = "old paragraph";
        let hash = hash_md5(text);
        let pid = article.alloc_paragraph(hash.clone(), text.to_string());
        article.paragraphs_ht.insert(hash.clone(), vec![pid]);

        let revision_prev = Revision {
            id: 100,
            ..Default::default()
        };
        let mut revision_curr = Revision {
            id: 200,
            ..Default::default()
        };
        let (analysis, matched) =
            run_paragraph_cascade(&mut article, text, &revision_prev, &mut revision_curr);

        assert_eq!(analysis.matched_paragraphs_prev, vec![pid]);
        assert!(analysis.unmatched_paragraphs_curr.is_empty());
        assert!(matched.paragraphs.contains(&pid));
    }

    #[test]
    fn determine_authorship_first_revision_allocates_tokens() {
        // End-to-end on a single-revision article: every paragraph,
        // sentence and token in the input text gets allocated fresh
        // and original_adds tracks the count.
        let mut article = Article::new("Test");
        let revision_prev = Revision::default();
        let mut revision_curr = Revision {
            id: 100,
            ..Default::default()
        };
        let text = "first paragraph.\n\nsecond paragraph here.";
        let out = determine_authorship(&mut article, text, &revision_prev, &mut revision_curr);

        assert!(!out.vandalism);
        assert_eq!(out.unmatched_paragraphs_curr.len(), 2);
        // Two paragraphs, each with one sentence (no internal "."
        // split — wikiwho's sentence-split regex requires 3+ chars
        // before the dot, "first paragraph" matches but "1st" wouldn't).
        assert_eq!(out.unmatched_sentences_curr.len(), 2);
        // Token counts: "first" "paragraph" "." | "second" "paragraph" "here" "." = 7.
        assert_eq!(article.tokens.len(), 7);
        assert_eq!(revision_curr.original_adds, 7);
        // Each Word's origin_rev_id is the current rev.
        for word in &article.tokens {
            assert_eq!(word.origin_rev_id, 100);
            assert_eq!(word.last_rev_id, 100);
        }
        // Token strings agree with what the plain tokenizer would
        // emit — a sanity check that the cascade walks the splitter
        // in document order.
        let direct: Vec<String> = crate::tokenize::tokenize_revision(text);
        let cascade: Vec<String> = article.tokens.iter().map(|w| w.value.clone()).collect();
        assert_eq!(cascade, direct);
    }

    #[test]
    fn determine_authorship_empty_text_is_no_op() {
        // Empty input has no paragraphs after the trim filter; nothing
        // should be allocated.
        let mut article = Article::new("Test");
        let revision_prev = Revision::default();
        let mut revision_curr = Revision {
            id: 100,
            ..Default::default()
        };
        let out = determine_authorship(&mut article, "", &revision_prev, &mut revision_curr);
        assert!(!out.vandalism);
        assert_eq!(article.tokens.len(), 0);
        assert_eq!(article.paragraphs.len(), 0);
        assert_eq!(article.sentences.len(), 0);
    }

    #[test]
    fn paragraph_cascade_duplicate_hash_in_prev_picks_each_in_order() {
        // Prev has paragraph A appearing twice. Curr has it appearing
        // twice as well. Both copies should match (each consumes one
        // bucket entry).
        let mut article = Article::new("Test");
        let text = "duplicate";
        let hash = hash_md5(text);
        let pid_a = article.alloc_paragraph(hash.clone(), text.to_string());
        let pid_b = article.alloc_paragraph(hash.clone(), text.to_string());
        article
            .paragraphs_ht
            .insert(hash.clone(), vec![pid_a, pid_b]);
        let mut revision_prev = Revision {
            id: 100,
            ..Default::default()
        };
        revision_prev
            .paragraphs
            .insert(hash.clone(), vec![pid_a, pid_b]);
        revision_prev.ordered_paragraphs.push(hash.clone());
        revision_prev.ordered_paragraphs.push(hash);

        let mut revision_curr = Revision {
            id: 101,
            ..Default::default()
        };
        let combined = format!("{text}\n\n{text}");
        let (analysis, _) =
            run_paragraph_cascade(&mut article, &combined, &revision_prev, &mut revision_curr);

        // Both prev paragraphs were consumed (one per curr instance).
        assert_eq!(analysis.matched_paragraphs_prev.len(), 2);
        assert!(analysis.matched_paragraphs_prev.contains(&pid_a));
        assert!(analysis.matched_paragraphs_prev.contains(&pid_b));
        assert!(analysis.unmatched_paragraphs_curr.is_empty());
        assert!(analysis.unmatched_paragraphs_prev.is_empty());
        // Arena didn't grow.
        assert_eq!(article.paragraphs.len(), 2);
    }
}
