# Decisions needed

Append-only queue of forks Sage should weigh in on. Newest at the top. Entries are removed only when superseded; resolved ones get a `> **Resolved YYYY-MM-DD:** …` line appended in place and stay in the file as a history.

Format:

```markdown
## YYYY-MM-DD — <short headline> [blocking | non-blocking]

**Context:** one or two sentences.

**Options:**
- **A.** description; pros; cons
- **B.** description; pros; cons

**Recommendation:** A, because …

> **Resolved YYYY-MM-DD:** chose A. <rationale>
```

---

## 2026-05-24 — WhoColor HTML source: Parsoid vs MW Action API parse [resolved-with-followup]

**Context:** PLAN.md §4.6 resolved on MW REST `/page/html` (Parsoid)
as the HTML substrate that `whocolor_html::inject_spans` walks.
The first WMCloud deploy ran the Wiki Education Dashboard's
ArticleViewer against our endpoint and surfaced a load-bearing
mismatch with production's wire format:

- **Production** fetches the wikitext for the target rev_id, then
  asks the MW Action API to render it via `action=parse&prop=text`.
  The output is a content-only `<div class="mw-content-ltr
  mw-parser-output">` tree — no DOCTYPE, no head, no RDF, no
  Parsoid section wrappers. Critically, production *injects spans
  into the wikitext* before calling action=parse, so spans survive
  the wikitext→HTML transformation by construction.
- **Ours** fetched Parsoid HTML and ran `inject_spans` over the
  rendered output, matching tokens against the visible text.
  Parsoid emits a full document with structural wrappers and
  expands templates differently; our text walk found only 16 of
  ~587 expected token spans on en/Delon_Hampton (Dashboard testing
  surfaced "only one word highlighted per user" because
  ArticleViewer regex-matches `<span class="editor-token
  token-editor-{userid}">` and most of a user's tokens never got
  spans at all).

**Options:**
- **A. Switch the HTML source to MW Action API `action=parse&
  oldid={rev_id}&prop=text&formatversion=2`.** Content-only HTML
  closer in text content to wikitext. Our HTML-level `inject_spans`
  should match a much higher fraction of tokens. Cheapest change;
  doesn't get us to byte-for-byte parity vs production because
  we still do HTML-level matching, not wikitext-level injection.
- **B. Adopt production's wikitext-level injection pipeline.**
  Fetch wikitext, walk it in tokenizer order, wrap each token in
  `<span class="editor-token token-editor-{class}" id="token-{n}">…
  </span>` markup at wikitext-byte positions, then send to
  `action=parse`. Byte-for-byte parity attainable. Bigger build —
  need a wikitext-position-aware tokenizer that emits the same
  sequence as the algorithm crate's tokenizer, plus the
  "special-element" exclusion logic that skips template / ref /
  infobox tokens (cf. `WhoColor/parser.py::__parse_wiki_text`).
- **C. Keep Parsoid, write a Parsoid-aware content extractor.**
  Walk the Parsoid DOM, strip head/meta/section/figure atoms, get
  closer to action=parse's text content. Easy to start, hard to
  finish — Parsoid's structure evolves, and we'd carry an extra
  data-mw-aware traversal for the rest of the project's life.

**Recommendation:** **A** as the immediate fix (this session), with
**B** filed as the next-iteration target for byte-for-byte parity
once the action=parse coverage is measured. **C** as a fallback if
WMF deprecates action=parse for new-style apps (no signal that
they will).

> **Resolved 2026-05-24:** chose **A** as the immediate switch.
> `crates/wikiwho-mwclient` gained `fetch_rendered_html(rev_id)`
> backed by `action=parse&oldid={}&prop=text&formatversion=2`,
> with `MwError::Api` translated to `PageMissing` for
> `nosuchrevid`/`missingtitle`/`invalidpageid`/`nosuchpageid`
> codes. The whocolor handler now calls `fetch_rendered_html`
> instead of `fetch_parsoid_html` (the latter kept around for
> the future smart-extractor path). PLAN.md §4.6 and the
> resolved-decisions table in CLAUDE.md updated. Tests updated:
> the whocolor integration mock now serves `action=parse`
> responses in the existing axum action_handler.
>
> **Followup queued (non-blocking):** option B (wikitext-level
> injection) for full byte-for-byte parity with production. Worth
> doing once we have measurements showing how much A leaves on the
> table — see `notes/cutover/02-<date>-coverage.md` for the
> next-session diagnostic.

---

