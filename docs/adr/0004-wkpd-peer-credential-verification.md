# ADR-0004: `wkpd`'s UDS peer-credential verification — `rustix` on Linux, fail closed elsewhere

Status: accepted
Date: 2026-09-08
Design sections affected: 4.2, 7.1, 7.5

## Context

M3-8 (`docs/plan/milestones.md`) requires `wkpd` to bind only a Unix
domain socket, "mode `0600`, peer UID verified via `SO_PEERCRED`/
`LOCAL_PEERCRED`". Design 7.1's threat model lists this exact check as
the primary control against "malicious or compromised local process...
talks to `wkpd`". This is not optional hardening; it is the control the
design cites for that threat row.

Retrieving peer credentials off a `UnixStream` is not available in safe,
stable Rust. `std::os::unix::net::UnixStream::peer_cred()` exists but is
gated behind the unstable `peer_credentials_unix_socket` feature (rust-lang/rust#42839) —
confirmed by hand against this repo's pinned toolchain (1.98.1) before
writing any code, not assumed from memory. Getting there on stable Rust
means calling the underlying `getsockopt` syscall, which is an `unsafe`
operation. CLAUDE.md forbids `unsafe_code` in every crate except
`wkp-sys` ("only crate allowed unsafe (bundled SQLite)").

## Options

1. **Extend `wkp-sys`'s unsafe surface** to also wrap `getsockopt`
   (Linux `SO_PEERCRED`, macOS `LOCAL_PEERCRED`) behind a safe function,
   calling it from `wkp-cli`. Keeps CLAUDE.md's "one unsafe crate" rule
   technically intact, but broadens what that crate is for beyond
   bundled SQLite, and means the CODEOWNERS-gated crate that must never
   panic on malformed untrusted binary input (SQLite files) now also
   owns a security-critical, own-process-facing syscall wrapper — a
   larger, more sensitive unsafe surface for one human reviewer to carry.
2. **Add `libc` + local `unsafe` to `wkp-cli`.** Rejected outright:
   directly contradicts the `#![forbid(unsafe_code)]` hard rule in the
   one crate this whole feature lives in.
3. **Add `rustix`** (`net` feature), calling
   `rustix::net::sockopt::socket_peercred`, a safe wrapper with no
   `unsafe` at the call site. Verified by hand: compiles clean under
   `#![forbid(unsafe_code)]` and returns correct real credentials at
   runtime in this sandbox. `rustix` is already a transitive dependency
   of this workspace (pulled in by `tempfile`; `Cargo.lock` already
   carries `rustix v1.1.4`), so this adds no new supply-chain surface,
   only promotes an already-present, already-resolved crate to a direct
   dependency. **Confirmed gap, checked by hand rather than assumed:**
   `socket_peercred` is Linux-only; macOS/FreeBSD support is an open,
   unimplemented upstream issue (bytecodealliance/rustix#1533) as of
   this writing. macOS instead uses a different mechanism entirely
   (`SOL_LOCAL`/`LOCAL_PEERCRED`, an `xucred` struct), which `rustix`
   does not yet wrap.

## Decision

Use `rustix` (option 3) for peer-credential verification. On Linux,
`wkpd` binds the socket, verifies every connecting peer's UID against
its own (`rustix::process::getuid()`, also safe, no local `unsafe`),
and drops any non-matching connection. On any other target OS (macOS,
BSDs — the only other platform this design names, via the `launchd`
unit), `wkpd` refuses to bind the socket at all and exits with a clear
error naming this ADR, rather than silently running with no peer check
or attempting an unverified, untested syscall path. Fail closed: an
unimplemented security control must never be indistinguishable from a
working one. The `launchd` unit is still shipped (`deploy/`) as the
documented intended deployment shape for when macOS support lands, not
as a claim that it works today.

Not decided here: whether wkp-sys should ever grow its unsafe surface
for this or a similar future need. This ADR chooses the narrower,
already-in-the-tree, safe-Rust path instead, for this specific need.

## Consequences

- Linux `wkpd` has a real, tested (this sandbox is Linux), safe
  peer-credential check from day one.
- macOS `wkpd` does not run at all yet — a real functional gap, not
  silently accepted. Revisit when rustix (or a comparably minimal,
  vetted alternative) adds macOS/BSD support, or when a maintainer
  decides the wkp-sys route is worth its broader review surface for
  this specific platform gap. Track as a follow-up task under M3-8 or a
  later milestone, not blocking this ADR's acceptance.
- `rustix` (`features = ["net"]`) becomes a direct dependency of
  `wkp-cli`; `cargo vet`/`cargo deny` exemptions added in the same PR
  that introduces the direct dependency edge, per this repo's existing
  precedent for dev/build-only and now-promoted-to-direct transitive
  dependencies.
