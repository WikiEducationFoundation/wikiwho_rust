# 2026-05-23 (part 9) — cache-miss path: MW fetch + background processing

**Goal:** make the server useful for arbitrary articles, not just
ones already on disk. PLAN.md §280-287 specified the shape — lazy
on-demand fetch from MW in a background task with the response
saying "still building" — so this session implements that path for
endpoints 2/3/4/5/6.

**Parity:** N/A — no algorithm changes. Storage round-trip parity
unchanged (5 fixtures still 100%). Cache-miss serves byte-identical
JSON to `build_rev_content` on the round-tripped article (verified
end-to-end through the mock-MW integration test).

**Counts before → after:**

- Total workspace tests: 219 → 239 (+20).
- New mwclient: 8 → 18 (+10 `parse_page_info` cases).
- New server lib: 8 → 14 (+6 cache-miss/state).
- New server integration: 11 → 15 (+4 cache-miss).
- Clippy clean with `-D warnings`.

**Done:**

- **`crates/wikiwho-mwclient/src/info.rs`** — new module with
  `parse_page_info` + `PageInfo { title, page_id, last_revid }`.
  Two new public methods on `MwClient`:
  - `resolve_title(&str) -> Result<PageInfo, MwError>`
  - `resolve_page_id(u64) -> Result<PageInfo, MwError>`

  Both issue `action=query&prop=info&inprop=lastrevid` with the
  appropriate `titles=` / `pageids=` selector and share the parser.
  10 unit tests cover normal pages, `normalized` title echo,
  `missing: true`, `invalid: true`, and three Shape-error fallbacks
  (no `query`, empty `pages`, missing field).

- **`crates/wikiwho-server/src/cache_miss.rs`** — new module with
  the pure pipeline:
  - `build_article_from_revisions(title, page_id, &[Revision]) -> Article`
    — skips `text_hidden` per `wikiwho.py:144`.
  - `process_and_persist(storage_root, lang, title, page_id,
    revisions) -> Result<Article, CacheMissError>` — runs the
    algorithm + writes to disk via `write_article`. Returns the
    article so callers can refresh in-memory caches without a
    re-read.
  - `collect_all_revisions(fetcher) -> Result<Vec<Revision>, MwError>`
    — drains the `RevisionFetcher` paginator into one Vec.

- **`crates/wikiwho-server/src/state.rs`** — `AppState` extended:
  - Per-language `MwClient` cache (`mw_client(language) -> Arc<MwClient>`,
    `install_mw_client(language, client)` for tests).
  - In-flight `Mutex<HashSet<(String, u64)>>` with atomic
    `try_claim_in_flight` + `is_in_flight` query.
  - `spawn_cache_miss(language, title, page_id, fetcher)` which:
    1. awaits the injected `fetcher` future,
    2. runs `process_and_persist` on `tokio::task::spawn_blocking`
       (algorithm + write_article are CPU-bound; they shouldn't tie
       up an async runtime thread),
    3. refreshes title + rev_id indexes so the next request hits
       disk,
    4. releases the in-flight slot on success or failure.
    Returns the `JoinHandle` so tests can `.await` it; production
    code drops it (fire-and-forget).

- **`crates/wikiwho-server/src/handlers/rev_content.rs`** —
  rewritten:
  - Endpoint 1 (rev_id-only): unchanged — uses `rev_id_index.bin`,
    falls back to 408. The MW `rev_id → page_id` lookup needed for
    cold-start is deferred (filed below in Next session).
  - Endpoints 2/5 (title-only): try local title index → on-disk; on
    miss, resolve via MW → spawn cache-miss + return 408. Backfills
    the title index when MW resolves to an article already on disk
    under a different casing.
  - Endpoint 3 (title + rev_id): same as 2/5 but the cache-miss
    `end_rev_id` is the request's rev_id, not MW's latest.
  - Endpoints 4/6 (page_id-only): try on-disk → if miss, resolve
    via MW for `(title, last_revid)` → spawn cache-miss + 408.

