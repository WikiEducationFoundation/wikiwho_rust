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

---

## 2026-05-22 — handling historical-tokenization divergence [non-blocking]

**Context:** First parity ratchet (tokenizer level) hit 90.02% — 15 of 16 fixtures at 100%, with COVID-19_pandemic at 5.50%. The COVID failure is a historical-state effect: the article has two multi-CJK-char tokens (`黄冈送别山东援鄂医疗队`, `黄梅戏大剧院`) introduced in 2022 *before* wikiwho's CJK-splitter logic existed. The current code (and our port) splits CJK chars individually; the sentence has been stable since, so production has been hash-matching at sentence level and preserving the pre-split tokens. Single-rev parity *cannot* reproduce this without replaying the article's full history. See `notes/2026-05-22-first-parity-ratchet.md` for the full analysis.

This will compound when we add the real algorithm: ANY article with old non-ASCII content + a stable sentence around it is exposed to the same effect.

**Options:**
- **A. Accept and quarantine.** Add a `--known-divergences` config (or inline annotations on fixtures) marking fixture+token-position combinations where the algorithm's *current* output is correct but production has accumulated a different value. Report them separately in the ratchet output: "100% modulo N known historical-state divergences." Keeps the ratchet honest; explicit about what we can't fix without full history.
- **B. Re-run full history per fixture.** Fetch every revision of each article up to the target rev_id, run our algorithm through all of them, then compare. Expensive (Obama = 57K revs = hours per parity run) but reproduces production state exactly. The plan calls this Level B parity (ALGORITHM.md §10).
- **C. Hybrid: A for now, B later.** Ship A for the current ratchet so algorithm work isn't held up; add B as an optional `--full-history` mode once the algorithm is correct enough that it's worth the cost.

**Recommendation:** **C.** Sage doesn't need to weigh in for the next sessions — the algorithm work proceeds either way. When the algorithm is ~90%+ on single-rev parity, revisit and decide whether B is worth building.

