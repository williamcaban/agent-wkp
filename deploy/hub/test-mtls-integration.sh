#!/bin/bash
# M5-13's own required integration test (docs/plan/milestones.md's M5
# exit criterion, ADR-0011): register a device over the real RFC 8628
# CSR flow (M5-9), push and fetch over HTTPS with mutual TLS (M5-10),
# revoke it via the control plane, attempt another push, confirm it
# fails -- specifically because the TLS handshake itself now rejects
# the certificate, not a later application-level error. Against the
# real, built `wkp-hub` image (deploy/hub/Containerfile), over a real
# mTLS connection and a real tenant pod, not a simulated call. This is
# the mTLS-era replacement for M5-5's own required
# test-ssh-integration.sh, which #125 (ADR-0011, gated on this test
# being green) will retire once this one is proven.
#
# Expects: `podman`, `git`, `curl`, `psql` on PATH; the `wkp-hub` image
# already built and tagged (default localhost/wkp-hub, override via
# WKP_HUB_IMAGE); a `wkp` (the CLI, not `wkp-hub`) binary on PATH or at
# WKP_BIN; DATABASE_URL pointing at a reachable Postgres whose schema
# this script's own `wkp-hub migrate` call will ensure exists (a fresh,
# empty database is fine -- this script only ever creates one tenant,
# with a fixed slug, and does not assume anything else in that database
# is untouched).
#
# Bridge networking with a mapped port for the front door's own client-
# facing port, same reasoning `test-ssh-integration.sh` already
# documents for its own SSH port (a real GitHub Actions runner's own
# services can collide with `--network host`). The front door
# container *also* joins the shared `wkp-hub-tenants` user-defined
# network (ADR-0009/0010) -- unlike M5-5's own container, which never
# needed to reach a tenant pod at all -- so it can resolve and proxy to
# the tenant pod this script starts, by that pod's own DNS alias on
# that network. `DATABASE_URL`'s host needs rewriting to
# `host.containers.internal` for the front door's own process (it talks
# to Postgres; the tenant pod's `serve-tenant` mode never does).
set -euo pipefail

IMAGE="${WKP_HUB_IMAGE:-localhost/wkp-hub}"
WKP_BIN="${WKP_BIN:-wkp}"
CONTAINER_NAME="wkp-hub-mtls-integration-test"
HTTPS_PORT="${WKP_HUB_TEST_HTTPS_PORT:-8443}"
TENANT="mtls-integration-test"
NETWORK_NAME="wkp-hub-tenants"
: "${DATABASE_URL:?DATABASE_URL must be set (e.g. postgres://postgres:wkp_hub_ci@localhost:5432/wkp_hub_test)}"
CONTAINER_DATABASE_URL="$(printf '%s' "$DATABASE_URL" | sed -E 's#@(localhost|127\.0\.0\.1):#@host.containers.internal:#')"

