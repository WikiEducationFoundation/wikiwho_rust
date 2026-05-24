#!/usr/bin/env bash
# wmcloud_setup.sh — first-boot setup for a wikiwho-rust VPS on
# Wikimedia Cloud (Horizon "User Data" / cloud-init runs this once).
#
# Goal: produce a fully operational `wikiwho-rs-server` + `wikiwho-rs-ingest`
# stack with **just** the Horizon web-proxy entry left as a manual step.
#
# What this script does (idempotent — safe to re-run):
#   1. apt update + install build tooling, nginx, git, ca-certs
#   2. Create the `wikiwho` user with $HOME at /home/wikiwho
#   3. Install rustup (stable toolchain) into wikiwho's HOME
#   4. Clone the repo (or pull) at /home/wikiwho/wikiwho_rust
#   5. Build the release binaries (server + ingest)
#   6. Install binaries to /usr/local/bin
#   7. Create /var/lib/wikiwho-rs/storage owned by wikiwho
#   8. Install systemd units + nginx site config
#   9. Enable + start the services
#  10. Print a summary + the URL to add to Horizon's web proxy
#
# Override variables by exporting them before invoking (Horizon's
# cloud-init lets you prefix env vars in the user-data block):
#   REPO_URL=https://github.com/WikiEducationFoundation/wikiwho_rust.git
#   REPO_REF=main
#   INGEST_LANGS=simple
#
# Logs to /var/log/wikiwho-rs-setup.log so cloud-init's transcript
# stays small.

set -euo pipefail

# --- Configurable (env-overrideable) ---------------------------------

REPO_URL="${REPO_URL:-https://github.com/WikiEducationFoundation/wikiwho_rust.git}"
REPO_REF="${REPO_REF:-main}"
INGEST_LANGS="${INGEST_LANGS:-simple}"
WIKIWHO_USER="${WIKIWHO_USER:-wikiwho}"
WIKIWHO_HOME="${WIKIWHO_HOME:-/home/${WIKIWHO_USER}}"
REPO_DIR="${REPO_DIR:-${WIKIWHO_HOME}/wikiwho_rust}"
STORAGE_DIR="${STORAGE_DIR:-/var/lib/wikiwho-rs/storage}"
LOG_FILE="${LOG_FILE:-/var/log/wikiwho-rs-setup.log}"

# --- Logging ---------------------------------------------------------

exec > >(tee -a "${LOG_FILE}") 2>&1
echo "=== wmcloud_setup.sh @ $(date -u +%FT%TZ) ==="
echo "REPO_URL=${REPO_URL}"
echo "REPO_REF=${REPO_REF}"
echo "INGEST_LANGS=${INGEST_LANGS}"

# --- Require root ----------------------------------------------------

if [[ "${EUID}" -ne 0 ]]; then
    echo "must be run as root (cloud-init usually runs it as root)" >&2
    exit 1
fi

# --- 1. apt --------------------------------------------------------

export DEBIAN_FRONTEND=noninteractive
apt-get update -y
apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    curl \
    git \
    nginx \
    pkg-config

# --- 2. wikiwho user ------------------------------------------------

if ! id -u "${WIKIWHO_USER}" >/dev/null 2>&1; then
    useradd --create-home --shell /bin/bash "${WIKIWHO_USER}"
fi

# --- 3. rustup (in wikiwho's HOME) ----------------------------------

if [[ ! -x "${WIKIWHO_HOME}/.cargo/bin/cargo" ]]; then
    runuser -u "${WIKIWHO_USER}" -- bash -c "
        set -eu
        cd '${WIKIWHO_HOME}'
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y --default-toolchain stable --profile minimal
    "
fi
RUSTC="${WIKIWHO_HOME}/.cargo/bin/rustc"
CARGO="${WIKIWHO_HOME}/.cargo/bin/cargo"

# --- 4. repo clone / pull -------------------------------------------

