# 2026-05-23 (part 4) — Port Differ, close the parity gap

**Goal:** with consumer-usage research showing three of four
downstream projects (Dashboard ArticleViewer, XTools Authorship/Blame,
WhoWroteThat gadget) render attribution per-token, the ~14% wrong-
`o_rev_id` floor under Myers is visibly bad. Port Python's
`difflib.Differ` to close the gap, keep Myers around for a future
revisit.

**Parity (full-history vs Python ground truth):**

| Fixture | Before (Myers) | After (Differ) |
|---|---|---|
| en/24544 Photosynthesis (5494 revs) | str 100%, o_rev_id 86.19%, all-fields **80.28%** | str 100%, o_rev_id 100%, all-fields **100.00%** |
| simple/27263 Wikipedia (3755 revs) | str 100%, o_rev_id 91.55%, all-fields **53.41%** | str 100%, o_rev_id 100%, all-fields **100.00%** |
| en/79023819 Israel-Hamas war (2 revs) | 100% / 100% | 100% / 100% |
| zh/1686258 中国 (7 revs) | 100% / 100% | 100% / 100% |
| **Aggregate** | 4 / 4 fixtures, **80–100%** all-fields | **4 / 4 at 100.00%** |

**Parity (single-rev fixtures):** 90.02% / 93.75% (unchanged — single-
rev fixtures don't exercise the Differ path because text_prev is empty
for the first revision).

**Done:**

- **`crates/wikiwho-attribute/src/differ.rs`** — new ~700-line port of
  Python's `difflib.SequenceMatcher` (Ratcliff/Obershelp matcher) +
  `difflib.Differ.compare` (`_fancy_replace` / `_fancy_helper` /
  `_plain_replace` chain). Generic over `T: Eq + Hash + Clone` so the
  same struct backs the outer token-level matcher (u32 interned IDs)
  and the inner character-level matcher inside `_fancy_replace`
  (`char`). Autojunk (popular-element elision for n ≥ 200) ported
  faithfully. The `'? '` hint lines Python emits for human-readable
  diffs are dropped — the cascade filters them out anyway.

- **`crates/wikiwho-attribute/src/cascade.rs`** — the general-case
  token cascade now calls `differ::differ_compare` instead of
  `diff::myers_diff`. The `DiffEntry` matching loop is unchanged; only
  the producer of the transcript switched.

- **`scripts/verify_differ.py`** — tiny Python helper that runs
  `Differ().compare(text_prev, text_curr)` over JSON-encoded test
  cases and emits filtered `(tag, value)` pairs. Drives the parity
  test.

- **`crates/wikiwho-attribute/tests/differ_python_parity.rs`** — new
  integration test that runs 22 curated cases (empty inputs, pure
  inserts, pure deletes, single substitutions, transpositions,
  duplicate tokens, `_fancy_replace` close-pair cases, vandalism-and-
  revert patterns, autojunk-triggering 250-element sequences) through
  Python's Differ and asserts the Rust port produces an identical
  `(tag, value)` sequence for every one.

- **Myers stays compiled** in `crates/wikiwho-attribute/src/diff.rs`
  but is no longer called from the cascade — see
  `notes/diff-algorithm-revisit.md` for the rationale (port-matching-
  Python and best-attribution-for-humans aren't necessarily the same
  answer; the second question is unanswered).

**Updated decisions:**

- The "close Myers-vs-Differ gap by porting Differ?" entry in
  `notes/decisions-needed.md` is now **Resolved** — chose A (port).
- New context doc: `notes/diff-algorithm-revisit.md` captures the
  philosophical question of which algorithm better reflects human
  editing intent, methodology for the revisit, and the trip-wires
  that would force one.

**Issues encountered + resolutions:**

- *First autojunk unit test was wrong.* I expected `find_longest_match`
  to return size=0 when the only candidate token was popular-elided
  from `b2j`. Actually the extension phase (post-initial-scan) does
  not check `bpopular`, only `bjunk`, so popular elements still get
  picked up by adjacency to a real match. Test rewritten to verify
  correctness on a 200+ sequence, not autojunk-specific behaviour.
- *Initial intern closure had a lifetime issue.* The intern HashMap
  with borrowed `&str` keys couldn't escape the closure; switched to
  owned `String` keys (one allocation per unique token, fine).
- *Clippy too_many_arguments on `_fancy_replace`/`_fancy_helper`/
  `_plain_replace`.* These mirror Python's `(a, alo, ahi, b, blo, bhi)`
  arg shape; signature is intrinsic to the source we're porting.
  Allow-listed locally rather than refactored.
- *Stale Myers reference in `parity-check`'s docstring.* Updated to
  reflect that the cascade now uses Differ and any < 100% (vs Python)
  is a port bug.

**Counts:** 103 lib tests + 1 integration test, all passing. Clippy
clean with `-D warnings`. Build clean.

**Performance check (release builds):**

- Photosynthesis full-history replay (5494 revs): **8.27 seconds**.
  Python's `python_replay.py` on the same fixture took ~70 seconds in
  the previous session. Differ does NOT obviously regress runtime
  relative to Myers despite its extra `_fancy_replace` work — both
  paths have similar costs per revision and Myers' theoretical edge
  doesn't dominate.

**Next session likely starts with:**

The algorithm correctness goal is essentially closed at this layer.
Natural next steps from the original PLAN.md §9:

1. **Capture more histories.** 12 of 16 fixtures are still skipped
   because they're missing `history.jsonl`. Running
   `scripts/capture_history.py` on the remaining ones would broaden
   the parity surface. Some are huge (Obama ~57K revs, Einstein ~13K,
   Hitler ~20K) — likely an overnight job.
2. **Start `wikiwho-storage`.** PLAN.md §9 calls for a blob format
   read/write/append/compact crate. The algorithm now produces stable
   data the storage layer needs to durably persist.
3. **Start `wikiwho-mwclient`.** The MW Action API + Wikipedia REST
   client. Needed both for the ingest path (revisions feed) and for
   the WhoColor endpoint (Parsoid HTML).

Recommendation is **1 then 3 or 2** — broadening the parity corpus
locks in correctness, then the non-algorithm work can proceed in
parallel.
