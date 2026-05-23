# 2026-05-23 (part 5) — wikiwho-mwclient + broaden parity corpus

**Goal:** with full-history parity at 100% on the 4 captured fixtures
(see `notes/2026-05-23-differ-port.md`), broaden the parity corpus
and start landing the non-algorithm crates from PLAN.md §9. The
recommended next step from the prior note was **1 then 3 or 2** —
capture more histories first, then mwclient/storage.

**Parity (full-history vs prod-cache):**

| Fixture | Revs | Spam | Parity (all-fields) |
|---|---|---|---|
| ar/4287 القاهرة (new) | 2987 | 5 | **100.00%** |
| de/2552494 Berlin (new) | 10000 | 44 | **100.00%** |
| en/22989 Paris (new) | 20453 | 214 | **100.00%** |
| en/2731583 Adolf_Hitler (new) | 28417 | 597 | **100.00%** |
| en/46827 Jesse_Owens (new) | 6461 | 88 | **100.00%** |
| en/24544 Photosynthesis | 5495 | 103 | **100.00%** |
| en/79023819 Israel–Hamas war | 2 | 0 | **100.00%** |
| simple/27263 Wikipedia | 3783 | 230 | 99.47% (vs prod) / 100% (vs python) |
| zh/1686258 中国 | 7 | 0 | **100.00%** |
| **Aggregate (9 fixtures)** | 77 605 revs | 1281 spam | **9 / 9 at 100% vs python** |

Aborted on the 30k-rev cap (Obama-class, queued for a later overnight
job with a higher cap):
- en/Jesus, en/Wikipedia, en/Barack_Obama, possibly en/COVID-19_pandemic,
  fr/Paris.

**Done:**

- **`crates/wikiwho-mwclient/`** — new async crate.
  - `MwClient` + `MwClientBuilder` (reqwest + tokio + rustls).
    Wraps `https://{lang}.wikipedia.org/w/api.php` with the same
    parameter shape as `../wikiwho_api/api/handler.py:461`
    (`rvprop=ids|timestamp|user|userid|comment|flags|sha1|content`,
    `rvlimit=max`, `formatversion=2`, `rvslots=main`, `rvdir=newer`).
  - Retry policy ports `handler.py:155-205`: 429 with `Retry-After`
    honored against a budget cap; 5xx with exponential backoff.
    Polite inter-batch delay (default 300 ms).
  - `RevisionFetcher` async paginator with a `next_batch()` loop.
    Supports resuming from a saved `rvcontinue` token (the path the
    eventual storage layer will use for catch-up).
  - `bin/capture-history.rs` — Rust replacement for
    `scripts/capture_history.py`. JSONL output is shape-identical
    (round-trip test in `tests/parse_revision.rs` proves it). The
    Python script still ships; switching the parity workflow off it
    is non-blocking.

- **`crates/wikiwho-attribute/src/response.rs`** — new module.
  - `build_rev_content(article, &[rev_id], params) -> Result<RevContentResponse, RevContentError>`
    mirrors `wikiwho_simple.py:23-71` (`get_revision_content`).
  - Wire format from API.md §1-6: `{article_title, page_id, success,
    message, revisions[{<rev_id_str>: {editor, time, tokens}}]}`.
  - Token field order matches the Python reference (str, o_rev_id,
    editor, token_id, in, out). Required enabling serde_json's
    `preserve_order` feature.
  - Half-open range semantics for the two-rev-id case
    (`wikiwho_simple.py:43-47`).
  - Error envelope keyed `"Error"` (capital E, per API.md).
  - End-to-end test (`tests/response_against_fixture.rs`) replays
    zh/1686258 and en/79023819 histories, builds the wire response,
    structurally diffs against the captured `python_replay.json`.
    Both pass.

- **Captures broaden by 5 fixtures.** ar/Cairo, de/Berlin, en/Paris,
  en/Adolf_Hitler, en/Jesse_Owens all captured cleanly under the 30 k
  cap. Five more aborted because their histories exceed the cap;
  queueing a follow-up pass with a higher cap is non-blocking.

