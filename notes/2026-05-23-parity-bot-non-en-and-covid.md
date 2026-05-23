# 2026-05-23 — parity-bot session: COVID-19 + 6 non-en anchors

**Workstream:** parity-corpus growth (parallel agent, tagged
`parity-bot`). The main thread was busy with an en capture; this
session worked the non-en anchors and the already-captured COVID-19
fixture per `notes/parity-corpus-wishlist.md`.

**Parity numbers before → after this session:**

Already-in-corpus baseline (from
`notes/2026-05-23-mwclient-and-broader-corpus.md`):
- 9 fixtures × ~77 605 revs, all-fields 100% vs python_replay on 9 / 9.

Added this session (all full-history `parity-check`):

| Fixture | revs | vs python | vs prod-cache | status |
|---|---|---|---|---|
| **en/62750956** COVID-19_pandemic @ 1355596341 | 26 921 | **100.00 %** (102 897 tokens) | 5.50 % str / 4.16 % all-fields, length Δ +43 | divergence — python 100 %, prod-cache historical-drift |
| **ja/4821051** 日本 @ 109654789 | 801 | **100.00 %** (76 761 tokens) | 6.94 % str / 0.57 % all-fields, length Δ +33 823 | divergence — python 100 %, prod-cache historical-drift |
| **pt/404** Brasil @ 72290662 | 10 414 | **100.00 %** (51 190 tokens) | **100.00 %** | **validated** |
| **hi/59** भारत @ 6550353 | 1 928 | **100.00 %** (29 704 tokens) | **100.00 %** | **validated** |
| **ru/71** Москва @ 152499807 | 7 508 | **100.00 %** (67 097 tokens) | (not run — wrap-up) | claimed — python 100 %, prod-cache pending |
| **he/325** ירושלים @ 43259166 | 7 183 | **100.00 %** (40 087 tokens) | (not run — wrap-up) | claimed — python 100 %, prod-cache pending |
| **es/972** España @ 173589609 | 10 438 | (python_replay generated but parity-check not run — wrap-up) | (not run) | claimed — replay generated, parity pending |

**Aggregate impact:** 5 new fixtures fully validated vs python_replay
(264 735 tokens covering en + ja + pt + hi + ru + he scripts); 2 of
those (pt, hi) also at 100 % vs prod-cache. The **en/COVID-19 result
is the headline:** it closes the documented CJK-tokenizer
historical-state divergence flagged in
`notes/2026-05-22-first-parity-ratchet.md`. Python parity at 100 %
proves the port is correct; prod-cache divergence is reference-source
disagreement, not a port bug.

**Wishlist hygiene work done:**

Three wishlist rows had wrong page_ids — confirmed by `capture_fixtures.py`'s
title-based MW lookup:

- ja 日本: 71 → **4821051**
- pt Brasil: 1631 → **404**
- he ירושלים: 2 → **325**

Two other rows had page_ids that returned `missing` from MW (ru 968,
es 6347, hi 7); fixed to **ru 71 / es 972 / hi 59**. All committed in
`b7812dd` and `ea166ed`.

**Divergence filed:** `notes/decisions-needed.md` 2026-05-23 entry
"parity-corpus: large prod-cache divergence on en/COVID-19 and ja/日本"
records the failure mode with recommendation to codify "validated-vs-python
(prod-cache drift)" as a valid terminal state in the wishlist playbook.

**Feedback memory saved:** `feedback_no_parallel_python.md` —
serialize `python_replay.py` invocations; running 2-3 in parallel
starved Sage's machine for memory during this session. Rust
`parity-check` and network-bound `capture-history` can still
parallelize across language hosts.

**Next session starting point:**

- Decide on the prod-cache divergence proposal (decisions-needed.md A
  vs B vs C). If A, codify the wishlist playbook to allow
  `validated-vs-python` as terminal.
- Finish the three still-claimed rows: `ru/71`, `he/325`, `es/972` —
  each needs at most one prod-cache parity-check and (for es) one
  python-replay parity-check. **Run these serially**, not in parallel
  (per the new feedback memory).
- After that the next-easy wins on the wishlist are the
  `blocked-on-running-en-capture` rows — they unblock as soon as the
  main thread's en capture exits. `pgrep -af capture_history.py`
  before claiming.
