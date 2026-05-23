# WikiWho rewrite — plan

This document is the strategy. It assumes the reader has skimmed
[README.md](README.md) and will refer to [ALGORITHM.md](ALGORITHM.md),
[API.md](API.md), and [STORAGE.md](STORAGE.md) for the load-bearing details.

## 1. Why rewrite

The current Django service at `../wikiwho_api` works but is expensive in
every dimension:

- **CPU.** The inner attribution loop is pure Python and uses
  `difflib.Differ` (the slow ndiff implementation, complexity closer to
  O(N·M) than O(N+M)) for per-revision token diff. For an Obama-class
  article (~57 K revisions), this is the bottleneck.
- **RAM.** Each token is a Python `Word()` object with seven attributes
  (~180 B serialized, several × that in memory). The whole article's
  attribution state is held in Python heap during processing. The
  production VPS is sized at 122 GB RAM partly because of this.
- **Disk.** Pickles are gzipped (~84% reduction per [T414075]), but the
  service still uses three 5 TB Cinder volumes (`pickle_storage{,02,03}`)
  to hold ~67 language directories. Most pickles are small; a few
  (Obama-class) are tens of MB compressed.
- **API latency.** Cold reads load and unpickle the entire article state
  before answering. For a small article that's ~25 ms; for a large one
  it's hundreds of ms to seconds of pure deserialization before any work
  begins.
- **Ingestion latency.** EventStreams updates trigger Celery tasks that
  load the full pickle, append one revision, and rewrite the whole file.
  For hot articles this serializes behind `fcntl.flock`. Adding a new
  language requires running 24-thread XML dump processing for days.
- **Maintainability.** Django 1.11 (out of LTS support), Python 2/3
  hybrid quirks, RabbitMQ + Celery + Flower + Memcached + Postgres +
  Wikimedia OAuth + custom throttling for per-user-agent rate overrides
  + `rest_framework_swagger` + account registration + i18n URL routing.
  Most of this is dead weight for the four consumers actually using
  the service. Nobody on the team knows the codebase deeply.

## 2. Scope

### Endpoints to keep (with identical URL paths and JSON shapes)

- `GET /{lang}/api/v1.0.0-beta/rev_content/{title}/` — latest revision
- `GET /{lang}/api/v1.0.0-beta/rev_content/{title}/{rev_id}/` — specific revision
- `GET /{lang}/api/v1.0.0-beta/rev_content/rev_id/{rev_id}/` — by rev id alone
- `GET /{lang}/api/v1.0.0-beta/rev_content/page_id/{page_id}/` — by page id
- `GET /{lang}/api/v1.0.0-beta/latest_rev_content/{title}/` — alias for latest
- `GET /{lang}/api/v1.0.0-beta/latest_rev_content/page_id/{page_id}/` — alias
- `GET /{lang}/whocolor/v1.0.0-beta/{title}/` — whocolor latest
- `GET /{lang}/whocolor/v1.0.0-beta/{title}/{rev_id}/` — whocolor specific

The exact JSON wire format is specified in [API.md](API.md). Consumers must
not have to change anything.

### Endpoints to deprecate

These are reachable in the current service but not called by any of the
four confirmed consumers. They should be removed in the rewrite (return 410 Gone
with a pointer to the new API documentation, or just 404):

- `all_content/*` — full article token dump
- `range_rev_content/*` — slice between two revisions
- `rev_ids/*` — list all revision ids
- `edit_persistence/*`, `api_editor/*` — separate Django app, not used by
  Dashboard/IV/WhoWroteThat
- `account/*`, `admin/*`, `download/*`, `contact/*`, swagger UI, sitemap

### Side services to drop entirely

- The Celery worker pool, RabbitMQ broker, Flower UI
- Memcached (used only as a per-page processing lock; replaced by an
  in-process or single-Redis lock)
- Postgres (used for OAuth tokens, throttling state, `RecursionErrorArticle`
  and `LongFailedArticle` tracking — none of which are load-bearing for the
  consumers)
