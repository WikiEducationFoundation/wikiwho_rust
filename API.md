# Wire format — what the consumers expect

This document specifies the HTTP API the rewrite must serve. Only the
endpoints actually called by the four production consumers (Dashboard,
Impact Visualizer, WhoWroteThat, XTools) are specified; everything else
is out of scope (see PLAN.md §2).

## Versioning

The current service has versioned URL paths: `v1.0.0-beta` and
`v1.0.0`. Both must continue to resolve, with identical behavior. The
rewrite should treat them as aliases.

## Languages

The `{lang}` URL segment is a wiki code (`en`, `de`, `fr`, `simple`,
`zh`, `sh`, `als`, etc. — the full list is in
`../wikiwho_api/wikiwho_api/settings_base.py:LANGUAGES`). The rewrite
should accept any language code that resolves to an existing wiki
(`{lang}.wikipedia.org`), not just an allowlist.

## Authentication

None. Anonymous read-only. The current service nominally requires
`IsAuthenticatedOrReadOnly` but in practice every endpoint is read-only
so authentication never fires. Drop the auth chain in the rewrite.

## CORS

Allow all origins on GET requests (`CORS_ORIGIN_ALLOW_ALL = True` in
the current settings). The data is public; the consumers run on
arbitrary domains (Dashboard at dashboard.wikiedu.org and Programs &
Events Dashboard, Impact Visualizer at impact.wikiedu.org, WhoWroteThat
inline on every Wikipedia page).

## Rate limiting

The current service has:

- Global anon: `100/sec` (was `2000/day`)
- Burst: `100/sec` (was `60/min`)
- Per-user-agent overrides via the `OVERRIDE_THROTTLE_RATES` setting:
  - `XTools`: `10000/sec`
  - `WhoWroteThat`: `3000/minute`

The rewrite should implement equivalent throttling. Recommendation:
token bucket per IP with a configurable overrides table keyed by
User-Agent prefix.

**Note on override key:** the current service keys overrides off the
Django *username* (`api/views.py:93-98`), not the User-Agent. XTools
sends no auth header (its `OVERRIDE_THROTTLE_RATES['XTools']` override
has never actually fired in production), so switching to UA-prefix
matching is a deliberate strict improvement, not parity drift. See
PLAN.md §4.3.

---

## Endpoints

### 1. `GET /{lang}/api/v1.0.0-beta/rev_content/rev_id/{rev_id}/`

The endpoint Impact Visualizer uses.

**Path parameters:**
- `lang` — wiki code
- `rev_id` — revision id (integer)

**Query parameters (all opt-in booleans, `=true` to include):**
- `o_rev_id` — include `o_rev_id` field on each token
- `editor` — include `editor` field on each token
- `token_id` — include `token_id` field
- `in` — include `in` (inbound) field
- `out` — include `out` (outbound) field

Reference implementation: `WikiwhoApiView.get_rev_content_by_rev_id`
(`api/views.py:313`), parameter parsing in
`WikiwhoApiView.get_parameters` (`api/views.py:188`), response builder
`Wikiwho.get_revision_content` (`wikiwho/wikiwho_simple.py:23`).

**Response (200):**

```json
{
  "article_title": "Barack_Obama",
  "page_id": 534366,
  "success": true,
  "message": null,
  "revisions": [
    {
      "1212345678": {
        "editor": "12345",
        "time": "2024-03-14T15:09:26Z",
        "tokens": [
          {
            "str": "barack",
            "o_rev_id": 9876,
            "editor": "999",
            "token_id": 0,
            "in": [],
            "out": []
          },
          {
            "str": "obama",
            "o_rev_id": 9876,
            "editor": "999",
            "token_id": 1,
            "in": [],
            "out": []
          }
        ]
      }
    }
  ]
}
```

Notes:
- The `revisions` array contains exactly one entry; the entry is an
  object with a single key — the rev id as a *string* — whose value is
  `{editor, time, tokens}`. This odd shape is preserved for backward
  compatibility.
- `editor` at the revision level is the string id of whoever made the
  revision; `editor` on each token is the editor of the token's
  *origin* revision. They are NOT the same thing.
- `time` is the timestamp of the revision; format matches what the MW
  API returns (`YYYY-MM-DDTHH:MM:SSZ`).
