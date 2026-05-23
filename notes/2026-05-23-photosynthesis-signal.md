# 2026-05-23 (part 2) — second multi-rev signal: Photosynthesis at 80%

**Goal:** confirm the formatversion=2 fix's headline number (53.70% on
simple Wikipedia) by capturing a second multi-rev fixture, and add a
rev-id-level histogram so future divergence investigations can move
from "look at token #N" to "look at rev X."

**Parity (single-rev fixtures, unchanged):**
- revisions: 15 / 16 (93.75%)
- tokens: 876,775 / 973,970 (90.02%)

**Parity (full-history, 4 fixtures — Photosynthesis is new):**
- en/24544 Photosynthesis (5494 of 5495 revs, 1 hidden, 103 spam):
  - str **27349 / 27349 (100.00%)**
  - o_rev_id **86.19%**
  - inbound **87.87%**, outbound **87.79%**, all-fields **80.28%**
- simple/27263 (3755 of 3783 revs, 28 hidden, 230 spam): unchanged
  - str 100%, all-fields **52.99%**
- en/79023819 Israel–Hamas_war: 23/23 = 100% all-fields
- zh/1686258 中国: 18/18 str, 16/18 all-fields (the documented
  Myers-vs-Differ on duplicate `{{`)

**Done:**

- **Captured Photosynthesis with `--max-revs 15000`** (5495 actual
  revs; just slightly over the previous 5K cap that aborted it last
  time). One revision hidden, 103 caught as spam by our cascade.

- **Added `--rev-id-histogram N`** to the parity binary. For every
  rev_id ever mentioned in inbound or outbound on either side, sum
  rust and production occurrences and sort by absolute diff. This is
  the diagnostic that turned "47% divergent, why?" into "588 specific
  rev_ids account for the gap; here are the top 25." On simple
  Wikipedia the top entries are pairs:
  ```
      rev_id     r_in   r_out    e_in   e_out   |diff|
     7882436        0     600       0     227     +373
     7882438      600       0     227       0     +373
     8482027        0    2866       0    3161     +295
     8482050     2866       0    3161       0     +295
  ```
  Each pair is one vandalism+revert sequence where both production
  and our cascade processed both revs, but matched a different subset
  of tokens. **Not a spam-detection issue any more** — both sides
  agree on which revs are spam — it's a cascade-matching issue at the
  paragraph or sentence level (or token level via Myers vs Differ).

  Photosynthesis shows the same shape with smaller deltas:
  ```
      rev_id     r_in   r_out    e_in   e_out   |diff|
    86417972        0    2386       0    2808     +422
    86418247     2386       0    2808       0     +422
   321986007     6763       8    7086      13     +328
  ```
  Mixed direction: we sometimes match MORE tokens than production
  (fewer go outbound), sometimes FEWER. So this isn't "we're too
  permissive" or "too strict" — it's path-dependent.

**Issues encountered + resolutions:**

- *Gaza_war capture started in parallel exceeded `--max-revs 10000`
  too.* Recent-issue articles on en accumulate revisions extremely
  fast (rate-of-edit on a current-events topic). Aborted by the cap;
  not blocking — Photosynthesis was enough for the second data point.

- *Clippy caught `sort_by` → `sort_by_key(Reverse(...))`.* Trivial
  fix; mentioning because the histogram code was the only new
  algorithmically-ish chunk this session and it's good to know clippy
  still has teeth on diagnostic code.

**Counts:** 84 tests (unchanged), clippy clean, build clean. No
algorithm code changed.

**What we know now that we didn't this morning:**

1. The formatversion=2 fix wasn't a one-off — it removed the bulk of
   the noise that was making the divergence opaque. Photosynthesis at
   80% all-fields is a much more useful starting point than simple
   Wikipedia at 53%.
2. The remaining divergence shape is identical between simple Wikipedia
   and Photosynthesis: **paired (vandalism + revert) rev_ids where
   both sides process both revs but match different token subsets.**
   The deltas range from O(100) to O(400) tokens per pair.
3. **simple Wikipedia is anomalous, not representative.** It's a
   high-vandalism, low-edit-quality wiki where many edit pairs have
   non-trivial article restructuring. Photosynthesis (low vandalism,
   careful editorial discipline) sits at 80%. Sage's three production
   consumers mostly target en.wikipedia (similar profile to
   Photosynthesis), so the 80% is the more honest read of where we
   are.

**New decisions queued:** none new this turn — the residual-divergence
queue entry from earlier still applies, and we now have a second data
point to support its "trace one pair" recommendation.

**Next session likely starts with:**

The most productive next step is now **option B from the queued
decision** — trace a single divergent rev pair through both Python and
our Rust cascade. Pick e.g. Photosynthesis rev 86417972 / 86418247
(422-token delta, well-isolated, comment makes the human intent
clear). Steps:

1. Spin up a minimal Python venv with the wikiwho library
   (`../wikiwho_api/lib/WikiWho/`) and a tiny driver script that calls
   `Wikiwho.analyse_article` on a slice of the captured history.jsonl
   ending at the divergent pair.
2. Add per-revision logging on both sides: `revision_curr.id`, count
   of matched_paragraphs_prev, matched_sentences_prev, matched
   words, unmatched on each side.
3. Diff the logs. The first revision where the numbers diverge is the
   culprit — usually a hash-bucket pick or a Myers-vs-Differ Keep.

Alternative: capture one more low-vandalism mid-size fixture (e.g.,
Albert_Einstein at `--max-revs 15000`) to confirm the 80% floor on
yet another article. Faster to set up than the Python harness; lower
information per minute.