## 2026-05-23 — windowed revision fetch for ingest apply loop [non-blocking]

**Context:** `wikiwho-ingest::apply::fetch_window` currently uses the
existing `MwClient::fetch_revisions(page_id, end_rev_id)` and discards
revs whose rev_id is `<= start_exclusive` client-side. That paginator
starts at the page's *first* revision when no `rvcontinue` is given —
so for an Obama-class article (30k+ revs) where we just missed one
edit, we read the whole history just to drop all but the trailing
revision. Correct, but wasteful.

**Options:**
- **A. Add `MwClient::fetch_revisions_window(page_id, start_exclusive,
  end_inclusive)` that sets MW's `rvstartid` param.** With
  `rvdir=newer&rvstartid=last_good+1&rvendid=event_rev`, MW returns
  exactly the window we want; the apply loop's client-side filter
  becomes a no-op. Pros: linear in window size; one new method on the
  client. Cons: minor — `rvstartid` is inclusive and we want
  exclusive, so callers compute `start + 1`.
- **B. Leave it.** For the typical "1-2 revs missed" case it's a
  single MW round-trip regardless; only catastrophic gaps (an ingest
  outage of hours) cause real waste. We could just monitor the
  per-event apply latency and add this if hot articles start dragging
  the cycle time.

**Recommendation:** **A**, but as a non-blocking follow-up. The cleanest
moment to land it is when we instrument ingest latency and see real
numbers; until then the current code is correct and a one-batch
overhead on hot articles is bounded (MW caps `rvlimit=max` at 500/page
per request, so even Obama is ~60 batches per apply, ~1-2 minutes
worst case). Surface when ingest latency dashboards exist.

---

## 2026-05-23 — revision-visibility-change SSE listener [non-blocking]

**Context:** The legacy service has two parallel SSE listeners:
`recentchange` (for edits/new pages) and `mediawiki.revision-
visibility-change` (for revdel / suppress). The rewrite has only the
first. When an editor visibility-changes a revision, the legacy
service flips its `text_hidden`/`user_hidden`/`comment_hidden` flags
to match the new MW state; the rewrite's stored articles remain at the
pre-revdel state until the article gets another edit (whereupon
`fetch_window` would see the suppressed slot and emit the modern
hidden flags).

**Options:**
- **A. Add a second SSE listener for `revision-visibility-change`** in
  `wikiwho-ingest` that triggers a "refetch + re-analyse from
  affected rev" path. Pros: real-time visibility compliance. Cons:
  re-analysing from an old rev means tearing down and rebuilding part
  of the cascade — more code than the current apply loop.
- **B. Don't add it; rely on re-bootstrap.** When a fixture or
  language is re-bootstrapped (e.g. nightly), the new state is
  picked up. In the interim, stored articles reflect the rev's MW
  state at the moment of last edit. This is what XTools / Dashboard
  / IV do today against the legacy service anyway, since the legacy
  flag update is per-article-celery-task and lags real edits.
- **C. Defer until a downstream consumer complains.** None of the
  four consumers we're cutting over has flagged this as a blocker.

**Recommendation:** **C** for now, **A** if/when a downstream consumer
or compliance audit asks for tighter revdel propagation. Worth
documenting so we don't forget it exists.

---

## 2026-05-23 — rev_id → page_id index for endpoint 1 [non-blocking]

**Context:** The `wikiwho-server` scaffold lands endpoints 2-6 (title /
page_id paths) end-to-end with byte-identical JSON round-trip against
the storage layer. **Endpoint 1** (`/{lang}/api/v1.0.0-beta/rev_content/
rev_id/{rev_id}/`) — used by Impact Visualizer — needs a `rev_id →
page_id` index that the current storage format does not provide. The
handler currently responds with the API.md §1 "still processing" (408)
envelope so consumer retry behavior fires, but that's a placeholder, not
a fix.

Three places we could put the index:

**Options:**
- **A. Per-language `rev_id_index.bin` sidecar built by the writer.**
  Every `write_article` updates a language-wide file mapping
  `rev_id → page_id` (could be a sorted u64 pair file with binary
  search). Pros: O(log N) lookup, cheap to update incrementally with the
  append-log when we land Strategy B writes. Cons: another file in the
  storage layout; concurrent writers need locking.
- **B. Brute-force scan on first cold lookup, cached in-memory.**
  On the first request for a rev_id we don't know, walk every article
  in the language directory and read each `revisions.bin`'s rev-id
  index table. Cache the resulting map. Pros: zero new on-disk format.
  Cons: first-request latency proportional to corpus size — fine for
  scaffolding (5-10 fixtures) but terrible at en-scale (8M articles).
