# deploy/

Deployment artifacts for running `wkpd` (M3-8) as a per-user background
service. `wkpd` is a thin, watch-triggered wrapper around subcommands
that already work standalone (`wkp index`, `wkp remember`'s underlying
commit machinery, `wkp sync`) -- nothing here is required for `wkp`
itself; these units exist purely so a human doesn't have to run
`wkp sync` by hand after every edit.

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
