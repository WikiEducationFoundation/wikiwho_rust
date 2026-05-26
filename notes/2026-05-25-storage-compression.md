# 2026-05-25 — zstd-compress the four remaining `.bin` files

**Goal:** extend the per-article zstd compression beyond
`revisions.bin` + `paragraphs.bin` (landed 2026-05-24 in
`193e57c`) to the rest of the storage files that would benefit.

**Parity:** unchanged. No algorithm code touched. All 8
`wikiwho-storage` integration tests pass, including the full-history
round-trips for Photosynthesis (5495 revs), simple/27263, and
zh/1686258.

**Done:**

1. **Wrote a measurement binary**
   `crates/wikiwho-storage/src/bin/measure_compression.rs` that
   replays a `history.jsonl` fixture, calls `write_article`, and
   reports `(uncompressed_bytes, on_disk_bytes, ratio)` per file.
   Lets us decide which files are worth compressing on real data
   rather than guessing.
2. **Measured three articles** (Photosynthesis, Adolf Hitler,
   Barack Obama). Before-state ratios for the previously-uncompressed
   files:

   | file             | Photo   | Hitler  | Obama   |
   |------------------|---------|---------|---------|
   | `tokens.bin`     | 6.80×   | 9.11×   | 8.13×   |
   | `sentences.bin`  | 2.31×   | 3.89×   | 3.50×   |
   | `strings.bin`    | 2.60×   | 11.02×  | 2.14×   |
   | `hashtables.bin` | 1.69×   | 1.68×   | 1.68×   |

   Every file shows a meaningful ratio; on a large article (~28k
   revs) compressing all four cuts ~34 MB → ~7.5 MB on disk.
3. **Wrapped the four file writers in `zstd::stream::write::Encoder`**
   (`crates/wikiwho-storage/src/writer.rs`) — same pattern the
   previous commit applied to revisions/paragraphs. The `ZSTD_LEVEL`
   doc comment was updated to mention all six files.
4. **Switched the reader to `zstd::decode_all` for those four files**
   (`crates/wikiwho-storage/src/reader.rs`). The block comment now
   explains the win is smaller (2-9×) than the
   revisions+paragraphs case (100-300×) but still worth the small
   CPU cost: per-file on-disk drops from MB to hundreds of KB.

**Counts:** no test or clippy changes. `cargo clippy --all-targets`
clean. `cargo test -p wikiwho-storage` 8/8 passing.

**Per-article savings (Photosynthesis, full file set):**

```
before this commit (revisions+paragraphs compressed only):
  ~5.6 MB on disk
after this commit (all six .bin files compressed):
  ~2.4 MB on disk
delta: -3.2 MB (-57%)
```

For Adolf Hitler the same delta is ~32 MB; for Obama ~30 MB.

**Format note:** filenames stay `.bin`; the contents are a single
zstd frame, matching the `193e57c` precedent. Old uncompressed
files written by previous binaries will fail to read — the parser
sees zstd magic instead of `WWST`/`WWTK`/`WWSN`/`WWHT`. Test
storage and the wikiwho-rs WMCloud VM need a wipe on next deploy
(use `scripts/remote-deploy.sh --wipe-storage`). No production
data is at risk because no production data has ever been written
in the Rust format.

**SCHEMA_VERSION not bumped.** Consistent with `193e57c`: the zstd
wrapper is an encoding change, not a schema change, and no
persistent data exists at the current version.

**Design notes / issues encountered:**

- *`StringsIndex` is still defined* (random-access lookup over the
  decompressed strings.bin payload), but only used in unit tests.
  Production callers do `fs::read` → decompress → `parse_*_blob` →
  Vec. No mmap-style consumer would break.
- *`RevisionsIndex` similarly works over decompressed bytes* — the
  rebuilder already calls `zstd::decode_all` before constructing
  the index. Nothing changes here.

**Resolved decisions (today):** none. The four-file compression
extends the existing storage-compression decision (`193e57c`)
along the same lines.

**Queued decisions (none new today).**

**Next session likely starts with:**

1. **`measure_compression` as a permanent tool** — fine to keep
   under `crates/wikiwho-storage/src/bin/`. Useful when revisiting
   any future encoding tweak (e.g. switching MD5 hex hashes to
   16-byte raw to avoid the 2× hex inflation that zstd partially
   reclaims).
2. **Deploy + bench** — re-run `scripts/bench_articleviewer.py`
   against wikiwho-rs after a `--wipe-storage` redeploy to confirm
   the per-article du shrinks as predicted. The bench harness
   already prints test_kb so the comparison is automatic.
3. **Possible follow-up: encode MD5 hashes as 16 raw bytes** instead
   of 32-char hex. That's a schema change (touches several files)
   so it's a bigger lift, but it would shave another 30-40% off
   hashtables.bin and sentences.bin. Queue as a non-blocking
   `notes/decisions-needed.md` entry if it ever becomes hot.
