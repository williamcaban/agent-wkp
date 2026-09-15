#!/bin/bash
# Issue #159's own required acceptance criterion: "A real test proving
# the indexing process cannot reach the network (not just a code-review
# claim)". A tenant's pod now runs two containers (tenant_pod.rs's own
# module doc): the serving one (unchanged, reachable over the shared
# network-alias address) and the indexing one (`wkp-hub index-worker`,
# `--network none` + `--security-opt seccomp=...`, never reachable and
# never able to reach anything itself).
#
# This proves both halves of that split for real, against real podman:
# 1. The indexing container cannot even create a socket -- not "can't
#    reach a specific host", genuinely cannot open one at all, which is
#    what the seccomp profile in deploy/hub/seccomp-no-network.json
#    denies. Checked from *inside* the running container via `podman
#    exec`, not by asking podman about its configuration.
# 2. A real push still gets indexed: the serving container's HEAD moves,
#    the indexing container's own `wkp-hub index-worker` loop (running
#    inside its no-network container the whole time) notices and
#    produces index.db, proving the network restriction doesn't
#    silently break the feature it's supposed to protect, not just that
#    isolation exists in isolation from anything actually working.
#    Checked on the *host* path (ADR-0014/issue #165: both containers
#    bind-mount the tenant's whole storage directory, not just the bare
#    repo, so index.db is host-persisted and visible from either
#    container, not only whichever one most recently wrote it) and,
#    for good measure, from the *serving* container too -- the actual
#    point of #165's fix, not just that the indexing container itself
#    can see its own output.
#
# Wired into CI as part of the `hub-pod-isolation` job
# (`.github/workflows/rust-ci.yml`), alongside its siblings
# test-pod-lifecycle.sh and test-cross-tenant-isolation.sh.
#
# Expects: `wkp-hub` built and on PATH (or WKP_HUB_BIN pointing at it),
# `podman`, `git`, `psql` on PATH; DATABASE_URL pointing at a reachable
# Postgres; the `wkp-hub` container image already built and tagged
# (default localhost/wkp-hub, override via WKP_HUB_IMAGE).
set -euo pipefail

WKP_HUB_BIN="${WKP_HUB_BIN:-wkp-hub}"
IMAGE="${WKP_HUB_IMAGE:-localhost/wkp-hub}"
TENANT="indexing-isolation-test"
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

log "creating tenant '$TENANT' and starting its (now two-container) pod"
"$WKP_HUB_BIN" migrate >/dev/null
"$WKP_HUB_BIN" tenant create "$TENANT"
"$WKP_HUB_BIN" start-pod "$TENANT"

SERVE_CONTAINER="wkp-tenant-${TENANT}-serve"
INDEX_CONTAINER="wkp-tenant-${TENANT}-index"

log "positive control: the serving container can still reach the network"
if ! podman exec "$SERVE_CONTAINER" sh -c \
    "echo | timeout 2 sh -c 'exec 3<>/dev/tcp/127.0.0.1/8080' 2>/dev/null"; then
    echo "FAIL: the serving container itself cannot open a socket -- something is wrong with the \
test setup, not with isolation" >&2
    exit 1
fi
log "PASS: serving container can open a socket (as expected)"

log "isolation check: the indexing container cannot create a socket at all"
if podman exec "$INDEX_CONTAINER" sh -c \
    "exec 3<>/dev/tcp/127.0.0.1/8080" 2>"$WORKDIR/index-net.err"; then
    echo "FAIL: the indexing container was able to open a socket -- network isolation is broken" >&2
    cat "$WORKDIR/index-net.err" >&2
    exit 1
fi
if ! grep -qi "permitted\|permission\|forbidden" "$WORKDIR/index-net.err"; then
    echo "FAIL: the indexing container's socket attempt failed, but not with the expected \
seccomp/permission error -- got instead:" >&2
    cat "$WORKDIR/index-net.err" >&2
    exit 1
fi
log "PASS: indexing container cannot open a socket (seccomp denied it)"

log "isolation check: the indexing container has no reachable network interface either"
IFACES="$(podman exec "$INDEX_CONTAINER" cat /proc/net/dev | tail -n +3 | awk -F: '{print $1}' | tr -d ' ')"
if [ "$IFACES" != "lo" ]; then
    echo "FAIL: expected only a loopback interface inside the indexing container, got: $IFACES" >&2
    exit 1
fi
log "PASS: indexing container has only loopback (--network none took effect)"

log "confirming the feature the isolation doesn't break: a real push still gets indexed"
CLIENT_DIR="$WORKDIR/client"
git init --quiet -b main "$CLIENT_DIR"
cat > "$CLIENT_DIR/shared.md" <<'EOF'
---
visibility: shared
title: indexing isolation test item
---

pushed by deploy/hub/test-tenant-indexing-isolation.sh
EOF
git -C "$CLIENT_DIR" add -A
git -C "$CLIENT_DIR" -c user.email=test@example.com -c user.name=test \
    commit -q -m "indexing isolation test"
podman run --rm --network wkp-hub-tenants \
    -v "$CLIENT_DIR:/repo:Z" \
    -w /repo \
    --entrypoint git \
    "$IMAGE" push --quiet "http://${TENANT}:8080/${TENANT}.git" main \
    || {
        echo "FAIL: push to http://${TENANT}:8080/ failed" >&2
        exit 1
    }

log "waiting for the (network-isolated) index-worker to notice and re-index"
# Checked on the host path (ADR-0014/issue #165): repos_root/<slug> is
# the tenant's whole storage directory, bind-mounted as a unit into
# both containers, so index.db (a sibling of repo.git inside it) is
# host-persisted now, not stuck inside whichever container wrote it.
INDEX_PATH="$WKP_HUB_REPOS_ROOT/${TENANT}/index.db"
DEADLINE=$((SECONDS + 15))
while [ ! -e "$INDEX_PATH" ]; do
    if [ "$SECONDS" -ge "$DEADLINE" ]; then
        echo "FAIL: index-worker did not produce $INDEX_PATH within 15s of the push" >&2
        exit 1
    fi
    sleep 0.5
done
log "PASS: the network-isolated indexing container still indexed the push (host-persisted)"

log "confirming the serving container sees the same index.db too (the actual point of #165's fix)"
if ! podman exec "$SERVE_CONTAINER" test -e "/srv/wkp-hub/repos/${TENANT}/index.db"; then
    echo "FAIL: the serving container cannot see index.db -- the two containers aren't sharing the same mount" >&2
    exit 1
fi
log "PASS: both containers see the same index.db"

log "ALL CHECKS PASSED"
