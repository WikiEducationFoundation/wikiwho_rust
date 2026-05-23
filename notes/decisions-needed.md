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

## 2026-05-22 — handling historical-tokenization divergence [non-blocking]

**Context:** First parity ratchet (tokenizer level) hit 90.02% — 15 of 16 fixtures at 100%, with COVID-19_pandemic at 5.50%. The COVID failure is a historical-state effect: the article has two multi-CJK-char tokens (`黄冈送别山东援鄂医疗队`, `黄梅戏大剧院`) introduced in 2022 *before* wikiwho's CJK-splitter logic existed. The current code (and our port) splits CJK chars individually; the sentence has been stable since, so production has been hash-matching at sentence level and preserving the pre-split tokens. Single-rev parity *cannot* reproduce this without replaying the article's full history. See `notes/2026-05-22-first-parity-ratchet.md` for the full analysis.

This will compound when we add the real algorithm: ANY article with old non-ASCII content + a stable sentence around it is exposed to the same effect.

**Options:**
- **A. Accept and quarantine.** Add a `--known-divergences` config (or inline annotations on fixtures) marking fixture+token-position combinations where the algorithm's *current* output is correct but production has accumulated a different value. Report them separately in the ratchet output: "100% modulo N known historical-state divergences." Keeps the ratchet honest; explicit about what we can't fix without full history.
- **B. Re-run full history per fixture.** Fetch every revision of each article up to the target rev_id, run our algorithm through all of them, then compare. Expensive (Obama = 57K revs = hours per parity run) but reproduces production state exactly. The plan calls this Level B parity (ALGORITHM.md §10).
- **C. Hybrid: A for now, B later.** Ship A for the current ratchet so algorithm work isn't held up; add B as an optional `--full-history` mode once the algorithm is correct enough that it's worth the cost.

**Recommendation:** **C.** Sage doesn't need to weigh in for the next sessions — the algorithm work proceeds either way. When the algorithm is ~90%+ on single-rev parity, revisit and decide whether B is worth building.