- **C. Embed the index in `meta.json`** as a `(first_rev_id,
  last_rev_id)` range. Useful for "could this article contain this
  rev_id?" prefilter but not a hash-map replacement. Could combine
  with B as a cheap rejection filter.

**Recommendation:** **A**, paired with a one-time `rebuild-rev-index`
admin command that scans the storage tree and produces the index file
for existing data. The format is tiny — `u64 rev_id, u64 page_id`
pairs sorted by rev_id, ~16 bytes per revision. For en's ~700M
revisions that's ~11 GB on disk; very tractable. For per-language
writes this also gives us a single place to detect duplicate rev_ids
(spam re-applying across articles) cheaply.

Worth surfacing now because Impact Visualizer's main code path hits
endpoint 1. Until this lands, IV testing against the rewrite will hit
the 408 placeholder. Non-blocking because Dashboard / XTools / WWT
all use endpoint 2 (title-based), which works.

> **Resolved 2026-05-23 (part 8):** chose **A**.
> `crates/wikiwho-storage/src/rev_id_index.rs` implements the sidecar
> as a 24-byte header + sorted `(u64 rev_id, u64 page_id)` body + 8-byte
> CRC trailer, written atomically via tmp-file + rename. `write_article`
> updates it on every per-article write (the in-writer
> cross-page-collision check surfaces sharing bugs early). The admin
> command lives at `crates/wikiwho-storage/src/bin/rebuild_rev_index.rs`
> and reuses `RevisionsIndex::rev_ids_sorted` to extract rev_ids
> cheaply from each article's `revisions.bin` without decoding token
> bodies. Server-side: `AppState::resolve_rev_id` lazy-loads the index
> per language (mirrors the title-index pattern); endpoint 1 now hits
> the index and delegates to the same code path as endpoints 4/6,
> falling back to the 408 envelope only when the rev_id is unknown.
> End-to-end byte-identical JSON for a known rev_id is covered by a
> new integration test.

---

## 2026-05-23 — how to persist paragraph + sentence arenas for resume-from-disk [non-blocking]

**Context:** This session landed `wikiwho-storage`'s read-side (strings,
tokens, revisions, hashtables, meta) and verified byte-identical
round-trip of the wire-format response on 5 captured fixtures including
en/Photosynthesis (5495 revs). The **write-side for live updates is
incomplete**: after a fresh ingest, applying a new revision needs the
algorithm to resume from disk with full state, but our `revisions.bin`
does NOT persist paragraph or sentence arenas (we store the resolved
token sequence per revision instead — cheaper for the read path).
STORAGE.md §4 contemplated hashtables.bin pointing into revisions.bin
via `(rev_id, position)` tuples, which presumes paragraphs/sentences
*are* in revisions.bin. They aren't.

So we need to decide *where* and *how* to persist the paragraph /
sentence state needed for `Article.paragraphs_ht`, `Article.sentences_ht`,
the per-revision `revision.paragraphs` + `revision.ordered_paragraphs`,
and the `Article.paragraphs` + `Article.sentences` arenas. Without
this, "Strategy B wholesale rewrite" can't actually rewrite — the
algorithm's working state isn't fully serialized.

**Options:**

- **A. Inline into revisions.bin.** Per revision, store
  `paragraphs[]` where each paragraph is `(hash, [sentence-of-(hash,
  [token_id, ...])])`. Token-sequence-per-revision derivable by
  concat-walking. `hashtables.bin` then points into revisions.bin
  by `(rev_id, paragraph_index)`. Pros: one file per article for
  per-rev state; spec aligns with STORAGE.md §4's original framing.
  Cons: revisions.bin grows ~3-5× (every paragraph hash gets stored
  per rev that contains it, not deduplicated); the read path slows
  because to get a token sequence you now walk paragraphs anyway.
- **B. Separate `paragraphs.bin` + `sentences.bin` arena files,
  mirroring `tokens.bin`.** Each paragraph stored once by arena id
  with `(hash, [sentence_arena_id, ...])`. Each revision references
  paragraph arena ids in document order. `hashtables.bin` maps
  `hash → [paragraph_arena_id]`. Pros: maximum dedup — a paragraph
  introduced once and unchanged across 1000 revs is stored once;
  the read path keeps the cheap "stored token sequence per rev"
  shortcut already implemented. Cons: 5 binary files per article,
  a bit more bookkeeping; the writer's first pass has to learn the
  full arena layout before writing.
