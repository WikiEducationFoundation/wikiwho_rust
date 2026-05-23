# Diff algorithm: Differ vs Myers — revisit later?

**Status:** non-blocking. The current cascade uses the Differ port
(`differ.rs`) — Ratcliff/Obershelp pattern matching, byte-for-byte
faithful to Python's `difflib.Differ().compare(text_prev, text_curr)`.
The Myers implementation in `diff.rs` is kept compiled (no longer
called from the cascade) so the comparison is cheap to re-enable.

This note exists because the *parity-with-Python* decision and the
*best-for-the-user* decision are not necessarily the same answer, and
we don't yet have the data to evaluate the second.

---

## Why we chose Differ

The initial goal of the rewrite is parity with the Python reference
implementation. Three of four downstream consumers (Wiki Education
Dashboard's ArticleViewer, XTools Authorship/Blame, WhoWroteThat
gadget) render attribution **per token** — they color tokens by editor
or show "this word added in rev X" UI. At ~86% per-token `o_rev_id`
parity (Myers vs the production cache), ~14% of words would appear
under the wrong author. After switching to Differ we hit 100%
all-fields on every captured-history fixture (en/24544 Photosynthesis,
en/79023819 Israel-Hamas war, simple/27263 Wikipedia, zh/1686258 中国).

Trip-wire: the parity ratchet currently bakes Python-Differ output as
ground truth (`scripts/python_replay.py`). Any regression below 100%
on captured fixtures is a bug in our port, not an algorithmic gap.

## What's NOT settled

The unanswered question is whether Ratcliff/Obershelp produces
attribution that a human reviewer would judge as **more correct** than
Myers (true LCS), or vice versa.

Both algorithms have plausible arguments:

- **Myers (LCS-optimal):** maximizes the number of preserved tokens
  across a revision. If a token didn't change, attribute it to whoever
  introduced it. More tokens get historical-attribution lineage; fewer
  get "fresh allocation" against the editing revision.
- **Differ (Ratcliff/Obershelp greedy contiguous):** matches how
  humans edit — they replace whole phrases, they don't scatter tokens
  across unrelated contexts. LCS-optimal can produce spurious
  per-token matches across totally different sentences (e.g.,
  matching "the" or "a" between paragraphs that have no semantic
  relationship). Contiguous matching is more conservative: if the
  token isn't appearing in the same neighborhood, treat it as a
  fresh addition.

The numbers we have so far:

- Structural state (paragraphs_ht, sentences_ht, spam_ids) matches
  byte-for-byte between Rust-Myers and Python-Differ. So the question
  is purely about token-level matching.
- Myers allocates ~3-4% fewer tokens than Differ across long histories
  (simple/27263 and en/24544). Concretely: Myers identifies more
  matching tokens, producing fewer "fresh adds."
- The token-level divergence concentrates on **vandalism-and-revert
  pairs** — adversarial revisions with no clean structure for either
  algorithm to anchor on. Outside vandalism the two algorithms agree
  on most positions.

That last point is interesting because the per-fixture all-fields
parity score under Myers (~80% on Photosynthesis, ~53% on
simple/27263) was driven heavily by simple Wikipedia's vandalism rate
— a *high*-vandalism wiki. Less-adversarial article histories likely
have a much smaller gap between the two algorithms.

## How to actually evaluate "which is better"

The honest answer to "which produces better attribution" requires
human-in-the-loop judgment on specific divergences. Methodology
sketch:

1. Run both algorithms on an article history (the parity binary
   already has both code paths reachable).
2. Diff the resulting per-token attribution streams. For each token
   where the two disagree, output:
   - The token value and its position.
   - The Myers-assigned `o_rev_id` / editor + the surrounding
     sentence context in that revision.
   - The Differ-assigned `o_rev_id` / editor + the surrounding
     sentence context in that revision.
3. Sample N divergent tokens. For each, a human (Sage or a domain
   expert) judges which attribution "feels right" given the editing
   intent visible in the diffs.

Cost: dozens of judgment calls per article × multiple articles ×
multiple editors of varying styles. Probably a couple of days of
focused work for a strong signal.

Outcome possibilities:
- **Myers wins clearly →** port consumers to expect Myers output and
  ship that. The 100%-parity-with-Python target becomes obsolete.
- **Differ wins clearly →** the current state is right; remove Myers
  from `diff.rs` for clarity.
- **It's a wash / case-by-case →** the parity-with-Python tiebreaker
  was the right call. Keep Myers as a debugging tool.

## What to keep operational for the revisit

- `crates/wikiwho-attribute/src/diff.rs` — Myers diff. Compiled, not
  called.
- `crates/wikiwho-attribute/src/differ.rs` — Differ port. The active
  matcher.
- The parity infrastructure (`scripts/python_replay.py` +
  `--python-replay` flag) — currently uses Differ as ground truth.
  Could be extended with a "Myers as alternate ground truth" mode if
  the revisit happens.
- `notes/2026-05-23-python-replay.md` — captures the structural
  evidence (spam_ids byte-identical, ht sizes within 0.3%, token
  allocations differ by 3-4%) that scoped this gap.

## Trigger for a revisit

- Some consumer (Dashboard, XTools, etc.) reports that attributions
  are "wrong" in a way that the Python reference is also wrong about.
  That would be evidence that humans don't actually prefer the
  Ratcliff/Obershelp matching either.
- A research project explicitly cares about which algorithm is more
  faithful to actual editor intent (e.g., a study of student
  contributions to Wikipedia).
- Performance: if Differ ends up materially slower than Myers on
  Obama-class articles during the storage-and-server work, we have a
  forcing function to look at how often Myers would be "close enough"
  and switch on the fast path.

Until then, parity-with-Python wins — this note is the marker that
the question isn't fully closed.
