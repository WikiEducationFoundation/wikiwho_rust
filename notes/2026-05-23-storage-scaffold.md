# 2026-05-23 (part 6) — wikiwho-storage scaffold + read-side round-trip

**Goal:** land the recommended next step from the prior note (part 5):
the `wikiwho-storage` crate, covering the read path
(`strings.bin` + `tokens.bin` + `revisions.bin` + `hashtables.bin` +
`meta.json`) and a byte-identical round-trip against captured-history
fixtures. This is the long-pole crate from PLAN.md §9 and unblocks the
server + ingest crates downstream.

**Parity:** N/A — no algorithm changes. The single edit to the
algorithm crate (a new `token_sequence_override: Option<Vec<TokenId>>`
field on `Revision`) is a bridge for the storage reader and is `None`
in the algorithm path, so `iter_rev_tokens` behaves identically when
populated by the cascade. All 108 `wikiwho-attribute` tests still pass.

**Done:**

- **`crates/wikiwho-storage/` — new crate.** 51 lib tests + 5
  integration round-trip tests, all passing. Clippy clean across the
  workspace with `-D warnings`.

  Modules:
  - `codec` — varint + zigzag LEB128, big-endian fixed-width
    readers/writers, CRC32 (IEEE) via the `crc32fast` crate.
  - `strings` — `strings.bin` (per-article interned UTF-8 strings,
    `WWST` / `TSWW` magic, random-access `StringsIndex`).
  - `tokens` — `tokens.bin` (per-lifetime-token records, varint+
    zigzag delta-encoded; `WWTK` / `KTWW` magic).
  - `revisions` — `revisions.bin` (per-rev token sequence + metadata,
    `WWRV` / `VRWW` magic, binary-search index by rev_id, random-access
    `RevisionsIndex`).
  - `hashtables` — `hashtables.bin` minimal stub (hash strings +
    occurrence counts only; full back-references deferred — see
    decision queue).
  - `meta` — `meta.json` shape (schema_version, page_id, language,
    title, last_processed_revid, etc.).
  - `layout` — two-level sharding (`page_id // 1_000_000 / page_id // 1000`)
    per STORAGE.md §1.
  - `writer::write_article` — projects an in-memory `Article` into
    all five files atop the sharded layout.
  - `reader::SnapshotReader` — inverse, hydrates a partial `Article`
    that has enough state to serve `rev_content` via
    `wikiwho-attribute`'s existing `build_rev_content` (no
    refactoring of the response builder).

- **Bridge to the algorithm crate.** Added
  `Revision::token_sequence_override: Option<Vec<TokenId>>` and a
  fast-path branch at the top of `iter_rev_tokens`. When the storage
  reader populates the override, `build_rev_content` works on a
  loaded `Article` whose paragraph + sentence arenas are empty.

- **Round-trip test against captured histories.**
  `tests/round_trip_history.rs` exercises the full path: parse
  `history.jsonl`, run through the algorithm, persist via
  `write_article`, reload via `SnapshotReader`, and compare
  `build_rev_content` JSON byte-for-byte. Covers:

  | Fixture | Revs | Result |
  |---|---|---|
  | zh/1686258 中国 | 7 | **byte-identical** |
  | en/79023819 Israel–Hamas war | 2 | **byte-identical** |
  | simple/27263 Wikipedia | 3 783 | **byte-identical** |
  | en/24544 Photosynthesis | 5 495 | **byte-identical** |
  | + `round_trip_every_revision_zh` — every rev_id in zh fixture | 7 | **byte-identical per-rev** |

  The release-mode suite runs in ~20 s end-to-end.

**Issues encountered + resolutions:**

- *Photosynthesis (5 495 revs) first failed.* The writer rejected a
  token with `last_rev_id 39174 < origin_rev_id 275452`. Investigation
  showed Wikipedia's pre-2002 enwiki revs have rev_ids out of
  chronological order: rev 275452 (Nov 2001) precedes rev 38939
  (Feb 2002) in time. The algorithm processes revs in **timestamp
  order**, so rev-id chains can move backward in numeric space.
  Fixed by switching every rev-id field in `tokens.bin` (origin,
  last, inbound, outbound chains) from unsigned monotonic deltas to
  signed zigzag deltas. Two unit tests added to lock in the
  non-monotonic case. Module docs updated with the rationale.

- *Format deviations from STORAGE.md §2.4*, all documented inline
  in `revisions.rs`:
  - Timestamp stored as length-prefixed UTF-8 (not i64 unix). The
    wire format echoes the MW-API string byte-for-byte; persisting
    the exact bytes avoids any round-trip drift risk for ~1.5 % file
    bloat on representative fixtures.
  - Editor stored as length-prefixed UTF-8 (not editor_kind +
    editor_id varint pair). Same reasoning.
  - Per-revision token sequence stored explicitly (not implied by
    a paragraph→sentence→word walk). This is what lets the read
    path serve `rev_content` without persisting paragraphs/sentences
    yet — but it forks STORAGE.md §4's `hashtables.bin` design (see
    queued decision below).

- *Clippy nits along the way:* useless `format!()` calls collapsed
  to `.to_string()`; `field_reassign_with_default` flagged on the
  reader, replaced with struct-literal construction.

**Counts:**
- wikiwho-storage: 51 lib tests + 5 integration tests, all passing.
- wikiwho-attribute: 108 lib + 1 differ-python + 2 response fixture
  tests still passing (no algorithm-side changes).
- wikiwho-mwclient: 8 parsing tests still passing.
- Total workspace: 175 tests passing, 0 failed.
- Clippy clean across the workspace with `-D warnings`.
- Build clean (dev + release).

**Performance check:**
- Photosynthesis (5 495 revs) round-trip — replay through algorithm,
  persist, reload, serve `rev_content` — in **~15 s release-mode**.
  The persist step alone is sub-second; most time is in the
  algorithm replay (independently measured at ~5 ms/rev).
- The on-disk format is byte-equivalent on every fixture we tested;
  per-fixture `--storage-size` benchmarking is queued for the next
  storage session.

**Queued decisions:**
- **How to persist paragraph + sentence arenas for resume-from-disk.**
  The current storage layer can read back enough to serve
  `rev_content`, but **cannot resume the algorithm from disk** —
  `paragraphs.bin` / `sentences.bin` don't exist. Three options
  surfaced (inline-in-revisions, separate arena files, or skip and
  rebuild). Recommendation: separate arena files (mirrors the
  token-arena pattern, lets the read path keep its cheap shortcut).
  See `notes/decisions-needed.md`.

**Next session likely starts with:**

Two roughly equal-priority paths, both unblocked by this session:

1. **Resolve the persist-paragraphs-and-sentences fork above and
   land the live-update write path.** Adds `paragraphs.bin` +
   `sentences.bin`, extends `hashtables.bin` with arena back-refs,
   and proves the round-trip on a "load → apply one new rev → save"
   loop. Multi-session work but closes Strategy B's actual write
   path.

2. **`wikiwho-server` (axum scaffold).** The read-side response
   builder works end-to-end via the storage layer; an axum handler
   exposing the wire format on a real port is straightforward and
   useful for testing against actual downstream consumer code paths
   (Dashboard, XTools). Doesn't depend on the live-update write
   path — the server can be bootstrap-then-read-only at first.

Recommendation: **2** as the next session. (1) is bigger, and (2)
gets the wire format onto a port faster — letting us exercise actual
HTTP request shapes against Dashboard / XTools / WhoWroteThat
without waiting for live-update plumbing.
