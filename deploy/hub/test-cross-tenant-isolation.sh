#!/bin/bash
# M5-7's own required cross-tenant isolation test (issue #110's
# acceptance criterion: "a test demonstrating actual cross-tenant
# isolation at the pod/namespace level -- not just the application-level
# path restriction M5-4 already has"). `serve_single_tenant`'s own unit
# test (`serve_single_tenant_refuses_a_request_for_a_different_tenant`)
# already proves the *application* refuses to answer for the wrong
# tenant; this script proves the *filesystem* boundary underneath that
# check is real too -- a tenant's own pod never has any other tenant's
# repo bind-mounted into it at all, so there is nothing there for a bug
# in that application-level check to accidentally reach, even in
# principle.
#
# Two tenants, two real pods (`tenant_pod::start_pod`'s own
# `repo_mount_arg`, ADR-0009/0010): from inside tenant A's own pod
# container, the other tenant's `.git` directory must not exist at all
# (not "exists but 403s" -- genuinely absent, `ENOENT`). Checked both
# directions. A positive control (each pod's own tenant repo *does*
# exist) rules out a vacuously-true negative check from, say, a broken
# mount making everything look absent.
#
# Same host-run pattern `test-pod-lifecycle.sh` already established
# (this script's own sibling): `wkp-hub` runs directly here, not inside
# a container, since this test only needs real podman/network access,
# not the HTTPS front door itself.
#
# Expects: `wkp-hub` built and on PATH (or WKP_HUB_BIN pointing at it),
# `podman`, `git`, `psql` on PATH; DATABASE_URL pointing at a reachable
# Postgres; the `wkp-hub` container image already built and tagged
# (default localhost/wkp-hub, override via WKP_HUB_IMAGE).
set -euo pipefail

WKP_HUB_BIN="${WKP_HUB_BIN:-wkp-hub}"
IMAGE="${WKP_HUB_IMAGE:-localhost/wkp-hub}"
TENANT_A="iso-test-a"
TENANT_B="iso-test-b"
: "${DATABASE_URL:?DATABASE_URL must be set}"

WORKDIR="$(mktemp -d)"
export WKP_HUB_REPOS_ROOT="$WORKDIR/repos"
export WKP_HUB_TENANT_IMAGE="$IMAGE"
mkdir -p "$WKP_HUB_REPOS_ROOT"

cleanup() {
    "$WKP_HUB_BIN" stop-pod "$TENANT_A" >/dev/null 2>&1 || true
    "$WKP_HUB_BIN" stop-pod "$TENANT_B" >/dev/null 2>&1 || true
    podman network rm "wkp-hub-tenants" >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

log() { printf '==> %s\n' "$*"; }

log "creating tenants '$TENANT_A' and '$TENANT_B'"
"$WKP_HUB_BIN" migrate >/dev/null
"$WKP_HUB_BIN" tenant create "$TENANT_A"
"$WKP_HUB_BIN" tenant create "$TENANT_B"

log "starting both tenants' pods"
"$WKP_HUB_BIN" start-pod "$TENANT_A"
"$WKP_HUB_BIN" start-pod "$TENANT_B"

CONTAINER_A="wkp-tenant-${TENANT_A}-serve"
CONTAINER_B="wkp-tenant-${TENANT_B}-serve"

log "positive control: each pod can see its own tenant's repo"
if ! podman exec "$CONTAINER_A" test -d "/srv/wkp-hub/repos/${TENANT_A}.git"; then
    echo "FAIL: tenant A's own pod cannot see its own repo -- something is wrong with the mount itself, not isolation" >&2
    exit 1
fi
if ! podman exec "$CONTAINER_B" test -d "/srv/wkp-hub/repos/${TENANT_B}.git"; then
    echo "FAIL: tenant B's own pod cannot see its own repo -- something is wrong with the mount itself, not isolation" >&2
    exit 1
fi
log "PASS: both pods see their own repo"

log "isolation check: tenant A's pod must not see tenant B's repo at all"
if podman exec "$CONTAINER_A" test -e "/srv/wkp-hub/repos/${TENANT_B}.git"; then
    echo "FAIL: tenant A's pod can see tenant B's repo path -- cross-tenant filesystem isolation is broken" >&2
    exit 1
fi
log "PASS: tenant B's repo does not exist inside tenant A's pod"

log "isolation check: tenant B's pod must not see tenant A's repo at all"
if podman exec "$CONTAINER_B" test -e "/srv/wkp-hub/repos/${TENANT_A}.git"; then
    echo "FAIL: tenant B's pod can see tenant A's repo path -- cross-tenant filesystem isolation is broken" >&2
    exit 1
fi
log "PASS: tenant A's repo does not exist inside tenant B's pod"

log "isolation check: tenant A's pod's own serve-tenant mode refuses tenant B's git path over the network"
# Real container-level confirmation of what
# `serve_single_tenant_refuses_a_different_tenant`'s own unit test
# already proves at the Rust-function level -- each pod's
# `serve-tenant --tenant <slug>` only ever answers for its own fixed
# slug, so asking tenant A's pod for tenant B's repo over its own
# exposed port must fail, not just 404 at the front door layer.
if podman run --rm --network wkp-hub-tenants \
    --entrypoint git \
    "$IMAGE" ls-remote "http://${TENANT_A}:8080/${TENANT_B}.git" \
    >/dev/null 2>"$WORKDIR/cross-fetch.err"; then
    echo "FAIL: tenant A's pod answered a request for tenant B's own repo path" >&2
    cat "$WORKDIR/cross-fetch.err" >&2
    exit 1
fi
log "PASS: tenant A's pod refuses to serve tenant B's repo path"

log "ALL CHECKS PASSED"