- Tokens are in **document order** (the order they appear in the
  revision).
- Token `str` values are **lowercased**. This is a deliberate algorithm
  property, not a bug. Consumers must not rely on case.
- Fields not requested via query parameters are omitted from token
  objects. `str` is always present (it's the default).
- The `o_rev_id` of a token's first revision is the same as the
  enclosing revision id.

**Response (200, error):**

```json
{"Error": "Revision ID (1234567) does not exist or is spam or deleted!"}
```

Status: 400. Used when the requested revision is in the article's
history but was flagged as spam by the algorithm, or simply doesn't
exist.

**Response (200, not-yet-processed):**

```json
{"Info": "Process took more than 240 seconds. Requested data will be available soon (Max 300 seconds). Please try again later."}
```

Status: 408. Used when the algorithm exceeded the per-request timeout
and processing was kicked off as a background task. The Impact
Visualizer client treats this as "skip this article" (its
`get_revision_tokens` returns nil on 408).

In the rewrite, this case should be much rarer because the warm path
is fast and the lazy-build path can complete quickly for most articles.
When it does happen (a cold Barack Obama request), return 408 with the
same shape.

### 2. `GET /{lang}/api/v1.0.0-beta/rev_content/{title}/`

Latest revision, by article title.

Used by:
- Dashboard's `wikiwhoColorRevisionURL()` (`URLBuilder.js:48`) with
  `?o_rev_id=true&editor=true&token_id=true&out=true&in=true`.
- XTools' Authorship + Blame tools (`AuthorshipRepository::getData()`
  in the `wikimedia/xtools` repo) with
  `?o_rev_id={true|false}&editor=true&token_id=false&out=false&in=false`
  — a strict subset of Dashboard's params, so no new shape requirements.

**Path parameters:**
- `lang` — wiki code
- `title` — URL-encoded article title (note: titles can contain `/`;
  see the workaround in (8) below)

Same query parameters and response shape as (1), except `revisions`
contains the latest revision the service knows about.

Reference: `WikiwhoApiView.get_rev_content_by_title` (`api/views.py:335`).

### 3. `GET /{lang}/api/v1.0.0-beta/rev_content/{title}/{rev_id}/`

A specific revision of a specifically-titled article. Used by XTools'
Blame tool (same `AuthorshipRepository::getData()` code path as (2),
with a non-null `rev_id`).

Reference: `WikiwhoApiView.get_article_rev_content` (`api/views.py:330`).

Same shape as (1).

### 4. `GET /{lang}/api/v1.0.0-beta/rev_content/page_id/{page_id}/`

Latest revision, by page id. Useful when title resolution is awkward.

Reference: `WikiwhoApiView.get_rev_content_by_page_id` (`api/views.py:339`).

Same shape as (1).

### 5. `GET /{lang}/api/v1.0.0-beta/latest_rev_content/{title}/`
### 6. `GET /{lang}/api/v1.0.0-beta/latest_rev_content/page_id/{page_id}/`

Aliases for (2) and (4). Same behavior. Keep them for backward compat.

### 7. `GET /{lang}/whocolor/v1.0.0-beta/{title}/{rev_id}/`
### 8. `GET /{lang}/whocolor/v1.0.0-beta/{title}/`

The endpoint Dashboard's `wikiwhoColorURL()` (`URLBuilder.js:35`) and
WhoWroteThat both use.

(8) is the latest-revision shortcut. Reference: `whocolor/views.py:60`
and `whocolor/urls.py`.

**Special case for titles containing `/`:** the URL-router can't
disambiguate `Foo/Bar` (a title with a slash) from `Foo` with `rev_id=Bar`.
The current workaround is to pass `0` as the rev_id when the title
contains a slash:
`/{lang}/whocolor/v1.0.0-beta/Post-9%2F11/0/?origin=*`. The handler
treats `rev_id == 0` as "no rev_id given" (`whocolor/views.py:67`).
Mirror this in the rewrite.

**Query parameters:**
- `origin=*` — historically a CORS workaround. The handler ignores it,
  but the consumers send it. The rewrite should ignore it too.

**Response (200, success):**