- Wikimedia OAuth consumer flow (we do not need authenticated MW writes)
- `rest_framework_swagger`, i18n URL routing (just put `{lang}` in the path)
- The `wikiwho_chobj` / Elasticsearch "change object" pipeline (currently
  enabled only for German and partially-broken; nobody downstream uses it)

### Languages

Continue supporting every language that has a Wikipedia (`*wiki`). The
current list of 67 is in `../wikiwho_api/wikiwho_api/settings_base.py`
under `LANGUAGES`. Adding a new wiki should be trivial — ideally a config
change plus a bootstrap run, not a code change.

### Algorithm parity

The rewrite must produce token-for-token identical output to the
reference implementation on every revision of every article, within the
limits of intentional improvements documented in [ALGORITHM.md](ALGORITHM.md).
This is the **single most load-bearing constraint** — downstream Impact
Visualizer word counts and Dashboard token coloring will silently break
otherwise.

## 3. Language choice

Recommendation: **Rust**.

### Why Rust for this workload specifically

- The inner loop is integer-keyed hash-table churn plus token-array diff.
  Both are workloads Rust handles 30–100× faster than CPython without
  resorting to FFI tricks.
- The data structures (per-token records with two variable-length lists
  of revision ids) want compact, allocation-aware representations to fit
  Obama-class articles in <100 MB working memory. Rust's `Vec<u32>`
  +  small-vec optimizations buy this directly; Go's GC and CPython's
  per-object header overhead do not.
- The on-disk format we want is column-oriented binary blobs (see
  [STORAGE.md](STORAGE.md)) memory-mapped and zero-copy decoded. `rkyv`,
  `zerocopy`, or hand-rolled `&[u8]` slicing in Rust gives this directly.
- Deployment is a single statically linked binary — drastically simpler
  than the current pip-installed Django world.
- The HTTP layer (axum or actix-web) is well-trodden and fast enough that
  the algorithm is the bottleneck, not the framework.

### Why not the alternatives

- **Go.** Reasonable second choice. Probably 3–5× slower than Rust on the
  inner diff loop (GC pauses on per-token allocations matter when an
  article has hundreds of thousands of lifetime tokens) but with a much
  gentler learning curve. If the team prefers Go, the rest of this plan
  ports directly. We'd want to be more careful about allocations in the
  hot path (`sync.Pool`, pre-allocated slices).
- **Python + Cython / PyO3.** Keeps the existing pain. Cython buys 5–20×
  on the inner loop but you still pay Python overhead at the boundaries,
  still need pickle/HDF5/parquet for storage, and onboarding is harder
  than it looks because the codebase is half-typed. Not recommended.
- **C++.** Faster than Rust at the limit, but the team-maintenance story
  is worse and the wire/storage codegen ecosystem is less ergonomic.
- **Java / JVM.** Throughput is fine, latency tails are not (GC pauses
  during attribution of large articles); deployment is heavier.

### Onboarding given Sage doesn't know Rust

The rewrite has three skill phases:

1. **Algorithm port** (the meat). This is dense logic but the surface
   area is small: ~700 lines in `wikiwho.py` + ~100 lines in `utils.py`.
   It can be written, tested, and iterated on in isolation, without
   touching HTTP or storage. A new Rust developer with the spec in
   [ALGORITHM.md](ALGORITHM.md) and the parity test corpus (see §7) can
   make progress.
2. **Storage**. Columnar binary files with mmap. This is junior-Rust-friendly
   once the format is specified ([STORAGE.md](STORAGE.md)).
3. **HTTP + ingestion**. axum + reqwest + tokio. Idiomatic Rust async, but
   the patterns are well-documented.

If at any point Rust feels like the wrong call, Go is a clean fallback.
Don't pick Python.

> **Resolved 2026-05-22:** Rust. Don't revisit unless the algorithm port
> hits a wall that's genuinely Rust-specific (lifetime puzzles in the
> attribution state, not "I had to look up syntax"). The bail-out is Go,
> never Python.

## 4. Architecture

