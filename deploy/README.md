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

## `hub/` (M5-5, ADR-0011/#125)

The hub's front-door container: HTTPS with mutual TLS only (the
original SSH transport -- `sshd` + `wkp-shell` -- was dropped by
ADR-0011 and its code removed, #125). Fedora-based
(`quay.io/fedora/fedora-minimal`), built and run with `podman`.

- `Containerfile` -- multi-stage build (a Rust builder stage compiling
  `wkp-hub` at the toolchain version `rust-toolchain.toml` pins, then a
  minimal runtime stage with `git`, `podman-remote` (ADR-0012 -- lets
  this container reach a `podman` engine on the host to start/stop
  tenant pods without running its own nested container runtime), and
  the compiled binary). No `sshd` here anymore.
- `entrypoint.sh` -- validates `DATABASE_URL` is set and execs
  `wkp-hub serve` directly; `wkp-hub serve` reads `DATABASE_URL` and
  its CA directory from its own process environment/defaults, so there
  is nothing left for this entrypoint to do beyond that.
- `test-mtls-integration.sh` -- the milestone's one required
  integration test (M5-13): register a device over the real RFC 8628 +
  CSR flow, push and fetch over HTTPS with mutual TLS, revoke it,
  attempt another push, confirm it fails at the TLS handshake itself.
  Run it locally against a built image
  (`podman build -f deploy/hub/Containerfile -t wkp-hub .`) with
  `DATABASE_URL` pointing at a reachable Postgres; CI wires the same
  script up against a service container (`hub HTTPS+mTLS integration
  test (M5-13)`, `.github/workflows/rust-ci.yml`).
- `test-pod-lifecycle.sh`, `test-cross-tenant-isolation.sh` (M5-7,
  #110) -- per-tenant pod provisioning/lifecycle and a real
  cross-tenant filesystem isolation check (a tenant's own pod
  container never has any other tenant's repo bind-mounted into it at
  all, not just an application-level path check). CI job:
  `hub per-tenant pod isolation test (M5-7)`.

## Top-level `Containerfile` (M0-6)

A separate, much smaller image: `FROM scratch`, containing nothing but
a statically linked `wkp` CLI binary (`x86_64-unknown-linux-musl`),
proving design 9.6's "no runtime" claim end to end. Not related to
`hub/`'s image -- this one exists purely to exercise the CLI binary's
own static-link property. See `docs/plan/build-targets.md`.
