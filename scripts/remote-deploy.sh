#!/usr/bin/env bash
# remote-deploy.sh — run wikiwho-redeploy on the WMCloud VPS from
# your laptop, via the Wikimedia Cloud bastion. Equivalent to:
#
#   ssh -J <wikitech>@bastion.wmcloud.org debian@<vps> sudo wikiwho-redeploy
#
# Usage:
#   ./scripts/remote-deploy.sh                       # plain redeploy
#   ./scripts/remote-deploy.sh --wipe-storage        # wipe storage too
#   ./scripts/remote-deploy.sh --ref some-sha        # deploy a specific ref
#
# Override via env vars:
#   WIKITECH_USER  — your Wikitech shell username (default: $USER)
#   VPS_HOST       — full FQDN of the VPS
#   BASTION        — bastion host (default: bastion.wmcloud.org)
#   SSH_USER       — login user on the VPS (default: debian)

set -euo pipefail

WIKITECH_USER="${WIKITECH_USER:-${USER}}"
BASTION="${BASTION:-bastion.wmcloud.org}"
SSH_USER="${SSH_USER:-debian}"
VPS_HOST="${VPS_HOST:-wikiwho-rust.globaleducation.eqiad1.wikimedia.cloud}"

# Forward all args verbatim to wikiwho-redeploy on the remote side.
REMOTE_CMD="sudo wikiwho-redeploy $*"

echo "=== remote-deploy @ $(date -u +%FT%TZ) ==="
echo "via ${WIKITECH_USER}@${BASTION} → ${SSH_USER}@${VPS_HOST}"
echo "remote command: ${REMOTE_CMD}"
echo

# -t forces a TTY so sudo's password prompt (if any) is interactive.
# -o BatchMode=no leaves room for SSH key passphrase prompts.
exec ssh -t \
    -J "${WIKITECH_USER}@${BASTION}" \
    "${SSH_USER}@${VPS_HOST}" \
    "${REMOTE_CMD}"
