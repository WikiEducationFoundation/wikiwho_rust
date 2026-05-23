# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Autonomy posture (read this first)

**Claude is the primary developer of this project. Sage steers; Sage does not write Rust.** That changes how the development loop works compared to Sage's other repos:

- **The parity corpus is the human-equivalent code review.** Sage cannot read every diff and judge whether the algorithm is right. The parity corpus can. Build it before writing algorithm code, run it after every algorithm change, and treat "parity number went down" as a regression equivalent to a failing test.
- **Iterate autonomously between stop points.** Don't ask permission for routine development moves. Do narrate one sentence on what you're about to do before each substantive step (per the project-wide narration norm), and end each session with a one-line parity-status + next-step summary.
- **The metric is `(passing_tokens / total_tokens, passing_revisions / total_revisions, ms_per_revision_obama)`.** Each session's notes entry records these before → after.

### What does NOT require permission

These are pre-approved via `.claude/settings.json`; just do them:

- `cargo build / test / check / clippy / fmt / bench / doc` and `cargo run --bin <anything in this workspace>`
- Edits anywhere under `wikiwho_rust/` (this repo) including creating crates, modules, fixtures, scripts, notes
- Reading anywhere under `../wikiwho_api/` (the reference implementation — read-only, never edit)
- Reading anywhere under `../WikiEduDashboardTwo/` and `../impact-visualizer/` (consumer code, also read-only)
- `python3` / `pip install --user` for capture / analysis scripts kept under `scripts/`
- WebFetch / curl to `wikiwho-api.wmcloud.org`, `*.wikipedia.org`, `*.wikimedia.org`, `dumps.wikimedia.org`
- `git add / commit` (commits accumulate locally; pushing is a separate gate)
- `git status / diff / log / branch / checkout / merge` within this repo

### What DOES require permission (always stop and ask)

- **Scope changes.** Adding endpoints not in API.md, changing the wire format, adding a new consumer, dropping a load-bearing constraint, choosing a different storage format. The plan documents the agreed scope; widening it is Sage's call.
- **Forks the plan didn't anticipate.** The "Resolved" stamps in PLAN.md / ALGORITHM.md / STORAGE.md close out the documented either/ors. Anything new — a third diff algorithm option, a different on-disk magic-number scheme, a new vandalism heuristic — gets queued in `notes/decisions-needed.md` and surfaced at the next interaction.
- **Anything destructive or externally visible.** `git push`, `cargo publish`, `cargo install` of system tools, deploying to Wikimedia Cloud, hitting the *production* wikiwho-api beyond read-only captures, modifying anything under `../wikiwho_api/` or other sibling repos, force-pushes, rewriting history.
- **Anything that would write to production Wikipedia.** This project only *reads* Wikipedia data. If you ever find yourself reaching for an authenticated MW write endpoint, you've gone wrong — stop.

### Resolved decisions (don't revisit without surfacing first)

The plan documents several option-without-pick situations. These are now closed; the rationale and bail-out conditions live at the linked locations:

| Decision | Pick | Where it's stamped |
|---|---|---|
| Implementation language | Rust (Go as bail-out, never Python) | PLAN.md §3 |
| Diff algorithm | Python `difflib.Differ` port (Ratcliff/Obershelp); Myers kept compiled for a possible later revisit | ALGORITHM.md §6 + notes/diff-algorithm-revisit.md |
| WhoColor HTML source | MW REST `/page/html` + `html5ever` injection (Option A) | PLAN.md §4.6 |
| Hash-table persistence | Strategy B, wholesale-rewrite initially, delta-log later | STORAGE.md §4 |

If a parity failure or perf wall makes one of these no longer the right pick, add an entry to `notes/decisions-needed.md` and stop. Don't silently switch.

### Session log and decision queue

Every working session writes a short note to `notes/YYYY-MM-DD-<topic>.md`:

