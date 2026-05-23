# wikiwho_rust

A planned rewrite of the [WikiWho](https://wikiwho-api.wmcloud.org/) service that
backs four production consumers:

- [Wiki Education Dashboard](https://github.com/WikiEducationFoundation/WikiEduDashboard)
  (`ArticleViewer` component — uses the `whocolor` and `rev_content` endpoints
  for token-level authorship highlighting)
- [Impact Visualizer](https://github.com/WikiEducationFoundation/impact-visualizer)
  (uses `rev_content/rev_id/{rev_id}/` for token counts and authorship)
- [Who Wrote That](https://www.mediawiki.org/wiki/Who_Wrote_That%3F) gadget
  (uses the `whocolor` endpoint)
- [XTools](https://xtools.wmcloud.org/) — its Authorship and Blame tools both
  route through `AuthorshipRepository::getData()`, calling
  `rev_content/{title}[/{rev_id}]/` with `o_rev_id={true|false}&editor=true`
  (no `token_id` / `in` / `out`). Confirmed via the May 2026 consumer survey;
  see commit history for details.

The current implementation lives in `../wikiwho_api` (Django 1.11 + Python
2/3 hybrid + gzipped pickle files on disk + Celery + RabbitMQ + Memcached +
Postgres + Wikimedia OAuth + 67 languages × pickle volume). It works, but it
is expensive in CPU, RAM, disk, and human attention, and there is no longer
anyone who knows the codebase deeply.

This directory contains the **plan** for a from-scratch rewrite. No code yet.

## What's here

| File | Purpose |
|------|---------|
| [PLAN.md](PLAN.md)         | The main strategy doc — scope, language choice, architecture, migration, risks, first-week tasks. **Start here.** |
| [ALGORITHM.md](ALGORITHM.md) | Specification of the WikiWho attribution algorithm with line-level references into the reference implementation. Parity with this spec is load-bearing. |
| [API.md](API.md)           | Wire-format spec for the endpoints the four consumers actually call. Backward compatibility constraints. |
| [STORAGE.md](STORAGE.md)   | Proposed on-disk format to replace the current Python pickles. |

## Source-of-truth pointers

When the plan refers to "the reference implementation" it means files in
`../wikiwho_api`. Key files to keep open while implementing:

- `../wikiwho_api/lib/WikiWho/WikiWho/wikiwho.py` — the core algorithm (one
  ~700-line file; this is what we are reimplementing)
- `../wikiwho_api/lib/WikiWho/WikiWho/utils.py` — tokenization, paragraph/
  sentence splitting, hash function, vandalism helpers
- `../wikiwho_api/lib/WikiWho/WikiWho/structures.py` — the Python data classes
  (`Word`, `Sentence`, `Paragraph`, `Revision`); reveals what data the
  algorithm carries
- `../wikiwho_api/wikiwho/wikiwho_simple.py` — the thin subclass that adds
  `get_revision_content`, `get_revision_min_content`, `get_all_content`,
  `get_deleted_content`, `get_revision_ids`, `get_whocolor_data`. These are
  the response builders that shape the wire format.
- `../wikiwho_api/api/handler.py` — Wikipedia API client, rvcontinue paging,
  Retry-After/429 handling, pickle loading, cache key locking
- `../wikiwho_api/api/views.py`, `../wikiwho_api/api/urls.py` — endpoint
  routing and parameter parsing
- `../wikiwho_api/whocolor/handler.py`, `../wikiwho_api/whocolor/utils.py` —
  the HTML annotation pipeline (Parsoid call → markup injection → HTML)
- `../wikiwho_api/api/utils_pickles.py` — current storage layout, gzip
  compression, file locking
- `../wikiwho_api/api/events_stream.py`, `../wikiwho_api/api/utils_celery.py`,
  `../wikiwho_api/api/tasks.py` — EventStreams ingestion path
- `../wikiwho_api/wikiwho_api/settings_wmcloud.py` — production settings
  (per-language pickle volumes, throttle rates, allowed hosts)
- `../wikiwho_api/wikiwho_api/settings_base.py` — base settings, the full
  list of supported languages (~67)
- `../wikiwho_api/WIKIMEDIA_VPS_SETUP.md` — how the current service is
  deployed on Wikimedia Cloud (Cinder volumes, systemd units, dump bootstrap
  procedure)

## Downstream-consumer code

- Dashboard URL builder: `../WikiEduDashboardTwo/app/assets/javascripts/components/common/ArticleViewer/utils/URLBuilder.js`
- Dashboard API client: `../WikiEduDashboardTwo/app/assets/javascripts/components/common/ArticleViewer/utils/ArticleViewerAPI.js`
- Impact Visualizer client: `../impact-visualizer/lib/wiki_who_api.rb`
- Impact Visualizer callers: `../impact-visualizer/app/services/article_token_service.rb`, `.../timepoint_service.rb`

Who Wrote That is not checked out locally; its repository is at
[gerrit.wikimedia.org/r/wikipedia/gadgets/WhoWroteThat](https://gerrit.wikimedia.org/r/admin/repos/wikipedia/gadgets/WhoWroteThat).
It is a Wikipedia gadget that fetches the `whocolor` endpoint and renders
its `extended_html` and token data in-page.

XTools is also not checked out locally; its repository is at
[github.com/wikimedia/xtools](https://github.com/wikimedia/xtools). The
only WikiWho call site in the whole codebase is `AuthorshipRepository::getData()`
(`src/Repository/AuthorshipRepository.php`), which both the Authorship
and Blame tools route through; both call only endpoint (2) or (3) from
`API.md` (`rev_content/{title}[/{rev_id}]/`) with a strict subset of
Dashboard's query params.

## Status

Active development. Two crates land so far:

- **`wikiwho-attribute`** — algorithm port + wire-format response
  builder. Full Differ-based cascade in place. **100 % full-history
  parity vs the Python reference** on every captured fixture
  (en/Photosynthesis, en/Israel-Hamas war, en/Adolf_Hitler, en/Paris,
  en/Jesse_Owens, de/Berlin, ar/Cairo, simple/Wikipedia, zh/中国 —
  collectively ~77 k revisions). `response::build_rev_content`
  produces the API.md §1-6 JSON envelope from in-memory `Article`
  state; validated structurally against `python_replay.json` fixtures.
- **`wikiwho-mwclient`** — async MW Action API client (tokio + reqwest).
  Revision-history fetcher with 429-Retry-After-honoring + 5xx
  exponential backoff. `capture-history` binary is a drop-in
  replacement for `scripts/capture_history.py` (round-trip-tested
  on existing fixtures).

Still to come (per [PLAN.md §9](PLAN.md)): `wikiwho-storage`,
`wikiwho-server`, `wikiwho-ingest`.

Supporting machinery: the `wikiwho-parity` binary (which has both
`--full-history` and `--python-replay` ground-truth modes), five
strategy docs (this README + PLAN/ALGORITHM/API/STORAGE), a
[CLAUDE.md](CLAUDE.md) with the autonomy posture for Claude-driven
iteration, `.claude/settings.json` with development-loop permissions,
the parity-fixture capture scripts under `scripts/`, the fixtures
themselves under `parity-fixtures/` (gitignored — regeneratable from
production wikiwho-api), and `notes/` for session logs +
decisions-needed queue.

Next session should read [CLAUDE.md](CLAUDE.md) first, then the latest
file under `notes/` for current parity numbers + next-step
recommendation.

## A note on language choice and the author's Rust experience

The plan recommends Rust. The author of this plan (Sage) does not know
Rust. PLAN.md §3 ("Language choice") discusses why Rust is recommended
anyway, what the alternatives look like (Go, Python+Cython), and the
specific characteristics of this workload that drive the recommendation. If
a fresh look says Go is the right call, that's fine — the rest of the plan
is largely language-independent.
