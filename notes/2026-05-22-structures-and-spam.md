# 2026-05-22 — structures + spam detection

**Goal:** scaffold the data model (Word/Sentence/Paragraph/Revision/Article) and port the spam-detection primitives. No algorithm logic yet — this is the setup for the cascade work.

**Parity:**
- revisions: 15 / 16 (93.75%) — unchanged, no algorithm changes
- tokens: 876,775 / 973,970 (90.02%) — unchanged
- ms: 870

Holds steady — the new modules don't touch the parity-check path; they're scaffolding for future cascade work.

**Done:**
- `crates/wikiwho-attribute/src/structures.rs`. `Word`, `Sentence`, `Paragraph`, `Revision` arena-allocated by typed-alias indices (`TokenId`, `SentenceId`, `ParagraphId` = u32; `RevId` = u64). `Article` is the lifetime container — holds the arenas, the cross-revision hash tables (`paragraphs_ht`, `sentences_ht`), the revision dict, and the spam-tracking sets. `MatchedSets` is the per-iteration scratch state that replaces the Python's shared `matched: bool` flag (per `ALGORITHM.md §4`). `Article` exposes `alloc_word/sentence/paragraph` builders and `word/word_mut/...` accessors. 4 unit tests.
- `crates/wikiwho-attribute/src/spam.rs`. Constants (`CHANGE_PERCENTAGE`, `PREVIOUS_LENGTH`, `CURR_LENGTH`, `TOKEN_DENSITY_LIMIT`, `TOKEN_LEN`, `UNMATCHED_PARAGRAPH`, `MOVE_FLAG`) verbatim from `wikiwho.py:22-29` and `ALGORITHM.md §3.3`. `length_shrink_is_vandalism(prev_length, curr_length, comment, minor)` ports the length-based heuristic with the good-faith-move escape hatch. `hash_matches_known_spam(rev_hash, spam_hashes)` ports the SHA-1 membership check. 11 unit tests covering the heuristic's boundary cases (size gates, ratio at threshold, escape hatch combinations).
- `crates/wikiwho-attribute/src/lib.rs` updated to re-export the new modules and explicitly document that the matching cascade is still unported.

**Issues encountered + resolutions:**
- Two spam tests had wrong arithmetic — I wrote `length_shrink_is_vandalism(10_000, 700, false, false)` expecting a "modest 30% shrink" but 10000→700 is 93%, well past the -0.40 threshold AND blocked by the `curr_length < 1000` gate. Fixed by picking values that actually exercise the boundary (prev=1500, curr=900 sits exactly at -0.40; prev=1500, curr=950 is ~-0.37). Caught by `cargo test`.

**Counts:** 30 tests → **45 tests** total; cargo clippy clean.

**New decisions queued:** none.

**Next session likely starts with:**

The matching cascade. Order suggested by `ALGORITHM.md §4`:

1. Port `analyse_paragraphs_in_revision` (`wikiwho.py:327-459`). This is the outermost cascade level. For each non-empty paragraph in the current revision: check the previous revision's `paragraphs` dict for an unmatched hash match; if not found, check the global `paragraphs_ht`. Track matched / unmatched paragraphs via `MatchedSets`.
2. Skeleton of `determine_authorship` (`wikiwho.py:207-325`) calling into the paragraph level — the sentence and token levels can return early (empty results) until they're ported.
3. Hash-table updates (`wikiwho.py:308-323`) — add fresh paragraphs/sentences to the global tables on successful processing.
4. Hook `analyse_article` (the entry point) into spam detection + paragraph cascade so we can finally run `Article` through a single revision.

At that point parity-check can be extended to compare per-revision: feed the wikitext into `analyse_article`, walk the resulting `Article`'s tokens, compare to the fixture's token sequence. The same tokenizer-level 90% baseline still holds, but the comparison would now also include `o_rev_id` (which for single-rev input is always the rev being processed) — a sanity check.

Sentence and token cascade levels come after. Then the diff (`wikiwho.py:584-691` with Myers replacing Differ per ALGORITHM.md §6). Then we can run multi-revision input.