- One-sentence framing of what was attempted
- Parity numbers before → after (or "N/A: no algorithm changes")
- New crates / files created
- Anything queued to `notes/decisions-needed.md`
- One sentence on the next session's likely starting point

`notes/decisions-needed.md` is an append-only queue of forks that need Sage's call. Each entry: brief context, the candidate options, the recommendation, and a "blocking" / "non-blocking" tag. Sage's next interaction starts by reading this file.

### Quality bar

- **Algorithm code: parity-or-die.** A change that lowers the parity number is a regression. If a refactor is correct but Myers tie-breaking shifts one fixture by a single token, that's worth investigating — don't accept regressions for cleanliness.
- **Non-algorithm code: clippy clean, tests pass, no `unwrap()` in non-test code without a comment justifying it.** Standard Rust hygiene.
- **No premature abstraction.** PLAN.md proposes a 6-crate workspace; create each crate when there's enough code to justify it, not before. The algorithm crate goes first and may live alone for weeks.
- **Don't write new `.md` docs unless they're load-bearing for future sessions.** Notes go in `notes/`. Updates to the existing five docs (`PLAN.md`, `ALGORITHM.md`, `API.md`, `STORAGE.md`, this file) are fine when warranted.

### Commits and PRs

- Commit liberally as work progresses. Use conventional-commit-ish subjects under 70 chars. Body explains *why* when the diff doesn't.
- **Never push without being asked.** Local commits are fine; remote isn't.
- Don't open PRs unless asked. There may not even be a remote configured at first.
- Include `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>` in commit message bodies.

## Repository status

Active implementation. **Always read the newest file under `notes/` before starting work** — it captures parity numbers, work in flight, queued decisions, and the next session's recommended starting point. Each session ends with a fresh `notes/YYYY-MM-DD-<topic>.md` entry; the chain plus `git log` is the project's running narrative.

`git log --oneline | head` gives the high-level trajectory at a glance.

### Parallel workstreams

