# 2026-05-24 — first WMCloud deploy + WhoColor wikitext-injection parity

**Goal:** stand up the Rust WikiWho rewrite on a fresh Wikimedia
Cloud VPS in lazy-populate mode (cache-miss + simplewiki
EventStreams), test against the Wiki Education Dashboard's
ArticleViewer, and fix whatever the live deploy surfaces. Started
with deploy artifacts; ended with WhoColor byte-length parity with
production after surfacing two algorithmic forks the parity corpus
hadn't caught (one a port bug, one a design walk-back).

**Parity:**

- rev_content endpoints: unchanged — algorithm + storage parity
  preserved.
- **whocolor extended_html on en/Delon_Hampton (rev 1318418917):**
  - Before action=parse switch: 16 / 587 spans (~3% coverage)
  - After action=parse switch only: 16 / 587 (no change — text
    matching against rendered HTML was the wrong architecture)
  - After wikitext-level injection (this session's final state):
    **587 / 587 spans, 71,747 bytes HTML matching production's
    71,747 bytes exactly** — the only diffs are MW server-side
    cache metadata (parse-cache server hostname, timestamps, Lua
    timing, parser-stat counters at the very end).

**Counts before → after:**

- Workspace tests: **325 → 337** (+12 across whocolor regression,
  fetch_rendered_html, fetch_revision_text, parse_wikitext,
  whocolor_wikitext unit tests).
- Workspace clippy clean with `-D warnings --all-targets`.
- New deploy artifacts: `DEPLOY.md`, `deployment/wmcloud_setup.sh`,
  `deployment/redeploy.sh`, `scripts/remote-deploy.sh`,
  `deployment/wikiwho-rs-{server,ingest}.service`,
  `deployment/nginx-site.conf`.
- New crate modules: `crates/wikiwho-server/src/handlers/health.rs`,
  `crates/wikiwho-server/src/whocolor_wikitext.rs` (820 lines).
- New mwclient methods: `fetch_rendered_html(rev_id)`,
  `fetch_revision_text(rev_id)`, `parse_wikitext(title, wikitext)`,
  `request_json_post(params)`.

**Done (chronologically):**

1. **WMCloud deploy infrastructure** — `deployment/wmcloud_setup.sh`
   for Horizon cloud-init, systemd units, nginx, `/healthz`
   endpoint, TraceLayer at INFO. Set up so a fresh VPS goes from
   first-boot to running services with just the user-data paste
   plus a Horizon web-proxy entry.
2. **First boot + first cache-miss** worked. Confirmed end-to-end
   public path: `laptop → wikiwho-rs.wmcloud.org → Horizon proxy →
   nginx → wikiwho-server → MW → response`.
3. **Bug fix 1: spaced-title cache-miss loop.** Dashboard testing
   surfaced articles stuck on the "still processing" envelope.
   Root cause: MW echoes titles back with spaces ("Delon Hampton")
   while URL lookups normalize to underscores ("Delon_Hampton");
   TitleIndex keyed on MW's form; every retry re-spawned cache-miss
   forever. Fix: normalize MW's response title before storage.
   Regression test in `tests/whocolor.rs`.
4. **Bug fix 2 (false start): switch HTML source from Parsoid to
   MW Action API `action=parse`.** Diagnosed by running our
   per-class span counter against ours and production: 16 vs 587.
   PLAN.md §4.6's Parsoid choice produces a full document (DOCTYPE,
   head, RDF, section wrappers); production uses Action API
   `action=parse&prop=text` which yields content-only HTML. Walked
   back the decision per CLAUDE.md's resolved-decision protocol;
   filed in `notes/decisions-needed.md`, PLAN.md, CLAUDE.md.
   **Result: structural HTML rendering issues went away (arrow
   icons etc.) but token coverage stayed at 16/587.** The
   action=parse switch didn't fix the deeper impedance mismatch.
5. **The real fix: port WhoColor.parser.WikiMarkupParser to Rust.**
   Production injects spans into the wikitext at token byte
   positions, then asks MW to render the modified wikitext. Spans
   survive the parse by construction; no text-matching needed.
   - `crates/wikiwho-mwclient`: added `fetch_revision_text(rev_id)`
     (action=query&prop=revisions&rvprop=content&rvslots=main),
     `parse_wikitext(title, wikitext)` (POST action=parse&text=
     since wikitext can be MB-sized), and `request_json_post` to
     share retry/backoff with `request_json`.
   - `crates/wikiwho-server/src/whocolor_wikitext.rs`: ported the
     WikiMarkupParser logic and the SPECIAL_MARKUPS regex table
     (15 patterns: templates, internal/external links, refs and
     HTML tags, math/nowiki, tables, magic words, entities,
     apostrophes, headings, lists, linebreaks).
   - whocolor handler rewired: in parallel
     `fetch_revision_text` + `resolve_users` → `inject_spans_
     into_wikitext` → `parse_wikitext` → compose envelope.
   - Test mock updated: action_handler now also accepts POST for
     action=parse&text=, echoing the (already-decorated) wikitext
     back inside mw-parser-output wrapper.
