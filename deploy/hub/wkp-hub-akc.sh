#!/bin/sh
# Wrapper for sshd_config's AuthorizedKeysCommand (M5-5).
#
# sshd invokes AuthorizedKeysCommand with a minimal, sanitized
# environment -- verified empirically while building this container: a
# DATABASE_URL set on the container's own process environment does
# *not* reach this subprocess. A Postgres connection string usually
# embeds a password, so CLAUDE.md's secrets rule ("read secrets from...
# a 0600 file") applies directly: entrypoint.sh writes it once, at
# container startup, to a file only this script's own user
# (AuthorizedKeysCommandUser, see sshd_config) can read, and this
# script sources it into *its own* short-lived subprocess environment
# right before exec -- never a long-lived, container-wide env var
# anything else could read via /proc/<pid>/environ.
set -e

# shellcheck disable=SC1091
. /etc/wkp-hub/env
export DATABASE_URL

exec /usr/local/bin/wkp-hub authorized-keys-command "$@"
