# 2026-05-22 — full-history parity infrastructure + first real signal

**Goal:** stand up the multi-rev parity ratchet so the new algorithm
code (Myers + recorder) is exercised against real production output.

**Parity (single-rev fixtures, unchanged):**
- revisions: 15 / 16 (93.75%)
- tokens: 876,775 / 973,970 (90.02%)

**Parity (NEW: full-history mode, 3 fixtures with history captured):**
- revisions passing: 1 / 3 (Israel-Hamas war only)
- tokens (str): 4536 / 4536 (100.00%)
- tokens (o_rev_id): 4154 / 4536 (91.58%)
- tokens (inbound): 88 / 4536 (1.94%)
- tokens (outbound): 88 / 4536 (1.94%)
- tokens (all-fields): 86 / 4536 (1.90%)

Token-string parity is **perfect** on all three fixtures — the cascade
walks the same splitter as production. `o_rev_id` is mostly right.
`inbound`/`outbound` is mostly wrong, and the pattern is consistent
across tokens (~2× as many entries as production); see queued
decision.

**Done:**

- `scripts/capture_history.py` (new). Pages the MW Action API for
  full revision history of each fixture, writes `history.jsonl` next
  to `meta.json`. Idempotent (skips fixtures whose last line already
  matches the target rev_id), polite (configurable delay), bounded
  (`--max-revs` aborts if history exceeds the cap; useful to validate
  the pipeline on small fixtures before pulling Obama-class
  articles). Each line is the normalized revision shape
  (rev_id / timestamp / sha1 / comment / minor / user / text /
  text_hidden) the parity binary consumes.

- `crates/wikiwho-attribute/src/structures.rs`. Ported
  `iter_rev_tokens` — walks paragraphs → sentences → words in
  document order, handling duplicate paragraph and sentence hashes
  with per-walk seen-counters (replaces the Python `tmp['p']`/`['s']`
  list dance with an identical-semantics `HashMap<&Hash, usize>`).
  Two new tests; the duplicate-hash case is the one that catches the
  off-by-one if the counter logic isn't right.

- `crates/wikiwho-parity/src/main.rs`. Added `--full-history` mode:
  loads `history.jsonl`, feeds every revision (skipping
  `text_hidden`) to `Article::analyse_revision`, walks the final
  revision's tokens, and compares per-field against
  `rev_content.json`. Per-field breakdown reported in the summary:
  str / o_rev_id / inbound / outbound / all-fields.

- `crates/wikiwho-attribute/src/cascade.rs`. **Bug fix:** the
  inbound/last_rev_id recorder was double-processing words that
  appeared in BOTH `matched_paragraphs_prev`'s sentence walk AND
  `matched_sentences_prev`'s direct word walk. Python avoids this
  via the `matched=False` reset (set to False after the first walk
  visits a word, skipped on the second). Our port had dropped the
  `matched` flag in favor of `MatchedSets`, but lost the dedup
  behavior. Fix: added an explicit `processed: HashSet<TokenId>` to
  the recorder. Caught by the 中国 fixture (rev 60012989: paragraphs_ht
  match of an old paragraph + tail-loop overlap with rev 39674010's
  sentence). Fixed 中国's `inbound` divergence from 6/18 to 0/18
  mismatched.

**Issues encountered + resolutions:**

- *MW API rate-limits and pagination.* `rvlimit=max` gives 50 revs
  per batch for unauthenticated callers. Simple Wikipedia (3.8K revs
  up to the target) took 76 batches. Obama-class (~50K) would take
  ~1000+ batches and tens of GB of disk; out of scope for this
  session. Added `--max-revs` as a circuit breaker.

- *`iter_rev_tokens` correctness on duplicate hashes.* The Python
  reference uses `tmp['p'].count(hash)-1` after appending; my first
  cut used a counter that incremented BEFORE getting the index. Off
  by one. Caught by the `iter_rev_tokens_handles_duplicate_paragraph_hashes`
  test before any real fixture would have hit it.

- *Recorder double-processing surfaced as `inbound=[60012989]` on
  every token of 中国's original sentence.* Symptom: after rev
  60012989, every word in the old sentence "$ redirect [[ 中 國 ]]"
  had its `inbound` bumped to include 60012989 — but Python's
  expected has empty inbound for most of those. Tracing showed the
  recorder was called twice per word: once via
  `matched_paragraphs_prev` → P_old → S_old → words, then again via
  `matched_sentences_prev` → S_new_39674010 (which had been added by
  the sentence-cascade tail loop) → words. First call: `last_rev_id`
  was 39674010 == revision_prev_id → no inbound, set
  `last_rev_id = 60012989`. Second call: `last_rev_id` was now
  60012989 != revision_prev_id → BUMP. The `processed` set
  short-circuits the second visit.

- *Simple Wikipedia is the messy fixture.* The history is mostly
  consistent (98% revs processed) and the token sequence comes out
  identical to production. But `inbound`/`outbound` lists are
  roughly 2× as long as production's. Spot checks suggest the extra
  entries cluster around vandalism-and-revert pairs that Python's
  cached output skipped but our algorithm processes through. Queued
  to `notes/decisions-needed.md` as a non-blocking investigation
  task — likely a mix of historical-state drift in the production
  fixture (the cached output was produced incrementally over years)
  and possibly a still-undetected double-count somewhere.

**Counts:** 82 → 84 tests; cargo clippy clean; cargo build clean.

**New decisions queued:** one — multi-rev inbound/outbound inflation
on simple Wikipedia (non-blocking; recommendation: get one more
fixture's signal before deep-investigating).

**Next session likely starts with:**

Two parallel tracks, depending on appetite:

1. **Capture one more multi-rev fixture and see if the inflation is
   article-specific.** Albert_Einstein, Photosynthesis, or Jesse_Owens
   are reasonable mid-size choices. Use `--max-revs 5000` initially
   so it doesn't run all night. If the inflation pattern matches
   simple Wikipedia, escalate to a single-rev-pair Python-vs-Rust
   trace to find the bug. If only simple Wikipedia is bad, it's
   probably a historical-state effect and we can ship at ~91%
   o_rev_id parity.

2. **Investigate the 中国 `o_rev_id` divergence on the two duplicate
   `{{` tokens.** That's the textbook Myers-vs-Differ tie-breaking
   case from `ALGORITHM.md §6`. If the corpus shows this happens
   often, we may need to port Differ's anchor-match heuristic on top
   of Myers. If it's rare, accept it per the resolved decision.

The fixture-capture work for Obama-class articles is the long pole.
At MW API rate limits (50 revs per request × ~2s polite delay) it's
~30 min per 50K-rev article. Plan: run capture overnight in the
background, but it's not blocking algorithm work — single-rev
fixtures still cover the cascade end-to-end.