WORKDIR="$(mktemp -d)"
PODMAN_SOCK="$WORKDIR/podman.sock"
service_pid=""
cleanup() {
    podman exec "$CONTAINER_NAME" /usr/local/bin/wkp-hub stop-pod "$TENANT" >/dev/null 2>&1 || true
    podman rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
    [ -n "$service_pid" ] && kill "$service_pid" >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

log() { printf '==> %s\n' "$*"; }

log "ensuring the shared tenant network exists"
podman network exists "$NETWORK_NAME" || podman network create "$NETWORK_NAME" >/dev/null

# ADR-0012: the front door's own `wkp-hub serve` process starts/stops
# tenant pods on demand via the `PodmanOrchestrator` backend, which
# talks to `podman-remote` inside the container -- pointed at *this*
# host-side API socket, not a container-runtime daemon nested inside
# the container itself (ADR-0009's own pod-per-tenant model needs pods
# as the *host's* siblings, reachable the same way whether started by
# this script's own `podman` or by the containerized front door).
log "starting a podman API socket for the front door's own pod orchestration (ADR-0012)"
podman system service --time=0 "unix://${PODMAN_SOCK}" &
service_pid=$!
for _ in $(seq 1 20); do
    [ -S "$PODMAN_SOCK" ] && break
    sleep 0.5
done
[ -S "$PODMAN_SOCK" ] || { echo "FAIL: podman API socket never appeared at $PODMAN_SOCK" >&2; exit 1; }

log "starting wkp-hub container on the shared network"
# `REPOS_ROOT` (ADR-0012, found by hand -- twice; see below): a
# tenant's pod is a *host-level sibling* of this front-door container,
# created by the real host podman through the socket above. That means
# the `-v <repos_root>/<slug>.git:...` argument `tenant_pod.rs`'s
# `start_pod` builds is resolved by the *host's* own podman, against
# the *host's* filesystem -- not against this container's private
# filesystem, even though the front door's own `wkp-hub serve` process
# (which writes the bare repo, via `tenant create`) is running inside
# this container. Mounting a host directory at the container-internal
# path `/srv/wkp-hub/repos` (first attempt, wrong) does not make that
# path exist on the *host* at all -- the host's real directory is
# still wherever `$WORKDIR` actually is, so the host's own podman
# still can't find it ("statfs ...: no such file or directory",
# unchanged even after that first fix). The only way both observers --
# this container's own `wkp-hub serve`, and the host's own podman
# building a sibling pod's mount args -- agree on one path is to give
# it the *same literal path* on both sides: mount `$REPOS_ROOT` at
# that identical absolute path inside the container too, and point
# `WKP_HUB_REPOS_ROOT` (the same override `main.rs`'s own `repos_root`
# already reads) at it instead of leaving the container-only default.
REPOS_ROOT="$WORKDIR/repos"
mkdir -p "$REPOS_ROOT"
# `--security-opt label=disable`: found by hand -- SELinux (Enforcing
# by default on a Fedora host/runner) denies the container's own
# `podman` (really `podman-remote`) permission to even `connect()` the
# bind-mounted host socket otherwise ("permission denied" at the
# socket dial, not an ordinary Unix DAC failure -- confirmed by hand
# that plain file permissions on the socket were never the actual
# blocker, only its SELinux label crossing into this container's own
# confined domain). Standard, narrowly-scoped escape hatch for exactly
# this "share the container-runtime socket into one container" shape;
# does not disable SELinux for anything else on the host. Also means
# this container's own view of `$REPOS_ROOT` needs no relabel flag of
# its own -- it isn't SELinux-confined at all, so it can't conflict
# with whatever private `:Z` label a tenant pod's own later mount of
# the same host directory sets.
podman run -d --name "$CONTAINER_NAME" --network "$NETWORK_NAME" -p "${HTTPS_PORT}:8443" \
    --security-opt label=disable \
    -e DATABASE_URL="$CONTAINER_DATABASE_URL" \
    -e WKP_HUB_TENANT_IMAGE="$IMAGE" \
    -e WKP_HUB_REPOS_ROOT="$REPOS_ROOT" \
    -e CONTAINER_HOST="unix:///run/podman/podman.sock" \
    -v "${PODMAN_SOCK}:/run/podman/podman.sock" \
    -v "${REPOS_ROOT}:${REPOS_ROOT}" \
    "$IMAGE" >/dev/null

# If the front door (or sshd) fails to start at all, the container
# exits almost immediately -- surfacing *why* here beats a much later,
# opaque failure the first `podman exec` would otherwise report instead.
sleep 1
if [ "$(podman inspect --format '{{.State.Running}}' "$CONTAINER_NAME" 2>/dev/null)" != "true" ]; then
    echo "FAIL: wkp-hub container is not running; its own logs:" >&2
    podman logs "$CONTAINER_NAME" >&2 || true
    exit 1
fi

hub_exec() {
    # `CONTAINER_HOST`/`WKP_HUB_REPOS_ROOT` (ADR-0012): needed by
    # `start-pod`/`stop-pod`/`tenant create`
    # (`tenant_pod::orchestrator`) whenever this function calls them --
    # spelled out explicitly here rather than relied on from `podman
    # run`'s own `-e`, matching this function's existing `DATABASE_URL`
    # pattern below.
    podman exec \
        -e DATABASE_URL="$CONTAINER_DATABASE_URL" \
        -e CONTAINER_HOST="unix:///run/podman/podman.sock" \
        -e WKP_HUB_REPOS_ROOT="$REPOS_ROOT" \
        "$CONTAINER_NAME" "$@"
}

log "waiting for the control plane's schema"
migrate_ok=0
for _ in $(seq 1 20); do
    if hub_exec /usr/local/bin/wkp-hub migrate >/dev/null 2>&1; then
        migrate_ok=1
        break
    fi
    sleep 0.5
done
if [ "$migrate_ok" -ne 1 ]; then
    echo "FAIL: could not reach the control plane's schema; container logs:" >&2
    podman logs "$CONTAINER_NAME" >&2 || true
    hub_exec /usr/local/bin/wkp-hub migrate
fi

log "waiting for the HTTPS front door to accept connections"
front_door_ok=0
for _ in $(seq 1 20); do
    if curl -sSk -o /dev/null "https://127.0.0.1:${HTTPS_PORT}/verify"; then
        front_door_ok=1
        break
    fi
    sleep 0.5
done
if [ "$front_door_ok" -ne 1 ]; then
    echo "FAIL: HTTPS front door never came up; container logs:" >&2
    podman logs "$CONTAINER_NAME" >&2 || true
    exit 1
fi

log "creating tenant '$TENANT'"
hub_exec /usr/local/bin/wkp-hub tenant create "$TENANT"

log "fetching the hub's own CA root (operator-side bootstrap, same as a real device would need)"
hub_exec /usr/local/bin/wkp-hub ca-cert > "$WORKDIR/ca-cert.pem"

log "registering a device over the real RFC 8628 + CSR flow (M5-9)"
CLIENT_DIR="$WORKDIR/client"
mkdir -p "$CLIENT_DIR"
"$WKP_BIN" hub register \
    --hub-url "https://127.0.0.1:${HTTPS_PORT}" --tenant "$TENANT" \
    --ca-cert "$WORKDIR/ca-cert.pem" --path "$CLIENT_DIR" > "$WORKDIR/register.log" 2>&1 &
register_pid=$!

user_code=""
for _ in $(seq 1 20); do
    if grep -q "enter code:" "$WORKDIR/register.log" 2>/dev/null; then
        user_code="$(grep "enter code:" "$WORKDIR/register.log" | sed -E 's/.*enter code: //')"
        break
    fi
    sleep 0.5
done
if [ -z "$user_code" ]; then
    echo "FAIL: wkp hub register never printed a user code; its own output:" >&2
    cat "$WORKDIR/register.log" >&2
    exit 1
fi

log "approving the device grant (scripted stand-in for a human clicking approve, same pattern M5-2's own test used)"
curl -sS --cacert "$WORKDIR/ca-cert.pem" -X POST "https://127.0.0.1:${HTTPS_PORT}/verify" \
    -d "user_code=${user_code}" >/dev/null

wait "$register_pid" || {
    echo "FAIL: wkp hub register did not complete successfully; its own output:" >&2
    cat "$WORKDIR/register.log" >&2
    exit 1
}
log "PASS: device registered, holding a hub-signed certificate"

# The control plane's own integer device id, parsed from
# `HubRegisterSummary`'s own "(device id N)" wording -- not
# `.wkp/device-id`, which is `wkp_git::sync`'s own, entirely unrelated
# per-store sync identifier (design 4.2/6.2), a trap worth flagging
# since the filename alone invites confusing the two.
device_id="$(grep -o 'device id [0-9]*' "$WORKDIR/register.log" | grep -o '[0-9]*')"
if [ -z "$device_id" ]; then
    echo "FAIL: could not parse a device id out of wkp hub register's own output:" >&2
    cat "$WORKDIR/register.log" >&2
    exit 1
fi

log "pre-starting the tenant's pod (test-pod-lifecycle.sh's own job for start/stop/reap mechanics -- this just needs one running)"
hub_exec /usr/local/bin/wkp-hub start-pod "$TENANT"

log "pushing a shared item over HTTPS with mTLS -- this must succeed"
cat > "$CLIENT_DIR/shared.md" <<'EOF'
---
visibility: shared
title: mtls integration test item
---

pushed by deploy/hub/test-mtls-integration.sh
EOF
git -c init.defaultBranch=main -C "$CLIENT_DIR" init --quiet
git -C "$CLIENT_DIR" checkout -b main --quiet 2>/dev/null || true
git -C "$CLIENT_DIR" -c user.email=test@example.com -c user.name=test add -A .wkp shared.md
git -C "$CLIENT_DIR" -c user.email=test@example.com -c user.name=test \
    commit -q -m "mtls integration test"

git_mtls() {
    git -c "http.sslCert=$CLIENT_DIR/.wkp/hub-device-cert.pem" \
        -c "http.sslKey=$CLIENT_DIR/.wkp/hub-device-key.pem" \
        -c "http.sslCAInfo=$WORKDIR/ca-cert.pem" \
        "$@"
}

git_mtls -C "$CLIENT_DIR" push --quiet "https://127.0.0.1:${HTTPS_PORT}/${TENANT}.git" main
log "PASS: push succeeded for an active, registered device"

log "fetching the same item back into a fresh clone -- this must succeed"
FETCH_DIR="$WORKDIR/fetch"
git_mtls clone --quiet "https://127.0.0.1:${HTTPS_PORT}/${TENANT}.git" "$FETCH_DIR"
if [ ! -f "$FETCH_DIR/shared.md" ]; then
    echo "FAIL: cloned repo is missing shared.md" >&2
    exit 1
fi
log "PASS: fetch succeeded and returned the pushed content"

log "revoking the device (by row id, M5-13: the real enrollment flow never surfaces a bare public-key string)"
hub_exec /usr/local/bin/wkp-hub device revoke-id "$device_id"

log "attempting another push with the same (now revoked) certificate -- this must fail at the TLS handshake"
echo "more content" >> "$CLIENT_DIR/shared.md"
git -C "$CLIENT_DIR" -c user.email=test@example.com -c user.name=test \
    commit -q -am "should never land"

if git_mtls -C "$CLIENT_DIR" push --quiet "https://127.0.0.1:${HTTPS_PORT}/${TENANT}.git" main \
    2>"$WORKDIR/push2.err"; then
    echo "FAIL: push succeeded with a revoked device certificate -- revocation is not effective" >&2
    cat "$WORKDIR/push2.err" >&2
    exit 1
fi

# The exact wording is OpenSSL's own ("...alert certificate revoked...",
# confirmed by hand against this same verifier); matched case-
# insensitively and loosely (TLS/SSL *and* "revoked") so this doesn't
# depend on one specific OpenSSL/libcurl version's exact phrasing --
# the load-bearing fact is that the failure is TLS-layer, not an
# ordinary git/HTTP error (a 401 would also make this push fail, but
# would *not* prove revocation is checked before request handling, per
# M5-10's own acceptance criterion).
if ! grep -qi "ssl\|tls" "$WORKDIR/push2.err" || ! grep -qi "revok" "$WORKDIR/push2.err"; then
    echo "FAIL: second push failed, but not clearly at the TLS layer for revocation:" >&2
    cat "$WORKDIR/push2.err" >&2
    exit 1
fi

log "PASS: push correctly refused after revocation, at the TLS handshake itself"
log "ALL CHECKS PASSED"
