#!/bin/sh
# Entrypoint for the wkp-hub front-door container (M5-5; ADR-0011/#125:
# HTTPS + mutual TLS only, the original SSH transport and this
# entrypoint's own sshd-related setup removed). `wkp-hub serve` reads
# `DATABASE_URL` directly from its own process environment, so there is
# nothing left for this entrypoint to do beyond validating it's set and
# execing the real process -- `wkp-hub serve` itself is the container's
# lifecycle-defining process now, not a backgrounded process under sshd.
set -eu

: "${DATABASE_URL:?DATABASE_URL must be set (the control plane Postgres connection string)}"

# `WKP_HUB_CA_DIR` defaults to `/srv/wkp-hub/ca` (`main.rs`'s own
# default): a fresh CA root is generated there on first boot, exactly
# the "nothing deployed yet, CI-testable slice" posture this
# milestone's own scope notes describe, not a production persistence
# story.
exec /usr/local/bin/wkp-hub serve --port "${WKP_HUB_PORT:-8443}"
