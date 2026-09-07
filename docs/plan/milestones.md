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

M1's exit criterion holds on `v2-rust` as of all nine tasks above merging (issues #14-22, PRs #23/#24/#25/#27/#28/#30/#31/#32/#33/#34/#35/#36). M2 tasks below are broken out per that.

## M2 tasks

1. Identity model and `allowed_signers` (design 7.3): the file format the store carries mapping a principal (`human:<id>` / `agent:<harness>[:<model>]`) to a `role` (`human`/`agent`) and an SSH public key; `wkp-git` plumbing to read and append entries; `wkp init` wires `gpg.format=ssh` and `gpg.ssh.allowedSignersFile` to point at it. Out of scope: OS keystore/Keychain ACL integration for agent private keys (that lands alongside M4's keystore work); this task assumes a key is already available to `ssh-agent` or referenced by path.
2. Signed plumbing commit wrapper in `wkp-git` (design 5.1, 7.3): a real SSH-signed commit primitive (plumbing, not the porcelain `commit_all` M1 left as a test-only placeholder) that takes a caller-specified identity (a principal from task 1), stages the given paths, and produces a commit `git verify-commit` accepts. Replaces `commit_all`'s doc-comment TODO.
3. Provenance trailers (design 5.4, 7.4): a commit-message trailer format (actor, session, source, confidence) appended by task 2's wrapper, plus a parser that reads them back off a commit for provenance queries.
4. Gitleaks-style secret detection on the write path (design 7.6): a compact, bundled ruleset (regex + entropy checks, evaluated in-process, no shelling out to a `gitleaks` binary) that refuses content matching a credential pattern and returns a redacted preview.
5. `wkp remember` (design 7.4, 7.6): reads content from stdin (never argv, per CLAUDE.md's secrets rule) plus minimal frontmatter fields, runs task 4's scan first, writes the file under `inbox/<slug>.md` with `confidence: proposed` and `provenance.source`, commits it via task 2/3's signed-plumbing-plus-trailers path.
6. Real provenance-gated tier computation (design 7.4), replacing `wkp_core::index::compute_tier`'s M1-4 type/confidence placeholder: Tier 0/1 requires the item's latest commit to be signed by a `role: human` principal (task 1), plus the `type: instruction` path-scoping rule from 7.4. Also closes the `expires` gap noticed auditing M1: an item whose `expires` date has passed is excluded from Tier 0/1 the same way an `inbox/`/agent-confidence item already is (design 5.4 says the indexer does this automatically; M1 never wired it in). Out of scope: verifying commits that arrive via sync/fetch from another machine (`wkp verify`, a fetch-time gate) — that is M3's "unsigned or unknown-signer commits are excluded from Tier 0 and 1" exit-criterion line, not this task's; this task's gate applies regardless of a commit's origin, sync just doesn't exist yet to originate one.
7. `wkp promote <path>` (design 7.4): moves an `inbox/` item into the durable tree (`user/`, `projects/<name>/`, `org/<name>/` per the 5.4 layout convention) with a human-signed commit; refuses when the calling identity isn't `role: human`, except for a harness explicitly configured `promote: auto` (7.4's documented, opt-in, not-default escape hatch).
8. Injection-corpus test suite (`tests/injection-corpus/`, CLAUDE.md: "there is a test for this; do not weaken it"): adversarial fixture stores (unsigned commits, wrong-role signers claiming `type: project-state`/`instruction`, prompt-injection payloads in otherwise-valid frontmatter, `inbox/` items claiming a tier-0 type) with a test asserting `wkp materialize --tier 0|1` never includes any of them — the regression gate for tasks 6 and 7 together.

Later milestones are broken into tasks when the previous milestone's exit criterion holds.
