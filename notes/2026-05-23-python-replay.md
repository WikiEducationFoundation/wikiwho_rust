# 2026-05-23 (part 3) — Python becomes the ground truth

**Goal:** per Sage's mid-session redirect, stop using production's
cached `rev_content.json` as the parity reference — those caches may
have evolved over years of incremental processing. Use a fresh local
Python run on the same `history.jsonl` instead. Find out whether the
remaining divergence is "historical-state artifact" or "real
Rust-vs-Python algorithm difference."

**Parity (single-rev fixtures, unchanged):** 90.02% / 93.75%

**Parity (full-history, vs *Python ground truth*):**
- en/24544 Photosynthesis: str **100%**, o_rev_id **86.19%**, inbound
  **87.87%**, outbound **87.79%**, all-fields **80.28%** *(identical to
  vs-prod-cache — confirms prod cache is not stale for this article)*
- simple/27263 Wikipedia: str 100%, o_rev_id 91.55%, inbound 53.70%,
  outbound 53.64%, all-fields **53.41%** *(vs 52.99% against prod-cache,
  tiny bump)*
- en/79023819, zh/1686258: 100% / 95.12% (unchanged)

**Answer to Sage's question:** for our current fixtures, the production
cache is essentially identical to a fresh Python run. The historical-
evolution concern was a real worry but not what's driving our gap. The
real gap is **token-level Rust-Myers vs Python-Differ**.

**Done:**

- **`scripts/python_replay.py`** now emits the full final-rev token
  sequence in the same shape as `rev_content.json`'s `tokens` array
  (str / o_rev_id / in / out / token_id). The output JSON is consumed
  directly by the parity binary as an alternate ground-truth source.

- **`parity-check --python-replay`** runs `python_replay.py` against
  the same fixture, caches the result at `<fixture>/python_replay.json`
  (which lives under the already-gitignored `parity-fixtures/`), and
  diffs against that instead of `rev_content.json`. `--refresh-python`
  forces re-run. Cache makes re-runs fast once the Python pass is paid
  for (Python on Photosynthesis = ~70s; cached re-load = ~50ms).

- **`parity-check --show-spam-ids`** dumps the article's `spam_ids` list
  plus the arena / hash-table totals. With matching companion fields in
  `python_replay.py`'s output (spam_ids, paragraphs_ht_size,
  paragraphs_total, etc.), this gives apples-to-apples structural
  comparison.

**Structural comparison vs Python (simple/27263, 3525 processed revs):**

| Metric                 | Python | Rust   | Δ      |
|------------------------|--------|--------|--------|
| spam_ids               | 230    | 230    | **0 (perfect)** |
| paragraphs_ht hashes   | 2888   | 2888   | 0      |
| paragraphs_ht totals   | 2915   | 2915   | 0      |
| sentences_ht hashes    | 7202   | 7202   | 0      |
| sentences_ht totals    | 8217   | 8233   | +16 (+0.2%) |
| **token allocations**  | 103742 | 100363 | **-3379 (-3.3%)** |

On Photosynthesis the same shape: paragraphs/sentences in ht match to
within 31; tokens diverge by **-5247 (-4.4%)**. The article-level
spam_ids list matches Python byte-for-byte on both fixtures.

So the divergence is fully concentrated in token-level matching. We
allocate ~3-4% *fewer* tokens than Python because our Myers finds
longer Keep sequences than Python's `difflib.Differ`, which uses
Ratcliff/Obershelp pattern matching (NOT true LCS — it's the
longest-contiguous-matching-subsequence heuristic that recurses on the
two halves). The few-percent token-count delta cascades into the
20-50% inbound/outbound divergence in the wire-format comparison.

**Issues encountered + resolutions:**

- *Apparent "arena bloat" was a vandalism-orphan artifact.* During
  binary-search of where Rust and Python diverged on simple Wikipedia,
  it looked at first like our cascade was allocating 25x more sentences
  than Python's. Adding `paragraphs_ht totals` and `sentences_ht
  totals` (sum of bucket lengths, not just unique hashes) revealed
  that the arena difference came entirely from orphan allocations on
  vandalism revs — paragraphs and sentences allocated by the cascade
  before the token-density gate fires and the revision rolls back. In
  Python those allocations are also created but never reach the ht
  (line 307 `if not vandalism:` gates the post-cascade ht update); in
  our arena they're inert but visible. The real per-fixture
  ht-allocated sentence counts agree to within 0.3%.

- *Python wikiwho.py imports cleanly in modern Python 3* despite the
  requirements.txt pinning 3.5.2. No compatibility shims needed for
  the parity harness — just `sys.path.insert` to the lib dir.

**Counts:** 84 tests (unchanged), clippy clean, build clean.

**Updated decisions:**

- The "residual inbound/outbound divergence" decision from this morning
  is now **Resolved**: it's intrinsic Rust-Myers-vs-Python-Differ, not
  a bug to fix in the cascade. Photosynthesis at 80% all-fields is the
  real floor for the current Myers-based implementation.

- **New decision queued:** whether to **port Differ** to close the gap
  to ~100%. The decision hinges on what o_rev_id parity Sage's three
  consumer projects need (we're at 86% on Photosynthesis). If they can
  tolerate 14% of tokens having a wrong attribution-revision, ship
  Myers; otherwise port Differ (~150 lines of Ratcliff/Obershelp).
  See `notes/decisions-needed.md`.

**Next session likely starts with:**

The clean fork is now visible:

1. **Probe consumer tolerance for o_rev_id divergence.** Look at how
   the Dashboard's ArticleViewer (`../WikiEduDashboardTwo/`) and Impact
   Visualizer (`../impact-visualizer/`) actually use `o_rev_id` /
   `editor`. If they're showing "this token was added by user X" UI,
   14% wrong assignments will be visible; if they're doing summary
   stats (e.g., "X% of this article was contributed by editors from
   group Y"), the wash-out may be acceptable.

2. **Port Differ.** Implement Python's `difflib.SequenceMatcher`
   (Ratcliff/Obershelp) on `&[u32]` interned token ids; thread it
   through `analyse_words_in_sentences` as either a replacement for
   Myers or an alternate path. Validate via `--python-replay` — should
   move all-fields from 80% to ~100% on Photosynthesis.

3. **Move on to non-algorithm work.** With algorithm correctness now
   bounded by an explicit, characterized difference, the rest of the
   stack (storage layer, HTTP server, MW client) can proceed in
   parallel. The `wikiwho-storage` crate is the natural next start —
   the algorithm produces data the storage layer needs to durably
   persist (PLAN.md §9).

The recommendation is **1 first** — without a forcing requirement
from consumers, porting Differ is speculative work; let the
requirement drive the decision.