The main development thread (this `CLAUDE.md`'s posture) drives algorithm / storage / server / ingest work. A second workstream — **growing the parity corpus** — is parallelizable and runs from a different work queue:

- **Queue file:** `notes/parity-corpus-wishlist.md` with rough-priority-ordered article entries and a playbook for capture → python_replay → validate → commit.
- **Scope:** the corpus agent should touch only the wishlist file, `notes/decisions-needed.md`, captured fixtures (gitignored), and rarely `scripts/*.py`. It must **not** touch crates under `crates/` — if a fixture exposes a port bug or design fork, the corpus agent files an entry in `notes/decisions-needed.md` for the main thread to pick up.
- **Coordination:** the wishlist's `claimed-<tag>` status entries are how multiple corpus agents avoid double-processing. The main thread can ignore the wishlist's churn entirely.

## What this project is

A planned from-scratch rewrite of [WikiWho](https://wikiwho-api.wmcloud.org/) — a per-token authorship attribution service for Wikipedia articles — that backs four production consumers: the Wiki Education Dashboard's ArticleViewer, the Impact Visualizer, the WhoWroteThat gadget (Wikimedia), and XTools' Authorship + Blame tools (Wikimedia). The current production implementation lives at `../wikiwho_api` (Django 1.11 / Python 2-3 hybrid / Celery / RabbitMQ / Memcached / Postgres / ~5 TB of gzipped pickles across three Cinder volumes on Wikimedia Cloud). The rewrite targets a single statically-linked Rust binary with mmap'd columnar storage.

## Reading order for the strategy docs

The four design docs cross-reference each other heavily; reading any one in isolation leaves load-bearing context missing.

1. **README.md** — orientation + pointers into the reference implementation
2. **PLAN.md** — scope, architecture, migration phases, risks, week-by-week plan, *resolved* language + HTML-source decisions
3. **ALGORITHM.md** — algorithm spec with line refs into `wikiwho.py`. Parity with this spec is *the* load-bearing constraint. Includes *resolved* diff-algorithm decision.
4. **API.md** — wire format the rewrite must serve identically
5. **STORAGE.md** — on-disk blob format with *resolved* hash-table persistence strategy

## Source-of-truth pointers (outside this directory)

"The reference implementation" in the docs means files in `../wikiwho_api/`. The hot files for algorithm work:

- `../wikiwho_api/lib/WikiWho/WikiWho/wikiwho.py` — the ~700-line core algorithm
- `../wikiwho_api/lib/WikiWho/WikiWho/utils.py` — tokenizer (regex-based, ~100 lines)
- `../wikiwho_api/wikiwho/wikiwho_simple.py` — response builders that shape the wire format
- `../wikiwho_api/api/handler.py` — MW Action API client, paging, 429/Retry-After handling
- `../wikiwho_api/whocolor/handler.py` — Parsoid integration + HTML span injection

Downstream consumers (read-only — these are who must keep working byte-for-byte after cutover):

- `../WikiEduDashboardTwo/app/assets/javascripts/components/common/ArticleViewer/`
- `../impact-visualizer/lib/wiki_who_api.rb` + `app/services/{article_token_service,timepoint_service}.rb`
- WhoWroteThat lives at gerrit.wikimedia.org/r/wikipedia/gadgets/WhoWroteThat (not checked out locally)
- XTools lives at github.com/wikimedia/xtools; the only call site is `src/Repository/AuthorshipRepository.php::getData()`. Calls endpoint (2) or (3) from API.md with `?o_rev_id={true|false}&editor=true&token_id=false&out=false&in=false` (a strict subset of Dashboard's params).

## Load-bearing constraints (do not let these drift)

These are decisions only discoverable by reading hundreds of pages of docs and Python; the bug class is silent downstream breakage, not loud test failures.

- **Token-for-token algorithmic parity** with the Python reference, validated against the captured snapshot corpus under `parity-fixtures/`. Without parity, Impact Visualizer word counts and Dashboard token coloring silently produce wrong output.
- **Tokenization regexes ported verbatim** from `WikiWho/utils.py`. Any tokenizer drift shifts every downstream `token_id`.
- **Text is lowercased before tokenization** (`wikiwho.py:123`). All token strings in output JSON are lowercase, even proper nouns. Deliberate.
- **Identical wire format** on the endpoints in API.md §1–8, including curiosities like `revisions` being a list-of-single-key-objects in `rev_content` responses, and the `success` boolean in the body being the real success indicator while HTTP status is often 200 even on `success: false`.
- **Hash table state (`paragraphs_ht` / `sentences_ht`) must persist across updates** (STORAGE.md §4). Rebuilding on demand from only the previous revision is wrong.
- **`{lang}` is a wiki code, not an i18n setting.** Drop Django's `i18n_patterns` machinery; just put it in the path.

## Planned crate layout (when code starts)

From PLAN.md §9. Create each crate when there's code to put in it, not before:

- `wikiwho-attribute` — algorithm library (no HTTP, no storage; testable in isolation)
- `wikiwho-storage` — blob format read/write/append/compact
- `wikiwho-mwclient` — MW Action API + Wikipedia REST client
- `wikiwho-server` — axum HTTP service
- `wikiwho-ingest` — dump bootstrap + EventStreams SSE listener
- `wikiwho-parity` — parity-check binary against captured production snapshots

## Operational context

The current production service runs on a 24-core / 122 GB / 5 TB Wikimedia Cloud VPS (`../wikiwho_api/WIKIMEDIA_VPS_SETUP.md`). The rewrite will deploy to Wikimedia Cloud with replica DB access — useful for sub-millisecond title→page_id resolution that currently requires a 150–300 ms MW API round-trip (PLAN.md §4.5). The replicas do NOT expose revision text; dumps and the Action API remain the only content sources.

Cutover is per-language. The current service stays up for languages not yet migrated. Easiest first (small wikis), English last.
