# 2026-05-22 — Myers diff + general-case token cascade + inbound/outbound

**Goal:** land the three pieces flagged by the previous session as
needed before multi-rev parity can move:
1. Myers diff on `&[u32]` interned token ids.
2. General-case token cascade (replace the `todo!()` panic).
3. Inbound / outbound / last_rev_id recording on words.

**Parity (single-rev fixtures, unchanged as expected):**
- revisions: 15 / 16 (93.75%) → unchanged
- tokens: 876,775 / 973,970 (90.02%) → unchanged
- ms: 1026 → 1040 (~1% noise; the Myers path is never hit on single-rev fixtures)

The metric didn't move because single-rev fixtures don't expose the new
code paths. Every fixture has `text_prev` empty (no prior revision), so
the cascade still bails out on the insertion-only branch. Multi-rev
parity is gated on the fixture-upgrade work that's queued separately.

**Done:**

- `crates/wikiwho-attribute/src/diff.rs` (new). Classical Myers (1986)
  shortest-edit-script over `&[u32]`, O((N+M)·D) time, with a clean
  backtrack and an `intern_sequences` helper that shares ids across
  both inputs. 14 dedicated unit tests cover the obvious edges
  (empty/identical/disjoint sequences, mid-string substitutions,
  duplicate tokens, 200-element long runs) plus a property-style
  "transcript applies to yield curr / reverses to yield prev" check
  across six representative pairs.

- `crates/wikiwho-attribute/src/cascade.rs`. Replaced the `todo!()`
  in `analyse_words_in_sentences` with the Myers-driven general case
  ported verbatim from `wikiwho.py:631-691`, including:
  - The DELETE-branch quirk where a curr token's value happens to
    coincide with a deleted prev token's value — Python consumes the
    prev word, appends outbound, but does NOT set `curr_matched`,
    leaving the curr word to be allocated fresh at the fallback.
    Comment in code calls this out as load-bearing per
    `ALGORITHM.md §4.3`.
  - The "fallback alloc" path at `wikiwho.py:679-689` for curr tokens
    that don't show up in the diff transcript (duplicates past the
    first occurrence).
  - Insertion / deletion / vandalism short-circuits unchanged.

- `crates/wikiwho-attribute/src/cascade.rs`. New
  `record_inbound_outbound` function: post-cascade walker that
  applies the outbound + inbound + last_rev_id updates from
  `wikiwho.py:257-305`. Skips the Python's well-known stale-
  `word_prev` bug in the `matched_words_prev` loop (line 301) on the
  ground that the kept-token last_rev_id update is already covered
  by `matched_sentences_prev` (because of the tail loop in
  `analyse_sentences_in_paragraphs` that double-lists tail sentences
  as both unmatched_prev and matched_prev). The comment in code
  documents both the Python bug and why our fix is functionally
  equivalent.

- `crates/wikiwho-attribute/src/cascade.rs`. Added `matched_token_ids:
  HashSet<TokenId>` to `CascadeOutput` so the recorder can ask "did
  this prev word survive the cascade anywhere?" without re-running
  the matching logic.

- `crates/wikiwho-attribute/src/pipeline.rs`. Wired the recorder into
  `Article::analyse_revision`, between cascade and hash-table updates.

- `crates/wikiwho-attribute/src/pipeline.rs`. Six new multi-rev
  integration tests:
  - `mid_sentence_edit_runs_myers_diff` — substitution forcing the
    Myers path; verifies kept tokens reuse ids, new token is fresh,
    deleted token gets outbound, kept tokens' last_rev_id bumps but
    no inbound.
  - `deleted_token_records_outbound` — pure shrink; outbound on the
    removed token, no new allocations.
  - `inserted_tokens_within_existing_sentence` — pure insert; new
    tokens allocated, kept tokens' last_rev_id bumps.
  - `reintroduced_sentence_records_inbound` — sentence falls out and
    comes back via `paragraphs_ht` / `sentences_ht`; inbound bumps
    correctly.
  - `reintroduced_token_via_diff_records_inbound` — documents that
    single-token reintroduction through the Myers diff does NOT
    recover the original token id (only sentence/paragraph-level hash
    reuse does). This is the right algorithm behaviour.
  - `three_revisions_chain_inbound_outbound_consistently` — three-rev
    sequence verifying both stable tokens and out-and-back tokens
    track correctly across multiple recorder runs.

