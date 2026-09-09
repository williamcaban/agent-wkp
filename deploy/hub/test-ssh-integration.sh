#!/bin/bash
# M5-5's own required integration test (docs/plan/milestones.md's M5
# scope notes): register a device, push over SSH, revoke it, attempt
# another push, confirm it fails -- against the real, built `wkp-hub`
# image (deploy/hub/Containerfile), over a real SSH connection, not a
# simulated call. Revocation only actually works because sshd
# re-evaluates AuthorizedKeysCommand fresh on the *next* connection;
# this test is what proves that, not just that the code compiles.
#
# Expects: `podman`, `git`, `ssh-keygen`, `ssh` on PATH; the image
# already built and tagged (default localhost/wkp-hub, override via
# WKP_HUB_IMAGE); DATABASE_URL pointing at a reachable Postgres whose
# schema this script's own `wkp-hub migrate` call will ensure exists
# (a fresh, empty database is fine -- this script only ever creates one
# tenant, with a fixed slug, and does not assume anything else in that
# database is untouched).
#
# `--network host`: the built-in default is the simplest way for this
# container to reach whatever DATABASE_URL names (a CI Postgres service
# container is reachable at `localhost` from the runner's own network
# namespace, which `--network host` shares) without needing this
# script to know the container-runtime-specific address a bridge
# network's gateway would otherwise require -- see the comment in
# ADR-0009 about why this container's *tenant* isolation model
# deliberately avoids depending on such runtime-specific addressing;
# this is just how the single hub container itself reaches its own
# control-plane database; ADR-0009's concern doesn't apply here.
set -euo pipefail

IMAGE="${WKP_HUB_IMAGE:-localhost/wkp-hub}"
CONTAINER_NAME="wkp-hub-ssh-integration-test"
# sshd_config hardcodes Port 22 -- with --network host this container
# shares the runner's own network namespace, so 22 here really means
# the runner's port 22, reachable at 127.0.0.1 below. A rootless
# sandbox without CAP_NET_BIND_SERVICE (found while developing this
# image locally) cannot bind 22 this way; run with bridge networking
# and a mapped port plus a rewritten DATABASE_URL host instead in that
# case -- this script targets a real CI runner, not that constraint.
SSH_PORT="${WKP_HUB_TEST_SSH_PORT:-22}"
TENANT="ssh-integration-test"
: "${DATABASE_URL:?DATABASE_URL must be set (e.g. postgres://postgres:wkp_hub_ci@localhost:5432/wkp_hub_test)}"

WORKDIR="$(mktemp -d)"
cleanup() {
    podman rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

log() { printf '==> %s\n' "$*"; }

log "starting wkp-hub container"
podman run -d --name "$CONTAINER_NAME" --network host \
    -e DATABASE_URL="$DATABASE_URL" \
    "$IMAGE" >/dev/null

hub_exec() {
    podman exec -e DATABASE_URL="$DATABASE_URL" "$@"
}

log "waiting for the control plane's schema"
for _ in $(seq 1 20); do
    if hub_exec "$CONTAINER_NAME" /usr/local/bin/wkp-hub migrate >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
done
hub_exec "$CONTAINER_NAME" /usr/local/bin/wkp-hub migrate

log "creating tenant '$TENANT' (control-plane row + repo; as the git user -- see Containerfile's own note on repo ownership)"
podman exec --user git -e DATABASE_URL="$DATABASE_URL" "$CONTAINER_NAME" \
    /usr/local/bin/wkp-hub tenant create "$TENANT"

log "generating a device keypair and registering it"
ssh-keygen -t ed25519 -f "$WORKDIR/device-key" -N "" -q
device_pubkey="$(cut -d' ' -f1,2 "$WORKDIR/device-key.pub")"
hub_exec "$CONTAINER_NAME" /usr/local/bin/wkp-hub device register "$TENANT" "$device_pubkey"

ssh_opts=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
          -o ConnectTimeout=5 -i "$WORKDIR/device-key" -p "$SSH_PORT")

log "cloning and pushing a shared item over SSH -- this must succeed"
git -c init.defaultBranch=main init --quiet "$WORKDIR/client"
git -C "$WORKDIR/client" checkout -b main --quiet 2>/dev/null || true
cat > "$WORKDIR/client/shared.md" <<'EOF'
---
visibility: shared
title: ssh integration test item
---

pushed by deploy/hub/test-ssh-integration.sh
EOF
git -C "$WORKDIR/client" -c user.email=test@example.com -c user.name=test \
    add -A
git -C "$WORKDIR/client" -c user.email=test@example.com -c user.name=test \
    commit -q -m "ssh integration test"
git -C "$WORKDIR/client" remote add origin \
    "ssh://git@127.0.0.1:${SSH_PORT}/${TENANT}.git"

GIT_SSH_COMMAND="ssh ${ssh_opts[*]}" \
    git -C "$WORKDIR/client" push --quiet origin main

log "PASS: push succeeded for an active, registered device"

log "revoking the device"
hub_exec "$CONTAINER_NAME" /usr/local/bin/wkp-hub device revoke "$device_pubkey"

log "attempting another push with the same (now revoked) key -- this must fail"
echo "more content" >> "$WORKDIR/client/shared.md"
git -C "$WORKDIR/client" -c user.email=test@example.com -c user.name=test \
    commit -q -am "should never land"

if GIT_SSH_COMMAND="ssh ${ssh_opts[*]}" \
    git -C "$WORKDIR/client" push --quiet origin main 2>"$WORKDIR/push2.err"; then
    echo "FAIL: push succeeded with a revoked device key -- revocation is not effective" >&2
    cat "$WORKDIR/push2.err" >&2
    exit 1
fi

if ! grep -q "Permission denied" "$WORKDIR/push2.err"; then
    echo "FAIL: second push failed, but not for the expected reason (revoked pubkey auth):" >&2
    cat "$WORKDIR/push2.err" >&2
    exit 1
fi

log "PASS: push correctly refused after revocation (sshd re-evaluated AuthorizedKeysCommand on the new connection)"
log "ALL CHECKS PASSED"