```
                           ┌────────────────────────────────┐
                           │   Dashboard / IV / WWT clients │
                           └──────────────┬─────────────────┘
                                          │ HTTPS, JSON
                                          ▼
                  ┌──────────────────────────────────────────┐
                  │  axum HTTP server  (stateless, N workers)│
                  │  ┌──────────────────┐ ┌────────────────┐ │
                  │  │ rev_content/...  │ │ whocolor/...   │ │
                  │  └────────┬─────────┘ └────────┬───────┘ │
                  └───────────┼────────────────────┼─────────┘
                              │ mmap reads          │ MW REST /page/html
                              ▼                     ▼          (Parsoid)
                  ┌─────────────────────┐  ┌───────────────────┐
                  │ Article blob store  │  │  HTML annotator   │
                  │  /pickles → /blobs  │  │  (markup→HTML)    │
                  │  per-language dirs  │  └───────────────────┘
                  └──────────┬──────────┘
                             │ append-only log + nightly compact
                             ▼
              ┌──────────────────────────────────────┐
              │  Attribution engine (the algorithm)  │
              └────────┬─────────────────────────────┘
                       │
        ┌──────────────┼───────────────────────────┐
        ▼              ▼                           ▼
   ┌─────────┐  ┌──────────────────┐    ┌─────────────────────┐
   │ Cold    │  │ EventStreams     │    │ Lazy on-demand      │
   │ dump    │  │ tail (kafka/SSE) │    │ fetch from MW API   │
   │ bootstrap│ │ per-language    │    │ (rvlimit=500)       │
   └─────────┘  └──────────────────┘    └─────────────────────┘
```

### 4.1 Attribution engine

A library crate (`wikiwho-attribute`) with a single entrypoint:

```rust
pub fn analyse_article(
    state: &mut ArticleState,
    revisions: impl Iterator<Item = Revision>,
) -> Result<(), AnalysisError>;
```

`ArticleState` holds the global hash tables of paragraphs/sentences,
the spam id list, the token symbol table, the lifetime token array,
and the current/previous revision pointers. The implementation must
mirror the semantics described in [ALGORITHM.md](ALGORITHM.md).

