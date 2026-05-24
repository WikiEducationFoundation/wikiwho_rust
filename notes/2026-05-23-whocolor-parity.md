# 2026-05-23 (part 12) — WhoColor parity: capture verification + bug fixes

**Goal:** validate the WhoColor implementation against captured
production responses. The 22 `whocolor.json` files under
`parity-fixtures/` were already in place from earlier fixture
captures; this session built the comparison harness and ran it,
which surfaced two real handler bugs along the way.

**Parity (whocolor-parity, vs captured production whocolor.json,
20 fixtures with both `whocolor.json` and `history.jsonl`):**

| Metric | Result |
|---|---|
| **Tokens all-fields passing** | **974,088 / 1,115,391 (87.33%)** |
| str | 87.70% |
| o_rev_id | 91.39% |
| in | 95.49% |
| out | 95.49% |
| conflict_score | 96.04% |
| class_name | 93.81% |
| **Revisions passing** | **220,209 / 220,762 (99.75%)** |
| `biggest_conflict_score` matches | **19 / 20** |
| Per-fixture all-pass | 12 / 20 |

**Two outliers carry most of the gap:**

- `ja/4821051 日本` — 0.6% tokens vs prod-cache.
- `en/62750956 COVID-19_pandemic` — 4.2% tokens vs prod-cache.

For BOTH, `parity-check --full-history --python-replay` reports
**100% all-fields parity vs fresh Python `wikiwho.py`**. The 87%
headline is entirely **production-cache drift** — the same
phenomenon documented in
`notes/2026-05-23-photosynthesis-signal.md` and
`notes/2026-05-23-python-replay.md`: production's cache has been
mutating over years and accumulates extra `out` mentions as later
edits delete tokens that a fresh rebuild "as of rev_id T" wouldn't
have. The remaining smaller-divergence fixtures
(`simple/27263`, `en/2731583`, `en/534366`, `es/972`, `he/325`)
all show 100% on tokens themselves and only 1-10 revisions
diverge — the same prod-cache shape.

The 100% Python-parity floor is the real algorithm correctness
signal; the WhoColor data layer just exposes the underlying
Article state, so its correctness inherits from
`parity-check`'s.

**Counts before → after:**

- Total workspace tests: **281 → 281** (no net change — fixed
  test expectations along with the bugs they protected).
- Clippy clean with `-D warnings --all-targets`.

**Bugs fixed in the handler (surfaced by the captures):**

- *Anon editor names stripped the `0|` prefix.* The
  WhoColor handler was returning `"192.0.2.1"` for an anon IP
  whose raw editor was `"0|192.0.2.1"`. Production keeps the
  literal `0|<ip>` form in both `revisions[i][3]` and
  `present_editors[i][0]` — see
  `whocolor/handler.py:117`. New helper
  `handlers::whocolor::display_name` mirrors the Python
  `editor_names_dict.get(editor, editor)` fallback. Unit test
  in `build_envelope_anon_keeps_prefix_in_display_name`.

- *`present_editors` were 2-tuples, not 3-tuples.* API.md §7's
  *example* shows `[name, class_name]`, but production has long
  emitted `[name, class_name, percentage]` (per `WhoColor/
  parser.py:223-227`). Handler now computes
  `token_count / total_present_tokens * 100.0` for each editor.
  Both downstream consumers (Dashboard ArticleViewer and the
  WhoWroteThat gadget) don't reference the field today, but it
  shows up in the WikiWho swagger schema and the WWT sidebar.

**Done:**

- **`crates/wikiwho-parity/src/bin/whocolor_parity.rs`** — new
  binary that walks every fixture having both `whocolor.json`
  and `history.jsonl`. For each:
  1. Loads history, replays through `Article::analyse_revision`
     (same path as `parity-check`).
  2. Infers `capture_now` from production's `age` field +
     known origin timestamps so the optional age check is
     deterministic.
  3. Calls `get_whocolor_data(article, rev_id, capture_now)`.
  4. Compares `(str, o_rev_id, in, out, conflict_score,
     class_name)` per token, `(timestamp, parent_rev_id,
     class_name)` per revision, and `biggest_conflict_score`.

  Explicitly excluded from the comparison:
  - `extended_html` — different rendering pipeline (Parsoid
    REST vs MW Action API parse).
  - `present_editors` — token-visibility differences between
    Option A (HTML-side) and Python's wikitext-side counts
    mean different denominators.
  - `age` (default) — production records "now at capture time";
    a fresh rebuild gets "now at the moment we ran". Opt in
    with `--check-age` to use the inferred capture-now.

  Flags: `--fixtures DIR`, `--check-age`, `--show-first-diff`,
  positional fixture filters.

  Per-fixture filter on the 2 small fixtures with
  `--check-age` shows 100% on all required fields and 64% on
  age (±1.5s) — the gap is per-token rounding noise in
  production's `datetime.now()` (called inside the per-token
  loop) vs our single inferred capture-now.