**Issues encountered + resolutions:**

- *`matched_token_ids` was empty on the all-paragraphs-matched path.*
  Adding the integration tests immediately surfaced a bug:
  `determine_authorship` had an early `return out;` when
  `unmatched_paragraphs_curr.is_empty()` — the case where every curr
  paragraph matched at the paragraph level — and that path didn't
  copy `matched.tokens` into `out.matched_token_ids`. Result: the
  recorder saw an empty set and skipped every inbound bump.
  Symptom in the tests was that `last_rev_id` and `inbound` stayed
  pinned at the original revision. Fix: assign
  `out.matched_token_ids = matched.tokens;` on both the early-return
  and normal-return paths.

- *Python's `for matched_word in matched_words_prev` loop uses a
  stale `word_prev`.* `wikiwho.py:298-305` references `word_prev`
  inside that loop — but `word_prev` is the loop variable from the
  earlier `matched_sentences_prev` loop, not the current
  `matched_word`. This is a latent Python bug. After tracing through
  what the bug actually does in practice, I'm confident the
  functionally-correct behaviour is what
  `update_inbound_and_last_rev_id` implements via the
  `matched_sentences_prev` walk alone (those sentences are
  double-listed by the analyse_sentences_in_paragraphs tail loop, so
  diff-kept words land there too). Our recorder skips the buggy
  matched_words_prev loop entirely and documents the reasoning in
  code.

- *Borrow checker around `matched.tokens` in the cascade.* The diff
  matching loop wants to (a) iterate `unmatched_words_prev` reading
  `matched.tokens.contains(&wid)`, (b) potentially insert into
  `matched.tokens`, (c) mutate `article` (alloc / sentence_mut /
  word_mut). Resolved by lifting the "find first unmatched prev
  word with this value" into a free function `find_unmatched_prev`
  that takes immutable refs to the parallel arrays + matched set,
  then doing the mutation back at the call site.

**Counts:** 62 → 82 tests; cargo clippy clean; cargo build clean on
stable Rust 2024 edition.

**New decisions queued:** none. The "multi-rev fixtures vs.
single-rev quarantine" decision from session 4 still gates moving
the parity number, but no new forks today.

**Next session likely starts with:**

The fixture upgrade. The new algorithm code is correct on small
synthetic inputs but is unexercised against real Wikipedia revision
histories. Plan:

1. **Capture multi-rev fixtures.** Adapt
   `scripts/capture_fixtures.py` (or write a sibling) to pull every
   revision of each existing fixture article up to its target
   `rev_id` from the live wikiwho API. Save as one JSON file per
   article with `revisions: [{rev_id, timestamp, text, sha1,
   user_id, user_name, comment, minor}, ...]`.

2. **Extend the parity binary.** Add a `--full-history` mode that
   feeds every revision to `Article::analyse_revision` in order,
   then compares the final state against the snapshot (token list,
   `o_rev_id` per token, `in[]`, `out[]`).

3. **Look at what diverges.** Expected outcomes, in increasing order
   of surprise:
   - Spam-detected revisions cause minor token-id drift (offset by 1
     per skip). Trivial to investigate; probably nothing to fix.
   - Myers vs `difflib.Differ` tie-breaking on duplicate tokens
     produces a small percentage of `o_rev_id` divergences. Quantify;
     if <0.1% and not on critical articles, accept per
     `ALGORITHM.md §6`'s resolved decision.
   - Larger divergences indicate an algorithm bug. Track down with
     small reproductions.

4. **Possible follow-up: revisions_skip handling.** Production
   skips revisions where `texthidden` or `textmissing` is set on
   the MW API response (`wikiwho.py: handler.py`). We haven't
   ported that path because single-rev fixtures don't expose it.
   Will need to be added before multi-rev parity can match
   production exactly.

Parity expectation on first multi-rev run: probably noisy until
revision_skip handling is wired up, then ~95-99% modulo Myers
tie-breaking divergences. If lower than that, there's a real bug.
