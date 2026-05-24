#!/usr/bin/env bash
# redeploy.sh — single-command redeploy of the latest code on a
# wikiwho-rust VPS. Run on the VPS as root (sudo).
#
# Pulls main (or a named ref), rebuilds the release binaries, swaps
# them into /usr/local/bin, restarts the systemd units, and verifies
# /healthz responds with the new version. Idempotent.
#
# Usage:
#   sudo wikiwho-redeploy                 # pull main, rebuild, restart
#   sudo wikiwho-redeploy --ref some-sha  # check out a specific commit
#   sudo wikiwho-redeploy --wipe-storage  # also rm -rf the storage tree
#                                         # (use after a SCHEMA_VERSION bump)
#
# The script self-installs a symlink at /usr/local/bin/wikiwho-redeploy
# on first run, so future invocations need only `sudo wikiwho-redeploy`.

set -euo pipefail

REPO_DIR="${REPO_DIR:-/home/wikiwho/wikiwho_rust}"
WIKIWHO_USER="${WIKIWHO_USER:-wikiwho}"
WIKIWHO_HOME="${WIKIWHO_HOME:-/home/${WIKIWHO_USER}}"
CARGO="${WIKIWHO_HOME}/.cargo/bin/cargo"
STORAGE_DIR="${STORAGE_DIR:-/var/lib/wikiwho-rs/storage}"

WIPE_STORAGE=0
REF=""

usage() {
    cat <<EOF
Usage: sudo wikiwho-redeploy [--wipe-storage] [--ref <branch-or-sha>]

Options:
  --wipe-storage     Stop services, remove everything under
                     ${STORAGE_DIR}, then start services. Use after
                     a storage SCHEMA_VERSION bump.
  --ref <ref>        Check out a specific git ref before building
                     (default: hard reset to origin/main).
  -h, --help         Show this help.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --wipe-storage) WIPE_STORAGE=1; shift ;;
        --ref) REF="${2:?--ref needs a value}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown argument: $1" >&2; usage; exit 2 ;;
    esac
done

if [[ "${EUID}" -ne 0 ]]; then
    echo "must be run as root (use sudo)" >&2
    exit 1
fi

echo "=== wikiwho-redeploy @ $(date -u +%FT%TZ) ==="
echo "REPO_DIR=${REPO_DIR}"
[[ -n "${REF}" ]] && echo "REF=${REF}"
[[ "${WIPE_STORAGE}" = "1" ]] && echo "WIPE_STORAGE=1"

# --- Self-install symlink so future runs are `sudo wikiwho-redeploy` ---
TARGET_LINK=/usr/local/bin/wikiwho-redeploy
DESIRED_TARGET="${REPO_DIR}/deployment/redeploy.sh"
if [[ "$(readlink -f "${TARGET_LINK}" 2>/dev/null || true)" != "${DESIRED_TARGET}" ]]; then
    ln -sf "${DESIRED_TARGET}" "${TARGET_LINK}"
    echo "installed symlink: ${TARGET_LINK} -> ${DESIRED_TARGET}"
fi

# --- Pull ---
echo "--- pulling latest code ---"
sudo -u "${WIKIWHO_USER}" git -C "${REPO_DIR}" fetch --tags origin
if [[ -n "${REF}" ]]; then
    sudo -u "${WIKIWHO_USER}" git -C "${REPO_DIR}" checkout "${REF}"
    # Fast-forward if REF is a branch; harmless on a detached HEAD ref.
    sudo -u "${WIKIWHO_USER}" git -C "${REPO_DIR}" pull --ff-only || true
else
    sudo -u "${WIKIWHO_USER}" git -C "${REPO_DIR}" checkout main
    sudo -u "${WIKIWHO_USER}" git -C "${REPO_DIR}" reset --hard origin/main
fi
HEAD_SHA=$(sudo -u "${WIKIWHO_USER}" git -C "${REPO_DIR}" rev-parse --short HEAD)
HEAD_SUBJECT=$(sudo -u "${WIKIWHO_USER}" git -C "${REPO_DIR}" log -1 --format='%s')
echo "HEAD now at ${HEAD_SHA}: ${HEAD_SUBJECT}"

# --- Build ---
echo "--- building release binaries (release, may take a few minutes) ---"
sudo -u "${WIKIWHO_USER}" "${CARGO}" build --release \
    --manifest-path "${REPO_DIR}/Cargo.toml" \
    --bin wikiwho-server --bin ingest

# --- Install (overwrites running-process executable; safe on Linux) ---
echo "--- installing binaries ---"
install -m 0755 "${REPO_DIR}/target/release/wikiwho-server" /usr/local/bin/wikiwho-server
install -m 0755 "${REPO_DIR}/target/release/ingest"        /usr/local/bin/ingest

# Pick up any systemd-unit changes that may have shipped in the new code.
install -m 0644 "${REPO_DIR}/deployment/wikiwho-rs-server.service" /etc/systemd/system/wikiwho-rs-server.service
install -m 0644 "${REPO_DIR}/deployment/wikiwho-rs-ingest.service" /etc/systemd/system/wikiwho-rs-ingest.service
systemctl daemon-reload

# --- Restart (optionally wipe storage first) ---
if [[ "${WIPE_STORAGE}" = "1" ]]; then
    echo "--- wiping storage at ${STORAGE_DIR} ---"
    systemctl stop wikiwho-rs-ingest wikiwho-rs-server
    rm -rf "${STORAGE_DIR:?}"/*
    systemctl start wikiwho-rs-server wikiwho-rs-ingest
else
    echo "--- restarting services ---"
    systemctl restart wikiwho-rs-server wikiwho-rs-ingest
fi

# --- Verify ---
sleep 1
echo "--- healthz ---"
if ! curl -fsS --max-time 5 http://127.0.0.1/healthz; then
    echo
    echo "healthz failed; check 'sudo journalctl -u wikiwho-rs-server -n 50'" >&2
    exit 3
fi
echo

echo "--- service status ---"
systemctl --no-pager --lines=0 status wikiwho-rs-server || true
systemctl --no-pager --lines=0 status wikiwho-rs-ingest || true

echo
echo "=== redeploy complete @ $(date -u +%FT%TZ) — running ${HEAD_SHA} ==="