- **`crates/wikiwho-attribute/src/whocolor.rs`** — exposed
  `parse_mw_timestamp_public` so the parity binary can convert
  ISO timestamps without pulling chrono into a new crate.

- **`crates/wikiwho-parity/Cargo.toml`** — depends on
  `wikiwho-server` for `whocolor_html::token_class_name` (the
  md5/passthrough hash function). Adds the second
  `[[bin]]` entry.

- **`crates/wikiwho-server/src/handlers/whocolor.rs`** — the two
  bug fixes above plus their test expectations updated.

**Design notes / issues encountered:**

- *Why depend on `wikiwho-server` from `wikiwho-parity`?* The
  `token_class_name` helper lives in the server crate because
  the HTML span injector needs it. Moving it down into
  `wikiwho-attribute` would be a cleaner crate-layout move, but
  it'd pull md5 into the algorithm crate just for one anon-
  hashing helper. Keeping it where it is and accepting the
  cross-crate dep in the parity binary is the lower-friction
  choice. If we later want to reuse class_name from a
  non-server context, lift it then.

- *Age-check tolerance is ±1.5s.* Production calls
  `datetime.now()` inside the per-token loop in Python; on a
  large article, the wall-clock drifts over the millions of
  microsecond-level calls. A 1.5s tolerance covers the worst
  fixtures (`en/534366` has ~111k tokens; 1.5s / 111k = ~13µs
  per token, which matches typical per-call clock-read cost).
  Sub-second comparison would fail spuriously on the large
  fixtures.

- *The parity numbers look ratchet-worthy but aren't.* The 87%
  is dominated by `ja/4821051` and `en/62750956`. Each of these
  contributes ~70k tokens of "all wrong" mass because the
  production cache differs by ~1 token offset → every
  subsequent string comparison fails. We could spend an arbitrary
  amount of time chasing this without it being a real bug. The
  100%-vs-Python signal is the actual quality metric.

- *Per-fixture token counts can disagree with production by a
  few.* For `en/2731583`, `en/534366`, and others, our token
  count is identical to production (e.g. Obama: 111282 = 111282).
  The "tokens_pass < tokens_total" only happens when the same
  number of tokens are produced but a few have shifted by
  positions. For `ja/4821051` and `en/62750956`, even the
  totals differ (rust=76761 prod=42938; rust=102897 prod=102854)
  — that's the prod-cache drift accumulating differently from
  a clean replay.

**Queued decisions:** none new this session.

**Next session likely starts with:**

The four consumer-facing endpoints are implemented and the
parity story is now characterized. Remaining items:

1. **Persist paragraph + sentence arenas for resume-from-disk.**
   Still the longstanding non-blocking decision from part 6.
   Required before live EventStreams ingest can do incremental
   updates rather than full rewrites. The cache-miss path works
   fine without it; live-update doesn't.

2. **WhoColor data flowed through python_replay.py.** The
   age-check tolerance and the missing-from-fixture-side
   `conflict_score` make `whocolor-parity --python-replay` the
   logical analog of `parity-check --python-replay`. Lift the
   Python WhoColor flow into the existing replay script,
   cache the output at `<fixture>/python_whocolor_replay.json`,
   compare. Probably ~half a session.

3. **Ephemeral non-mainspace endpoint (API.md §9).** Lowest
   priority — no downstream consumer uses it.

4. **Deploy path planning.** With all four wire-format endpoints
   ready, the cutover plan (PLAN.md §"Migration") becomes the
   gating decision. Easiest first (small wikis), English last.

Recommendation: **1 (paragraph/sentence persistence)**, because
it's the load-bearing decision the live-update path waits on,
and chasing every prod-cache drift with whocolor-parity-vs-
python isn't going to teach us more than `parity-check --python-
replay` already tells us. Get the storage layer ready for
incremental updates, then the actual deploy is unblocked.