Internal improvements over the reference:
- Replace `difflib.Differ` with **Myers diff over arrays of `u32` token
  ids**. The Python implementation diffs strings; we intern strings into
  a per-article symbol table and diff ids, which is both faster and
  avoids the special-character handling Differ does (which doesn't
  matter once you're working on already-tokenized symbols).
- Replace `for word_prev in unmatched_words_prev: if not w.matched and
  w.value == word` linear scans (`wikiwho.py:643–656`) with a hash map
  from token-id to the first unmatched word.
- Store `inbound` / `outbound` as **delta-encoded varint streams** in
  the persistent representation. In memory they can stay as `Vec<u32>`
  during processing; we encode on write-out.

### 4.2 Storage

Per-article, write a small set of immutable files plus an append log.
Full spec in [STORAGE.md](STORAGE.md). Summary:

- `tokens.bin` — per-lifetime-token record stream (sorted by token_id).
- `revisions.bin` — per-revision header + token-id sequence.
- `strings.bin` — interned token strings + perfect hash for lookup.
- `meta.json` — title, page_id, language, last_processed_revid, schema
  version, hash-table state-machine checkpoint.
- `appendlog.bin` — append-only WAL for live updates between compactions.

All compressed with **Zstd level 9**. `mmap`'d and decompressed on
demand at frame granularity (1–4 MB frames) so the working set for
reading one revision is small.

Per-article directory: `<volume>/<lang>/<page_id // 1_000_000>/<page_id // 1000>/<page_id>/`.
The two-level shard keeps any one directory at <1000 entries.

### 4.3 HTTP layer

axum, stateless workers, no per-request DB query. The only IO per
request is the mmap blob read (warm = page cache hit, microseconds) and
the JSON serialization.

- Throttle: token bucket per IP, configurable per-user-agent overrides
  read at startup from a small TOML (replaces the
  `OVERRIDE_THROTTLE_RATES` dict for `XTools`, `WhoWroteThat`).
  **Deliberate behavior shift from the reference:** the current Python
  service keys overrides off the *Django username*
  (`api/views.py:93-98`), not the User-Agent. That has meant XTools' Guzzle
  client (which sends no auth header but does send
  `User-Agent: XTools/<ver> ...`) has been living under the 100/sec
  anon-global limit despite the `10000/sec` setting — the override
  never fired. Switching to UA-prefix matching is a strict improvement,
  not parity drift; flag it that way if anyone notices throughput
  changing after cutover. Verify WhoWroteThat's UA before launch
  (current value at `OVERRIDE_THROTTLE_RATES['WhoWroteThat']` in
  `settings_base.py:303`).
- CORS: allow all origins on GET (matches current behavior; the data
  is public anyway).
- No authentication. Anonymous read-only. Drop the `IsAuthenticatedOrReadOnly`
  + BasicAuth + SessionAuth chain entirely.

### 4.4 Ingestion

Three paths, all funneling into `wikiwho-attribute::analyse_article`:

1. **Cold dump bootstrap** — `mediawiki_content_history` bz2 dumps from
   `https://dumps.wikimedia.org/other/mediawiki_content_history/<lang>wiki/<date>/xml/bzip2/`.
   Streamed with a Rust mwxml-equivalent (consider `parse_mediawiki_dump`
   crate as a starting point; may need to write our own for performance).
   Parallel per-stream (each bz2 file contains a chunk of articles).
2. **Live tail** — Wikimedia EventStreams `recentchange` SSE (URL in
   `events_stream.py:57`). For each `edit`/`new` event matching the
   article namespace and a supported wiki, fetch the one new revision
   from the MW Action API and append to the article's append-log.
3. **Lazy on-demand** — when a request arrives for an article with no
   blob yet, fetch revisions from the MW Action API at `rvlimit=500`
   (negotiate an OAuth consumer that gets the higher limit — the
   reference implementation already has one) in parallel chunks, build
   the blob, then answer. For median articles this is ~2–5 s; for
   Obama-class it is 30 s to several minutes and must run in a
   background task with the response saying "still building."

The 410 Gone-equivalent of `LongFailedArticle`/`RecursionErrorArticle`
tracking can move to a small SQLite file per language with one table
of `(page_id, last_attempt, reason)`.

### 4.5 Wikimedia Cloud replica DB usage

Per Sage's note, the rewrite will run on Wikimedia Cloud VPS with access
to the replica databases. Useful applications:

- **Title → page_id** lookup. The current code makes a `?action=query&prop=info&titles=…`
  API round trip every request (`handler.py:_get_latest_revision_data`).
  The replicas have `page` table: `SELECT page_id, page_namespace, page_latest
  FROM page WHERE page_title = ? AND page_namespace = 0`. Sub-millisecond
  vs. 150–300 ms.
- **Latest revision metadata.** `SELECT rev_id, rev_timestamp, rev_actor
  FROM revision WHERE rev_page = ? ORDER BY rev_id DESC LIMIT 1`. Useful
  for the "latest revision" endpoints.
- **Editor name resolution.** The whocolor pipeline needs to turn
  `user_id` into username (`whocolor/utils.py:WikipediaUser`). The
  replicas have `actor` and `user` tables.
- **Revision ID ranges for backfill.** `SELECT rev_id, rev_timestamp
  FROM revision WHERE rev_page = ? ORDER BY rev_id ASC` — cheaper than
  iterating with `rvcontinue`.

The replicas do **not** expose revision text (no `text` table, no
External Store). For content the dumps and Action API remain the only
sources.

### 4.6 WhoColor / HTML annotation

The whocolor endpoint is the trickiest part of the consumer-facing API:
it has to return `extended_html` — the article's rendered HTML with
per-token `<span>` wrappers carrying class names that identify the
authoring editor. The reference implementation:

1. Fetches the wikitext for the rev via MW API.
2. Loads the article's attribution from the pickle.
3. Calls Parsoid via `whocolor/parsoid.sh` to render wikitext → HTML.
4. Runs `WhoColor.parser.WikiMarkupParser` to inject `<span>`s before
   handing off to Parsoid (actually injects markers into wikitext first,
   then HTML-renders).
5. Resolves editor user_ids → usernames via the MW API.

For the rewrite, two viable approaches:

- **Option A (preferred):** Call the MediaWiki REST API endpoint
  `https://{lang}.wikipedia.org/api/rest_v1/page/html/{title}/{rev_id}`
  (which is Parsoid-rendered HTML). Inject the spans in HTML using a
  fast HTML5 parser (`html5ever` / `kuchikiki`). Cache by `(lang, rev_id)`
  since this output is immutable.
- **Option B:** Run Parsoid as a sidecar (as the current setup
  effectively does). More complex deployment but full control.

Start with A. The MediaWiki REST API caches aggressively at the WMF
edge, so we'd benefit from that.

> **Resolved 2026-05-22:** Option A (MW REST `/page/html` + `html5ever`
> for span injection). Abstract the HTML source behind a trait from day
> one so swapping to Option B (Parsoid sidecar) is a fallback, not a
> rewrite. Cache by `(lang, rev_id)` aggressively — the input is
> immutable.

## 5. Performance targets

These are estimates; validate them with the parity test corpus and the
profiling script described in §7.

| Operation | Current | Target | Notes |
|-----------|---------|--------|-------|
| Warm `rev_content` read, recent rev, small article | 150–400 ms | **<20 ms** | mmap + JSON serialize |
| Warm `rev_content` read, Obama-class article | 1.5–4 s | **<100 ms** | dominated by JSON output size; consider streaming response |
| Warm `whocolor`, small article | 800 ms – 2 s | **50–150 ms** | mostly Parsoid/REST round trip; cache hit reduces to <30 ms |
| EventStreams update applied | 0.3–2 s | **<10 ms** | append to log, no full rewrite |
| Cold ingest, median article (~500 revs) | 15–60 s | **2–5 s** | from dump blob; from API at rvlimit=500: ~5–15 s |
| Cold ingest, Obama (~57 K revs) | hours, historically failed | **30–60 s** from dumps; **3–8 min** from API | background task; not a synchronous request |
| Storage per article (avg, enwiki) | ~25 KB gzipped pickle | **~6–10 KB** | Zstd columnar |
| Storage total, enwiki | ~120 GB | **~30–50 GB** | fits on one volume |
| Storage total, all 67 languages | ~5 TB across 3 volumes | **~1–1.5 TB** | one volume plausible |
| Throughput per 8-core VPS | few hundred req/s | **5K–15K req/s** | mmap reads, no DB |
| RAM working set | hundreds of MB – several GB | **tens of MB** mmap | no Python GC stalls |

## 6. Migration

The migration must be **per-language** because each language has its own
storage volume and bootstrap. The current service stays up and serves
languages that haven't been migrated.

### Phase 1: parity harness (week 1)

Build the test infrastructure before anything else.

1. Pick 10,000 (page_id, rev_id) pairs from production logs (the current
   service has `rest_framework_tracking` enabled). Distribution:
   - 70% English, 30% spread across all other supported languages
   - Skew toward articles actually hit recently
   - Include the known-hard cases: Barack Obama, Donald Trump,
     COVID-19, Israel–Hamas war, Adolf Hitler, Jesus, Wikipedia itself
2. For each pair, snapshot the JSON response from the current production
   service for both `rev_content/rev_id/{rev_id}/?o_rev_id=true&editor=true&token_id=true&in=true&out=true`
   and `whocolor/v1.0.0-beta/{title}/{rev_id}/`.
3. Store these as fixtures in this repo (or a sibling repo if too big).
4. Build a `parity-check` binary: takes a (lang, page_id, rev_id) and
   compares the rewrite's JSON against the snapshot, reporting diffs
   token-by-token.

### Phase 2: algorithm port (weeks 2–6)

Algorithm-only crate. No HTTP, no storage. Driven by replaying an XML
dump or a captured stream of revision objects.

- Implement `analyse_article` per [ALGORITHM.md](ALGORITHM.md).
- Run parity-check on every fixture. Goal: 100% token-for-token match.
- Profile against Jesse Owens (6,307 revisions) and Obama (~57 K revs).
  Target: Obama processed cold in <60 s on a single core.

Acceptance gate: 100% of the parity corpus matches.

### Phase 3: storage layer (weeks 7–8)

Implement the blob format per [STORAGE.md](STORAGE.md). Implement
write-to-blob, read-from-blob, append-log, and compaction. Round-trip
test: load → serialize → load → compare; must be identity-equivalent
on every parity fixture.

### Phase 4: HTTP layer (weeks 9–10)

axum service exposing the endpoints in [API.md](API.md). Stateless. Read
from blob store directly. JSON shapes must match the snapshots
byte-for-byte except for whitespace (the reference uses default Python
JSON serialization; we should normalize to the same field order).

Acceptance gate: HTTP-level parity check against snapshots for all
fixtures. Same JSON, same status codes.

### Phase 5: ingestion (weeks 11–13)

- Dump bootstrap binary. Reads `mediawiki_content_history` bz2,
  produces blob directories.
- EventStreams listener. Tails `recentchange`, appends to blobs.
- Lazy on-demand fetch. Triggered by a request for an unbuilt article.

Acceptance gate: bootstrap a small wiki (`simple`, ~250 K articles)
end-to-end. Shadow traffic from the Dashboard staging environment
returns matching responses.

### Phase 6: per-language cutover (weeks 14+)

For each language:

1. Bootstrap from dump on the new service.
2. Catch up to live via EventStreams.
3. Run shadow traffic from a small sample of production requests for
   that language; compare responses to current service. Alert on
   mismatch.
4. Flip DNS / proxy routing for that language to the new service.
5. After two weeks of clean operation, decommission the language's
   pickle directory on the old service.

Easiest first (small wikis), English last.

### Phase 7: decommission (final week)

After all languages are cut over and stable for 30 days:

- Tear down RabbitMQ, Celery workers, Flower, Memcached, Postgres.
- Archive the pickle volumes to cold storage; delete after 90 days.
- Retire the old VPS.

Estimated total: **4–6 months of one developer's time**, longer if
they are learning Rust from scratch. The riskiest weeks are 2–6
(algorithm parity); everything else is more conventional engineering.

## 7. Testing strategy

### Parity test corpus

Built in Phase 1. The single most important artifact in this project.
Without it, we have no way to know whether the rewrite is correct.

### Property tests

Use `proptest` or `quickcheck` for invariants:

- A token's `origin_rev_id` is always ≤ all rev_ids in its `inbound` and `outbound`.
- `inbound` and `outbound` for any token alternate (a delete is followed
  by an insert which is followed by a delete…). The reference doesn't
  enforce this but it's the implied semantics.
- Re-running attribution on the same revision sequence produces an
  identical blob.

### Fuzz tests

For the diff implementation specifically: generate random pairs of
token-id arrays and verify that our Myers diff produces a valid
transcript (preserves common elements, sums to the correct lengths).

### Bench corpus

A small set of articles with known wall-clock baselines:

- Jesse Owens (6,307 revs, 85K lifetime tokens — there's a sample
  pickle at `../wikiwho_api/tmp_pickles/en/46827.p`)
- Barack Obama (the worst case)
- A randomly-sampled small article (~50 revs)
- A non-English article (`fr/Paris` or similar)

`cargo bench` against these on every PR.

### End-to-end test

A docker-compose with the rewrite service + a fake MW Action API
(serves canned responses for a small set of pages) + the Dashboard
ArticleViewer running against it. Manual smoke test before each
language cutover.

## 8. Risks

### Algorithm parity
The reference algorithm has subtle behavior that is not documented
elsewhere. See [ALGORITHM.md](ALGORITHM.md) §"Quirks to mirror".
**Mitigation:** the parity corpus from Phase 1 is the ground truth.
Do not skip Phase 1.

### Dependency on Wikipedia's REST API for whocolor HTML
If WMF deprecates `/api/rest_v1/page/html`, we need a fallback. They
have been migrating toward `/w/rest.php/v1/page/html` (same data,
different URL). **Mitigation:** abstract the HTML source behind a
trait/interface from day 1; have option B (Parsoid sidecar) as a
known-implementable fallback.

### Rate limiting against the MW Action API
The current code has elaborate Retry-After handling
(`handler.py:155–205`). The rewrite needs the same care. **Mitigation:**
put this in a shared client crate (`mediawiki-action-client`) with
tests against a mocked 429 response.

### Backfill cost on enwiki
A full bootstrap from dump takes days on the current setup. With Rust
it should be hours, but it's still real wall-clock time and disk IO.
**Mitigation:** lazy ingest means we don't need to backfill everything
before launch. We can ship with only EventStreams-driven population and
let the cache build naturally over ~30 days.

### Diff algorithm semantic differences
`difflib.Differ` has specific tie-breaking behavior on ambiguous diffs
(when the same token appears multiple times and could be matched to
multiple positions). Myers diff has different tie-breaking. This
could cause `o_rev_id` differences for tokens that appear multiple
times in a revision. **Mitigation:** if parity fails on these edge
cases, implement Differ's exact tie-breaking on top of Myers (it's
specifiable; see ALGORITHM.md). Worst case, port Differ's behavior
directly into our diff impl.

