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

