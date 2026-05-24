# 2026-05-23 (part 15) — wikiwho-ingest scaffold: EventStreams listener + load/apply/save loop

**Goal:** stand up the `wikiwho-ingest` crate per PLAN.md §9 — the
last load-bearing infrastructure piece before a per-wiki cutover can
begin. With resume-from-disk landed in part 14 and the algorithm
parity-locked, what was missing was the daemon that watches the live
Wikimedia EventStreams feed, drives the load → fetch → analyse → save
loop on each edit, and persists a checkpoint so a restart doesn't
lose work.

**Parity:** N/A — no algorithm-layer changes. The end-to-end ingest
test verifies the loop produces byte-identical wire format vs a
fresh in-memory full-history replay on the zh/中国 fixture (7 revs);
parity-check and whocolor-parity numbers from part 13/14 still hold.

**Counts before → after:**

- Workspace crates: **5 → 6** (`wikiwho-ingest` added).
- Workspace tests: **296 → 321** (+25 across-suites: events ×16,
  checkpoint ×5, apply ×2 unit, apply_end_to_end ×2 integration).
  All passing. (The +3 vs part 14's 293 count is reconciliation —
  part 14 didn't count the binary-target unit-test suites that
  always-pass at 0.)
- Clippy clean with `-D warnings --all-targets` across all 6 crates.
- Release build of `target/release/ingest` succeeds in ~6s.

**Done:**

- **`crates/wikiwho-ingest/Cargo.toml`** — workspace member;
  deps on reqwest (+`stream` feature for `bytes_stream()`),
  tokio (+`fs`, `signal`, `io-util`), futures-util, async-stream,
  tracing, plus path-deps on attribute/mwclient/storage.

- **`src/events.rs`** — SSE listener for `https://stream.wikimedia.
  org/v2/stream/recentchange`. `recentchange_stream_filtered` returns
  a pinned `Stream<Item=Result<PageEdit, EventStreamError>>` that
  reconnects transparently on network errors (using the saved
  `Last-Event-ID` header for resume). Filters server-side by
  `type ∈ {edit, new}`, `namespace == 0`, and configured wiki set
  (e.g. `enwiki`, `simplewiki`). `SseFrameBuffer` is the unit-testable
  frame accumulator that decouples line parsing from the network
  layer.

- **`src/apply.rs`** — `apply_event(storage_root, mw_client, event)
  -> ApplyOutcome`. Opens the article via `SnapshotReader`,
  fetches the rev window `(last_good_rev_id, event.rev_id]` from the
  MW Action API, runs each new rev through `Article::analyse_revision`,
  and persists via `write_article`. Returns:
  - `SnapshotMissing` if the article isn't on disk (cold builds are
    the server's job, not ingest's)
  - `AlreadyAtOrAhead` if `event.rev_id <= last_good_rev_id`
    (idempotent skip; protects against SSE replays)
  - `Applied { applied_revs }` on success

- **`src/checkpoint.rs`** — per-language SSE resume state. Single
  JSON file at `<storage_root>/ingest/checkpoint.json` keyed by
  language; atomic tmp-file + rename writes; load-or-init
  idempotent; corrupt files reset cleanly. Dirty counter lets the
  main loop flush every N events rather than on every advance.

- **`src/config.rs`** — `IngestConfig` (storage_root, languages,
  stream_url, checkpoint_every). Stream URL defaults to the
  production EventStreams endpoint; tests override.

- **`src/lib.rs`** — `run_ingest(config, shutdown)` ties it together:
  builds one `MwClient` per configured language, opens the
  EventStreams stream with the resume id, drives the apply loop,
  advances the checkpoint after each event. Per-process
  `ShutdownSignal` is a tiny `tokio::sync::Notify` + atomic-bool
  shim so the binary can stop cleanly on SIGINT/SIGTERM without
  pulling in tokio-util.

- **`src/bin/ingest.rs`** — daemon entrypoint. Reads
  `WIKIWHO_STORAGE`, `WIKIWHO_INGEST_LANGS`, optional
  `WIKIWHO_EVENTSTREAMS_URL`. Spawns a signal handler task that
  triggers shutdown on SIGINT or SIGTERM; main task waits for the
  ingest loop to drain (which includes a final checkpoint flush).

