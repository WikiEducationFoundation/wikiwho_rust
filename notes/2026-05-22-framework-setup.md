# 2026-05-22 — framework setup

**Goal:** lay down the framework that lets Claude iterate on the rewrite mostly autonomously between explicit stop points. Sage asked for this because he doesn't write Rust and wants the development cycle hands-off.

**Parity:** N/A — no algorithm code exists yet. This session captured the baseline corpus that future sessions will measure against.

**Done:**
- Closed out the documented options-without-pick decisions inline in PLAN.md (Rust, WhoColor=Option A), ALGORITHM.md (Myers + 0.1% escalation threshold), STORAGE.md (Strategy B with later delta-log optimization). Each carries a `> **Resolved 2026-05-22:**` stamp at the source location and is indexed in CLAUDE.md.
- Rewrote CLAUDE.md to lead with the autonomy posture (gate, stop rules, allow / deny, session-notes pattern), keeping the orientation/source-of-truth pointers below.
- `.claude/settings.json` pre-approves cargo / Python / git-without-push / curl-to-wikimedia-domains / WebFetch-to-wikimedia / read-only access to `../wikiwho_api/` and consumer repos. Denies `python -c`, `git push`, `cargo publish/install`, sudo, sweeping rm, and writes to sibling repos.
- `git init -b main`; `.gitignore` excludes `/target`, `/parity-fixtures` (regeneratable), Python noise, editor/OS noise, `settings.local.json`.
- `notes/` scaffolding with `README.md` (template + cadence) and `decisions-needed.md` (empty queue with format docs).
- `scripts/capture_fixtures.py` — Python capture script for parity fixtures from production wikiwho-api with 408/still-processing retry, polite throttling, `--refresh`, `--only`, `--extra`. Seed list = 16 articles: 11 English (the known-hard 7 + Jesse Owens / Paris / Einstein / Photosynthesis) plus fr:Paris / de:Berlin / simple:Wikipedia / zh:中国 / ar:القاهرة.
- Memory: `user_role.md` (Sage at Wiki Edu, doesn't write Rust), `feedback_autonomy.md` (parity = gate, stop-rules, why).

**Captured fixtures:** 16 articles, 367 MB total (176 MB rev_content, 190 MB whocolor).

| lang | title | page_id | rev_id | notes |
|---|---|---|---|---|
| en | Barack_Obama | 534366 | 1354984261 | 26 MB rev_content |
| en | COVID-19_pandemic | 62750956 | 1355596341 | |
| en | Israel–Hamas_war | 79023819 | 1277418181 | redirect to Gaza_war (small fixture, useful for redirect edge case) |
| en | Adolf_Hitler | 2731583 | 1354738283 | |
| en | Jesus | 1095706 | 1354664189 | |
| en | Wikipedia | 5043734 | 1355374251 | 44 MB rev_content (the largest) |
| en | Jesse_Owens | 46827 | 1355508503 | bench-corpus reference |
| en | Paris | 22989 | 1354657462 | |
| en | Albert_Einstein | 736 | 1355112534 | |
| en | Photosynthesis | 24544 | 1354638187 | |
| en | Gaza_war | 74998519 | 1355554720 | redirect target of Israel–Hamas_war |
| fr | Paris | 681159 | 236388385 | |
| de | Berlin | 2552494 | 267155005 | |
| simple | Wikipedia | 27263 | 10855732 | |
| ar | القاهرة | 4287 | 74668889 | Cairo (RTL) |
| zh | 中国 | 1686258 | 64806634 | redirect to 中國 (also useful redirect case) |

**Deferred:** two articles couldn't capture this session because production wikiwho-api couldn't build the whocolor view inside the retry window (`200 success=false: ...will be available soon`):
- `en:Donald_Trump` — Obama-scale; production was likely catching up after building Obama for us.
- `zh:中國` — small wiki, less warm cache, the *target* of the 中国 redirect.

Re-run `python3 scripts/capture_fixtures.py --only en:Donald_Trump --only zh:中國` later; production should have built these by then.

**Script bug fixed mid-session:** the original capture wrote both endpoints' bodies AFTER both fetched; a failure on the second endpoint discarded the first. Updated to write each endpoint immediately, with a `try/finally` ensuring `meta.json` lands even on partial capture. Empty directories from the two failures were `rmdir`'d so the corpus inventory is clean.

**Late addition — consumer survey:** mid-session, Sage's parallel research dropped `CONSUMER-SURVEY.md` into the working tree. The survey identified **XTools** as a fourth consumer (Authorship + Blame tools, calling `rev_content/{title}[/{rev_id}]/` via `AuthorshipRepository::getData()` with a strict subset of Dashboard's params) and flagged that the **rate-limit override key is shifting from Django username to User-Agent** — a strict improvement, not parity drift, because XTools' Guzzle client sends no auth header so its `10000/sec` override never actually fired in production. Folded into README.md (consumer list + downstream-consumer-code section), PLAN.md (consumer count, §4.3 throttle behavior note), API.md (intro count, endpoints 2/3 caller list, rate-limit note about override-key shift), and CLAUDE.md (intro + source-of-truth pointers). Survey file deleted per Sage's instruction.

**New decisions queued:** none.

**Next session likely starts with:**
1. Read CLAUDE.md and the latest `notes/` file.
2. Stand up the `wikiwho-attribute` crate (algorithm library), starting with the tokenizer. Port `WikiWho/utils.py:split_into_paragraphs / split_into_sentences / split_into_tokens / calculate_hash` verbatim. Add property-based tests that the tokenizer output matches the reference on a corpus of revision strings extracted from the captured fixtures.
3. Then port `Wikiwho.__init__` and the spam-detection heuristic (the easy first slice of the algorithm). Run the parity-check binary (yet to be built) against the fixtures and record the first non-zero parity number.

The early algorithm work has no parity gate to ratchet on yet (we need the parity-check binary first), so the first task list should also include building the parity-check binary — even a stub that loads a fixture, fakes a comparison, and reports `0% passing` is enough to bootstrap the loop.
