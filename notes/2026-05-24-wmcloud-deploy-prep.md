# 2026-05-24 — WMCloud deploy prep: setup script, systemd units, nginx, /healthz

**Goal:** produce the artifacts needed to stand up a Rust WikiWho
server on Wikimedia Cloud (Horizon `globaleducation` project) in
lazy-populate mode, with EventStreams ingest watching only
`simplewiki`. Sage's intent (this session): "prepare a server in
wmcloud, with just lazy populate, to see how it performs in
practice."

**Parity:** N/A — no algorithm changes.

**Counts before → after:**

- Workspace tests: **321 → 322** (+1: healthz handler smoke test).
- Workspace clippy clean with `-D warnings --all-targets`.
- Release builds for `wikiwho-server` + `ingest`: 11 MB / 8.6 MB
  dynamically-linked ELF on Debian 14 (glibc 2.42). Decision: build
  on the VPS rather than ship the host binary — typical WMCloud
  VPSes run Debian 11/12 (glibc 2.31/2.36) and the host binary will
  not run there. Setup script does a release build in-place.

**Done:**

- **`crates/wikiwho-server/src/handlers/health.rs`** — new module.
  `GET /healthz` returns `{"status":"ok","version":"0.0.0",
  "storage_root":"…"}`. Tested in-crate with `tower::ServiceExt::
  oneshot`. Wired into `routes::router` at top of the route list so
  nginx + humans can liveness-probe cheaply.
- **`crates/wikiwho-server/src/routes.rs`** — added
  `tower_http::trace::TraceLayer::new_for_http()` configured with
  INFO-level spans + on_response. Now every request emits a single
  structured log line with method, path, status, and latency,
  visible via `journalctl -u wikiwho-rs-server -f` in production.
- **`deployment/wikiwho-rs-server.service`** — systemd unit. Runs
  as a `wikiwho` user, binds to `127.0.0.1:8088`, storage at
  `/var/lib/wikiwho-rs/storage`, `RUST_LOG=info,tower_http=info`,
  basic hardening (`NoNewPrivileges`, `ProtectSystem=full`,
  `ProtectHome=read-only`, `PrivateTmp`).
- **`deployment/wikiwho-rs-ingest.service`** — systemd unit. Same
  user / storage / hardening; `WIKIWHO_INGEST_LANGS=simple` by
  default (env override in user-data can widen).
- **`deployment/nginx-site.conf`** — nginx site config. Proxies
  `:80 → 127.0.0.1:8088`. Cheap `location = /healthz` carve-out
  with `access_log off`. Long `proxy_read_timeout` (360s) for the
  cache-miss path on cold articles.
- **`deployment/wmcloud_setup.sh`** — Horizon cloud-init user-data
  script. Idempotent. Installs apt deps (build-essential, nginx,
  git), creates the `wikiwho` user, installs rustup, clones the
  repo from
  `https://github.com/WikiEducationFoundation/wikiwho_rust.git`
  (overridable via `REPO_URL`), builds release binaries, installs
  them to `/usr/local/bin`, drops systemd units + nginx site,
  starts the services, and prints a healthz response + a reminder
  to add the Horizon web proxy entry. Logs to
  `/var/log/wikiwho-rs-setup.log`.
- **`notes/cutover/01-wmcloud-deploy.md`** — runbook. Step-by-step
  for the actual deploy: VPS creation in Horizon, pasting the
  setup script into user-data, post-setup smoke tests (curl
  healthz + a cache-miss-triggering request), where to watch
  logs, disk budget (~15 GB after OS + toolchain on a 20 GB
  flavor), how to add more languages, how to reset storage if it
  fills, how to re-deploy after a code change. Includes an
  explicit "what this deploy does NOT verify" section so the
  observation goals don't get over-claimed.

**Design notes / issues encountered:**

- *Why build-on-VPS instead of cross-compile?* Three options
  considered: (a) ship the host binary — host is Debian 14
  (glibc 2.42), WMCloud VPSes are typically glibc 2.31/2.36, so
  the binary won't run; (b) cross-compile `x86_64-unknown-linux-
  musl` — requires the musl target installed and produces a
  ~14 MB static binary, fine but adds CI setup complexity; (c)
  build-on-VPS — slow first boot (~5–10 min on a 4-core VPS)
  but no artifact-hosting story needed and the binary always
  matches the host. Picked (c) for the first deploy; revisit if
  iteration speed becomes a problem (option B is the natural
  upgrade).
- *Why no Cinder volume?* Sage explicitly chose root-disk storage
  for the first deploy: "no Cinder volume. we'll plan to operate
  primary in lazy mode and can clear storage if it runs up against
  the small limits of the VPS." The runbook documents the reset
  procedure (stop services, wipe `/var/lib/wikiwho-rs/storage`,
  restart) so this is a known operational move, not a recovery
  scenario.
- *Why simplewiki for ingest, not nothing?* Sage's original answer
  said "no EventStreams… one language seems like a good idea too
  though. let's go with simple." So ingest is on; just narrowly.
  This exercises the apply loop end-to-end while keeping
  disk-growth surface small (simplewiki edits are infrequent and
  articles are small).
- *Why TraceLayer at INFO, not DEBUG?* Production observability
  is the explicit goal of this deploy ("see how it performs"). At
  DEBUG the per-request lines are buried under tokio/hyper debug
  noise; at INFO they're the dominant signal in `journalctl`.
  Tunable per-deployment via `RUST_LOG` if it becomes too chatty.
- *Setup-script idempotency.* `apt-get install` is idempotent;
  `useradd` checks for existing user; rustup install is gated on
  presence of `~/.cargo/bin/cargo`; repo step uses `git fetch +
  reset --hard` if the dir exists, `git clone --depth=1` otherwise;
  binaries are `install -m 0755` (overwrites); storage dir uses
  `install -d` (no-op if present); systemd / nginx files are
  `install -m 0644` (overwrites). Re-running the script after a
  code push is a valid update path.

**Queued decisions / open questions:**

- None new for `notes/decisions-needed.md`. The runbook flags
  three forward-looking questions (disk fill rate, MW rate-limit
  behavior, Prometheus `/metrics`) but as observation targets,
  not decisions that block work.

**Pre-deploy checklist for Sage:**

1. **Push the repo to GitHub.** The default URL in the setup script
   is `github.com/WikiEducationFoundation/wikiwho_rust`; either
   create that and push, or override `REPO_URL` in the user-data
   block of the Horizon VPS creation.
2. **Confirm `globaleducation` Horizon project access** —
   <https://horizon.wikimedia.org/> top-left switcher.
3. **Decide hostname** for the Horizon web proxy. Runbook suggests
   `wikiwho-rs.wmcloud.org`; anything in the `.wmcloud.org` zone
   the proxy will accept works.

**Next session likely starts with:** observation work after the
deploy. Numbers to capture (file under `notes/cutover/02-<date>-
observations.md`):

- Memory + disk footprint after N requests
- Cache-miss wall-clock for a small + medium article
- Ingest events/sec on simplewiki + apply-loop latency
- Any unexpected log lines (rate-limit, reconnect storms, panics)

If the deploy reveals concrete bugs or design gaps, file each as a
`notes/decisions-needed.md` entry for the main thread to pick up;
treat the operational session as gathering ground truth, not
fixing things on the VPS directly.
