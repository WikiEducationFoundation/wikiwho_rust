# Deploying wikiwho_rust on Wikimedia Cloud

Stand up a fresh VPS in the **`globaleducation`** Horizon project,
running the Rust WikiWho rewrite in **lazy-populate** mode
(cache-miss fills storage on demand) with **EventStreams ingest
following only `simplewiki`**.

Goal: observe how it performs under real traffic without affecting
the live service at `wikiwho.wmcloud.org`.

Total hands-on time: ~5 minutes. First-boot build runs unattended
for ~5–10 minutes after that.

---

## Prerequisites

1. **Horizon access** to the `globaleducation` project.
   Switch to it from the top-left selector at
   <https://horizon.wikimedia.org/>.
2. **An SSH key** registered in Horizon → Compute → Key Pairs.
3. **A target hostname** in mind (e.g. `wikiwho-rs.wmcloud.org`).
   Anything in the `.wmcloud.org` zone the Horizon web proxy will
   accept works.

---

## Step 1 — Launch the VPS

Horizon → Compute → Instances → **Launch Instance**:

| Field | Value |
|---|---|
| Instance Name | `wikiwho-rs-1` |
| Source | Image: latest **Debian** (12 or 13) |
| Flavor | `g3.cores4.ram8.disk20` (or similar). Bigger disk = more headroom for cache-miss growth. |
| Networks | `lan-flat-cloudinstances2b` |
| Security Groups | `default`, `web` |
| Key Pair | Your registered key |

Open the **Configuration** tab and paste the contents of
[`deployment/wmcloud_setup.sh`](deployment/wmcloud_setup.sh)
into **Customization Script** (a.k.a. User Data). Cloud-init runs
it once as root on first boot.

Click **Launch Instance**.

> **To customize without editing the script,** prepend env-var
> exports at the top of the user-data block:
>
> ```bash
> #!/usr/bin/env bash
> export INGEST_LANGS=simple,zh   # follow simple + Chinese
> export REPO_REF=some-branch     # build a non-main branch
> # ...rest of wmcloud_setup.sh below
> ```

## Step 2 — Watch first boot finish

Once the instance shows **Active** in Horizon, grab its floating IP
and SSH in:

```bash
ssh debian@<floating-ip>
sudo tail -f /var/log/wikiwho-rs-setup.log
```

Wait for the line `=== Setup complete @ ... ===`. Below it you
should see a healthz JSON line:

```json
{"status":"ok","version":"0.0.0","storage_root":"/var/lib/wikiwho-rs/storage"}
```

Verify both services are running:

```bash
sudo systemctl status wikiwho-rs-server wikiwho-rs-ingest
```

Both should report `active (running)`.

## Step 3 — Add the Horizon web proxy

Horizon → Network → **Web Proxies** → **Add Web Proxy**:

| Field | Value |
|---|---|
| Hostname | e.g. `wikiwho-rs` (becomes `wikiwho-rs.wmcloud.org`) |
| Backend instance | `wikiwho-rs-1` |
| Backend port | `80` |

Wait ~30 seconds for DNS to propagate, then verify from your
laptop:

```bash
curl -s https://wikiwho-rs.wmcloud.org/healthz | jq .
```

You should see the same healthz JSON.

## Step 4 — Smoke-test the cache-miss path

Cold-build a small simplewiki article. The **first** request
returns `408` with the "still processing" envelope (per
[API.md §1](API.md)); the **second** request, after the
background task finishes, returns real data:

```bash
URL="https://wikiwho-rs.wmcloud.org/simple/api/v1.0.0-beta/rev_content/Photosynthesis/"

curl -i "$URL"                         # expect 408 + retry envelope
sleep 15                               # let the background task finish
curl -s "$URL" | jq '.revisions | length'
```

In another shell, tail the server log to watch the cache-miss in
real time:

```bash
ssh debian@<floating-ip> 'sudo journalctl -u wikiwho-rs-server -f'
```

You'll see TraceLayer's per-request lines plus a structured
`cache_miss` info log when the background task completes.

## Step 5 — Watch ingest activity

```bash
ssh debian@<floating-ip> 'sudo journalctl -u wikiwho-rs-ingest -f'
```

Log lines per simplewiki edit:

- `applied` — a request-built article got a new revision applied.
- `snapshot not on disk, skipping` (debug) — an edit for an article
  nobody has requested yet; lazy populate does not proactively pull
  it.
- `snapshot already at-or-ahead of event` — SSE replay; idempotent.
- `apply failed` — investigate; check the error field.

---

## Operations

### Reset storage if disk fills

The starter flavor has ~15 GB usable after OS + Rust toolchain.
Lazy-populate against cold large articles can fill that quickly.
Safe to wipe — it's a cache:

```bash
sudo systemctl stop wikiwho-rs-ingest wikiwho-rs-server
sudo rm -rf /var/lib/wikiwho-rs/storage/*
sudo systemctl start wikiwho-rs-server wikiwho-rs-ingest
```

### Add more languages to ingest

```bash
sudo systemctl edit wikiwho-rs-ingest
# In the editor:
[Service]
Environment=WIKIWHO_INGEST_LANGS=simple,zh,eu

sudo systemctl restart wikiwho-rs-ingest
```

The server doesn't need a per-language allowlist — it serves any
language the storage tree contains and triggers cache-miss for any
it doesn't.

### Re-deploy after a code change

```bash
ssh debian@<floating-ip>
cd /home/wikiwho/wikiwho_rust
sudo -u wikiwho git pull --ff-only
sudo -u wikiwho /home/wikiwho/.cargo/bin/cargo build --release --bin wikiwho-server --bin ingest
sudo install -m 0755 target/release/wikiwho-server /usr/local/bin/wikiwho-server
sudo install -m 0755 target/release/ingest        /usr/local/bin/ingest
sudo systemctl restart wikiwho-rs-server wikiwho-rs-ingest
curl -s http://127.0.0.1/healthz | jq .version
```

If the storage format bumped (rare; see `SCHEMA_VERSION` in
`crates/wikiwho-storage`), reset storage first.

### Check disk and memory

```bash
df -h /var/lib/wikiwho-rs
systemctl status wikiwho-rs-server   # Memory: line
systemctl status wikiwho-rs-ingest
```

---

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Setup script hangs on `cargo build` | First boot — building all deps from source on a 4-core VPS takes 5–10 minutes. | Wait. `tail -f /var/log/wikiwho-rs-setup.log` shows progress. |
| `systemctl status` shows the unit failed | Build failed, or binary path wrong. | `sudo journalctl -u wikiwho-rs-server -n 50` for the error. |
| `curl /healthz` returns nginx 502 | Server bound to wrong address, or not running. | Confirm `WIKIWHO_BIND=127.0.0.1:8088` in the unit; restart. |
| First cache-miss request hangs | Cold large article (e.g. Obama-class) — fetching tens of thousands of revisions from MW takes minutes. | Test with a small article (e.g. Photosynthesis on simplewiki) first. |
| Disk full | Cache-miss filled `/var/lib/wikiwho-rs/storage`. | Reset storage (see Operations). |
| EventStreams log shows reconnects every few seconds | Network instability or a stream filter mismatch. | Inspect `WIKIWHO_INGEST_LANGS`; if narrow, expect quiet stretches. Persistent reconnects warrant a closer look at the journal. |

---

## What this deploy does NOT verify

- **Wire-format byte parity vs the live legacy service.** The
  algorithm's parity vs the Python reference is already covered by
  the corpus under `parity-fixtures/`; the open question is
  *operational* — does the live service stay correct under real
  request mixes and race conditions? Validating that needs shadow
  traffic, which is a separate piece of work.
- **Performance at enwiki scale.** Lazy-populate on simplewiki is a
  smoke test, not a stress test.
- **Sustained ingest behavior.** Hours-to-days of EventStreams
  uptime will surface issues a short test won't.
- **Big-article cache-miss latency.** Don't trigger an
  Obama-class cold build by accident; it will fill the disk and
  take many minutes.

---

## Related docs

- [README.md](README.md) — what this project is.
- [PLAN.md](PLAN.md) — overall plan, scope, migration phases.
- [API.md](API.md) — wire format (the public contract).
- [STORAGE.md](STORAGE.md) — on-disk format.
- [ALGORITHM.md](ALGORITHM.md) — the attribution algorithm + parity constraints.
- [notes/cutover/01-wmcloud-deploy.md](notes/cutover/01-wmcloud-deploy.md) — fuller session-style runbook with rationale + open questions.