### Tokenization edge cases
`split_into_paragraphs`, `split_into_sentences`, `split_into_tokens`
in `WikiWho/utils.py` have specific regex patterns. Any difference in
these silently shifts every downstream `token_id`. **Mitigation:**
port the regexes verbatim. Property-test that the tokenizer output is
identical to the reference on a corpus of revisions.

### Operational unknowns on Wikimedia Cloud
The current service runs on a 24-core / 122 GB / 5 TB VPS. We don't
yet know what the rewrite needs. **Mitigation:** start on a smaller
VPS (8-core / 32 GB / 1 TB) for the first language cutover; size up
only if needed.

### Lone-maintainer risk
The current service has no deep maintainer; the rewrite shouldn't
either. **Mitigation:** the Rust codebase will be ~10× smaller than
the current Django stack; document everything in the repo (this is
that documentation); keep dependencies minimal.

## 9. What to do first

Concrete first-week tasks for the next session:

1. **Skim the reference implementation.** Read [README.md](README.md),
   [ALGORITHM.md](ALGORITHM.md), [API.md](API.md), [STORAGE.md](STORAGE.md)
   in this repo. Then read `../wikiwho_api/lib/WikiWho/WikiWho/wikiwho.py`
   and `../wikiwho_api/lib/WikiWho/WikiWho/utils.py` end-to-end with the
   spec open. The algorithm is ~700 lines; the tokenizer is ~100. This
   should take ~half a day.
