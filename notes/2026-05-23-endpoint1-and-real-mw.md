# 2026-05-23 (part 10) — real-MW smoke + endpoint 1 cold-start

**Goal:** the prior session left two near-term gaps in the
cache-miss work: (1) only the mock-MW integration tests exercised the
end-to-end pipeline, and (2) endpoint 1 (`/rev_content/rev_id/{rev_id}/`)
still 408'd whenever the rev_id wasn't already in `rev_id_index.bin`.
This session closes both.

**Parity:** N/A — no algorithm changes. Round-trip parity unchanged
(5 fixtures still 100%). Cache-miss now also serves real-MW responses
byte-identical to the production `rev_content.json` capture on
`zh/中国/64806634` (verified twice end-to-end against the live wiki).

**Counts before → after:**

- Total workspace tests: 239 → **243** (+4).
- New mwclient parse tests: 10 → 12 (+2: `badrevids` → PageMissing;
  known-revid parses like titles).
- New server integration tests: 4 → 6 (+2: endpoint 1 cold-start
  byte-identical via mock MW; endpoint 1 unknown-rev_id 408 without
  spawning).
- Clippy clean with `-D warnings --all-targets`.

**Done:**

- **Real-MW smoke test of cache-miss.** Booted the release binary
  against `./var/smoke-storage` (fresh dir) and hit:
  - `GET /zh/api/v1.0.0-beta/rev_content/中国/64806634/` → 408,
    cache-miss task spawned (`spawning cache-miss task lang=zh
    page_id=1686258 end_rev_id=64806634`), 165 ms later
    `cache-miss processed and persisted ... revisions=7`. Second
    request: byte-identical JSON to
    `parity-fixtures/zh/1686258/64806634/rev_content.json`.
  - `GET /simple/api/v1.0.0-beta/rev_content/page_id/27263/` (endpoint
    4) → 408, 64 s of MW fetch + algorithm, then on-disk serve.
    3525 processed revisions matches the captured `python_replay`'s
    `processed` field. Diff vs production: 144 lines out of ~71k
    tokens, all in `out` arrays — within the previously-characterized
    historical-drift envelope for prod-cache vs fresh rebuild.
  - Endpoints 2 / 5 (title-only and `latest_rev_content/{title}/`)
    both serve byte-identical responses from the on-disk article
    after the page_id miss filled storage.

- **`crates/wikiwho-mwclient/src/info.rs`** — `parse_page_info`
  recognises `query.badrevids` (no `pages` block) and surfaces it as
  `MwError::PageMissing { page_id: 0 }`. Existing callers
  (`resolve_title` / `resolve_page_id`) are unaffected because their
  responses never include `badrevids`.

- **`crates/wikiwho-mwclient/src/lib.rs`** — new
  `MwClient::resolve_rev_id(rev_id: u64) -> Result<PageInfo>`. Same
  shape as `resolve_page_id` and `resolve_title` (also uses
  `prop=info`); only the selector param changes to `revids=`. Module
  doc comment in `info.rs` updated to list all three callers.

- **`crates/wikiwho-server/src/handlers/rev_content.rs`** —
  `rev_content_by_rev_id` rewritten:
  1. Index hit: serve from disk with `target_rev_id =
     Some(path.rev_id)` (matches endpoint 3's snapshot semantics).
  2. Index miss: `mw.resolve_rev_id(path.rev_id)` →
     `CacheMissPlan { title, end_rev_id: path.rev_id }`. Override
     of `last_revid` mirrors endpoint 3's behavior — the cache-miss
     fetch terminates at the requested snapshot, not the live tip.
  3. `badrevids` or any other MW failure → 408 envelope, no slot
     claimed.
  4. After MW resolves, re-try `try_serve_from_disk` in case another
     request had already populated the article — handles a narrow
     race where the rev_id_index just hadn't been refreshed yet.

  Removed now-dead `serve_or_trigger` helper — the only caller was
  the old endpoint 1 path.

- **`crates/wikiwho-server/tests/cache_miss.rs`** — two new tests:
  - `cache_miss_by_rev_id_persists_and_serves_byte_identical` —
    full cold-start cycle through the rev_id endpoint, validating
    byte-identical JSON against a freshly-built expected
    `Article`.
  - `cache_miss_by_unknown_rev_id_returns_408_without_spawning` —
    custom mock returns `badrevids`; handler must not claim an
    in-flight slot under the fixture's page_id (since it never
    learned about that page).

