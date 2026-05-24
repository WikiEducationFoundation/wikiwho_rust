# 01 — WMCloud deploy: first lazy-populate VPS

**Goal:** stand up a Rust WikiWho server on Wikimedia Cloud
(`globaleducation` Horizon project), running in lazy-populate mode
(cache-miss path fills storage on demand) with EventStreams ingest
watching only `simplewiki`. Observe how it performs in practice
without touching the live Python service at `wikiwho.wmcloud.org`.

This is the **first** cutover artifact — observation-only. No
traffic is redirected from the legacy service; downstream consumers
keep pointing at the existing URL. The new VPS gets its own
hostname (suggested: `wikiwho-rs.wmcloud.org`).

---

## Prerequisites

1. **Repo pushed to GitHub.** The setup script clones from
   `https://github.com/WikiEducationFoundation/wikiwho_rust.git`
   by default. Override via the `REPO_URL` env var in the user-data
   block if the repo lives elsewhere.
2. **Horizon account** with permissions in the `globaleducation`
   project. Confirm by visiting
   <https://horizon.wikimedia.org/> and selecting the project from
   the top-left switcher.
3. **An SSH key** registered in Horizon (Compute → Key Pairs) —
   needed to log into the VPS after first boot.

---

## Step 1 — Create the VPS

In Horizon → Compute → Instances → "Launch Instance":

| Field | Value |
|---|---|
| Instance Name | `wikiwho-rs-1` (or similar) |
| Source | Image: latest Debian (12 or 13) |
| Flavor | `g3.cores4.ram8.disk20` is a reasonable starting point. The cache-miss path will be the dominant disk consumer; 20 GB will fill fast if you do a lot of cold lookups. Bump to a larger disk variant if you can. |
| Networks | `lan-flat-cloudinstances2b` (the default cloud network) |
| Security Groups | `default` + `web` |
| Key Pair | Your registered key |

**User Data** (Configuration tab): paste the contents of
`deployment/wmcloud_setup.sh` from this repo, verbatim. Cloud-init
will run it as root on first boot. To customize without editing the
script, prepend environment-variable assignments at the top of the
user-data block, e.g.:

```bash
#!/usr/bin/env bash
export REPO_URL=https://github.com/MyFork/wikiwho_rust.git
export REPO_REF=main
export INGEST_LANGS=simple
# <rest of wmcloud_setup.sh below this line>
```

Click **Launch Instance**. First boot + build takes ~5–10 minutes on
a 4-core VPS.

## Step 2 — Watch the setup finish

SSH in once the instance shows as Active (Horizon → Instances →
click the instance → console URL):

```bash
ssh debian@<floating-ip>
sudo tail -f /var/log/wikiwho-rs-setup.log
```

When you see `=== Setup complete @ … ===` and a healthz JSON line,
the binaries are running. The systemd units are:

- `wikiwho-rs-server.service` — bound to `127.0.0.1:8088`
- `wikiwho-rs-ingest.service` — connected to EventStreams,
  filtering `simplewiki` only

Verify:

```bash
sudo systemctl status wikiwho-rs-server wikiwho-rs-ingest
curl -s http://127.0.0.1/healthz | jq .
```

Expected healthz body:

```json
{
  "status": "ok",
  "version": "0.0.0",
  "storage_root": "/var/lib/wikiwho-rs/storage"
}
```

## Step 3 — Add the Horizon web proxy

Horizon → Network → Web Proxies → "Add Web Proxy":

| Field | Value |
|---|---|
| Hostname | `wikiwho-rs` (becomes `wikiwho-rs.wmcloud.org`) |
| Backend instance | `wikiwho-rs-1` |
| Backend port | `80` |

Wait ~30s for DNS to propagate. Test from your workstation:

```bash
curl -s https://wikiwho-rs.wmcloud.org/healthz | jq .
```

If you get the healthz JSON, public routing is up.

## Step 4 — Smoke-test the cache-miss path

Trigger a cold build for a small simplewiki article. The first
request returns the "still processing" envelope (HTTP 408 per
API.md §1); the second request (after the background task finishes)
returns the real data:

```bash
# First request — triggers cache-miss, expect 408 with retry envelope.
curl -i https://wikiwho-rs.wmcloud.org/simple/api/v1.0.0-beta/rev_content/Photosynthesis/

# Wait ~10 seconds for a small article, longer for big ones.
sleep 10

# Second request — should now return the real data.
curl -s https://wikiwho-rs.wmcloud.org/simple/api/v1.0.0-beta/rev_content/Photosynthesis/ \
  | jq '.revisions | length'
```

Behind the scenes:

- `wikiwho-server` calls the MW Action API to fetch the full
  revision history for `Photosynthesis`.
- Each revision is analysed via `Article::analyse_revision`.
- The article is persisted under
  `/var/lib/wikiwho-rs/storage/simple/<page_id>/…`.
- The in-memory title + rev_id indexes refresh so the next request
  serves from disk.

Watch the server log to confirm:

```bash
sudo journalctl -u wikiwho-rs-server -f
```

Look for the cache-miss `tracing::info!` lines and the per-request
`TraceLayer` lines (method, path, status, latency).

## Step 5 — Observe ingest

Tail the ingest log:

```bash
sudo journalctl -u wikiwho-rs-ingest -f
```