2. **Stand up a Cargo workspace** in this directory. Suggested initial
   crates:
   - `wikiwho-attribute` (algorithm library)
   - `wikiwho-storage` (blob format library)
   - `wikiwho-mwclient` (MW Action API + Wikipedia REST client)
   - `wikiwho-server` (axum HTTP service)
   - `wikiwho-ingest` (dump bootstrap + EventStreams listener)
   - `wikiwho-parity` (parity-check binary against snapshots)
3. **Build the parity test fixture corpus.** Write a small Python script
   that takes a list of (lang, page_id, rev_id) and saves the current
   production responses to disk. Start with the 100 known-hard articles
   listed in §6 Phase 1; expand to 10,000 later. The 100 is enough for
   the first few weeks of algorithm work.
4. **Get one revision through the algorithm.** Even before any storage
   code, port `Wikiwho.__init__` and the first few revisions of
   `analyse_article` for a tiny test article. Confirm the per-token
   outputs match. This is the proof that the spec is implementable.
5. **Don't write storage code yet.** Storage is the easy part; do it
   after the algorithm is proven.

After week 1 the work parallelizes: ingestion can be drafted while the
algorithm port is still in progress, storage can be designed in parallel,
HTTP scaffolding can be tried out, etc.

[T414075]: https://phabricator.wikimedia.org/T414075
[T414087]: https://phabricator.wikimedia.org/T414087