```json
{
  "extended_html": "<div class=\"mw-parser-output\"><p><span class=\"token-author-12345\">Barack</span> ...",
  "present_editors": [["EditorName1", "12345"], ["EditorName2", "67890"]],
  "tokens": [
    [3, "barack", 9876, [], [], "999", 12345678.5]
  ],
  "revisions": {
    "9876": ["2004-03-19T15:00:00Z", 0, "999", "OriginalEditor"],
    "1234": ["2004-04-01T10:00:00Z", 9876, "abc123def...", "AnotherEditor"]
  },
  "biggest_conflict_score": 17,
  "success": true,
  "rev_id": 1212345678,
  "page_title": "Barack_Obama"
}
```

Reference: `whocolor/handler.py:50` and
`wikiwho/wikiwho_simple.py:get_whocolor_data` (lines 362–414).

**Field by field:**

- `extended_html` — HTML rendering of the revision (the same HTML
  Wikipedia would show), with token-level `<span>` wrappers. The exact
  CSS class names matter for the Dashboard and WWT UIs. Classes are
  derived from `editor`:
  - Registered users: class is the user_id as a string
  - Anons: class is `md5(`'0|<name>'`)` (lowercase hex digest)
  - The current implementation passes this through `WhoColor.parser.WikiMarkupParser`
    + Parsoid; see PLAN.md §4.6 for the recommended rewrite approach.
- `present_editors` — flat list of `[name, class_name]` pairs for each
  editor whose tokens are still present in this revision. Used for
  Dashboard's editor sidebar.
- `tokens` — array-of-arrays in a fixed order:
  `[conflict_score, str, o_rev_id, in, out, class_name, age_seconds]`.
  - `conflict_score` — integer count of editor-vs-editor conflicts on
    this token; algorithm at `wikiwho_simple.py:373–389`
  - `str` — token value (lowercased)
  - `o_rev_id` — origin revision id
  - `in` — list of rev ids where token was reintroduced
  - `out` — list of rev ids where token was deleted
  - `class_name` — same as the editor class derived above
  - `age_seconds` — `(now - origin_revision_timestamp).total_seconds()`
    as a float.
