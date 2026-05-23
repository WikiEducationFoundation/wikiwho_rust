# 2026-05-23 (part 8) — rev_id → page_id sidecar for endpoint 1

**Goal:** close out the recommended next step from part 7: stand up
the per-language `rev_id_index.bin` sidecar so endpoint 1
(`/{lang}/api/v1.0.0-beta/rev_content/rev_id/{rev_id}/`, used by
Impact Visualizer) resolves to a real article instead of the
placeholder 408 envelope.

Picked option A from `notes/decisions-needed.md` (per-language
sidecar populated by the writer, with a one-shot rebuild binary for
existing trees) — that entry is now stamped Resolved (part 8).

**Parity:** N/A — no algorithm changes. All 108 `wikiwho-attribute`
+ 76 `wikiwho-storage` lib + 5 storage round-trip + 11 server
integration + 8 server lib + 8 mwclient + 3 parity tests still pass
(219 total workspace tests vs 191 before, +28 new).

**Done:**

- **`crates/wikiwho-storage/src/rev_id_index.rs`** — new module.
  24-byte header + sorted `(u64 rev_id, u64 page_id)` body + 8-byte
  CRC trailer; magic bytes `WRIX`/`XIRW`. Lookups are
  `binary_search_by_key` over the in-memory entries; writes go
  through a tmp-file + rename to avoid partial-file corruption.
  Public surface:
  - `RevIdIndex::load(volume, lang)` — returns empty index if the
    file is missing (so older storage trees keep working until
    `rebuild_rev_index` runs).
  - `RevIdIndex::lookup(rev_id) -> Option<u64>`
  - `RevIdIndex::replace_article(page_id, &[rev_id]) -> Result<()>` —
    drops prior entries for `page_id`, adds the new ones, and
    refuses to merge if a rev_id collides with a *different*
    page_id (a same-page duplicate is silently de-duped).
  - `RevIdIndex::save(volume, lang)` — atomic write.
  - `RevIdIndex::update_for_article(volume, lang, page_id, rev_ids)` —
    convenience load→replace→save for the writer.
  - 17 unit tests in module: round-trip, empty, lazy-load, sort
    invariants, atomic-rename, parser hardening (bad head/tail
    magic, CRC corruption, truncation, unsorted entries).

- **`crates/wikiwho-storage/src/writer.rs`** — hook. After each
  successful `write_article`, the per-language sidecar is updated
  via `RevIdIndex::update_for_article`. Article files land on disk
  *before* the sidecar publishes their rev_ids so a concurrent
  reader chasing a freshly-indexed rev_id never reaches a
  half-written article dir. 3 new tests cover (a) first-write
  populates the index, (b) two-article merge, (c) re-write of one
  article correctly replaces its rev_ids without duplicating them.

- **`crates/wikiwho-storage/src/revisions.rs`** — adds
  `RevisionsIndex::rev_ids_sorted()` — a cheap walk of just the
  index table, no varint decoding of token bodies. Used by the
  rebuild binary to extract rev_ids without paying for full revision
  parse.

- **`crates/wikiwho-storage/src/rebuild.rs`** + binary
  `crates/wikiwho-storage/src/bin/rebuild_rev_index.rs`. The library
  exposes `discover_languages` and `rebuild_one_language` (returning
  a `RebuildStats { articles, entries }`); the binary is a thin CLI
  wrapper:
  ```
  rebuild_rev_index <volume>            # all languages
  rebuild_rev_index <volume> <language> # one language
  ```
  Walks the shard tree, reads `meta.json` (for `page_id`) and the
  rev-id table at the tail of `revisions.bin` for each article,
  then writes a fresh sidecar. 5 tests cover the happy path, empty
  language dirs, missing language dirs, `.git`/non-dir filtering,
  and detection of duplicate rev_ids across articles.

- **`crates/wikiwho-server/src/state.rs`** — `AppState` now caches a
  per-language `Arc<RevIdIndex>` next to the existing title-index
  cache. New methods:
  - `resolve_rev_id(language, rev_id) -> Option<u64>` (the read
    path).
  - `refresh_rev_id_index(language) -> io::Result<()>` (test helper
    + future ingest helper).
  Storage-layer load errors (CRC mismatch / bad magic) become
  `io::ErrorKind::InvalidData` at the `AppState` boundary so the
  rest of the server doesn't need to import `StorageError`.

