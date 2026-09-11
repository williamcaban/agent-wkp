#!/bin/sh
# Entrypoint for the wkp-hub front-door container (M5-5, extended for
# M5-13/ADR-0011). Runs as root (needed to write a root:wkp-akc-owned
# secret file and to bind port 22), starts the HTTPS/mTLS front door
# (`wkp-hub serve`, M5-8 through M5-10) in the background, then execs
# sshd itself in the foreground -- sshd stays the container's own
# lifecycle-defining process for now, matching M5-5's original design,
# since removing it entirely is #125's own separate, sequenced task
# (ADR-0011), not this one's. Both transports run side by side until
# then; neither depends on the other.
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

# M5-13 (ADR-0011): the HTTPS/mTLS front door, backgrounded. Its own
# process environment (this shell's) is untouched by the
# AuthorizedKeysCommand sanitization problem the file above works
# around -- `wkp-hub serve` reads `DATABASE_URL` directly, the same way
# every other `wkp-hub` subcommand already does. `WKP_HUB_CA_DIR`
# defaults to `/srv/wkp-hub/ca` (`main.rs`'s own default): a fresh CA
# root is generated there on first boot, exactly the "nothing deployed
# yet, CI-testable slice" posture this milestone's own scope notes
# describe, not a production persistence story.
/usr/local/bin/wkp-hub serve --port "${WKP_HUB_PORT:-8443}" &

exec /usr/sbin/sshd -D -e
