# 2026-05-23 (part 7) — wikiwho-server scaffold + read-side HTTP

**Goal:** land the recommended next step from part 6: an axum-based
`wikiwho-server` crate exposing the wire format documented in API.md
on top of `wikiwho-storage`. The server is bootstrap-then-read-only;
the cache-miss + live-update paths are follow-ups.

**Parity:** N/A — no algorithm changes. All 108 `wikiwho-attribute`
tests + 51 `wikiwho-storage` tests + 5 storage round-trip tests still
pass. The server crate adds 8 lib tests + 8 integration tests.

**Done:**

- **`crates/wikiwho-server/` — new crate.** axum 0.8 / tower-http /
  tokio. Workspace clippy clean with `-D warnings`. Binary at
  `bin/wikiwho_server.rs` boots from env vars (`WIKIWHO_BIND`,
  `WIKIWHO_STORAGE`); smoke-launched and serves the expected 408
  envelope on cold storage.

  Modules:
  - `error` — `ServerError` variants (ArticleNotFound, RevisionNotFound,
    TitleNotFound, Storage, Io, Json). HTTP mapping in the route layer.
  - `params` — `RawTokenParams` deserialized from query string;
    parity with `api/views.py:188 (get_parameters)` — only literal
    `"true"` enables a field, capital `True` / `1` / etc. all read as
    false. `in`/`out` mapped to `inbound`/`outbound` via serde renames.
  - `index` — in-memory `title -> page_id` index built by walking the
    sharded storage tree. Lazy on first lookup per language.
  - `state` — `AppState` (Arc-shared) with storage root + RwLock'd
    title-index cache, plus a `refresh_title_index` entry-point for
    tests.
  - `handlers::rev_content` — endpoints 1-6 from API.md. Title
    normalization (space→underscore) applied before lookup. Errors
    routed to the right status (200/400/408/500) per API.md
    §"Error codes".
  - `routes` — axum router with both `v1.0.0` and `v1.0.0-beta`
    accepted as version segments. CORS allow-any (read-only public
    data; matches `CORS_ORIGIN_ALLOW_ALL` in current production).

- **End-to-end integration test** (`tests/rev_content_round_trip.rs`).
  Eight cases, all passing:

  | Test | Endpoint | Result |
  |---|---|---|
  | rev_content_by_page_id_round_trip | §4 | byte-identical JSON |
  | rev_content_by_title_round_trip | §2 | byte-identical JSON |
  | rev_content_by_title_and_rev_round_trip | §3 | byte-identical JSON |
  | version_alias_v1_0_0_works | §"Versioning" | HTTP 200 |
  | latest_rev_content_alias_works | §5 | HTTP 200 |
  | missing_article_returns_still_processing | §1 envelope | HTTP 408 + `Info` field |
  | missing_title_returns_still_processing | §"Error codes" | HTTP 408 + `Info` field |
  | rev_content_token_field_filtering | §1 query params | str-only when no params |

  Each spins up the server in-process on an ephemeral port via
  `axum::serve`, writes the zh/1686258 中国 fixture (7 revs, 100 %
  parity) through `write_article`, hits the route via `reqwest`, and
  asserts byte-identical JSON against `build_rev_content` output.

**Issues encountered + resolutions:**

- *Endpoint 1 (rev_id-only)* requires a `rev_id → page_id` index
  that doesn't exist yet in the storage layer. The Python service
  uses Postgres for this; the rewrite needs an equivalent. Chose to
  ship the handler with a "still processing" (408) placeholder so
  the route resolves and Impact Visualizer's existing retry logic
  fires; filed `notes/decisions-needed.md` with three design
  options (sidecar file / lazy scan / rev-id range in meta.json).
  Recommendation is **A: per-language `rev_id_index.bin` sidecar.**

- *Title normalization.* MW Action API returns titles with
  underscores; URL paths can use either. Added a small
  `normalize_title` helper that replaces spaces with underscores
  before index lookup. Doesn't try to handle other Wikipedia title
  normalizations (capitalization, namespace canonicalization,
  Unicode NFC) — those happen upstream in `wikiwho-mwclient` when
  we fetch revisions.

- *Lazy title-index race.* First request after server start triggers
  a directory walk on the language's storage dir; concurrent
  requests for the same language could each build their own copy.
  The `build_and_cache_index` path uses a `RwLock`-write entry
  guard: whoever wins the lock keeps the existing entry if one was
  inserted while building. Fine for the scaffold; revisit if we ever
  see lock contention in production traces.

**Counts:**
- wikiwho-server: 8 lib tests + 8 integration tests, all passing.
- wikiwho-attribute: 108 + 1 + 2 = 111 tests still passing.
- wikiwho-storage: 51 + 5 = 56 tests still passing.
- wikiwho-mwclient: 8 parsing tests still passing.
- **Total workspace: 191 tests passing, 0 failed.**
- Clippy clean across the workspace with `-D warnings`.

**Performance check:**
- Cold-start title-index build is a directory walk; for the test
  fixture sizes (1-5 articles) it's sub-ms. Per-request overhead
  is the storage `SnapshotReader::open` cost (load + parse 4 binary
  files), measured at ~50 ms for the zh fixture in debug build —
  not measured in release. Will benchmark once we have a multi-rev
  fixture loaded into a running server.

**Queued decisions:**
- `notes/decisions-needed.md` updated with the rev_id → page_id
  index question (sidecar file vs scan vs range-in-meta).

**Next session likely starts with:**

Three roughly equal-priority paths:

1. **Cache-miss path: when the article isn't on disk, fetch its
   history via `wikiwho-mwclient`, run the algorithm, persist, then
   serve the response.** This is the path that makes the server
   useful for arbitrary articles instead of only those already on
   disk. Requires plumbing the existing `mwclient` history-fetching
   code into the server's request handler with a timeout policy.

2. **Persist paragraph + sentence arenas for resume-from-disk.**
   The longstanding decision from part 6. Mirrors the token-arena
   pattern. Multi-session work but closes Strategy B's actual write
   path.

3. **Add the `rev_id → page_id` index** for endpoint 1 (the
   Impact Visualizer path). Standalone, well-scoped: design the
   sidecar file format, add a writer that updates it on
   `write_article`, add a reader, plumb into the endpoint 1
   handler.

Recommendation: **3** as the next session. It unblocks Impact
Visualizer testing against the rewrite without depending on
mwclient integration (1) or the bigger storage refactor (2). After
that, **1** so the server can serve arbitrary articles —
that's what would let us point Dashboard / XTools / WWT at a real
running instance.
