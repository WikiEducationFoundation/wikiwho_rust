# 2026-05-23 (part 13) — WhoColor parity: Python ground truth via --python-replay

**Goal:** lift the Python WhoColor flow into `scripts/python_replay.
py`, cache it per fixture, and make `whocolor-parity --python-replay`
the algorithm-correctness gate (analog of `parity-check --python-
replay`). This separates real port bugs from prod-cache drift —
last session showed the WhoColor headline was 87.33% vs prod-cache,
with ja/4821051 and en/62750956 as the dominant outliers, but
parity-check `--python-replay` already showed both fixtures at 100%
vs Python on the underlying algorithm. We needed the same separation
on the WhoColor data layer.

**Parity (whocolor-parity --python-replay, 20 fixtures):**

| Metric | Result |
|---|---|
| **Tokens all-fields passing** | **1,149,257 / 1,149,257 (100.00%)** |
| str | 100.00% |
| o_rev_id | 100.00% |
| in | 100.00% |
| out | 100.00% |
| conflict_score | 100.00% |
| class_name | 100.00% |
| **Revisions passing** | **220,227 / 220,227 (100.00%)** |
| `biggest_conflict_score` matches | **20 / 20** |
| Per-fixture all-pass | **20 / 20** |

Every fixture, every field, byte-for-byte. The two outliers from
the prod-cache run (`ja/4821051 日本` and `en/62750956
COVID-19_pandemic`) now match the Python source-of-truth identically,
which is what `parity-check --full-history --python-replay` had
already shown for the algorithm layer. The WhoColor data layer
inherits its correctness from the underlying `Article` state, so
this 100% wasn't surprising — but having it characterized end-to-end
in the same comparator format is the load-bearing piece for any
future ratchet work.

**Prod-cache baseline (unchanged):** `whocolor-parity` (no flags)
still reports **974,088 / 1,115,391 (87.33%)** with the same two
outliers contributing most of the gap. No regression — the gap is
entirely accumulated prod-cache state, not a port bug.

**Counts before → after:**

- Total workspace tests: **281 → 281** (no algorithm changes — the
  scope was just adding the python-replay comparison path).
- Clippy clean with `-D warnings --all-targets`.
- 20 new cache files at `parity-fixtures/<lang>/<page_id>/
  <rev_id>/python_whocolor_replay.json`. Gitignored along with the
  rest of `parity-fixtures/`.

**Done:**

- **`scripts/python_replay.py`** — new `--whocolor` flag. When set,
  the summary dict gains a `whocolor` top-level key containing
  wire-shape `tokens` (six-tuple, no `age`), `revisions` dict, and
  `biggest_conflict_score`. The `get_whocolor_data` logic is
  inlined (not imported) because `wikiwho_simple.py` is Django-
  coupled; we faithfully port the per-token loop from
  `wikiwho_simple.py:362-414`, drop `age` (which depends on
  `datetime.now()`), and pre-apply the md5 anon-hash from
  `whocolor/handler.py:108-112` so the output is comparator-ready.

- **`crates/wikiwho-parity/src/bin/whocolor_parity.rs`** — new
  `--python-replay` and `--refresh-python` flags. The existing
  `ProdWhoColor` struct is renamed to `ComparisonSource` (same shape
  on the wire so the comparator code is unchanged); a new
  `load_python_whocolor` regenerates the cache at
  `<fixture>/python_whocolor_replay.json` via subprocess if absent
  or `--refresh-python` is set. `walk_fixtures` gains a `python_mode`
  flag so the gate on `whocolor.json` is skipped in python mode (we
  only need `history.jsonl` + `meta.json`). `process_one`
  branches on `args.python_replay` for the source; `--check-age` is
  a no-op in python mode (the cache deliberately doesn't carry
  age data).

- Diff-message terminology unified with `parity-check`'s usage:
  user-facing "prod=" → "expected=" so the source label in the
  header (`vs python` / `vs prod-cache`) is the only thing
  identifying the reference.

**Design notes / issues encountered:**

- *Separate cache file rather than extending `python_replay.json`.*
  Parity-check and whocolor-parity both invoke `python_replay.py`,
  but the analyse_article work is the bulk and we re-do it for both.
  Combining caches would halve Python wall-time on a corpus that
  runs both. Kept separate for now because (a) the wikiwho_replay
  flow is the rarely-rerun part, (b) keeping the cache shapes
  decoupled means parity-check users don't need to know about
  whocolor and vice versa, and (c) `--refresh-python` semantics
  stay clean. Worth revisiting if Python-replay wall-time becomes
  a friction point on a much larger corpus.

- *Why omit `age` entirely from the Python output instead of
  passing `capture_now` as an arg?* Production whocolor.json has
  `age` baked in because it was captured at one wall-clock moment;
  inferring `capture_now` from that is what `--check-age` does in
  prod mode. For python_replay there's no captured moment — the
  Python script could take `--capture-now <unix>` and compute age
  with it, but that adds an arg and makes the cache key effectively
  include the timestamp. Simpler to drop `age` from the cache and
  document that `--check-age` is prod-only. If we want
  reproducible age comparisons later, we can add the arg without
  schema-changing the cache (it'd just gain a 7th tuple element).

- *Six-tuple vs seven-tuple decode path.* The comparator reads token
  fields by `pt.get(i)`. In python-replay mode `pt.get(6)` returns
  None (no age), which the score function maps to `false`; that's
  fine because `check_age` is forced off in python mode.
  `ComparisonSource.success` defaults to `true` via serde so the
  Python cache doesn't have to emit it.

- *Order of `revisions` dict.* Python 3.7+ dict preserves insertion
  order on serialization. Rust's `BTreeMap` re-sorts on
  deserialization. The comparator only looks up by `rev_id`, so
  ordering doesn't affect the result — but if a future addition
  needs the *order* (e.g. to verify `ordered_revisions` shape), it
  would need a list-of-pairs structure instead.

**Queued decisions:** none new this session.

**Next session likely starts with:**

The four consumer endpoints serve byte-identical wire format, both
parity binaries hit 100% vs Python on the full corpus, and prod-
cache divergence is fully characterized as drift rather than port
bugs. Remaining items, in priority order:

1. **Persist paragraph + sentence arenas for resume-from-disk.**
   The longstanding non-blocking decision from part 6. Still
   required before live EventStreams ingest can do incremental
   updates rather than full rewrites. Recommendation B in
   `notes/decisions-needed.md`.

2. **Ephemeral non-mainspace endpoint (API.md §9).** Lowest
   priority — no downstream consumer uses it. Fills out the
   wire-format surface.

3. **Deploy path planning.** With all four wire-format endpoints
   ready and parity numbers locked at 100% vs Python, the cutover
   plan (PLAN.md §"Migration") becomes the gating decision.
   Easiest first (small wikis), English last.

Recommendation: **1 (paragraph/sentence persistence)**, because the
live-update story can't ship without it and the storage decision is
the last unresolved fork in the storage layer.
