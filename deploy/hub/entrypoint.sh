#!/bin/sh
# Entrypoint for the wkp-hub front-door container (M5-5). Runs as root
# (needed to write a root:wkp-akc-owned secret file and to bind port
# 22), then execs sshd itself, which drops privileges per-connection as
# usual.
set -eu

# Idempotent: only generates keys that don't already exist, so a
# persisted volume across restarts keeps the same host identity;
# a fresh container without one gets a new ed25519 host key on first
# boot. sshd_config restricts HostKeyAlgorithms to ed25519 only, so
# that's the only key type this needs to produce.
if [ ! -f /etc/ssh/ssh_host_ed25519_key ]; then
    ssh-keygen -q -t ed25519 -f /etc/ssh/ssh_host_ed25519_key -N ""
fi

# See wkp-hub-akc.sh's own comment: DATABASE_URL is secret-shaped and
# sshd does not pass its own process environment through to
# AuthorizedKeysCommand, so it has to reach that subprocess via a file
# instead. Written fresh on every container start from whatever the
# runtime (podman run -e, a Kubernetes Secret env var, ...) provided --
# never baked into the image itself.
: "${DATABASE_URL:?DATABASE_URL must be set (the control plane's Postgres connection string)}"
umask 077
printf 'DATABASE_URL=%s\n' "$DATABASE_URL" > /etc/wkp-hub/env
chown root:wkp-akc /etc/wkp-hub/env
chmod 640 /etc/wkp-hub/env

exec /usr/sbin/sshd -D -e
