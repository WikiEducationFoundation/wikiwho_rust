# 2026-05-22 — tokenizer port + parity-check stub

**Goal:** stand up the Cargo workspace, port the tokenizer verbatim from `WikiWho/utils.py`, and get the parity-check binary running so the ratchet has a real starting number.

**Parity:**
- revisions: 0 / 16 (0.00%)
- tokens: 0 / 973,970 (0.00%)
- ms / fixture-load: ~130 (2.1s total for 16 fixtures, dominated by `serde_json` parsing of large rev_content.json files — Gaza_war alone is 126K tokens)

Stubbed comparison; passing counts climb when `wikiwho-attribute::analyse_article` exists. This entry establishes the baseline.

**Done:**
- Workspace `Cargo.toml` with `resolver = "2"`, `edition = "2024"`, members under `crates/*`, shared deps (`md-5`, `regex`, `serde`, `serde_json`, `anyhow`) in `workspace.dependencies` so member crates pin to the same versions via `{ workspace = true }`.
- `crates/wikiwho-attribute/` — library crate. `src/tokenize.rs` ports `calculate_hash`, `split_into_paragraphs`, `split_into_sentences`, `split_into_tokens`, and `compute_avg_word_freq` from `WikiWho/utils.py`. 25 unit tests covering MD5 vectors, paragraph/sentence/token splitting, CJK handling, pipe-escaping, currency symbols, sentence normalization round-trips, and the vandalism-helper edge cases. Module-level `#[allow(clippy::collapsible_str_replace)]` with a justifying comment: staying close to the Python source line-for-line makes future diffs against the reference trivial.
- `crates/wikiwho-parity/` — `parity-check` binary. Walks `parity-fixtures/{lang}/{page_id}/{rev_id}/`, loads `meta.json` + `rev_content.json`, counts revisions and tokens per fixture, reports passing-count tallies. Comparator itself is stubbed (TODO comments inline; the note at the bottom of every run also calls this out so future sessions can't miss it).
- `scripts/verify_tokenizer.py` runs the reference Python tokenizer against a fixed probe list (kept in sync with the Rust `#[test]` cases). Used during this session to verify one test assertion that was wrong — the dot-rule splits all "X. " sequences via non-overlapping replace_all, not just the first.
- `cargo test`: 25 passed, 0 failed.
- `cargo clippy --all-targets -- -D warnings`: clean.
- `cargo run --bin parity-check`: succeeds, reports baseline.

**Issues encountered + resolutions:**
- Initial dot-rule test assertion was wrong. Caught by `cargo test`, verified against the Python reference via `scripts/verify_tokenizer.py`, fixed the assertion. The Rust tokenizer was correct from the start.
- `clippy::collapsible_str_replace` flagged two chained `.replace()` calls in `split_tokens`. Allowed with comment rather than collapsed — verbatim alignment with the Python source is worth more than the lint here.
- `cargo clippy` initially failed because the stable toolchain didn't have the component installed; `rustup component add clippy` (60s timeout) resolved it.

**New decisions queued:** none.

**Next session likely starts with:**

The big jump: porting the attribution algorithm itself. Suggested order:

1. **Data structures** (`crates/wikiwho-attribute/src/structures.rs`). Mirror `wikiwho_api/lib/WikiWho/WikiWho/structures.py`: `Word`, `Sentence`, `Paragraph`, `Revision`. The Python uses `matched` flags shared across iterations; ALGORITHM.md §4 already notes the Rust port should use per-iteration sets instead, so design accordingly from the start.
2. **`analyse_article` skeleton + spam detection** (`wikiwho.py:139-205`). The spam constants are already documented in ALGORITHM.md §3.3; port them as `const` module items. The hash-duplicate check and the length-based vandalism heuristic don't depend on the matching cascade, so they're a clean first slice.
3. **Source wikitext fetching for parity-check.** The captured fixtures don't include source wikitext; the algorithm needs it. Either (a) add a `--cache-wikitext` flag that fetches missing revs from the MW Action API (`https://{lang}.wikipedia.org/w/api.php?action=query&prop=revisions&rvprop=content&revids={rev_id}`) on demand and stores under `parity-fixtures/.wikitext-cache/{lang}/{rev_id}.txt`, or (b) extend `scripts/capture_fixtures.py` to also capture wikitext alongside each fixture. Option (a) is more disk-efficient since wikitext is only needed at parity-check time; option (b) makes the fixture self-contained. **Recommendation: (a).**
4. **Wire up `parity-check` to actually compare.** Load expected token sequence from `rev_content.json`, run `analyse_article` on the (cached) wikitext for that rev_id, compare token-by-token. Report per-fixture passing counts. The single-revision fixtures we have only exercise the "first revision" path — the algorithm's full power requires multi-revision inputs, which will need the prior-revision wikitext too. For the first parity ratchet, single-rev parity (does the algorithm produce the right tokens for ONE revision of the article?) is enough.

Probably ~2–3 sessions of work to get from 0% → "non-zero parity on small fixtures." Obama-class parity is much later.
