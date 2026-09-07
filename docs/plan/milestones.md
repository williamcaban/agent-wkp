# Implementation milestones

Each milestone has an exit criterion that is observable, not a checklist of files. A milestone is done when its exit criterion holds on `main` with CI green. Issues carry the `M<n>` label. Order is strict: M1 replaces the Python tool for daily use before any sync or hub work starts, so every later milestone is dogfooded on a real store.

| Milestone | Theme | Exit criterion |
|---|---|---|
| M0 | Foundations | `wkp --version` builds as a static binary on macOS arm64 and Linux x86_64 from a clean checkout; CI runs fmt, clippy, test, audit, deny, bench; binary under 10 MB; minimum git version decided and enforced at runtime |
| M1 | Read path parity | On the author's real store, `wkp init`, `index`, `search`, `context`, `materialize`, `hooks --framework claude_code` work end to end; Claude Code SessionStart hook injects Tier 0; latency targets in design 4.3 met on the bench corpus; Python tool no longer used day to day |
| M2 | Write path and audit | `wkp remember` produces SSH-signed plumbing commits with provenance trailers; agent writes land in `inbox/`; `wkp promote` requires a human-signed commit; injection-corpus test passes; gitleaks rules block credential-like content |
| M3 | Local sync | Two machines and a plain bare repo (NAS or GitHub) stay in sync through `wkpd` per-device branches; the `wkp` merge driver resolves modify/delete and add/add without data loss; `git bundle` round trip works air-gapped; unsigned or unknown-signer commits are excluded from Tier 0 and 1 |
| M4 | Encryption | `visibility: private` items are age-encrypted in git via clean/smudge; keys live in macOS Keychain or Linux secret-service with ssh-agent fallback; recovery key flow; `wkp forget` rotates recipients; `wkp purge` documented and tested |
| M5 | Hub | Container with sshd + `wkp-shell` + `git http-backend`; per-tenant UID, bare repo and SQLite index; RFC 8628 device registration; device revocation effective on next connection; integration test: register, push, revoke, push-must-fail |
| M6 | Hardening and release | Landlock + seccomp on Linux, sandbox on macOS verified; cosign-signed reproducible releases with SLSA provenance and SBOM; Homebrew tap; OCI image; OpenSSF Scorecard gate; self-update verifies signatures |

## M0 tasks

1. Scaffold the Cargo workspace per design 3.3 with `rust-toolchain.toml`, release profile (`lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `strip = true`), `deny.toml`, `supply-chain/` for cargo-vet, `#![forbid(unsafe_code)]` everywhere except `wkp-sys`.
2. CI baseline: fmt, clippy `-D warnings`, test, `cargo audit`, `cargo deny`, `cargo vet`, Semgrep with the Rust ruleset, actions pinned by SHA, least-privilege `permissions:`.
3. Git minimum-version spike: confirm SSH signing (`gpg.format=ssh`, `gpg.ssh.allowedSignersFile`) and builtin fsmonitor behavior on the current macOS and Fedora git builds; record the decision as ADR-0001; `wkp` refuses to run below the minimum with a clear message.
4. Remove the Python tree from this branch after tagging `v0-python` on `main`; keep `AGENTS.md` (to be rewritten in M1) and `LICENSE`.
5. Benchmark harness: criterion benches for cold `wkp search`, incremental index, materialize; a fixture corpus generator (5k and 50k items); `benches/baseline.json` and a regression threshold documented in `benches/README.md`.
6. Cross-compile and static-link check: `x86_64-unknown-linux-musl`, `aarch64-apple-darwin`; size gate.

## M1 tasks

1. OKF frontmatter parser with the v2 fields (`scope`, `provenance`, `confidence`, `expires`, new `type` values); tolerant of missing or malformed frontmatter; fuzz target.
2. Index: bundled SQLite with FTS5, schema from design 5.3 (porter unicode61 content column, trigram column for identifiers, metadata columns), atomic swap of `index.db`.
3. Change detection via `git update-index --refresh` + `git status --porcelain=v2`, fsmonitor when available; hash only changed files.
4. `wkp search` with BM25, tier and budget filters, `--format paths|text|json`; cold-start budget enforced by bench.
5. `wkp context` and `wkp traverse` parity with the Python behavior (graph edges from `refs:` and wikilinks).
6. `wkp materialize --tier 0|1` with atomic write; `wkp hooks --framework claude_code` prints hook text only.
7. Importer for `~/.claude/projects/*/memory/`, `CLAUDE.md`, `AGENTS.md` tagged `source: import`.
8. Golden tests for `tier0.md` output; rewrite `AGENTS.md` for the v2 CLI.
9. Optional hybrid search via `--embed-url` with RRF and BM25 fallback, kept out of the default path.

Later milestones are broken into tasks when the previous milestone's exit criterion holds.
