# Parity corpus wishlist

Articles we want to add to the parity corpus, beyond what's already
captured. This file is the work queue for a **parallel agent** running
on its own — independent of the main development thread, which moves
on storage / server / ingest while this queue grows the parity surface
over time.

## How to claim and process an item

1. Read this file. Pick the topmost row whose `status` is `pending`.
   The list is rough-priority ordered; topmost is highest-value.
2. Update that row's `status` to `claimed-<short-agent-tag>` so other
   agents don't double-process. Commit that change first (1-line
   commit, "claim X").
3. Capture, replay, validate (see the playbook below).
4. Update the row's `status` to `validated` (or `divergence` if parity
   < 100 %) and commit.
5. Loop to step 1.

If parity-check returns < 100 %, **do not** mark the entry validated.
File an entry in `notes/decisions-needed.md` with:
- The fixture identifier
- The exact parity numbers (str, o_rev_id, inbound, outbound, all-fields)
- A few sample divergent tokens via
  `parity-check --full-history --python-replay --show-field-mismatches 6 {lang}/{page_id}`
- A recommendation (port bug? historical-state artifact? Myers-vs-Differ
  fallout? something else?)

The main development thread will pick that up.

## Playbook (verbatim commands)

Replace `{lang}`, `{page_id}`, `{rev_id}` from the row. Replace
`<cap>` with the row's `max_revs` hint (or `100000` if blank).

```bash
# 1. Capture (if not already present)
./target/release/capture-history --only {lang}/{page_id} --max-revs <cap>

# 2. Generate fresh-Python ground truth
python3 scripts/python_replay.py parity-fixtures/{lang}/{page_id}/{rev_id} \
  > parity-fixtures/{lang}/{page_id}/{rev_id}/python_replay.json

# 3. Validate parity (target: 100% all-fields)
cargo run --release --bin parity-check -- {lang}/{page_id} \
  --full-history --python-replay

# 4. Validate vs production cache too (this catches historical-state drift
#    in the prod cache — divergence here is NOT a port bug, just a
#    reference-source disagreement)
cargo run --release --bin parity-check -- {lang}/{page_id} --full-history
```

If both 100 %: update the row, commit. Suggested commit message:

```
Parity corpus: add {lang}/{page_id} {title}

{revs} revs, all-fields 100% vs python_replay and vs prod-cache.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
```

## Coordination notes

- `parity-fixtures/` is gitignored — the captured `history.jsonl`,
  `python_replay.json`, etc. do not get committed. Only this wishlist
  file and any newly-discovered fixture metadata (a future `parity-fixtures-metadata/`
  if we want one) get committed.
- The parallel agent should avoid touching the algorithm crate or
  any code under `crates/`. The legitimate edits this agent makes
  are: this file, `notes/decisions-needed.md`, and (rarely)
  `scripts/*.py`. Anything else is out of scope; file an entry in
  `notes/decisions-needed.md` instead.
- Captures hit `*.wikipedia.org`; be polite. The default `--between
  0.3` is safe. Don't run more than one capture process simultaneously
  against the same `{lang}.wikipedia.org` host (the polite delay is
  per-process, not per-host).
- Long captures: Obama-class (60 k+ revs) takes about 15-30 min wall
  time. fr/Paris-class (20 k revs) takes about 10 min. Plan accordingly.

## Rate-limit coordination

**One capture process per host.** The `capture-history` binary uses a
polite per-process delay (default 300 ms between batches), but that's
*per process* — two corpus agents both hitting `en.wikipedia.org` at
the same time effectively halves the polite delay and risks MW
throttling. Before claiming any `en/*` row, run

```bash
pgrep -af "capture-history.*--only en/" ; pgrep -af "scripts/capture_history.py"
```

and wait for any existing en-targeted capture process to exit before
starting your own. Different language hosts (e.g. `ja.wikipedia.org`,
`ru.wikipedia.org`) don't share rate-limit state, so non-en captures
can run in parallel with each other AND in parallel with an existing
en capture.

Suggested first-pass order when starting fresh:

1. **Validate rows whose capture is already done** (status `pending —
   capture-only-step-2-and-3`). These need python_replay + parity-check;
   no MW API calls.
2. **Non-en language anchors** (ja, ru, es, pt, he, hi) — different
   hosts, no contention.
3. **En articles**, only after `pgrep` shows no other en capture is
   running.

## Queue

Status values: `pending`, `claimed-<tag>`, `validated`, `divergence`,
`failed-<reason>`. The `blocked-on-<thing>` status means "don't pick
this row up until the named blocker clears."

