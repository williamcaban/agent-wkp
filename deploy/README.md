# deploy/

Deployment artifacts for `wkpd` (the laptop-side background service) and
for `wkp-hub` (the hosted hub, design section 8).

## `wkpd` (M3-8)

A thin, watch-triggered wrapper around subcommands that already work
standalone (`wkp index`, `wkp remember`'s underlying commit machinery,
`wkp sync`) -- nothing here is required for `wkp` itself; these units
exist purely so a human doesn't have to run `wkp sync` by hand after
every edit.

- `systemd/wkpd.service` -- Linux, `systemd --user`. The only platform
  `wkpd`'s Unix-domain-socket peer-credential check currently runs on;
  see `docs/adr/0004-wkpd-peer-credential-verification.md`.
- `launchd/com.agent-wkp.wkpd.plist` -- macOS. Ships the intended
  deployment shape for when macOS peer-credential support lands
  upstream (tracked in the same ADR); `wkpd` itself refuses to start on
  macOS today rather than running with an unverified or absent peer
  check.

Both units run as the invoking user, never root/system-wide, and both
need their placeholder paths (`REPLACE_ME_*`) filled in for the specific
store being watched -- see the comments inside each file.

## `hub/` (M5-5)

The hub's front-door container: `sshd` + `wkp-shell` in front of a
single shared filesystem (per-tenant hard isolation via a pod per
tenant is ADR-0009/M5-7's job, not this image's). Fedora-based
(`quay.io/fedora/fedora-minimal`), built and run with `podman`.

- `Containerfile` -- multi-stage build (a Rust builder stage compiling
  `wkp-hub` at the toolchain version `rust-toolchain.toml` pins, then a
  minimal runtime stage with `sshd`, `git`, and the compiled binary).
- `sshd_config` -- design 8.1's hardening list, checked in verbatim.
- `entrypoint.sh` -- generates the host key on first boot, and writes
  `DATABASE_URL` to a `0640` file `wkp-hub-akc.sh` sources: `sshd`
  invokes `AuthorizedKeysCommand` with a sanitized environment (a
  container-level env var does not reach it -- confirmed by hand while
  building this image), and a Postgres connection string is
  secret-shaped, so CLAUDE.md's "read secrets from... a 0600 file" rule
  applies directly rather than relying on env-var passthrough that
  doesn't happen anyway.
- `wkp-hub-akc.sh` -- the `AuthorizedKeysCommand` wrapper described
  above; must stay root-owned and non-group/world-writable (`sshd`'s
  own requirement for this directive).
- `test-ssh-integration.sh` -- the milestone's one required integration
  test: register a device, push over SSH, revoke it, attempt another
  push, confirm it fails. Run it locally against a built image
  (`podman build -f deploy/hub/Containerfile -t wkp-hub .`) with
  `DATABASE_URL` pointing at a reachable Postgres; CI wires the same
  script up against a service container (a separate PR, per CLAUDE.md's
  rule against mixing workflow-file changes with feature code).