**Issues encountered + resolutions:**

- *anyhow was in `[dev-dependencies]`.* Binaries can't use dev-deps;
  moved `anyhow` to regular dependencies for `wikiwho-mwclient`.
- *Tests had `assert_eq!(r.minor, false)`.* Clippy's
  `bool_assert_comparison` fires; switched to `assert!(!r.minor)`.
- *Token field order was alphabetical.* `serde_json::Map` defaults to
  `BTreeMap` which sorts keys. Enabled `preserve_order` in the
  workspace `serde_json` dep — pulls in `indexmap` but keeps the
  wire-format byte-equivalent to Python's output.
- *Round-trip JSONL test caught a tiny serde quirk.* The Python script
  writes `"k": "v"` (space after colon); serde writes `"k":"v"`. Both
  are valid JSON; parity-check accepts both. Documented in the
  capture-history binary docstring.

**Counts:**
- wikiwho-attribute: 108 lib tests + 2 fixture integration tests +
  1 differ-python-parity test, all passing.
- wikiwho-mwclient: 8 parsing tests, all passing.
- Clippy clean across the workspace with `-D warnings`.
- Build clean (dev + release).

**Performance check:**
- Full-history replay of the 5 newly-captured fixtures (~67 k revs
  total) ran end-to-end in **6 min 8 s** release-mode (vs prod-cache
  comparison). That's roughly 5.5 ms per revision, well under any
  reasonable latency budget for the lazy-fetch path the server will
  drive.

**Updated decisions:**

- The "capture more histories" recommendation from
  `notes/2026-05-23-differ-port.md` is essentially closed at this
  cap level. Five small-to-medium-sized articles broadened the
  corpus by ~245 % (31 885 → 77 605 tokens compared). All hit 100 %.

**Storage calibration (mid-session conversation, separate commit):**

While the captures were running, Sage and I cross-checked STORAGE.md
§5's hand-waved estimates against production. The "18 KB compressed
average article × 7 M articles ≈ 250 GB for enwiki" target was off by
~10×: en is actually **1.88 TB across 8.18 M articles, avg 224 KB
compressed**. Across all three cinder volumes prod uses ~7 TB / 14.7
TB allocated. Per-revision cost in production is **0.5-0.7 KB
compressed** (calibrated against the 5 captured en fixtures).

STORAGE.md §5 is fully rewritten with the calibrated numbers and a
per-fixture rewrite-target table. Two non-blocking follow-ups queued
in `notes/decisions-needed.md`:

- Compress remaining legacy raw `.p` files in `/pickles/en` in place
  (sample first to size the win).
- Quantify the per-pickle-attribute byte breakdown when storage format
  tuning starts.

Net: the rewrite has ~2× headroom even worst case; no storage
request needed for cutover.

**Next session likely starts with:**

The non-algorithm pieces from PLAN.md §9 still missing:

1. **`wikiwho-storage`** — blob format read/write/append/compact
   (per STORAGE.md). The algorithm output is stable; the storage
   layer is the next big piece. Strategy B (persist hash tables) is
   already resolved; initial implementation is wholesale-rewrite per
   STORAGE.md §4. This is multi-session work.

2. **Higher-cap capture pass for Obama-class articles.** Run
   `capture-history --max-revs 100000` (or no cap) on the five
   aborted fixtures. Most are 30-60 k revs. Likely a half-hour
   overnight job. Confirms parity holds on the largest single-article
   histories we have.

3. **`wikiwho-server` (axum scaffold).** Now that
   `build_rev_content` exists, an axum handler is straightforward.
   Useful for end-to-end testing against actual consumer code paths,
   though it has no persistence until (1) lands. Could be a fun
   sanity-check session.

Recommendation: **1**. Storage is the longest path to first ship and
unblocks everything else. (2) can be queued for an overnight cron.
(3) is most useful after (1).