- **`crates/wikiwho-server/tests/cache_miss.rs`** — new integration
  suite. Stands up a tiny axum-based MW mock that serves
  `prop=info` and `prop=revisions` from a captured fixture (zh
  中国, 7 revs in one batch — no pagination needed). Tests:
  - `cache_miss_by_title_persists_and_serves_byte_identical`
  - `cache_miss_by_page_id_persists_and_serves_byte_identical`
  - `cache_miss_concurrent_requests_spawn_one_task` (in-flight
    de-dup under contention)
  - `cache_miss_no_mw_client_returns_408_without_spawning`

  Each waits on `state.is_in_flight(lang, page_id)` to flip false
  before issuing the second request, so the test reliably catches
  the on-disk serve without a sleep.

**Design notes / issues encountered:**

- *Where does the title→page_id lookup live during cache-miss?* The
  handler first tries `state.resolve_title`; on miss it asks MW.
  The cache-miss path uses MW's echoed canonical title for the
  stored title — `mwclient` already normalizes case (the
  `normalized` block + the page record's `title` field), so we
  trust that.

- *Why `spawn_blocking` for the algorithm run?* `analyse_revision`
  is CPU-bound and not cancellable; the prior session's
  `wikiwho-storage` round-trip already showed Photosynthesis at
  ~30s for 5495 revs in release. Holding that on a tokio worker
  thread would block other request handlers. Putting it on the
  blocking pool keeps the runtime responsive; the cost is a Send
  bound on the surrounding types, easy to satisfy.

- *Index refresh after success.* `refresh_title_index` is needed
  so a subsequent request for the same article (or any other
  article in the same language) sees the new mapping without a
  full directory walk — the lazy-build path would still work, but
  re-walking on every per-language insert is wasteful. Same for
  `refresh_rev_id_index`: subsequent rev_id-keyed requests hit the
  freshly-saved `rev_id_index.bin`.

- *In-flight slot leak avoidance.* `spawn_cache_miss` always
  releases the slot in its task body, even if the fetcher or the
  blocking task panic. `trigger_cache_miss` resolves the MwClient
  *before* it claims the slot, so a transient client-build failure
  doesn't strand a claim.

- *Title index inconsistency with article files.* If the title
  index says "article at page_id X" but `SnapshotReader::open`
  returns NotFound, we log a warning and fall through to the MW
  resolution path — same code as a fresh cache-miss. That handles
  the edge case where storage was hand-tampered with.

- *Endpoint 1 cold-start is deferred.* The rev_id→page_id lookup
  needs an MW call (`prop=revisions&revids={rev_id}&prop=ids`) and
  a slightly different cache-miss shape (we'd need to fetch from
  rev_id 0 through some upper bound — probably the article's
  latest_rev_id rather than the requested rev_id, since Impact
  Visualizer's typical pattern is "I have a recent rev_id from MW;
  give me its authorship"). Skipping for now; the title and
  page_id endpoints unblock Dashboard / XTools / WWT, which are
  the three of four downstream consumers.

**Queued decisions:** none new this session. The cache-miss shape
was already nailed down in PLAN.md §280-287; this session executed
that plan.

**Next session likely starts with:**

Three roughly equal-priority paths:

1. **Endpoint 1 cold-start (rev_id → page_id MW lookup).** Closes
   the last cache-miss gap for Impact Visualizer; not blocking
   because IV's main code path also accepts the
   title-based / page_id-based endpoints if we point it there.

2. **Persist paragraph + sentence arenas for resume-from-disk.**
   The longstanding decision from part 6; required before the
   cache-miss path can do *incremental* updates (right now every
   write_article rewrites from scratch — fine for cache-miss but
   not for live-update from EventStreams).

3. **Smoke-test the cache-miss path against real MW.** Hit
   `en.wikipedia.org/w/api.php` from a local server instance with
   a small article (e.g. `Simple_Test_Page` or one of our captured
   fixtures). Validates that the production-shaped MwClient still
   works against real MW under the cache-miss orchestrator.

Recommendation: **3 then 1.** A real-MW smoke test confirms that
the mock-MW integration test isn't masking a paginator or
formatversion bug; then 1 closes the remaining endpoint gap.