| lang | page_id | rev_id | title | est. revs | rationale | max_revs hint | status |
|---|---|---|---|---|---|---|---|
| en | 62750956 | 1355596341 | COVID-19_pandemic | 26 921 (captured) | tests CJK-tokenizer historical-state — closes the documented divergence from `notes/2026-05-22-first-parity-ratchet.md` if parity holds | (already captured) | claimed-parity-bot |
| en | 736 | 1355112534 | Albert_Einstein | ~13 k | biography; well-known size class | 30000 | blocked-on-running-en-capture (main thread is capturing this now; check `pgrep` before claiming, validate via steps 2-3 once file exists) |
| en | 74998519 | 1355554720 | Gaza_war | unknown | current-event article, heavy vandalism + revert pairs | 30000 | blocked-on-running-en-capture |
| fr | 681159 | 236388385 | Paris | unknown | fr.wikipedia anchor, non-English mainstream | 30000 | blocked-on-running-en-capture (the running capture process picks this up next; once it's done OR you confirm via pgrep that no fr capture is active, this is safe) |
| ja | 71 | (latest) | 日本 | unknown | Japanese-script anchor, often huge | 100000 | pending — different host, safe to start now |
| ru | 968 | (latest) | Москва | unknown | Russian/Cyrillic anchor | 100000 | pending — different host, safe to start now |
| es | 6347 | (latest) | España | unknown | Spanish anchor, top-traffic | 100000 | pending — different host, safe to start now |
| pt | 1631 | (latest) | Brasil | unknown | Portuguese anchor | 100000 | pending — different host, safe to start now |
| he | 2 | (latest) | ירושלים | unknown | Hebrew/RTL script | 100000 | pending — different host, safe to start now |
| hi | 7 | (latest) | भारत | unknown | Hindi/Devanagari script | 100000 | pending — different host, safe to start now |
| en | 534366 | 1354984261 | Barack_Obama | ~60 k | biggest mainstream biography; the canonical "Obama-class" baseline; tests >30 k cap | 100000 | blocked-on-running-en-capture |
| en | 1095706 | 1354664189 | Jesus | ~50 k | contentious religious topic, heavy vandalism + revert pairs | 100000 | blocked-on-running-en-capture |
| en | 5043734 | 1355374251 | Wikipedia (the article) | ~40 k | meta-article; self-referential edit patterns | 100000 | blocked-on-running-en-capture |
| en | 6097297 | (latest) | List_of_legendary_creatures_(M) | unknown (huge?) | list-page style; large lifetime-token count | 100000 | blocked-on-running-en-capture |
| en | 5042916 | (latest) | Climate_change | unknown | high-traffic, heavily-edited, contentious science topic | 100000 | blocked-on-running-en-capture |
| en | 19283 | (latest) | Microsoft | unknown | corporate/biography crossover, many editors | 30000 | blocked-on-running-en-capture |

When the main-thread en capture process exits, **all `blocked-on-running-en-capture`
rows automatically convert to `pending`** (the corpus agent doesn't
need permission, just needs to verify via `pgrep` that no other en
capture has started in the meantime).

The `(latest)` rev_ids in the language-anchor rows need to be resolved
before capture. Use:

```bash
curl -s "https://{lang}.wikipedia.org/w/api.php?action=query&prop=revisions&rvprop=ids&pageids={page_id}&format=json&formatversion=2" \
  | jq '.query.pages[0].revisions[0].revid'
```

then create `parity-fixtures/{lang}/{page_id}/{rev_id}/meta.json` with
the shape used by the existing fixtures (see one of the already-captured
fixtures for the JSON schema).

## Already in corpus (don't re-capture; here for context)

| lang | page_id | rev_id | title | revs | status |
|---|---|---|---|---|---|
| en | 24544 | 1354638187 | Photosynthesis | 5495 | validated |
| en | 22989 | 1354657462 | Paris | 20453 | validated |
| en | 2731583 | 1354738283 | Adolf_Hitler | 28417 | validated |
| en | 46827 | 1355508503 | Jesse_Owens | 6461 | validated |
| en | 79023819 | 1277418181 | Israel–Hamas war | 2 | validated |
| de | 2552494 | 267155005 | Berlin | 10 000 (cap-hit but reached target) | validated |
| ar | 4287 | 74668889 | القاهرة | 2987 | validated |
| simple | 27263 | 10855732 | Wikipedia | 3783 | validated |
| zh | 1686258 | 64806634 | 中国 | 7 | validated |