You'll see one log line per simplewiki edit:

- `applied` — a known article got a new revision applied
- `snapshot not on disk, skipping` (at debug level) — an edit on an
  article that's never been requested. Lazy populate means ingest
  doesn't proactively pull these.
- `snapshot already at-or-ahead of event` — SSE replay; idempotent
  skip.
- `apply failed` — investigate.

Checkpoint state lives at
`/var/lib/wikiwho-rs/storage/ingest/checkpoint.json`. On restart,
ingest resumes from this offset.

## Step 6 — Disk usage budget

The starter flavor has ~20 GB of root disk. The OS + repo + Rust
toolchain claim ~5 GB. That leaves ~15 GB for storage.

Per-article disk cost in our storage layout is in the same ballpark
as the legacy gzipped pickles per
[STORAGE.md](../../STORAGE.md) §5:

- Tiny article (~50 revs): ~30 KB
- Small article (~500 revs): ~300 KB
- Jesse-Owens-class (~6 k revs): ~3 MB
- Obama-class (~57 k revs): ~30 MB (estimated; not yet measured at
  scale)

If lazy-populate traffic skews toward big articles, the VPS can
fill in days. To reset:

```bash
sudo systemctl stop wikiwho-rs-ingest wikiwho-rs-server
sudo rm -rf /var/lib/wikiwho-rs/storage/*
sudo systemctl start wikiwho-rs-server wikiwho-rs-ingest
```

(This is safe — the storage is a cache; the next request rebuilds
on demand.)

## Step 7 — Add more languages later

Edit the ingest unit to widen the language set:

```bash
sudo systemctl edit wikiwho-rs-ingest
# In the editor, add:
[Service]
Environment=WIKIWHO_INGEST_LANGS=simple,zh,eu
```

Then `sudo systemctl restart wikiwho-rs-ingest`.

The server doesn't need a per-language allowlist — it serves any
language the storage tree contains (and triggers cache-miss for
any it doesn't).

## Step 8 — Re-deploy after a code change

After pushing a new commit upstream:

```bash
ssh debian@<floating-ip>
cd /home/wikiwho/wikiwho_rust
sudo -u wikiwho git pull --ff-only
sudo -u wikiwho /home/wikiwho/.cargo/bin/cargo build --release --bin wikiwho-server --bin ingest
sudo install -m 0755 target/release/wikiwho-server /usr/local/bin/wikiwho-server
sudo install -m 0755 target/release/ingest /usr/local/bin/ingest
sudo systemctl restart wikiwho-rs-server wikiwho-rs-ingest
curl -s http://127.0.0.1/healthz | jq .version
```

If the storage format changed (schema bump), wipe storage first
(Step 6's reset) before restarting. Otherwise the reader will fail
to open existing snapshots.

---

## Things to watch for during observation

- **Disk fill rate** — `df -h /var/lib/wikiwho-rs`. If filling
  aggressively, the lazy-populate trade-off is the wrong one for
  this VPS size; either bump the disk or add a small Cinder volume.
- **Per-request latency** — TraceLayer's INFO lines include
  request_time. Cache-miss responses to the *client* are fast
  (408 returned immediately); the spawned background task is what
  to measure. Add explicit timing instrumentation if it becomes
  important — currently logged at debug.
- **EventStreams reconnect rate** — frequent `event stream error;
  reconnect handled internally` lines suggest network instability
  or a misbehaving upstream filter. The reconnect-with-Last-Event-ID
  loop is built in, but a noisy log might mask real issues.
- **MW Action API errors** — cache-miss + ingest both call MW.
  429s should auto-retry per `wikiwho-mwclient`, but a sustained
  burst could trip rate limits.
- **Memory** — server holds in-memory title + rev_id indexes per
  language. For lazy-populate on small wikis these stay tiny;
  watch `systemctl status wikiwho-rs-server` Memory: line.

## What this deploy does NOT verify

- **Wire-format byte parity vs the legacy service in production.**
  Shadow traffic comparison is a separate piece of work — would
  involve replaying captured production requests against both
  services and diffing. The parity corpus under `parity-fixtures/`
  already validates the algorithm; the open question is
  *operational* (does the live service stay correct under real
  request mixes, race conditions, etc.).
- **Performance at scale.** Lazy-populate against one small wiki
  is a smoke test, not a stress test. Numbers from this VPS
  shouldn't be projected to enwiki without re-measurement.
- **Sustained ingest behavior.** Hours-to-days of EventStreams
  uptime will surface issues that a short test won't.
- **Cold-build behavior on Obama-class articles.** Don't trigger
  one of these by mistake — it will fill the disk and take
  many minutes. Use a small fixture article (Photosynthesis on
  simplewiki, say) for smoke tests.

---

## Open questions

(File these to `notes/decisions-needed.md` as they get answered or
diverge from expectations.)

- What's the actual disk-per-article distribution under lazy
  populate on a real wiki? Run for a week, then sample.
- Does the MW Action API rate-limit us when many cold articles
  arrive at once? Add backpressure if so.
- Should the server expose `/metrics` for Prometheus scraping?
  Useful but not load-bearing for the first deploy.
- Pickle re-compression on the legacy /pickles/en (queued
  decision) — irrelevant for this Rust deploy but worth coordinating
  if/when we re-bootstrap from the existing prod data.
