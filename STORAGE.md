# Storage format

This document specifies the on-disk format that replaces the current
Python pickles (`../wikiwho_api/api/utils_pickles.py`). The format is
designed to be:

- **Compact** — Obama-class articles in tens of MB compressed, not hundreds.
- **Lazy** — readers should be able to answer a single-revision query
  without loading the whole article into memory.
- **Append-friendly** — applying a new revision should be a small
  append, not a full rewrite.
- **Crash-safe** — a half-written update should not corrupt the article.
- **Portable** — no Python pickle protocol nonsense; an independent
  reader (in any language) should be able to parse the format from
  this spec.

## 1. Directory layout

```
<volume>/
  <lang>/
    <page_id // 1_000_000>/
      <page_id // 1000>/
        <page_id>/
          meta.json
          strings.bin
          tokens.bin
          revisions.bin
          appendlog.bin    (optional, present after live updates)
          appendlog.idx    (optional)
```

Two-level sharding (`page_id // 1_000_000` and `page_id // 1000`)
keeps any one directory at well under 1000 entries even with millions
of articles per wiki. Compare the current single-level layout in
`api/utils_pickles.py:get_pickle_path` which can have up to 1000
entries per shard; the rewrite's deeper sharding scales better for
the larger wikis.

`<volume>` is per-language and configured at startup; in production
this will probably be `/blobs/<lang>` mirroring the current `/pickles/<lang>`
arrangement (or whichever Cinder volume the language lives on).

## 2. File-by-file format

### 2.1 `meta.json` — small, human-readable

```json
{
  "schema_version": 1,
  "page_id": 534366,
  "language": "en",
  "title": "Barack_Obama",
  "last_processed_revid": 1212345678,
  "last_processed_timestamp": "2024-03-14T15:09:26Z",
  "rvcontinue": "20240314150926|1212345678",
  "n_revisions": 56789,
  "n_lifetime_tokens": 412345,
  "n_spam_revisions": 1023,
  "spam_revisions_sha1_count": 412,
  "next_token_id": 412345,
  "appendlog_revisions": 17,
  "appendlog_starts_after_revid": 1212340000,
  "checksum_tokens": "sha256:abc123...",
  "checksum_revisions": "sha256:def456...",
  "checksum_strings": "sha256:789..."
}
```

Required for every article. Kept small enough (~500 bytes) that
listing all articles is cheap.

### 2.2 `strings.bin` — interned token strings

Per-article symbol table. Token strings in WikiWho are lowercased
and (after normalization) often short. Many are repeated (millions
of occurrences of "the" across an article's history, but only one
entry in the table).

```
header (16 bytes):
  4 bytes: magic "WWST"        ("WikiWho strings")
  2 bytes: format version (u16, big-endian)  = 1
  2 bytes: reserved (must be 0)
  4 bytes: number of strings (u32 BE)
  4 bytes: total bytes of string data (u32 BE)

index table (8 × n_strings bytes):
  for each string i in 0..n_strings:
    4 bytes: offset into string data (u32 BE)
    4 bytes: length in bytes (u32 BE)

string data (variable):
  UTF-8 bytes, no separators

trailer (8 bytes):
  4 bytes: magic "TSWW"
  4 bytes: CRC32 of preceding bytes (u32 BE)
```

Reader: mmap the file. To resolve string id `i`, read the index entry
at offset `16 + 8*i`, then read `length` bytes starting at the
specified offset.

