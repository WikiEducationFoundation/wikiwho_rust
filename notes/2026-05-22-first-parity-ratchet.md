# 2026-05-22 — first parity ratchet (tokenizer level)

**Goal:** wire the parity-check binary to actually compare Rust output to fixtures, and get the first non-zero parity number.

**Parity:**
- revisions: 0 / 16 → **15 / 16 (93.75%)**
- tokens: 0 / 973,970 → **876,775 / 973,970 (90.02%)**
- ms / full-corpus: 879 ms (release build)

This is **tokenizer-level parity only** — does our paragraph / sentence / token splitter produce the same string sequence as the reference on real Wikipedia text? It doesn't validate `o_rev_id`, `token_id`, `editor`, `in`, or `out`; those need the matching cascade.

**Done:**
- `wikiwho_attribute::tokenize::tokenize_revision(text)` — walks paragraphs → sentences → tokens with the algorithm's filtering (skip empty paragraphs per wikiwho.py:340, skip empty sentences per wikiwho.py:476), returns flat `Vec<String>` in document order. Caller lowercases. Verified against the Python reference via 5 new probes in `scripts/verify_tokenizer.py` + 5 Rust unit tests. Currently 30 tests, all green.
- `scripts/cache_wikitext.py` — fetches wikitext for each fixture from the MW Action API (`action=query&prop=revisions&rvprop=content&rvslots=main`) and writes `wikitext.txt` alongside `meta.json`. Idempotent (skips existing). Polite (1s between requests). Ran successfully on all 16 fixtures, ~371 MB now under `parity-fixtures/` (gitignored).
- `parity-check` extended: loads wikitext.txt, lowercases, runs `tokenize_revision`, positionally compares to `rev_content.json`'s `tokens[i].str`. Reports PASS/FAIL per fixture, length mismatches with delta, optional `--show-first-diff`. The summary block now describes the comparison level explicitly (tokenizer-only) so future-Claude doesn't mistake the percentage for full algorithm parity.

**Per-fixture results:**

15 of 16 fixtures pass at 100%, including the big ones:

| fixture | tokens | result |
|---|---|---|
| en/534366 Barack_Obama | 111,282 | ✓ 100% |
| en/74998519 Gaza_war | 126,096 | ✓ 100% |
| fr/681159 Paris | 111,647 | ✓ 100% |
| en/5043734 Wikipedia | 91,249 | ✓ 100% |
| en/1095706 Jesus | 75,381 | ✓ 100% |
| en/22989 Paris | 62,457 | ✓ 100% |
| de/2552494 Berlin | 61,558 | ✓ 100% |
| en/736 Albert_Einstein | 59,786 | ✓ 100% |
| en/46827 Jesse_Owens | 49,850 | ✓ 100% |
| ar/4287 Cairo | 46,602 | ✓ 100% (RTL works) |
| en/2731583 Adolf_Hitler | 43,323 | ✓ 100% |
| en/24544 Photosynthesis | 27,349 | ✓ 100% |
| simple/27263 Wikipedia | 4,495 | ✓ 100% |
| en/79023819 Israel–Hamas_war | 23 | ✓ 100% (redirect) |
| zh/1686258 中国 | 18 | ✓ 100% (redirect + CJK) |
| **en/62750956 COVID-19_pandemic** | 102,854 | **✗ 5.50% (5,659 / 102,854)** |

**On the COVID-19 failure (this is the interesting finding):**

The Rust tokenizer is correct. The failure is a **historical-state effect** that single-revision parity *cannot* reproduce.

Specifically: the fixture has two multi-CJK-char tokens stored as single strings — `黄冈送别山东援鄂医疗队` (token_id 981084) and `黄梅戏大剧院` (token_id 981085) — both with `origin_rev_id=1077610178` (introduced 2022). The current `WikiWho/utils.py` *does* split CJK characters individually (verified by running the Python reference against the exact sentence; output matches Rust exactly: each char becomes its own token).

These tokens were introduced when wikiwho's CJK-splitter logic didn't exist yet, and the surrounding sentence has been stable since. The algorithm's sentence-level hash-match has been re-using the pre-split tokens on every subsequent revision, so production wikiwho-api keeps emitting them as single tokens despite the code now wanting to split them.

When we eventually port the full attribution algorithm and run it from scratch on rev 1355596341, we'd produce the *current* tokenization (each CJK char separately) — different from what production has accumulated. The only way to reproduce production's exact output is to replay the article's full history starting from when those tokens were introduced.

Implication for the upcoming algorithm parity work: any per-revision parity check is going to undercount in the same way for any article with old CJK content. The fix isn't "fix the tokenizer" — the tokenizer is right. The fix is to either (a) accept the historical-state divergence and report it explicitly as a known-acceptable category, or (b) reproduce historical state by re-running the algorithm against the full revision history. (b) is the right thing eventually but expensive.

**New decisions queued:** one — see `notes/decisions-needed.md` for the "how to handle historical-tokenization divergence" question. **Non-blocking** — the ratchet works fine without an immediate answer.

**Next session likely starts with:**

1. Port `WikiWho/structures.py` to `crates/wikiwho-attribute/src/structures.rs` — `Word`, `Sentence`, `Paragraph`, `Revision`, plus an `Article` container that holds the arena-allocated tokens / sentences / paragraphs and the cross-revision hash tables (`paragraphs_ht`, `sentences_ht`). Use arena indices (typed aliases for now — `TokenId = u32`, `SentenceId = u32`, `ParagraphId = u32`) rather than `Rc<RefCell<...>>`. Mirror Python field names where the algorithm references them by name.
2. Port `Wikiwho.__init__` and the spam-detection heuristic (`wikiwho.py:139-205`). Spam constants from `ALGORITHM.md §3.3`. This is the easy first slice — no matching cascade.
3. Skeleton of `determine_authorship`. Don't implement the matching cascade yet, but get the function signatures + per-iteration matched-set scaffolding in place.
4. Begin porting `analyse_paragraphs_in_revision` (the first level of the cascade). This is where the parity number will start to validate `o_rev_id` / `token_id`, not just `str`.

Estimate: ~2 sessions to get the structures + the paragraph-level cascade running. After that, sentence and token cascades. Obama-class parity is much later.