- **`crates/wikiwho-server/src/handlers/rev_content.rs`** — endpoint
  1 now does `resolve_rev_id` → `handle_page_id(.., Some(rev_id))`
  instead of always returning 408. The 408 path remains for the
  unknown-rev_id case so Impact Visualizer's existing retry policy
  still fires.

- **`crates/wikiwho-server/tests/rev_content_round_trip.rs`** — 3
  new integration tests:
  - `rev_content_by_rev_id_round_trip`: spin up server, write zh
    fixture, hit endpoint 1, assert byte-identical JSON against
    `build_rev_content`.
  - `rev_content_by_rev_id_unknown_returns_still_processing`: known
    article, unknown rev_id → 408 + `Info` envelope.
  - `rev_content_by_rev_id_uses_lazy_index_load`: no explicit
    `refresh_rev_id_index` call; first request must build the cache
    lazily.

**Issues encountered + resolutions:**

- *Cross-article duplicate-rev_id detection inside `replace_article`*
  is strict: if rev 500 is in both page 7 and page 8 (which can
  happen if a corrupt sidecar makes it past the writer), the
  rebuilder fails with `duplicate rev_id 500 ...`. The first test
  for this fired panics inside `unwrap_err()` because the writer
  was happily merging a *fresh* sidecar — once the sidecar was
  removed between writes, the in-writer collision check no longer
  fired. Resolved by simplifying the test to set up the corrupted
  state directly (write, delete sidecar, write, delete sidecar,
  rebuild) and removing the unwrap_err in the middle.

- *Underscore in URL u64 path param.* The "unknown rev_id" test
  initially used `rev_id/9_000_000_000/`, which axum routed but
  the path-deserializer rejected with HTTP 400 (`9_000_000_000`
  isn't a valid `u64`). Replaced with bare digits.

- *Where to surface `iter().copied().collect()`.* Clippy nudged on
  three call sites; switched to `to_vec()` / passed
  `&article.ordered_revisions` directly (it's already `&[u64]`).

**Counts:**
- wikiwho-storage: 76 lib + 5 round-trip = 81 storage tests.
- wikiwho-server: 8 lib + 11 integration = 19 server tests.
- wikiwho-attribute / mwclient / parity unchanged.
- **Total workspace: 219 tests passing, 0 failed.** (+28 vs part 7.)
- Clippy clean across the workspace with `-D warnings`.

**Performance check:**
- Sidecar lookup: `binary_search_by_key` over an in-memory `Vec`.
  For the test fixture (1-5 articles × 5500 revs each) the file
  is under 100 KB and the lookup is O(log N) on a hot slice.
- Sidecar write: read existing file → mutate → atomic-rename
  rewrite. At fixture scale (<100 KB) this is microseconds; at
  en-scale (~700 M entries → ~11 GB) it would dominate write
  latency. That's the documented STORAGE.md §4 trajectory:
  delta-log will replace this for production en. Tracked under the
  existing "STORAGE.md §4 Strategy B / append-log" follow-up.
- Rebuild on a 5-article tree: sub-ms (no revision-body decoding).

**Queued decisions:** none new this session. The previously-queued
"rev_id → page_id index" entry is now stamped Resolved.

**Next session likely starts with:**

Two roughly equal-priority paths, both inherited from part 7's
recommendations:

1. **Cache-miss path: fetch arbitrary articles via
   `wikiwho-mwclient`, run the algorithm, persist, then serve.**
   This is what lets the server respond to articles that aren't
   already on disk — the path needed to point Dashboard / XTools /
   WWT at a running instance for real testing. Requires plumbing
   the existing mwclient history-fetching into the request handler
   with a timeout policy and probably an in-flight de-dup map.

2. **Persist paragraph + sentence arenas for resume-from-disk.**
   The longer-standing decision from part 6
   (`notes/decisions-needed.md` — "how to persist paragraph +
   sentence arenas"). Mirrors the token-arena pattern; multi-session
   work but closes Strategy B's actual write path so
   `analyse_revision` can continue from a snapshot instead of a
   cold rebuild.

Recommendation: **1** as the next session. Endpoint 1 is now end-to-end
for articles already in storage; (1) makes the server useful for
arbitrary articles — that's what unblocks pointing real consumers at a
running instance.