- **C. Skip persistence of paragraph/sentence state entirely. Only
  support cold rebuild from the full revision history.** When a new
  revision arrives, read history.jsonl (or fetch all revisions
  again), replay everything from scratch, re-stitch state in memory,
  then rewrite all files. Pros: storage layer stays simple; only 4
  files. Cons: catastrophic on update latency for Obama-class
  articles — 50k revs × 5.5 ms = 275 s per single-rev update. Plus
  it requires storing or re-fetching the full revision text history
  somewhere, which isn't part of the current storage format either.

**Recommendation:** **B.** Mirrors the existing token arena pattern;
storage-side complexity is bounded; lets the read path stay cheap.
The on-disk numbers calibrate cleanly against the STORAGE.md §5
budget (paragraphs and sentences are mostly hash + list-of-ids;
should be ~5-10 % of per-article bytes on top of tokens).

Worth surfacing now because it sets the shape of the next storage
crate session — it determines whether the writer learns to emit
`paragraphs.bin` / `sentences.bin` (B), embed everything in
revisions.bin (A), or punt entirely (C).

> **Resolved 2026-05-23 (part 14):** chose **B**.
> `crates/wikiwho-storage/src/paragraphs.rs` and
> `crates/wikiwho-storage/src/sentences.rs` are the new arena files
> (`WWPG`/`GPWW` and `WWSN`/`NSWW` magics, varint+CRC like the rest).
> `hashtables.bin` grows from `(hash, count)` to `(hash, Vec<arena_id>)`
> buckets; `revisions.bin` gains per-rev `ordered_paragraphs` (hash +
> arena-id pairs in document order) alongside the existing
> `token_sequence`; `meta.json` gains `spam_revisions` + `spam_hashes`.
> `SCHEMA_VERSION` bumps to 2 (no production data at v1 to preserve).
> Reader now hydrates the full `Article` including paragraph/sentence
> arenas, full hashtables, per-revision `paragraphs` +
> `ordered_paragraphs`, `length`, `original_adds`, `spam_ids`,
> `spam_hashes`. New integration test `resume_from_disk_*` proves the
> end-to-end story: load a snapshot, apply more revisions, get
> byte-identical wire format and identical structural state to a
> single end-to-end replay. Validated on zh/1686258 (7 revs), simple/
> 27263 (3.8k revs, high-vandalism), and en/24544 Photosynthesis
> (5.5k revs).

---

## 2026-05-23 — parity-corpus: large prod-cache divergence on en/COVID-19 and ja/日本 (python ground truth at 100 %) [non-blocking]

**Context:** Captured two new full-history fixtures, ran them through
`parity-check --full-history --python-replay` and `parity-check
--full-history` (vs prod-cache). Results:

| Fixture | revs | vs python_replay | vs prod-cache |
|---|---|---|---|
| en/62750956 COVID-19_pandemic @ 1355596341 | 26 921 | **100.00 %** (102 897 tokens) | 5.50 % str / 4.16 % all-fields (Δ length = +43) |
| ja/4821051 日本 @ 109654789 | 801 | **100.00 %** (76 761 tokens) | 6.94 % str / 0.57 % all-fields (Δ length = +33 823; prod-cache only has 42 938 tokens) |

