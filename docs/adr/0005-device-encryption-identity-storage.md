# ADR-0005: Device encryption identity storage — keystore-or-`0600`-file, no ssh-agent fallback

Status: accepted
Date: 2026-09-08
Design sections affected: 7.2, 7.3, 7.5

## Context

M4-2 (`docs/plan/milestones.md`) requires generating a device's X25519
encryption identity (M4-1, `wkp_crypto::Identity`) once and persisting it.
Design 7.2 and 7.5 both say private keys "live in macOS Keychain or Linux
secret-service with ssh-agent fallback" — but that sentence describes SSH
*signing* key storage (design 7.3's own wording: "Agent keys are stored in
the keystore with an ACL that permits `wkp` to use them... Linux via
`ssh-agent` confirmation or a dedicated agent socket"), applied to an age
X25519 identity by analogy, without being re-derived for it.

`ssh-agent` implements the SSH agent protocol: it holds a signing key and,
on request, produces a *signature* over data it's handed — it never
returns the key material itself. An age X25519 identity is a *decryption*
key: using it means performing an X25519 Diffie-Hellman + HKDF to unwrap a
per-file symmetric key, an operation `ssh-agent` has no protocol message
for and was never designed to perform. There is no such thing as
"ssh-agent holding an age identity" — the design text's fallback doesn't
describe a real mechanism for this key type, it's a copy-forward from the
signing-key paragraph next to it.

CLAUDE.md: "When the design and reality disagree... open an ADR... state
the conflict, options, and recommendation" rather than silently
reinterpreting the text. This is that ADR, per M4-2's issue (#74), which
already flagged the gap and a likely resolution when the M4 task
breakdown was written.

## Options

1. **Literally attempt an ssh-agent-shaped fallback** — e.g. store the
   X25519 identity's bytes as an opaque ssh-agent-protocol blob and shell
   out to `ssh-add`/read it back. Rejected: there is no standard
   ssh-agent message for "store and later return an opaque decryption
   key on demand" (the closest real primitives, `ssh-agent` confidential
   data extensions, are OpenSSH-specific, not portable across the macOS
   and Linux agents this project targets, and would make `wkp-crypto`
   depend on an external agent process being present and cooperative —
   directly against D2's "no mandatory daemon" framing extended to a
   second daemon dependency this design never asked for).
2. **Keystore-only, hard failure with no fallback.** Simple, but violates
   CLAUDE.md's own secrets rule ("read them from the OS keystore, **or**
   a `0600` file, or stdin") and leaves `wkp` unusable for encryption on
   any machine without a working keystore daemon reachable (headless
   Linux boxes with no D-Bus session — exactly this project's own CI
   runners and this session's own sandbox, confirmed by hand: no
   `dbus-daemon`, no `$DBUS_SESSION_BUS_ADDRESS`, `Store::new()` fails
   with a clear `PlatformFailure` in that environment).
3. **Keystore first, `0600` file fallback on any keystore error — the
   general secrets-storage rule CLAUDE.md already states, applied here
   with no special case.** Matches M2's existing precedent for every
   other secret this project handles (signing keys are "already
   available to `ssh-agent` or referenced by path" per M2-1's own scope
   note); doesn't invent a fourth storage mechanism. Chosen.

## Decision

A device's encryption identity is stored via the OS keystore
(`apple-native-keyring-store`'s Keychain backend on macOS,
`zbus-secret-service-keyring-store`'s Secret Service backend on Linux,
both accessed through the storage-agnostic `keyring-core` API) when
reachable, and a `0600` file otherwise — the same two places CLAUDE.md
already names for every secret this project handles, no third mechanism
invented for this key type specifically. There is no ssh-agent-shaped
fallback; design 7.2/7.5's phrasing is corrected by this ADR to mean
"OS keystore, else a `0600` file" for encryption identities specifically,
distinct from design 7.3's ssh-agent-based story for *signing* keys,
which this ADR does not touch or reinterpret.

Explicitly not decided here: whether the fallback file lives inside a
specific `wkp` store's `.wkp/` directory or at a machine-wide location —
`wkp_crypto::device_identity::ensure(key_id, fallback_path)` takes both
the keystore entry's scoping key and the fallback path as caller-supplied
parameters, keeping this crate git/store-agnostic per M4-1's own scope
boundary. The caller that actually wires this into the CLI (M4-4) decides
what to pass; that choice isn't forced by anything in this ADR.

## Consequences

- `wkp-crypto` gains real dependencies on `keyring-core` plus one
  platform-gated backend crate per target OS (`apple-native-keyring-store`
  on macOS, `zbus-secret-service-keyring-store` on Linux) — both new,
  justified by this decision, `cargo vet`-exempted alongside `age`'s own
  large exemption set from M4-1 rather than hand-certified.
- On any platform other than macOS or Linux, the keystore backend is a
  fixed stub that always errors (matching `wkpd`'s `peer_is_self`
  precedent in ADR-0004) — `ensure` transparently uses the file fallback
  there. No behavior is claimed for those platforms beyond "the file
  fallback works", which is exercised by real tests.
- The Linux keystore path is exercised in this project's own tests as the
  **fallback-triggering failure case**, not a full round trip — neither
  this sandbox nor (presumably) this project's CI runners have a D-Bus
  session or Secret Service daemon reachable. A real Keychain/secret-service
  round trip is unverified by automated tests on either platform; the
  macOS code path is verified only by the macOS cross-compile job
  building it, same allowance M4-2's own issue already granted. This gap
  should be revisited if this project ever adds a CI job that provisions
  a real secret-service/Keychain (out of scope for M4).
- A transient keystore read error (not "entry doesn't exist") must never
  be treated as "doesn't exist yet" and trigger generating a replacement
  identity, which would silently overwrite whatever the keystore already
  held and destroy access to anything already encrypted to it — enforced
  by matching `keyring_core::Error::NoEntry` specifically before
  generating a new identity, everything else propagates as a real error.