- `revisions` — dict keyed by rev_id (as string), value is a 4-tuple
  `[timestamp, parent_rev_id, class_name, editor_name]`.
  - `parent_rev_id` is the *previously-processed* revision id (so
    spam-skipped revisions don't appear in the chain).
- `biggest_conflict_score` — max `conflict_score` seen.

**Response (200, in progress):**

```json
{
  "info": "Requested data is not currently available in WikiWho database. It will be available soon.",
  "success": false,
  "rev_id": 1212345678,
  "page_title": "Barack_Obama"
}
```

The Dashboard retries up to 5 times with exponential backoff
(`ArticleViewerAPI.js:107–151`); a 5-minute client-side cooldown is
applied after exhaustion.

**Response (200, vandalism):**

```json
{
  "info": "Requested revision (1212345678) is detected as vandalism by WikiWho.",
  "success": false,
  "rev_id": 1212345678,
  "page_title": "Barack_Obama"
}
```

**Response (400/503, error):**

```json
{
  "error": "<message>",
  "success": false,
  "rev_id": null,
  "page_title": "Barack_Obama"
}
```

### 9. Non-mainspace / ephemeral processing

The current production service only stores mainspace (namespace 0)
articles. The rewrite extends coverage to other namespaces (Talk:,
User:, User_talk:, Wikipedia:, etc.) via on-demand processing: fetch
the revision history, run the algorithm, return the response, discard
the in-memory `Article` (or cache it in a memory-only LRU). Nothing
hits durable storage.

The trade-off: we don't carry the full revision history for
high-revision pages, so the algorithm sees only a recent window. For
talk pages and most non-mainspace content this is fine — the use case
is "who's contributing to the discussion now," not "who originally
wrote this token in 2007."

#### Endpoint URLs

Same routes as mainspace, with the namespace-prefixed title:

- `GET /{lang}/api/v1.0.0-beta/rev_content/Talk:Photosynthesis/`
- `GET /{lang}/api/v1.0.0-beta/rev_content/User:Foo/Sandbox/`
- `GET /{lang}/whocolor/v1.0.0-beta/Wikipedia:Village_pump/`

No new route schemas. The server determines whether a page is
mainspace or not by passing the URL-decoded title to the MW Action
API and using the returned `ns` field. Mainspace (`ns: 0`) goes
through the durable storage path; everything else goes through the
ephemeral path.

This means non-Latin namespace names work transparently
(`de:Diskussion:Photosynthese`, `ja:トーク:光合成`, etc.) — we never
parse namespace prefixes ourselves.

#### Window of analysis

The ephemeral path replays at most **N** revisions ending at the
target (default: **500**). When the page has fewer than N revisions,
attribution is complete and indistinguishable from the durable path
for that page. When the page has more than N revisions, attribution
is *relative to the start of the window*: tokens that existed at the
window's first revision get `o_rev_id = window_start_rev_id`, not
their true introduction rev id.

Override via query parameter:

- `window=N` — replay the last N revisions (default 500, capped at
  2000 to bound per-request latency)
- `window=full` — replay the entire history. Latency unbounded; use
  with caution. The server may reject this with 400 for pages over
  a configurable revision count (default 10 000) to protect the MW
  API quota.

#### Response envelope additions

To keep strict byte-for-byte parity with current production on
mainspace endpoints, the four ephemeral-mode fields are **only present
when the response was built from an on-demand replay** (`ns ≠ 0`, or an
explicit `window=N` query). Mainspace storage-backed responses are
unchanged from (1)–(6).

```json
{
  "article_title": "Talk:Photosynthesis",
  "page_id": 1234567,
  "namespace": 1,
  "ephemeral": true,
  "window_start_rev_id": 123456000,
  "window_revisions": 500,
  "success": true,
  "message": null,
  "revisions": [...]
}
```

- `namespace` — the MW namespace number (1 = Talk, 2 = User, etc.).
  Absent on mainspace responses (consumers already know it's ns 0
  from the URL they sent).
- `ephemeral` — always `true` when present. Absent on mainspace
  responses, which means the same thing as `false` would.
- `window_start_rev_id` — the first rev id the algorithm saw, or
  `null` for `window=full`. Consumers interpret `o_rev_id ==
  window_start_rev_id` to mean "token was already present at the
  start of the analysis window — true origin unknown."
- `window_revisions` — the actual number of revs replayed (≤ `window`
  query param).

Tokens carrying `o_rev_id == window_start_rev_id` are NOT semantically
the same as tokens introduced *in* `window_start_rev_id` in mainspace;
they may have been introduced much earlier. Consumers that highlight
"new tokens" should treat them as "unknown origin" rather than
attributing to that revision. The presence of the `ephemeral` field is
the signal that this reinterpretation applies.

#### Caching semantics

Ephemeral responses are cacheable in a memory-only LRU keyed by
`(lang, page_id, latest_rev_id_at_request_time, window)`. The cache
entry becomes stale when the page receives a new revision (detected
via MW's `lastrevid` or via the EventStreams listener). Default TTL
1 hour for active discussion pages.

No durable persistence — restarting the server clears the cache.

#### Latency budget

In Rust release mode, the algorithm runs at ~5 ms/rev on contemporary
hardware. Per ephemeral request:

| Window | MW fetch | Algorithm | Total p99 |
|--------|---------:|----------:|----------:|
| 500 revs (default) | ~1.5 s | ~2.5 s | ~4 s |
| 2000 revs (max) | ~5 s | ~10 s | ~15 s |
| Full (10 000-rev page) | ~30 s | ~50 s | ~80 s |

The 4-second p99 for the default case is acceptable for talk-page
hover-to-load UI patterns. The 15-second 2000-rev case is the upper
bound consumers should expect before timeout (consumer's HTTP client
should have ≥ 20-second timeout for the ephemeral path; the existing
mainspace path responds in <100 ms from cache so its timeout can be
much lower).

#### Reference: Why not durable for all namespaces

Storage budget. Talk pages on contentious articles can accumulate
50 000+ revisions; supporting them in durable storage would
significantly expand the per-language disk footprint (STORAGE.md §5
estimates the current production layout at 224 KB/article compressed,
already filling 2 TB on en alone). Ephemeral processing lets us
serve the same data without adding to that budget, accepting a few
seconds of cold-cache latency in exchange.

If a specific non-mainspace namespace turns out to be high-traffic
enough that ephemeral processing's MW API hit rate becomes a problem,
we can selectively flip it to durable storage per namespace — the
storage format doesn't care which namespace it's serving.

## URL routing quirks

The current URL patterns have several gotchas; mirror them:

1. **Slashes in titles** are accepted because the URL patterns use
   `(?P<article_title>.+)` (greedy). The router disambiguates
   `rev_content/Foo/1234/` from `rev_content/Foo/Bar/` by requiring
   rev_id to have ≥ 5 digits — see `api/urls.py:25`:
   `^rev_content/(?P<article_title>.+)/(?P<start_rev_id>[0-9]{5,})/…`.
   In the rewrite, prefer explicit query parameters when a title has
   a slash, but keep the 5-digit-rev-id heuristic for backward
   compat.
2. **Colons in titles** are legal both inside mainspace (e.g.
   `Mission:_Impossible`, `Pride_and_Prejudice:_The_Wild_and_Wanton_Edition`,
   `Star_Trek:_The_Next_Generation`) and as namespace prefixes
   (`Talk:Photosynthesis`, `User:Foo`). MediaWiki interprets the prefix
   before the first `:` as a namespace **only if it matches a known
   namespace name on that wiki** — otherwise the whole thing stays in
   mainspace. The rewrite must not try to parse this locally: pass the
   URL-decoded title straight to the MW Action API (`titles=` param)
   and use the canonical namespace + page_id it returns. The same is
   true for non-Latin-script wikis where namespace names are translated
   (e.g. `de:Diskussion:`, `ja:トーク:`).
3. **Trailing slash matters.** All URLs end with `/`. Requests without
   the trailing slash currently 301 to the slashed form. Preserve this.
4. **The `{version}` segment** is currently `v1.0.0-beta` OR `v1.0.0`.
   Both must work.

## Error codes

The current implementation uses these HTTP status codes; the rewrite
should match them:

| Status | Meaning |
|--------|---------|
| 200 | Success, or "still processing" (with `success: false` in body), or "vandalism" |
| 400 | Bad request (invalid page_id, invalid rev_id, missing revision in history) |
| 408 | Timeout — algorithm took too long; processing kicked off async |
| 503 | Upstream Wikipedia API error or rate-limited |

The `success` boolean in the body is the *real* success indicator;
status codes are sometimes 200 even when `success: false`. Don't
change this — the consumers depend on it.

## What the rewrite can simplify

These fields/behaviors exist in the current implementation but
**aren't used by any of the four consumers**. Safe to omit or simplify
in the rewrite if doing so saves work:

- `message` field — currently always `null`. Could drop.
- `revisions` (in the rev_content response) being a list of single-key
  objects rather than just the object — this is a wire-format
  curiosity. **Keep it** for backward compat; the consumers parse it.
- Per-language URL i18n routing — the current Django setup uses
  `i18n_patterns` to route `{lang}` as a Django language code, which
  also enables admin/account UI translations. The rewrite can just
  treat `{lang}` as a parameter; no Django i18n needed.

## What the rewrite does NOT need to serve

(For PLAN.md §2 reasons — these endpoints exist but no consumer calls
them; they should 404 in the rewrite.)

- `/api/.../all_content/...`
- `/api/.../range_rev_content/...`
- `/api/.../rev_content/{title}/{start}/{end}/` (range form)
- `/api/.../rev_ids/...`
- `/edit_persistence/...`, `/api_editor/...`
- `/account/...`, `/admin/...`, `/contact/...`, `/download/...`
- `/sitemap.xml`, `/robots.txt` (the rewrite can serve `robots.txt`
  trivially if desired)
- Browsable HTML for any endpoint — JSON only.

## Validation against current production

Two ways to verify the rewrite matches:

1. **Snapshot diff.** Capture responses from the production service
   for the parity corpus and store them as fixtures. The rewrite must
   produce byte-identical JSON (modulo whitespace and key ordering;
   normalize both before comparing).
2. **Shadow traffic.** Mirror a small percentage of production GET
   traffic to a staged instance of the rewrite; log diffs. This is the
   final validation before each language cutover.
