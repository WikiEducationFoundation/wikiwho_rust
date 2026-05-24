# 2026-05-23 (part 14) — Paragraph/sentence persistence + resume-from-disk

**Goal:** land option B from `notes/decisions-needed.md` —
`paragraphs.bin` + `sentences.bin` arena files, full
`hashtables.bin` back-refs, per-revision paragraph references in
`revisions.bin`, and the metadata that lets the algorithm resume
from disk and apply a new revision on top of a loaded snapshot.
Closes the last storage-layer fork before live EventStreams
ingest can ship.

**Parity:** N/A — no algorithm-layer changes. Existing `parity-check`
and `whocolor-parity` continue to land at 100% (`--python-replay`)
and 87.33% (vs prod-cache); the bump to the storage format doesn't
touch the cascade.

**Counts before → after:**

- Workspace tests: **281 → 293** (+12: paragraphs.rs ×6,
  sentences.rs ×5, hashtables expanded, reader full-state ×1,
  resume_from_disk ×3, revisions retest).
- Clippy clean with `-D warnings --all-targets`.
- SCHEMA_VERSION bumped from 1 to 2 (no persistent v1 data
  exists; the corpus's `parity-fixtures/` directories are
  gitignored captures only).
- Storage tests (release): **8 integration tests pass in ~25s**
  including 3 resume-from-disk tests up to 5.5k revs.

**Done:**

- **New file `paragraphs.bin`** (`crates/wikiwho-storage/src/
  paragraphs.rs`) — per-paragraph arena. Each entry stores
  `hash_value`, `value`, and a flat `ordered_sentences` list
  of `(sentence_hash, sentence_id)` pairs in document order.
  The in-memory `Paragraph::sentences: HashMap<Hash, Vec<SentenceId>>`
  is reconstructed at read time by grouping the ordered list — same
  pattern as `iter_rev_tokens` does for the cascade. Sentence-id
  deltas are signed (`varint_i64`) because arena order is not
  globally monotonic when paragraphs are reintroduced.

- **New file `sentences.bin`** (`crates/wikiwho-storage/src/
  sentences.rs`) — per-sentence arena. Each entry stores
  `hash_value`, `value`, and the flat `words: Vec<TokenId>` in
  document order (delta-encoded with signed varints since the
  algorithm sometimes re-uses earlier token ids for matched
  paragraphs).

- **`hashtables.bin` v2** (`crates/wikiwho-storage/src/hashtables.rs`)
  — grew from `(hash, count)` entries to full
  `(hash, Vec<arena_id>)` buckets. The `Article::paragraphs_ht` /
  `sentences_ht` invariant — every paragraph/sentence hash ever
  observed maps to the list of arena ids that share it — is now
  preserved across disk round-trips. Magic stays `WWHT`/`THWW`;
  format change is gated by the schema-version bump.

- **`revisions.bin` v2** (`crates/wikiwho-storage/src/revisions.rs`)
  — extended each record with `length`, `original_adds`, and
  `ordered_paragraphs` (paragraph hash + arena id pairs in document
  order, delta-encoded). The flat `token_sequence` stays as the
  read-hot path; the paragraph references are what the resume path
  walks. The two coexist deliberately — `rev_content` queries are
  100× more common than incremental updates, so paying ~3-5×
  per-rev size for the second view is worth it for the cheaper
  reads.

- **`meta.json`** — gained `spam_revisions: Vec<u64>` and
  `spam_hashes: Vec<String>` so the algorithm's first checks on a
  newly arrived revision (`if sha1 in spam_hashes: skip`,
  `wikiwho.py:80-82`) and the spam-detection cascade have the same
  snapshot the in-memory algorithm would.

- **Writer + reader** rewired to round-trip every Article field
  that `analyse_revision` reads. The reader now hydrates:
  - `tokens` arena (unchanged)
  - `sentences` arena (new)
  - `paragraphs` arena (new) — with `sentences: HashMap<Hash, Vec<SentenceId>>`
    rebuilt by grouping `ordered_sentences`
  - `paragraphs_ht` / `sentences_ht` — full arena-id buckets, not
    just hash-set membership
  - per-revision `Revision::paragraphs` + `ordered_paragraphs` +
    `length` + `original_adds`
  - `spam_ids` + `spam_hashes`
  - `last_good_rev_id` + `next_token_id` from meta (unchanged)