if [[ ! -d "${REPO_DIR}/.git" ]]; then
    runuser -u "${WIKIWHO_USER}" -- git clone --depth=1 --branch "${REPO_REF}" \
        "${REPO_URL}" "${REPO_DIR}"
else
    runuser -u "${WIKIWHO_USER}" -- git -C "${REPO_DIR}" fetch origin "${REPO_REF}"
    runuser -u "${WIKIWHO_USER}" -- git -C "${REPO_DIR}" reset --hard "origin/${REPO_REF}"
fi

# --- 5. build (release) ---------------------------------------------

echo "--- building release binaries (this can take a few minutes on a small VPS) ---"
runuser -u "${WIKIWHO_USER}" -- bash -c "
    set -eu
    cd '${REPO_DIR}'
    '${CARGO}' build --release --bin wikiwho-server --bin ingest
"

# --- 6. install binaries --------------------------------------------

install -m 0755 "${REPO_DIR}/target/release/wikiwho-server" /usr/local/bin/wikiwho-server
install -m 0755 "${REPO_DIR}/target/release/ingest"        /usr/local/bin/ingest

# Self-updating redeploy helper. Symlinked so the in-repo script is
# what gets executed — the next `sudo wikiwho-redeploy` picks up
# whatever shipped in main.
ln -sf "${REPO_DIR}/deployment/redeploy.sh" /usr/local/bin/wikiwho-redeploy

# --- 7. storage dir -------------------------------------------------

install -d -o "${WIKIWHO_USER}" -g "${WIKIWHO_USER}" -m 0750 /var/lib/wikiwho-rs
install -d -o "${WIKIWHO_USER}" -g "${WIKIWHO_USER}" -m 0750 "${STORAGE_DIR}"

# --- 8. systemd units + nginx site ----------------------------------

install -m 0644 "${REPO_DIR}/deployment/wikiwho-rs-server.service" /etc/systemd/system/wikiwho-rs-server.service
install -m 0644 "${REPO_DIR}/deployment/wikiwho-rs-ingest.service" /etc/systemd/system/wikiwho-rs-ingest.service

# Substitute INGEST_LANGS in the ingest unit if the user overrode it.
if [[ "${INGEST_LANGS}" != "simple" ]]; then
    sed -i "s|^Environment=WIKIWHO_INGEST_LANGS=.*|Environment=WIKIWHO_INGEST_LANGS=${INGEST_LANGS}|" \
        /etc/systemd/system/wikiwho-rs-ingest.service
fi

install -m 0644 "${REPO_DIR}/deployment/nginx-site.conf" /etc/nginx/sites-available/wikiwho-rs
ln -sf /etc/nginx/sites-available/wikiwho-rs /etc/nginx/sites-enabled/wikiwho-rs
# Remove Debian's default site so our default_server takes effect.
rm -f /etc/nginx/sites-enabled/default

# --- 9. enable + start ----------------------------------------------

systemctl daemon-reload
systemctl enable --now wikiwho-rs-server.service
systemctl enable --now wikiwho-rs-ingest.service

nginx -t
systemctl reload nginx

# --- 10. summary ----------------------------------------------------

echo
echo "=== Setup complete @ $(date -u +%FT%TZ) ==="
echo
echo "Services:"
systemctl --no-pager --lines=0 status wikiwho-rs-server.service || true
systemctl --no-pager --lines=0 status wikiwho-rs-ingest.service || true
echo
echo "Local healthz probe:"
curl -sS --max-time 5 http://127.0.0.1/healthz || echo "  (curl failed)"
echo
echo "Final manual step:"
echo "  In Horizon, add a Web Proxy entry pointing your chosen public"
echo "  hostname (e.g. wikiwho-rs.wmcloud.org) at this VPS on port 80."
echo "  Once routed, the public URL serves the WikiWho API."
echo
echo "Tail logs:"
echo "  sudo journalctl -u wikiwho-rs-server -f"
echo "  sudo journalctl -u wikiwho-rs-ingest -f"