The whole file is Zstd-compressed if it exceeds 64 KB (most articles
will compress; small articles won't bother). When compressed, the
filename is `strings.bin.zst` and the format above describes the
uncompressed contents.

### 2.3 `tokens.bin` — per-lifetime-token records

One record per `token_id`, in token_id order (which is the order
tokens were introduced).

```
header (16 bytes):
  4 bytes: magic "WWTK"
  2 bytes: format version = 1
  2 bytes: reserved
  4 bytes: number of tokens (u32 BE)
  4 bytes: reserved

records (variable):
  for each token:
    varint: string_id      (delta-encoded from previous if compresses well; otherwise absolute)
    varint: origin_rev_id  (delta-encoded from previous record's origin_rev_id, mostly 0 for runs from the same revision)
    varint: last_rev_id_delta_from_origin   (0 if never deleted)
    varint: inbound_len    (length of inbound list)
    varint × inbound_len: inbound rev ids (each delta-encoded from origin_rev_id and from previous)
    varint: outbound_len
    varint × outbound_len: outbound rev ids (delta-encoded)

trailer (8 bytes):
  4 bytes: magic "KTWW"
  4 bytes: CRC32
```

**Varint encoding:** standard zigzag-encoded LEB128, like protobuf
sint32. Most deltas will be ≤ 7 bits.

The `inbound` and `outbound` lists for a token are usually short
(0–5 entries for most tokens, occasionally hundreds for highly-edited
text in a contentious article). Delta encoding plus varints make
typical tokens 6–12 bytes total.

**Random access** to token id `i` requires either:

- Sequential scan from the start (fine for the bootstrap path).
- An auxiliary `tokens.idx` file with a fixed-size offset table —
  every 256th token's byte offset. To find token `i`, seek to offset
  `tokens.idx[i // 256]` and scan forward `i % 256` records. This
  keeps random access at <100 µs amortized.

We probably want the `.idx` for the `rev_content` path (which needs
to read every token in a revision) but not for the algorithm itself
(which always processes sequentially).

### 2.4 `revisions.bin` — per-revision headers and token sequences

This is the read-heavy file: serving a `rev_content` request reads
exactly one revision header + its token sequence.

```
header (24 bytes):
  4 bytes: magic "WWRV"
  2 bytes: format version = 1
  2 bytes: reserved
  4 bytes: number of revisions (u32 BE)
  4 bytes: offset of revision-id index table from start of file (u32 BE)
  4 bytes: byte size of revision data section
  4 bytes: reserved

revision-id index table (12 × n_revisions bytes):
  sorted by rev_id ascending:
    8 bytes: rev_id (u64 BE — Wikipedia rev ids may eventually exceed u32, plan ahead)
    4 bytes: offset into revision data section (u32 BE)

revision data section (variable):
  for each revision in processing order:
    varint: rev_id          (NOT delta-encoded — these are absolute for binary search)
    8 bytes: timestamp_unix (i64 BE, seconds since epoch)
    varint: editor_kind     (0 = registered, 1 = anon with name, 2 = missing)
    varint: editor_id       (if registered) OR string_id of anon name (if anon)
    varint: parent_rev_id   (the previous *processed* revision id; 0 for first)
    varint: n_tokens        (number of tokens visible in this revision)
    varint × n_tokens: token ids (delta-encoded from previous token in same revision)

trailer (8 bytes):
  4 bytes: magic "VRWW"
  4 bytes: CRC32
```

**Random access by rev_id:** binary search the revision-id index table
(O(log N) comparisons, each a single u64 read), then a single mmap
read to get the token sequence.

The token sequence is delta-encoded *within* a revision because
adjacent tokens tend to have nearby token_ids (text written in the
same revision is consecutive in the symbol table).

For Obama (50K revisions, ~12K tokens per revision), this file is
~600 MB uncompressed. With Zstd-9 we should see ~10× compression on
the rev-id sequences because of how repetitive they are; expect
50–80 MB compressed.

### 2.5 `appendlog.bin` — live updates

Between compactions, new revisions go here. The format is identical
to the revision data section of `revisions.bin`, plus an extra
trailing block per revision that captures the algorithm state delta
needed to resume from this revision (new tokens, hash-table
additions, etc.).

```
header (16 bytes):
  4 bytes: magic "WWAL"
  2 bytes: format version = 1
  2 bytes: reserved
  8 bytes: base revid (the last_processed_revid at log creation, u64 BE)

per-revision entries (until EOF):
  varint: entry length (in bytes, excluding this varint)
  -- begin entry --
  1 byte: entry kind
          0 = revision (normal)
          1 = spam revision
          2 = compaction marker (after this point, log was rolled into base)
  for entry kind 0:
    everything from "varint: rev_id" through "token ids" as in revisions.bin
    varint: n_new_tokens
    for each new token: same record format as tokens.bin
    varint: n_new_strings
    for each new string: u32 length + UTF-8 bytes
    varint: n_new_paragraph_hashes  (for hash-table state)
    ... (similar for sentence hashes)
  for entry kind 1:
    varint: rev_id
    16 bytes: sha1 of revision (for spam_hashes update)
  for entry kind 2:
    (empty — marks where compaction started)
  -- end entry --
  4 bytes: CRC32 of entry bytes (excluding the length varint, including the kind byte)
```

The CRC per entry is what makes the append log crash-safe: a partial
write of the next entry is detected by CRC mismatch and the log is
truncated to the last valid entry.

`appendlog.idx` (optional) is a parallel file with one record per
entry holding `(rev_id, offset)` for O(1) lookup by rev_id. Without
the index, reading a specific recent revision from the log is a scan
from the start of the log, which is fine if the log stays under ~1000
entries.

### 2.6 Compaction

Periodically (nightly cron, or when `appendlog` exceeds N entries or N
MB), the log is folded into the base files:

1. Acquire a per-article write lock (file-based, not held during reads).
2. Read base files + log; rebuild new tokens.bin, revisions.bin,
   strings.bin in temp files (in the same directory).
3. Update meta.json's `last_processed_revid`, `n_revisions`, etc.,
   write to `meta.json.tmp`.
4. Truncate appendlog.bin to 16-byte header (or delete it).
5. Atomic rename of temp files into place.

While compaction is in progress, readers continue to read the current
base files + log; they only see the new base after the rename.

If the process is killed between writing the new files and renaming
them, the next startup deletes stale `.tmp` files and the article is
unaffected.

## 3. Concurrency model

- **Many readers, one writer per article.** The writer is the
  ingestion path (EventStreams listener, bootstrap, or lazy-on-demand
  fetch).
- Readers do not lock. They mmap the files and read. If the writer
  compacts, readers continue using their mmapped view until they
  re-open; the new files exist alongside the old until all readers
  release. The mmap'd files being unlinked on rename is fine on Linux
  (the inode persists until the last fd closes).
- Writers acquire a `flock`-style lock on a sentinel file
  (`<article_dir>/.lock`) for the duration of the write. Concurrent
  writes for the same article are rare (one EventStream listener per
  language) but the lock prevents corruption when bootstrap and live
  updates race.

Compared to the current code (`utils_pickles.py:31`), the lock is
held for much less time — just the rename, not the whole file rewrite
— so reader-writer contention is largely eliminated.

## 4. Hash-table state — the tricky part

The reference algorithm carries `paragraphs_ht` and `sentences_ht`
forward across revisions. These can grow to millions of entries on
high-history articles. They are NOT needed when answering
`rev_content` requests; they ARE needed when applying a new revision.

Two strategies:

### Strategy A: rebuild on demand

On EventStreams update, load the most recent revision's content from
the base + log, build a partial hash table just for the previous
revision (`revision_prev.paragraphs`), and run the new revision
through the algorithm. The full `paragraphs_ht`/`sentences_ht` is NOT
loaded — we lose the "reintroduced from much older revision" matching.

This is **wrong**: tokens that were deleted 5 revisions ago and now
reintroduced will get NEW token ids instead of inheriting the
originals. The downstream effect is wrong `o_rev_id`s.

### Strategy B: persist hash tables

Add a fourth file `hashtables.bin` containing:

```
header:
  4 bytes: magic "WWHT"
  2 bytes: format version = 1
  2 bytes: reserved
  4 bytes: number of paragraph hashes (u32 BE)
  4 bytes: number of sentence hashes (u32 BE)

paragraph hash entries:
  for each paragraph hash:
    16 bytes: MD5 hash
    varint: number of occurrences (almost always 1, occasionally > 1)
    varint × occurrences: (rev_id, position) — enough to locate the paragraph in the revisions file

sentence hash entries:
  same shape
```

This file can be hundreds of MB for huge articles, but it is read
ONLY at ingestion time, not at request time. Compress aggressively
(Zstd-9; this data is repetitive).

On update: load `hashtables.bin` into memory (decompressed), run the
algorithm for the new revision(s), write back updated `hashtables.bin`.
For Obama-class articles this is the main IO cost of an update — but
EventStreams updates are async background work, not user-facing
latency.

**Use strategy B.** It's the only way to get correct attribution.

> **Resolved 2026-05-22:** Strategy B with the delta-log optimization
> from §"Optimization: hash-table delta" below. Initial implementation
> can rewrite `hashtables.bin` wholesale per update — the delta log is
> an optimization that lands after correctness is proven.

### Optimization: hash-table delta

Instead of rewriting the whole `hashtables.bin` on every update,
append new entries to a `hashtables.appendlog`. Read = base + log;
compaction folds the log into the base, same as for `revisions.bin`.

## 5. Total size estimates

> **Calibrated against production, 2026-05-23.** An earlier version of
> this section assumed an 18 KB compressed average article and ~100
> revisions per article. Production measurements (one of the three 4.9 TB
> cinder volumes on `wikiwho01`, sampled with the Claude session that
> drove this revision) show the real averages are ~10× larger; the
> numbers below replace the hand-waved ones.

### 5.1 Current production footprint

Measured on `/dev/sdc` (one of three identical 4.9 TB cinder volumes;
~2.0 TB used on this one, languages alphabetically distributed across
the three):

| Language | Articles on this volume | Bytes | Avg / article |
|----------|------------------------:|------:|--------------:|
| en | 8 178 631 | 1.88 TB | **224 KB** |
| he | 416 104 | 17.5 GB | 41 KB |
| cy | 287 917 | 17.8 GB | 60 KB |
| da | 323 975 | 6.8 GB | 20 KB |
| bg | 314 555 | 8.2 GB | 25 KB |
| ku | 94 086 | 0.84 GB | 8.8 KB |
| ur | 1 291 110 | 6.6 GB | 5.0 KB |
| (15 other languages here) | … | … | 10-30 KB |

en is the outlier by a large margin: 94 % of this volume's bytes despite
being one of 22 languages on it. The other two volumes hold the
remaining ~45 languages. Total production usage across all three
volumes is approximately **7 TB out of 14.7 TB allocated** — so the
rewrite has roughly 2× headroom before needing more storage.

The pickle format is gzipped Python pickle, transparent: `pickle_load`
(`api/utils_pickles.py:118`) tries gzip first and falls back to raw
pickle for legacy files. Within en specifically, *most* files are
gzipped on disk (verified by magic-byte sampling of the captured
fixtures) — the very oldest files may still be raw, but they are a
minority. The 224 KB / article average is the compressed average.

### 5.2 Per-revision cost

Measured on the captured-parity fixtures:

| Fixture | revs | gzipped on disk | KB / rev |
|---------|-----:|----------------:|---------:|
| en/79023819 Israel–Hamas war (raw, gz6 estimated) | 2 | 1.2 KB | 0.57 |
| en/24544 Photosynthesis | 5 495 | 2.7 MB | 0.49 |
| en/46827 Jesse_Owens | 6 461 | 2.3 MB | 0.37 |
| en/22989 Paris | 20 453 | 13.4 MB | 0.67 |
| en/2731583 Adolf_Hitler | 28 417 | 20.6 MB | 0.73 |

Production averages **0.5-0.7 KB / revision compressed**. Linear in
revision count (the per-revision token-sequence dominates); per-article
fixed costs (`meta.json`, file inodes, directory shards) only matter
below ~100 revs.

### 5.3 Rewrite target

Two factors should let the rewrite beat the current per-rev cost
slightly:

1. **Zstd-9 vs gzip-6.** On Wikipedia-like text + rev-id sequence data,
   zstd-9 typically compresses 15-30 % tighter than gzip-6.
2. **No Python pickle overhead.** Pickle carries class refs, attribute
   dicts, and list-of-string headers; our binary format is the actual
   bytes the algorithm needs. Even before compression, the binary form
   is denser.

Net target: **0.3-0.4 KB / revision compressed**. Per-fixture targets:

| Fixture | revs | prod (gz) | rewrite target |
|---------|-----:|----------:|---------------:|
| en/24544 Photosynthesis | 5 495 | 2.7 MB | ~1.6 MB |
| en/22989 Paris | 20 453 | 13.4 MB | ~7 MB |
| en/2731583 Adolf_Hitler | 28 417 | 20.6 MB | ~10 MB |
| en/534366 Barack_Obama (extrapolated) | ~57 000 | ~40 MB | ~20 MB |
| Top-of-tail List_of_… class (observed) | ~600 000 | ~400 MB | ~200 MB |

`hashtables.bin` (Strategy B, §4) adds bytes; current production may
already serialize hash tables inside the pickle, so this likely doesn't
*add* to the on-disk total — it just exposes what's there. A
conservative budget puts hash tables at 20-40 % of per-article cost.
Even with that, the rewrite stays at or below current production per
article.

The `wikiwho-parity` binary will grow a `--storage-size` mode once the
storage crate lands, so per-fixture rewrite-vs-prod bytes are tracked
continuously as a regression metric alongside parity.

### 5.4 Total volume budget

Two scenarios:

| Scenario | en | Other ~66 langs | Total |
|----------|---:|----------------:|------:|
| Rewrite matches production (no gain) | 1.9 TB | ~5 TB | ~7 TB |
| Rewrite beats production by 30-50 % | 0.9-1.3 TB | 3-4 TB | 4-5 TB |

Either way, comfortably under the 14.7 TB allocated; no storage request
needed for the rewrite cutover. We have ~2× headroom in the worst case.

**Namespace policy:** these figures cover **mainspace only** (ns 0),
matching what production stores today. Non-mainspace pages (Talk:,
User:, Wikipedia:, etc.) are served via the ephemeral path
(API.md §9) and do **not** consume disk. If a specific non-mainspace
namespace later moves to durable storage for traffic reasons, its
size should be added separately to this table — talk pages on
contentious articles can accumulate 50 000+ revisions and are
correspondingly heavy, so per-namespace durability is a deliberate
operational choice rather than a default.

### 5.5 Where the bytes go (still hand-waved)

Without an in-pickle attribute breakdown we don't yet know what fraction
of the per-article cost is tokens vs sentences/paragraphs vs revisions
vs hash tables vs spam list. A sample article unpickled with
`pympler.asizeof` would close that gap and inform whether
`hashtables.bin`'s size estimate (currently a guess of 20-40 % of total)
is right. Queued as a non-blocking follow-up; not required to start
storage implementation.

### 5.6 Operational headroom (separate from the rewrite)

If a significant fraction of en's `.p` files are still legacy raw
pickle (i.e., predate the `gzip-6` write path at
`api/utils_pickles.py:103`), running a one-shot read-and-re-write sweep
would compress them in place and recover free disk. The magic-byte
sample in the session conversation suggested *most* en files are
already gzipped, so the upside here may be small; sample more broadly
before scheduling the sweep if it ever matters. Tracked as a non-blocking
operational follow-up in `notes/decisions-needed.md`.

## 6. What we are NOT doing

- **No HDF5, no Parquet, no Arrow IPC.** These are great for analytics
  workloads but heavier than we need for single-record reads. Custom
  binary is small and fast.
- **No SQLite per article.** Each article having its own SQLite file
  was considered; the overhead per-article (page size, journal,
  schema) is too high for the millions of small articles.
- **No relational DB across articles.** Each article is independent;
  no joins needed. Saves enormous operational complexity (no Postgres
  to maintain, no migrations, no replication lag).
- **No object storage from day one.** Local disk is fast enough and
  simpler. If we later need to move cold articles to Swift/S3, the
  directory layout is already content-addressable enough to support
  it; just add an `s3://` prefix to the path.

## 7. Format evolution

`schema_version` in `meta.json` and `format version` in each binary
file's header allow forward compatibility. Rules:

- A version-N reader MUST refuse to read version-(N+1) files. Refuse
  loudly; don't try to muddle through.
- A version-(N+1) writer MUST be able to read version-N files (so
  upgrades don't require a full rewrite).
- Compaction is the natural upgrade path: when a version-N file is
  next compacted, it gets written out as the writer's current version.

## 8. Reference reader

To unblock independent reimplementation, a small reference reader in
Rust (and ideally also Python, for parity testing) should exist as
soon as the format is finalized. It should:

- Open an article directory.
- List all rev_ids.
- Read the token sequence for a given rev_id.
- Validate CRCs and magic numbers.

This is also the first user of the format from the rewrite side, so
it'll catch spec errors early.