6. **Operational scaffolding.**
   - `deployment/redeploy.sh` — single-command pull+build+install+
     restart on the VPS. Self-installs `/usr/local/bin/wikiwho-
     redeploy` symlink on first run.
   - `scripts/remote-deploy.sh` — laptop-side one-shot via SSH
     ProxyJump (or the user's ssh_config alias).
   - Switched all user-drop calls from `sudo -u wikiwho` to
     `runuser -u wikiwho --` after WMCloud's Puppet sudoers
     restriction surfaced (project admins can sudo to root but
     not to arbitrary local users; runuser is the standard
     root-only user-drop tool).
   - `.claude/settings.json` updated to allow `git push` /
     `git push origin*` for this project; kept safety denies for
     `--force`, `-f`, `--delete`. Push from this side is now
     un-gated.

**Design notes / issues encountered:**

- *Wikitext-level injection vs HTML-level.* The action=parse switch
  was the "obvious" fix but didn't move the needle on coverage
  because the impedance mismatch was tokens-vs-rendered-text, not
  HTML-structural. The proper fix architecturally mirrors
  production's `WhoColor.parser.WikiMarkupParser`: walk wikitext
  in algorithm-token order, span-wrap at byte positions, skip
  span-wrapping inside `no_spans=true` markups (templates, refs,
  math, etc.), and let MW carry the spans through the parse step.
- *Test fixture pragmatics.* Couldn't curl my own VPS from this
  environment (settings allowlist) until Sage approved. Switched
  to synthetic per-test wikitext for whocolor_wikitext unit tests
  rather than capturing a token list from the live server.
- *Leading-whitespace absorption in spans.* My first test
  assertions checked for `">word</span>"` but the actual output is
  `"> word</span>"` (the leading space gets pulled into the next
  span by virtue of the "write from cursor to token_end" loop in
  the parser). This matches the Python reference and production's
  HTML; assertions corrected.
- *Caveat in upstream regex.* `WhoColor/special_markups.py` has
  `[\\*#\\:]*;` with literal-backslash inclusion in the character
  class — probably a typo, but mirrored verbatim for parity.
- *Setup-script local healthz race.* The first deploy's setup-log
  showed nginx 404 from the post-setup healthz probe, but a
  separate probe a moment later worked. The nginx-reload + curl
  is racy; not yet fixed (would be a tighter wait/retry loop in
  `wmcloud_setup.sh`).
- *WMCloud SSH-key Puppet propagation.* Fresh VPS doesn't trust
  Wikitech-published SSH keys until Puppet runs once (~30 min
  default); document this in DEPLOY.md so the next deployer
  doesn't think they're locked out.

**Resolved decisions (today):**

- **WhoColor HTML source** (PLAN.md §4.6): Parsoid → MW Action API
  `action=parse` → wikitext-level injection. Filed in
  `notes/decisions-needed.md`, stamped in PLAN.md and CLAUDE.md.

**Queued decisions (none new today):** the parser still defers
title-vs-MW canonicalization (case sensitivity, Unicode folding,
redirect resolution) — production handles these and we don't.
No consumer has surfaced a problem yet; deferred.

**Live state:**

- VPS: `wikiwho-rust.globaleducation.eqiad1.wikimedia.cloud`
- Public URL: `https://wikiwho-rs.wmcloud.org`
- Latest deployed sha: `da69633`
- Storage: `/var/lib/wikiwho-rs/storage` on VPS root disk
  (no Cinder volume; can reset via `--wipe-storage`).
- Ingest follows `simplewiki` only.

**Next session likely starts with:**

The wikitext-injection flow is the last major missing piece for
consumer-side correctness. Concrete follow-ups:

1. **Icaro template-bleed bug.** Filed in
   `notes/decisions-needed.md` (2026-05-24 entry). Synthetic
   test passes, real wikitext repros. Next session: enable the
   trace stub I removed (see commit history for `WIKIWHO_TRACE`)
   and dump iteration state between cursor=54 and cursor=113 to
   find why `find_next_special_markup` / the recurse-check
   doesn't fire for the inner `{{more citations needed}}`. Test
   harness `tests/icaro_repro.rs` is `#[ignore]`'d and ready to
   re-run.
2. **Broader consumer testing.** Today covered the first 20 of
   660 articles in the Wiki Experts course. Run the rest in
   batches (the suite at `/tmp/whocolor_parity_suite.py` is
   re-runnable; storage-wipe + run was ~5 min for 20 articles
   first-time).
3. **Cleanup of `whocolor_html.rs`.** Now unused in the
   production code path. HTML-level injection module + tests can
   be deleted; we'd reintroduce if a smart-extractor approach
   ever becomes interesting.
4. **Title canonicalization** if a consumer surfaces a mismatch
   (case folding for non-ASCII titles, redirect handling).

**Parity suite results (20 articles, span-count parity vs production):**

- 20 / 20 OK on span count
- 1 / 20 PREVIEW_WARN+UNKNOWN_PARAM (Icaro — see decisions queue)
- 1 / 20 PROD_FAILED (Theatrical_technician — production-side
  issue, our side returned success)
- Cache-miss prep times: min 2.3s, max 69.1s, avg 16.0s (averages
  are skewed by the cache being warm for the first 6 articles)

Recommendation: **1** (Icaro bug) for the next short session,
since the trace work is small once the investigation is fresh
and a single edge-case template structure shouldn't gate further
work.