Both match a fresh Python replay byte-for-byte, so this is **not** a
port bug. Both diverge wildly from prod-cache. The playbook in
`notes/parity-corpus-wishlist.md` explicitly calls out this case
("divergence here is NOT a port bug, just a reference-source
disagreement") — these are the first concrete instances we've recorded.

The ja case is especially striking: prod-cache has only 42 938 tokens
vs our fresh-replay 76 761, suggesting the wikiwho-api prod cache for
日本 was last written when the article was ~55 % its current size and
the rev_id=109654789 lookup is returning that older state. Possible
explanations:

- prod is serving a cached older snapshot keyed by something other than
  the queried rev_id;
- the wikiwho-api Python in production drifted from the Python in
  `../wikiwho_api/lib/WikiWho/WikiWho/wikiwho.py` that
  `scripts/python_replay.py` invokes;
- there's a token-stripping or hide-revision policy in prod that the
  replay doesn't reproduce.

For COVID-19 the lengths agree to within 43 tokens but the structure is
shifted — feels like an off-by-one along the rev cascade, not a
fundamentally different article state.

**Options:**
- **A. Accept and document.** Treat prod-cache as advisory; the
  Python replay is the load-bearing ground truth for parity. Mark
  these fixtures as `divergence — python 100%, prod-cache
  historical-drift` in the wishlist; do not block the corpus on them.
- **B. Investigate ja in depth.** Pull the prod-cache token list and
  diff against `python_replay.json` to identify which rev's editor
  set / token set is preserved in prod-cache. If it correlates with a
  specific rev_id, that points at a stale-cache bug we should report
  to wikiwho-api upstream.
- **C. Just mark both as `divergence` and move on.** Don't even file
  an upstream issue — the rewrite cares about python parity, not
  prod-cache parity.

**Recommendation:** A. The playbook already says prod-cache disagreement
is expected; with two new concrete cases we have enough evidence that
prod-cache shouldn't be a corpus gate. Investigating (B) is interesting
for the report-to-upstream side conversation but isn't blocking the
rewrite. Recommend codifying A by updating the wishlist playbook to
distinguish "validated (both 100 %)" from "validated-vs-python
(prod-cache drift)" as a valid terminal state.

---

## 2026-05-23 — operational: re-compress legacy raw `.p` files in /pickles/en in place [non-blocking]

**Context:** Production calibration (STORAGE.md §5) showed en's
`/pickles/en/*.p` files include a mix of gzipped (most) and raw (some).
The raw ones predate the `gzip-6` write path at
`api/utils_pickles.py:103`. Magic-byte sampling of the captured fixtures
suggested *most* of en is already gzipped, but we did not run the
broader per-language sampling script that would actually count it.

If the raw fraction is non-trivial — say 10 % or more of en's 8.2 M
articles — a one-shot read-and-re-write sweep through the compressed
path would shrink en's footprint with no rewrite work. Even a 5 %
shrinkage on 1.88 TB is ~95 GB recovered.

This is **operational maintenance**, not part of the algorithm rewrite.
It can be done independently and in parallel.

**Options:**
- **A. Sample first, then decide.** Run the magic-byte counting script
  (per-language sampling, 200 random files each) to size the actual win
  before scheduling. ~5 min of read I/O per language.
- **B. Just do it.** Walk `/pickles/en` and for any file whose first 2
  bytes aren't `\x1f\x8b`, read+re-write it through `pickle_dump`. Idempotent
  (touches gzipped files harmlessly, no-ops would be detected by the
  first 2 bytes). ~hours to weeks depending on I/O throttling.
- **C. Defer.** We have 2× headroom (7 TB used / 14.7 TB allocated).
  No urgency until the rewrite cutover starts consuming the headroom
  for parallel-format data.

**Recommendation:** **A.** Five minutes of sampling tells us whether B
is worth doing at all.

---

## 2026-05-23 — quantify per-pickle-attribute byte breakdown [non-blocking]

**Context:** STORAGE.md §5.5 assumes hash tables are 20-40 % of an
article's compressed bytes, but we don't actually know — pickle wraps
the whole `Wikiwho` object opaquely. The right way to settle this is to
load a sample big article and call `pympler.asizeof` (or
`len(pickle.dumps(attr))` if pympler isn't installed) on each top-level
attribute: `tokens`, `sentences`, `paragraphs`, `revisions`,
`paragraphs_ht`, `sentences_ht`, `spam_ids`, `ordered_revisions`.

The answer informs two specific design choices in STORAGE.md:

1. Whether `hashtables.bin` deserves the per-article fourth file at all
   (if hash tables are 50 % of the bytes, it's a major slice; if 5 %,
   they could be inlined).
2. Whether `revisions.bin` deserves the most optimization attention
   (revisions list dominating vs token arena dominating).

**Options:**
- **A. Run the breakdown script on production now** — needs production
  access (Sage has it) and the wikiwho venv. ~2 min per sample article.
- **B. Defer until we're actually writing the storage crate** and use
  the breakdown to make specific format choices. The 20-40 % estimate
  in STORAGE.md §5.5 is conservative enough to start design without.

**Recommendation:** **B.** Storage implementation starts with the
framework (file headers, varint encoding, mmap reader) regardless of
byte-distribution details; the breakdown matters when we tune compressor
choice or decide whether to inline small `hashtables.bin` into another
file. Surface again when we hit that fork.

---

## 2026-05-22 — inbound/outbound list inflation on multi-rev replay [non-blocking]

**Context:** First full-history parity run lands. Israel-Hamas war (2 revs) and 中国 (7 revs) replay cleanly — 41/41 token strings match, 39/41 all-fields (the 2 misses are Myers vs Differ on duplicate `{{` tokens, exactly the documented divergence in `ALGORITHM.md §6`). But simple Wikipedia (3755 processed revs, 28 hidden, 90 spam) shows a much worse pattern: token strings 100% (4495/4495), o_rev_id 91.58%, but inbound/outbound only 1.94%. Spot-checking shows our `inbound` and `outbound` lists are roughly **twice as long** as Python's — we record drop/re-add events Python doesn't. Example: token `"{{"` (id 0) has our `inbound.len=100` vs Python's `49`. Affected revs include known vandalism-and-revert pairs (e.g. rev 6330300 "Replaced content with F U C K", reverted at 6330301), and our code processes both while Python's expected output doesn't record them.

This isn't a Myers-vs-Differ issue — Myers vs Differ would also disturb `o_rev_id`, but `o_rev_id` is mostly right. It's specifically about which rev_ids get recorded into `inbound`/`outbound`. Candidate causes:

1. **Algorithm version drift in the cached fixture.** The captured `rev_content.json` was produced by a production wikiwho-api that processed the article incrementally over years. If the spam-detection heuristics evolved during that window, the cached output reflects the mix.
2. **A spam-detection rule we haven't ported.** The Python length-shrink heuristic skips checks when `comment AND minor` is true (the good-faith-move escape hatch). That's intentional for both. But maybe production has an additional check we missed.
3. **A subtle inbound/outbound double-count we still have.** The dedup fix this session closed one path (paragraphs_ht-matched paragraph words + tail-loop sentence overlap) but there may be others.

**Options:**
- **A. Investigate.** Pick one of the affected revs (e.g. the 6330300 vandalism pair) and trace the cascade + recorder step by step in both Python and Rust to identify the exact divergence. Then decide if it's a bug or a historical-state effect.
- **B. Bigger sample first.** Capture multi-rev history for one larger fixture (say 5000-rev cap on Albert_Einstein or Photosynthesis) and see if the divergence pattern is consistent or article-specific. If it's article-specific to simple Wikipedia, lower the priority; if it's systematic, escalate.
- **C. Defer until consumers actually break.** The downstream consumers (`../WikiEduDashboardTwo/`, etc.) mostly care about `o_rev_id` and `editor` (which is derived from `o_rev_id`). Inbound/outbound history is exposed through WhoColor but probably less critical. Document the divergence, ship at 91% o_rev_id, revisit if a consumer complains.

**Recommendation:** **B then A.** Run on one more fixture to characterize the divergence shape before spending hours on a Python-vs-Rust trace.

> **Resolved 2026-05-23:** Root cause was a **bug in `scripts/capture_history.py`**, not in the algorithm. The script used `"minor" in rev` to test for the minor-edit flag — correct for formatversion=1, where MW omits the key when not minor, but wrong for formatversion=2 (which we use) where `minor` is always present as a bool. Every captured revision was wrongly tagged `minor=true`, which trips the `comment AND minor` good-faith-move escape hatch in the length-shrink check (`wikiwho.py:161`). The escape hatch was hiding most blanking vandalism from our cascade. Fix: `"minor": bool(rev.get("minor", False))`. After re-capture, simple Wikipedia jumped from 90 → 230 spam catches and inbound/outbound parity from 1.94% → 53.70%. The remaining 47% looks like a mix of Myers-vs-Differ artifacts and (smaller) algorithm divergences worth a follow-up trace — see new entry below.

---

## 2026-05-23 — residual inbound/outbound divergence on simple Wikipedia (~47%) [non-blocking]

**Context:** After fixing the capture-script formatversion=2 bug (see prior entry), simple Wikipedia full-history parity reaches `inbound 53.70% / outbound 53.64% / all-fields 53.37%` (was 1.94%). The remaining gap is no longer a 2× inflation — it's a scattered per-token divergence. `--show-field-mismatches 6` on simple/27263 shows:

```
token #0 "{{"   : rust=48 expected=49  expected-only=[6710716] / [6710715]
token #1 "about": rust=50 expected=47  rust-only=[6536549, 7882429, 7882438] / [6536548, 7882426, 7882436]
token #2 "|"    : rust=43 expected=46  expected-only=[7864020, 10612098, 10612125] / ...
```

All the rust-only and expected-only rev_ids are **vandalism-and-revert pairs**. Production records the events on SOME tokens but not others (e.g. token "{{" records 6710715/6710716, but token "about" doesn't); we do the opposite. So this is no longer a missed-spam-detection issue — it's a cascade-ordering / matching difference between Python's Differ and our Myers (or one of the matching sub-cases) that causes a token to be matched-vs-allocated-fresh differently for vandalism-burst revisions. This is the documented Myers-vs-Differ class of issue from `ALGORITHM.md §6`, just larger than expected.

**Options:**
- **A. Get more data first.** The current sample size is N=1 article (simple Wikipedia). 中国 + Israel-Hamas war replay at ~100% all-fields. Capture one more mid-size en fixture (Photosynthesis and Jesse_Owens both >5K — need `--max-revs 10000` or a smaller article like Gaza_war / a newer biographical) to see if 53% is the new floor or simple Wikipedia is uniquely bad.
- **B. Trace a single mismatching rev pair.** Pick e.g. rev 6710715 / 6710716 on "{{" — run both Python (in a small standalone harness) and our cascade with verbose logging and see exactly where the token-id assignment diverges.
- **C. Accept the floor and ship.** WhoColor consumers visualize inbound/outbound history; consumers care most about `o_rev_id` + `editor`. Document the divergence shape (Myers-vs-Differ cascading through vandalism revs), ship at 91% o_rev_id, revisit if a consumer complains.

**Recommendation:** **A then B.** Bigger sample first; the 53% number is one fixture's signal.

> **Resolved 2026-05-23:** All three sub-questions answered.
>
> **A.** Photosynthesis (5495 revs) lands at **80.28% all-fields** vs prod-cache *and* **80.28% vs fresh Python** (identical). Simple Wikipedia is the outlier — high-vandalism wiki with structural restructuring. The 80% number is the more representative floor.
>
> **(Bonus — per Sage's redirect:)** The historical-evolution concern in the original framing turned out to be a non-issue for these fixtures. New `--python-replay` mode runs `scripts/python_replay.py` against the same `history.jsonl` and uses that as ground truth instead of the captured `rev_content.json`. Result: Photosynthesis went from 80.28% (vs prod-cache) to 80.28% (vs Python) — *identical*; simple Wikipedia 52.99% → 53.41% — tiny bump. So the production caches for these specific fixtures match what fresh Python would produce; the residual gap is not a historical-cache artifact.
>
> **B + structural finding.** A direct structural comparison (`scripts/python_replay.py` + new `--show-spam-ids` arena/ht counters in `parity-check`) shows:
> - `spam_ids` count and contents: **PERFECT** match (230 = 230 on simple, both lists byte-identical).
> - `paragraphs_ht` hashes & totals: PERFECT (2888 hashes, 2915 total on simple) or essentially-perfect (+3 on Photosynthesis).
> - `sentences_ht` hashes: PERFECT; totals +16 on simple, +31 on Photosynthesis (0.2-0.3%).
> - **Token allocations: -3.3% on simple (100,363 vs 103,742), -4.4% on Photosynthesis (114,223 vs 119,470).**
>
> The structural state (which paragraphs and sentences ever appeared) agrees byte-for-byte; the divergence is concentrated in **token-level matching**. We allocate *fewer* tokens than Python, meaning our Myers diff finds longer Keep sequences than Python's Differ (which uses Ratcliff/Obershelp pattern matching, not true LCS). The few-percent token-id divergence cascades into ~20-50% inbound/outbound divergence on the final-rev token stream.
>
> **C resolution.** The 80% floor is now the documented intrinsic Myers-vs-Differ divergence per `ALGORITHM.md §6`. The path to 100% is to port Differ exactly. That's a follow-up decision — not blocking ship. Filing a new entry below.

---

## 2026-05-23 — close Myers-vs-Differ gap by porting Differ? [non-blocking]

**Context:** With Python ground truth via `--python-replay`, the remaining all-fields divergence is now characterized as **token-level Myers-vs-Differ**. Structural state (paragraphs_ht, sentences_ht, spam_ids) matches Python byte-for-byte. Token allocations diverge by 3-4% (we allocate fewer, because Myers finds longer LCS than Python's `difflib.Differ` which uses Ratcliff/Obershelp pattern matching — *not* true LCS). The few-percent token-count delta cascades into 20-50% per-token inbound/outbound divergence on the final-rev wire format.

`ALGORITHM.md §6` documented the bail-out condition ">0.1% token divergence OR any divergence on known-hard articles." We're at 3-4%, well above 0.1%. The decision to ship Myers was made before we had Python-replay data to measure against; now that we do, we have a choice.

**Options:**
- **A. Port Differ.** Write a Rust implementation of Python's `difflib.SequenceMatcher.get_opcodes()` (the Ratcliff/Obershelp matcher) and use it inside the cascade. Expected outcome: ~100% all-fields parity. Cost: a non-trivial diff algorithm port (Ratcliff/Obershelp is ~150 lines of Python; not optimal LCS, has its own quirks). Maintenance cost: now we own a Python-stdlib re-implementation forever.
- **B. Accept the gap and ship.** Document the divergence shape. Downstream consumers (`../WikiEduDashboardTwo/`, `../impact-visualizer/`, XTools, WhoWroteThat) mostly care about `o_rev_id` + `editor`. The Photosynthesis test shows o_rev_id at **86.19%** (vs Python). That's the metric the consumers will feel. Decide whether 86% is shippable.
- **C. Add an alternate diff path.** Keep Myers as the default; add a `--differ` flag (or similar) that switches to a Differ port for parity-critical paths. Only worth it if option A's correctness payoff is high enough but the runtime cost matters.

**Recommendation:** Investigate **how much o_rev_id parity downstream consumers actually need before deciding** — Sage's three consumer projects are the ground truth here. If Dashboard's ArticleViewer can tolerate 14% of tokens having a wrong attribution-revision, ship with Myers. If not, port Differ.

> **Resolved 2026-05-23 (part 4):** chose **A — port Differ**. Investigation of consumer usage showed three of four downstream projects (Dashboard ArticleViewer, XTools Authorship/Blame, WhoWroteThat gadget) render attribution per-token — coloring individual tokens by editor — so the 14% per-token `o_rev_id` divergence under Myers would be visibly wrong. Only Impact Visualizer aggregates over revision ranges (drift, not visible). The Differ port (`crates/wikiwho-attribute/src/differ.rs`) is a faithful port of `difflib.SequenceMatcher` + `difflib.Differ.compare`, validated against Python subprocess output on 22 curated cases including autojunk, `_fancy_replace`, and vandalism patterns. After threading it into the cascade, all four captured-history fixtures (en/24544 Photosynthesis, en/79023819 Israel-Hamas war, simple/27263 Wikipedia, zh/1686258 中国) hit **100.00%** on every field (str / o_rev_id / inbound / outbound / all-fields). Myers is kept compiled in `diff.rs` for a possible later revisit — see `notes/diff-algorithm-revisit.md` for the rationale and methodology.

---

## 2026-05-22 — handling historical-tokenization divergence [non-blocking]

**Context:** First parity ratchet (tokenizer level) hit 90.02% — 15 of 16 fixtures at 100%, with COVID-19_pandemic at 5.50%. The COVID failure is a historical-state effect: the article has two multi-CJK-char tokens (`黄冈送别山东援鄂医疗队`, `黄梅戏大剧院`) introduced in 2022 *before* wikiwho's CJK-splitter logic existed. The current code (and our port) splits CJK chars individually; the sentence has been stable since, so production has been hash-matching at sentence level and preserving the pre-split tokens. Single-rev parity *cannot* reproduce this without replaying the article's full history. See `notes/2026-05-22-first-parity-ratchet.md` for the full analysis.

This will compound when we add the real algorithm: ANY article with old non-ASCII content + a stable sentence around it is exposed to the same effect.

**Options:**
- **A. Accept and quarantine.** Add a `--known-divergences` config (or inline annotations on fixtures) marking fixture+token-position combinations where the algorithm's *current* output is correct but production has accumulated a different value. Report them separately in the ratchet output: "100% modulo N known historical-state divergences." Keeps the ratchet honest; explicit about what we can't fix without full history.
- **B. Re-run full history per fixture.** Fetch every revision of each article up to the target rev_id, run our algorithm through all of them, then compare. Expensive (Obama = 57K revs = hours per parity run) but reproduces production state exactly. The plan calls this Level B parity (ALGORITHM.md §10).
- **C. Hybrid: A for now, B later.** Ship A for the current ratchet so algorithm work isn't held up; add B as an optional `--full-history` mode once the algorithm is correct enough that it's worth the cost.

**Recommendation:** **C.** Sage doesn't need to weigh in for the next sessions — the algorithm work proceeds either way. When the algorithm is ~90%+ on single-rev parity, revisit and decide whether B is worth building.