- **`.gitignore`** — added `/var/` so the smoke-test storage root
  and server logs don't show up in `git status`.

**Design notes / issues encountered:**

- *MW's `inprop=lastrevid` is unrecognized* but harmless — `prop=info`
  returns `lastrevid` by default. Confirmed via curl; the warning in
  the response body is logged but doesn't affect parsing. Left the
  param in for explicitness; could remove in a future cleanup.

- *`badrevids` semantics.* When `?revids=X` names a rev_id MW doesn't
  recognize, the response has `query.badrevids` instead of `query.pages`.
  No `pageid`, no `title`. The simplest mapping is `PageMissing
  { page_id: 0 }`, which the handler logs and converts to 408. The
  alternative (a separate `RevIdMissing` variant) would let logs name
  the rev_id explicitly, but the handler already has `path.rev_id` in
  scope at log time — so the variant would only carry the value
  through the parse boundary for no real benefit. Left as PageMissing.

- *Why override `last_revid` to `path.rev_id` for endpoint 1?* For
  endpoint 1 the user is asking "show me authorship as of this exact
  rev". If we fetched all the way to the article's current tip the
  rendered response would *still* show only the requested rev's
  tokens (the renderer picks one rev out of `ordered_revisions`), but
  the on-disk article would include hundreds or thousands of
  revisions newer than what the user cares about — wasted work on
  cache-miss latency and storage. Stopping at the requested rev_id
  matches what endpoint 3 already does.

- *Smoke-test storage in `./var/smoke-storage`.* Sage's restricted-
  permission setup denied writes to `/tmp`, so the smoke storage
  lives inside the repo. Now gitignored. The cache-miss timings
  observed (165 ms for 7 revs; 64 s for 3525 revs) are useful
  ballpark numbers for the deployment story but should be re-measured
  on production hardware before we ship.

- *Production-cache vs fresh-rebuild divergence on simple/Wikipedia.*
  The 144-line diff against `rev_content.json` (out of ~71k token
  positions) is the same shape we characterized in
  `notes/2026-05-23-python-replay.md` — production's cache has been
  mutating over years and accumulates `out`-list entries that a fresh
  rebuild from the same target rev_id won't have. Not a server bug.
  Validating against fresh-rebuild python output (parity-check's
  `--python-replay`) remains the right ground-truth path.

**Queued decisions:** none new this session. Both gaps the prior
session flagged are now closed.

**Next session likely starts with:**

The cache-miss path is now functionally complete for endpoints 1-6.
The remaining "near-term" gaps from prior sessions:

1. **Persist paragraph + sentence arenas for resume-from-disk.** The
   longstanding decision from part 6
   (`notes/decisions-needed.md` §2026-05-23 "how to persist
   paragraph + sentence arenas"). Still non-blocking for cache-miss
   (every write_article rewrites from scratch — fine while we don't
   support live incremental updates) but blocking for the future
   EventStreams live-update path and for the eventual delta-log
   optimization (STORAGE.md §4 Strategy B).

2. **WhoColor endpoint (API.md §8 and PLAN.md §4.6).** The fourth
   downstream consumer (WhoWroteThat gadget) reads
   `/{lang}/whocolor/v1.0.0-beta/{title}/{rev_id}/` rather than
   `rev_content`. We have the data (Parsoid HTML + token attribution)
   but no handler. PLAN.md §4.6 settled on Option A (MW REST
   `/page/html` + `html5ever` injection); this session is when that
   becomes the load-bearing path for shipping anything other than
   the three rev_content-consuming projects.

3. **Bench cache-miss against a larger article on production-shaped
   hardware.** 64 s for 3525 revs of simple/Wikipedia is fine on
   this laptop; need a Cloud VPS measurement before we can size the
   in-flight set and pick reasonable retry/timeout knobs.

Recommendation: **2 (WhoColor)**. (1) keeps blocking the same
hypothetical "live-update" use case that nothing has actually asked
for yet; (3) needs production hardware we don't have until we
deploy. WhoColor closes the last consumer-facing endpoint and lets
all four downstream projects exercise the rewrite end-to-end.
