# 2026-05-24 — Icaro `{{multiple issues}}` template bleed: parity-match with production (not a Rust bug)

**Goal:** chase the Icaro template-bleed flag from the first-deploy
parity suite, decide whether it's a real Rust port divergence or an
inherited upstream behavior.

**Parity:** unchanged. No algorithm code touched.

**Done:**

1. **Captured Icaro reproduction inputs.** `/tmp/icaro.wt` (8,100
   bytes) and `/tmp/icaro_tokens.json` (2,144 tokens) were already
   on disk from a prior session.
2. **Ran our Rust output against production.**
   `scripts/icaro_compare_prod.py` fetched both
   `https://wikiwho-api.wmcloud.org` and `https://wikiwho-rs.wmcloud.org`
   for en/Icaro:
   - HTML bytes: 68,790 vs 68,790 (identical).
   - 194 differing bytes total, all in trailing MW parser-cache
     footer (server hostname, timestamps, Lua/CPU timings, parser
     stat values, Render ID).
   - Both contain the `<span ...>}}</span>` around the outer
     `{{multiple issues}}` close; both surface
     `Preview warning: ... unknown parameter`.
3. **Ran the upstream Python parser on synthetic + real inputs.**
   `scripts/icaro_run_python.py` confirmed:
   - Curzon-style synthetic (single-line nested templates): no
     bleed.
   - Icaro-style synthetic (newline between inner templates): same
     bleed as our Rust port. **Upstream bug, ported faithfully.**
4. **Traced the Python parser via monkey-patch.**
   `scripts/icaro_trace_python.py` instrumented
   `__parse_wiki_text`, `__get_next_special_element`,
   `__get_special_elem_end`, `__add_spans`, `__set_token` in the
   multi-issues region. Trace pinpointed the cause: at pos=54
   (just inside the multi-issues `{{`), `__get_next_special_element`
   skips the entire template-markup type because `{{` regex's first
   match is at pos=54 itself, which is in `_jumped_elems`. It
   never re-searches for the next `{{`. The chosen candidate is
   the `=` heading marker at pos=100 — deep inside the inner
   `{{more citations needed}}` body. The parser misses the descent
   into the inner template entirely.
5. **Filed
   [issue #1](https://github.com/WikiEducationFoundation/wikiwho_rust/issues/1)**
   on the repo documenting symptom, parity-match evidence, precise
   root cause, and the upstream fix sketch. Per parity-or-die, we
   won't unilaterally diverge — escalation path is upstream
   contribution to wikimedia/WhoColor.
6. **Marked
   [notes/decisions-needed.md](decisions-needed.md) entry**
   as Resolved-2026-05-24 (chose C = accept and document) with the
   precise root cause inlined.

**Counts:** no test changes, no clippy changes, no code changes.

**Design notes / issues encountered:**

- *Trace tooling lives in `scripts/`.* The Python parser is
  dynamically patchable via `type(p)._WikiMarkupParser__method =`
  due to name-mangling — useful pattern for tracing other
  upstream-behavior questions if they come up.
- *The synthetic regression test
  `nested_template_inside_template_does_not_emit_spans`
  (Curzon shape) is still correct* — that case doesn't bleed in
  upstream either. Don't remove it.
- *`tests/icaro_repro.rs` asserts the wrong thing* (assumes the
  bleed is a Rust bug). Its assertion `span_count == 0` is wrong vs
  production behavior. Leaving in place but flagged in the issue
  body; rewrite/delete is a follow-up not load-bearing for this
  session.
- *Parity-suite asymmetry.* `whocolor_parity_suite.py` flags
  `Preview warning` and `unknown parameter` only in our HTML, not
  prod's. A small follow-up would tighten that to only flag
  asymmetric warnings.

**Resolved decisions (today):** Icaro template-bleed — accept and
document; not a Rust port bug; upstream issue tracked in
[wikiwho_rust#1](https://github.com/WikiEducationFoundation/wikiwho_rust/issues/1).

**Queued decisions (none new today).**

**Next session likely starts with:**

1. **Broader consumer testing** — the remaining ~640 articles in
   the Wiki Experts course (or whichever course Sage points at).
   Re-run `/tmp/whocolor_parity_suite.py` or its successor; iterate
   on any new divergences.
2. **Parity-suite tightening** — flag warnings only when asymmetric
   between ours and prod, so future runs don't refile this
   already-resolved Icaro issue.
3. **Cleanup of `whocolor_html.rs`** — still unused in the
   production code path (HTML-level injection module + tests). Per
   the 2026-05-24 first-deploy note, this is a deferred follow-up.
4. **`tests/icaro_repro.rs`** — delete or rewrite to assert
   parity-with-production (1 span at token-31) once we move past
   this issue.
