# Build targets (M0-6)

Design 9.6's "no runtime" claim: `wkp` is a single static binary on Linux
and links only system frameworks on macOS. CI verifies this on every push
and PR (`.github/workflows/rust-ci.yml`); this doc is a reference for what
each job actually checks and how to reproduce it locally.

## Linux: `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`

Both build `wkp-cli` with `--release --locked`, then check:

- `file <binary>` and (`x86_64` only, since the runner can execute it)
  `ldd <binary>` report a static binary -- rustc's musl targets default to
  static-PIE, where `ldd` prints "statically linked" rather than "not a
  dynamic executable"; the CI check accepts either string.
- Binary size stays under the 10 MB gate (design 9.6).

`x86_64-unknown-linux-musl` builds directly on the `ubuntu-latest` runner
(`musl-tools` installed via `apt`) -- there is a real musl cross-linker for
this pair in Ubuntu's own repos. `aarch64-unknown-linux-musl` does not have
one, so that job uses `cross` (`cross-rs/cross`, pinned to `v0.2.5`), which
builds inside a prebuilt cross-compilation container via the runner's own
Docker daemon instead of this repo vendoring or hand-rolling a musl-cross
toolchain (CLAUDE.md's slim-core priority: reuse existing tooling).

Locally, `x86_64-unknown-linux-musl` needs `musl-tools` installed
(`apt-get install musl-tools` on Debian/Ubuntu; the package providing
`x86_64-linux-musl-gcc`); `aarch64-unknown-linux-musl` needs `cross`
(`cargo install cross --locked`) and a working Docker or Podman install.

```bash
cargo build --release --locked -p wkp-cli --target x86_64-unknown-linux-musl
cross build --release --locked -p wkp-cli --target aarch64-unknown-linux-musl
```

## `deploy/Containerfile`: `FROM scratch`

Proves the static-link claim end to end: an image containing nothing but
the musl binary still runs. Not `wkp-hub`'s image
(`deploy/hub/Containerfile`, Fedora-based, a real service with `sshd` and
`git`) -- this one exists only to exercise the CLI binary's own
static-link property.

```bash
cargo build --release --locked -p wkp-cli --target x86_64-unknown-linux-musl
podman build -f deploy/Containerfile -t wkp .
podman run --rm wkp --version
```

Runs as numeric UID 65532 (no `/etc/passwd` entry needed or possible in a
`scratch` image); fine for a binary that never does an NSS/username
lookup, which `wkp --version` and normal store operations don't.

## macOS: `aarch64-apple-darwin`

Builds `wkp-cli` with `--release --locked`, then checks:

- `otool -L <binary>` lists nothing besides base-system dylibs
  (`/usr/lib/*.dylib` -- e.g. `libSystem.B.dylib`, and `libiconv.2.dylib`,
  which this binary also links, pulled in transitively) and paths under
  `/System/Library/Frameworks/` -- any other entry is a non-system dynamic
  dependency that would break "links only system frameworks" on a machine
  without that library installed.
- Binary size stays under the 10 MB gate (design 9.6).

```bash
cargo build --release --locked -p wkp-cli --target aarch64-apple-darwin
otool -L target/aarch64-apple-darwin/release/wkp
```

## Out of scope here

Signing and multi-arch manifest publishing are M6 (release hardening), not
this task -- see `docs/plan/milestones.md`'s M0-6 entry.
