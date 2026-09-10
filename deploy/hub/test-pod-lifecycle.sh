#!/bin/bash
# M5-7's own pod-provisioning/lifecycle test (ADR-0009, ADR-0010):
# starts a real tenant pod, proves it's reachable over the shared
# user-defined network by its container-runtime DNS alias (not just
# from another container in the *same* pod, which sidesteps the actual
# mechanism ADR-0010 picked), stops it, then proves the reaper stops an
# idle one on its own.
#
# Not wired into CI yet (a separate, later PR does that, alongside
# front-door networking) -- this script exists so the pod-lifecycle
# code has a real, repeatable end-to-end check independent of unit
# tests, the same split M5-5's own test-ssh-integration.sh established.
#
# Expects: `wkp-hub` built and on PATH (or WKP_HUB_BIN pointing at it),
# `podman`, `git`, `psql` on PATH; DATABASE_URL pointing at a reachable
# Postgres; the `wkp-hub` container image already built and tagged
# (default localhost/wkp-hub, override via WKP_HUB_IMAGE).
#
# **Known gap in a rootless sandbox with no systemd user session/D-Bus**
# (this repo's own dev sandbox, confirmed by hand while building this):
# `aardvark-dns` (the DNS backend for podman's user-defined networks)
# fails to start there, so the network-alias steps below reliably fail
# in that specific environment. A real CI runner or production host is
# expected not to have this constraint -- this script targets that
# environment, not a workaround for the sandbox one.
set -euo pipefail

WKP_HUB_BIN="${WKP_HUB_BIN:-wkp-hub}"
IMAGE="${WKP_HUB_IMAGE:-localhost/wkp-hub}"
TENANT="pod-lifecycle-test"
: "${DATABASE_URL:?DATABASE_URL must be set}"

WORKDIR="$(mktemp -d)"
export WKP_HUB_REPOS_ROOT="$WORKDIR/repos"
export WKP_HUB_TENANT_IMAGE="$IMAGE"
mkdir -p "$WKP_HUB_REPOS_ROOT"

cleanup() {
    "$WKP_HUB_BIN" stop-pod "$TENANT" >/dev/null 2>&1 || true
    podman network rm "wkp-hub-tenants" >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

log() { printf '==> %s\n' "$*"; }

log "creating tenant '$TENANT'"
"$WKP_HUB_BIN" migrate >/dev/null
"$WKP_HUB_BIN" tenant create "$TENANT"

log "starting the tenant's pod"
"$WKP_HUB_BIN" start-pod "$TENANT"

log "pushing a shared item over the pod's own network-alias address (not just intra-pod localhost)"
CLIENT_DIR="$WORKDIR/client"
git init --quiet -b main "$CLIENT_DIR"
cat > "$CLIENT_DIR/shared.md" <<'EOF'
---
visibility: shared
title: pod lifecycle test item
---

pushed by deploy/hub/test-pod-lifecycle.sh
EOF
git -C "$CLIENT_DIR" add -A
git -C "$CLIENT_DIR" -c user.email=test@example.com -c user.name=test \
    commit -q -m "pod lifecycle test"

# Runs as its own container on the same user-defined network podman
# gave the tenant's pod, so it resolves "$TENANT" via that network's
# own DNS (aardvark-dns) exactly the way the front door will once PR
# 4/4 joins that network too -- not `podman run --pod`, which would
# only prove intra-pod localhost reachability, a weaker claim.
podman run --rm --network wkp-hub-tenants \
    -v "$CLIENT_DIR:/repo:Z" \
    -w /repo \
    --entrypoint git \
    "$IMAGE" push --quiet "http://${TENANT}:8080/${TENANT}.git" main \
    || {
        echo "FAIL: push to http://${TENANT}:8080/ over the shared network failed" >&2
        exit 1
    }

log "PASS: pod reachable and serving git over its network alias"

log "stopping the pod"
"$WKP_HUB_BIN" stop-pod "$TENANT"
if podman pod exists "wkp-tenant-${TENANT}" 2>/dev/null; then
    echo "FAIL: pod still exists after stop-pod" >&2
    exit 1
fi
log "PASS: pod removed"

log "testing the reaper: start again, backdate activity, sweep"
"$WKP_HUB_BIN" start-pod "$TENANT"
psql "$DATABASE_URL" -c \
    "UPDATE tenants SET last_active_at = now() - interval '1 hour' WHERE slug = '${TENANT}'" \
    >/dev/null
"$WKP_HUB_BIN" reap-idle-pods --idle-minutes 30
if podman pod exists "wkp-tenant-${TENANT}" 2>/dev/null; then
    echo "FAIL: reaper did not stop an idle pod" >&2
    exit 1
fi
log "PASS: reaper stopped the idle pod"

log "ALL CHECKS PASSED"