- **Load-bearing integration tests** (`tests/round_trip_history.rs`):

  | Test | Fixture | Split point | Result |
  |---|---|---|---|
  | `resume_from_disk_zh` | zh/1686258 | rev 3 of 7 | **byte-identical wire + identical structural state** |
  | `resume_from_disk_simple_27263` | simple/27263 | rev 1000 of 3783 | **byte-identical** |
  | `resume_from_disk_photosynthesis` | en/24544 | rev 2000 of 5495 | **byte-identical** |

  Each test: replay the first N revs in memory, persist, reload,
  apply revs N+1…end on top, compare against a fresh in-memory
  replay of all revs. Asserts both wire-format identity on the
  target rev AND structural counters (`tokens.len()`,
  `paragraphs_ht.len()`, `sentences_ht.len()`, `spam_ids`,
  `ordered_revisions`).

- **Reader unit test** (`reader::tests::round_trip_preserves_
  full_article_state`) does the no-persist-no-apply field-by-field
  check on a small fixture so divergence is caught at the
  arena/hashtable layer rather than masked by downstream wire
  format equality.

**Design notes / issues encountered:**

- *Why keep both `token_sequence` and `ordered_paragraphs` per
  revision?* They duplicate state — you could derive
  `token_sequence` by walking ordered_paragraphs → sentences →
  words. But the read-hot `rev_content` path runs on every API
  request, while resume-from-disk only fires on new edits (rare per
  article). Paying ~3-5× size for the per-rev paragraph refs in
  exchange for a single-varint-stream read on the hot path is the
  right tradeoff. STORAGE.md §5 budget has ample room.

- *HashMap insertion order on read.* Both `Paragraph::sentences`
  and `Revision::paragraphs` are HashMaps whose buckets must
  preserve algorithm-side ordering. The writer flattens via
  `ordered_*` walks (matching the cascade's own duplicate-counting
  trick); the reader rebuilds via `entry(hash).or_default().push(id)`
  walking the same ordered list. The bucket contents end up in the
  same order they were originally pushed — confirmed by the
  resume-from-disk tests passing on the high-vandalism simple
  fixture, which exercises duplicate hashes heavily.

- *No backwards-compat for v1.* The `parity-fixtures/` corpus is
  gitignored captures; no persistent storage at v1 exists.
  `SCHEMA_VERSION = 2` rejects v1 files — there are none in the
  wild, and a clean break keeps the read paths simple.

- *Strategy B writer is still wholesale-rewrite.* The "load → apply
  → save" loop now works correctly, but each save still rewrites
  every file. The delta-log optimization from STORAGE.md §4 is the
  next storage milestone if EventStreams update latency on
  Obama-class articles (30k+ revs) turns out to matter; not blocking
  ship.

**Queued decisions:** none new this session. The original
paragraph/sentence persistence entry in `notes/decisions-needed.md`
is marked resolved.

**Next session likely starts with:**

With the storage write-side complete, the remaining items are:

1. **EventStreams ingest scaffold (`wikiwho-ingest`).** The
   load-apply-save loop now works end-to-end; what's missing is the
   SSE listener that watches recent-changes for the target wiki,
   batches edits, and drives the loop. Includes title→page_id
   resolution via the replica DB or MW API, and a checkpoint format
   so a restart doesn't lose in-flight edits.

2. **Ephemeral non-mainspace endpoint (API.md §9).** Still lowest
   priority — no downstream consumer uses it. Fills out the wire-
   format surface.

3. **Deploy path planning.** All four wire-format endpoints work,
   parity locked at 100% vs Python, the storage layer can be live-
   updated. Sketch the cutover plan (PLAN.md §"Migration") starting
   with smaller wikis.

Recommendation: **1 (EventStreams ingest scaffold)**, because that's
the last load-bearing infrastructure piece before a per-wiki cutover
can begin. The ingest crate's existence is in PLAN.md §9 already.