- **`tests/apply_end_to_end.rs`** — integration test. Replays
  the first N revs of a fixture, persists, spins up a tiny axum
  mock of the MW Action API with the remaining revs in
  formatversion=2 shape, synthesizes a `PageEdit`, runs
  `apply_event`, reloads the snapshot, and asserts both wire-format
  identity (via `build_rev_content`) AND structural counters
  (tokens / paragraphs_ht / sentences_ht / ordered_revisions)
  match a fresh full-history in-memory replay. Two variants:
  - `apply_one_event_zh` — split at rev 6 of 7, single-rev catch-up
  - `apply_gap_window_zh` — split at rev 3 of 7, simulates a gap of
    4 missed events that the apply loop's window-fetch path recovers

  The gap test specifically guards against the "happy path" trap
  where dropped events leave the snapshot permanently behind.

- **Workspace `Cargo.toml`** — added `stream` to reqwest's features
  (needed for `Response::bytes_stream()`). No other crates use it
  yet; harmless to existing builds.

**Design notes / issues encountered:**

- *Why not OpenAPI/eventsource-stream crate?* I considered
  `eventsource-stream` and `reqwest-eventsource` — both would work,
  but the SSE framing is ~50 lines and we already needed reqwest's
  `bytes_stream`. The added complexity of a third-party SSE parser
  (whose error model doesn't map cleanly to our reconnect-with-
  Last-Event-ID semantics) wasn't worth the dep. `SseFrameBuffer`
  is small, isolated, and unit-tested at per-byte chunking
  granularity.

- *Why one shared MwClient per language, not per article?* The
  client owns a connection pool and reuses it across requests; one
  per language is the standard pattern (mirrors the server's
  `AppState`). The ingest loop is currently single-threaded — when
  we shard by language we can clone the client cheaply.

- *Window fetch goes from page beginning, not from `rvstartid`.*
  The current `RevisionFetcher` paginates from the start of the
  page when no `rvcontinue` is given; we then discard revs whose
  rev_id is `<= start_exclusive` client-side. For the common
  "single new rev" case this is one wasted batch read of size ≤500;
  for big articles with small windows it's wasteful but correct.
  An `rvstartid`-aware variant is a future perf win — surfaced in
  `notes/decisions-needed.md` so we don't lose it.

- *Cold-start path NOT in ingest.* Per PLAN.md §4.4, cold builds
  happen via the server's cache-miss path. The ingest stream
  yields `SnapshotMissing` for any unseen page; we don't queue
  these for proactive build. If we want proactive prefetch later
  (e.g. to follow newly created articles before someone requests
  them) it's a small addition — surface it as a decision when
  the cutover plan asks for it.

- *Checkpoint at event-id granularity, not article granularity.*
  EventStreams' `id:` field is a JSON array of partition/offset
  records — opaque to us but accepted verbatim by the server as
  `Last-Event-ID`. The checkpoint stores one id per language; on
  restart we pick any non-empty value (the stream is global, so
  all languages converge within seconds). Per-language ids exist
  for forward-compat if someone runs separate ingest workers per
  language with separate filters.

- *No delete / revision-visibility-change listener yet.* The legacy
  service has a parallel SSE listener for `mediawiki.revision-
  visibility-change` (`events_stream.py:88`); we'll add that as a
  second stream when the visibility model is wired up. Tracked as
  a queued decision.

**Queued decisions:** see `notes/decisions-needed.md` for two new
non-blocking items (windowed `rvstartid` fetch; revision-visibility
listener).

**Next session likely starts with:**

The remaining items in part 14's note are now:

1. ~~EventStreams ingest scaffold~~ ✅ this session.
2. **Ephemeral non-mainspace endpoint (API.md §9).** Still lowest
   priority — no downstream consumer uses it. Fills out the wire-
   format surface.
3. **Deploy path planning.** With ingest landed, the algorithm is
   parity-locked, storage round-trips, server endpoints work, and
   the live-update loop is in place. The cutover plan can now
   sketch concretely: small wikis first (zh, simple), the existing
   prod-cache stays for languages not yet migrated, per-language
   bootstrap is a `capture-history` dump + a cold-built corpus
   followed by an ingest start.
4. **Operational concerns.** Title→page_id resolution via the
   Wikimedia Cloud replica DB (PLAN.md §4.5) — currently we use
   the MW Action API on cache miss. Sub-ms replica lookup vs
   150-300 ms MW round-trip is a meaningful latency win once we're
   serving production traffic.

Recommendation: **3 (deploy path planning)**, because the
infrastructure inventory is now complete and the next concrete
unknowns are operational — pickle conversion timeline, per-language
bootstrap order, prod-cache cutover handshake. A short PLAN.md
update sketching the cutover sequence would unblock the next batch
of work.
