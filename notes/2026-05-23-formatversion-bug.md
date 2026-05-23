# 2026-05-23 — capture-script formatversion=2 bug eats most of the inflation gap

**Goal:** characterize the inbound/outbound inflation on simple Wikipedia
(queued from last session) by capturing one more multi-rev fixture and,
ideally, identifying the root cause.

**Parity (single-rev fixtures, unchanged):**
- revisions: 15 / 16 (93.75%)
- tokens: 876,775 / 973,970 (90.02%)

**Parity (full-history mode, 3 fixtures — same shape as last session):**
- revisions passing: 1 / 3 (only Israel-Hamas war full pass)
- simple/27263: str **4495 / 4495 (100%)**, o_rev_id 91.55%, **inbound
  53.28%, outbound 53.21%, all-fields 52.99%** (was 1.94% inbound)
- zh/1686258: str 18/18, o_rev_id 88.89%, inbound 100%, outbound 100%,
  all-fields 88.89% (2 misses = the documented Myers-vs-Differ on `{{`)
- en/79023819: str 23/23, all-fields 100%

**Done:**

- **Identified and fixed a bug in `scripts/capture_history.py`.** The
  script was normalizing MW Action API revisions with
  `"minor": "minor" in rev` — correct for formatversion=1 (where MW
  omits the `minor` key when not minor), wrong for formatversion=2
  (which we use; `minor` is *always* present as a bool). Every
  captured revision was wrongly tagged `minor=true`, which fires the
  `comment AND minor` good-faith-move escape hatch in the length-shrink
  check (`wikiwho.py:161`) and let almost all blanking vandalism
  through. Fix: `bool(rev.get("minor", False))`. While touching the
  function I also cleaned up the texthidden lookup (which lives at
  `slots.main.texthidden` in v2, not at revision top-level — the
  fallback path was already catching it but the primary check was a
  dead branch).

- **Re-captured all three multi-rev fixtures** (simple/27263 = 3783
  revs, zh/1686258 = 7 revs, en/79023819 = 2 revs) with the fix.
  Simple Wikipedia's `minor=true` count went from 3783/3783 to
  ~1750/3783 — a much more believable distribution.

- **Diagnostic options on `parity-check`.** Added two flags that paid
  for themselves multiple times this session:
  - `--show-field-mismatches N`: prints the symmetric difference of
    inbound/outbound rev_ids for up to N mismatching tokens. This is
    what told us the pre-fix divergence was *paired* rev_ids (the
    vandalism + revert pattern) rather than scattered noise.
  - `--show-spam-ids`: prints the cascade's `spam_ids` list, sorted.
    Used to confirm we were NOT catching individual revs we suspected
    we were (e.g. 6710715, which is processed normally on both sides).

- **Helper scripts under `scripts/`** for ad-hoc fixture inspection:
  `inspect_revs.py` (dump comment/minor/text-len for given rev_ids) and
  `inspect_origins.py` (count tokens whose o_rev_id / in / out mention
  given rev_ids in production output). Both pure stdlib.

**Issues encountered + resolutions:**

- *Pivoted away from "capture more fixtures first."* The session
  started chasing the queued recommendation — capture Photosynthesis or
  Jesse_Owens as a second multi-rev signal. Both exceeded the
  `--max-revs 5000` cap; rather than bump the cap and wait, I switched
  to investigating simple Wikipedia's divergence directly while the
  background captures aborted. That investigation found the real bug,
  which made the "more fixtures" path much less urgent. Lesson: when a
  multi-rev fixture has a clear divergence pattern, trace it before
  collecting more — the cost of N=1 deep is much lower than N=2 shallow
  when the bug is in the framework, not the algorithm.

- *Verifying the v1-vs-v2 difference required hitting the live MW API
  directly* — our captured history.jsonl had been normalized away the
  raw `minor` value, so we had to re-fetch a handful of vandalism revs
  in both formatversion=1 and formatversion=2 to confirm the
  always-present-bool behavior in v2 vs. presence-only in v1. Worth it:
  the diagnostic confirmed that *only* `minor` has this quirk in v2 —
  `userhidden` / `commenthidden` / `suppressed` / `sha1hidden` /
  `texthidden` are still presence-when-true in v2 (so the existing
  `"X" in rev` checks for those are fine).

**Counts:** 84 tests (unchanged), cargo clippy clean, cargo build
clean. No algorithm code changed; the win came from feeding the
algorithm the correct input.

**New decisions queued:**

- One — **residual inbound/outbound divergence on simple Wikipedia
  (~47%)** (non-blocking). The pattern is no longer "2× inflation"; it's
  scattered per-token differences clustered around vandalism-burst
  revisions where Differ and Myers might be making different
  token-id-assignment choices. Recommendation: capture one more
  mid-size fixture first before tracing a single rev pair through both
  Python and Rust. See `notes/decisions-needed.md`.

**Next session likely starts with:**

Either of two clean tracks:

1. **Bump `--max-revs` to ~10K and capture Photosynthesis (~7-8K revs
   estimated)** to get a second multi-rev signal. If Photosynthesis
   also lands around 50% all-fields, that's the new floor and the
   right move is to characterize the Myers-vs-Differ remainder. If it
   lands much higher (say 85%+), simple Wikipedia is anomalously bad
   and a single-rev-pair Python trace is the right move.

2. **Trace one mismatch directly without more capture.** Pick a
   specific token + rev pair from the current diagnostic output
   (e.g., token #0 "{{" missing 6710715/6710716), stand up a minimal
   Python harness that runs `Wikiwho.analyse_article` on the same input
   slice, and diff the two cascades step by step. Faster signal, harder
   to set up.

Tactical aside: when you DO bump `--max-revs`, capture in the
background and run the parity check on the existing fixtures in
parallel. The capture is I/O-bound (MW polite delay), the parity is
CPU-bound — no contention.
