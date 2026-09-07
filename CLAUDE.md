# CLAUDE.md: working agreement for agent-wkp v2

This file is for any agent or human contributing code on this branch. Read it fully before the first edit. `AGENTS.md` is a different document: it tells agents how to *use* the `wkp` CLI, not how to build it.

## What this repo is becoming

`wkp` is being rewritten from a Python CLI into a single static Rust binary that gives any agentic harness durable, cross-machine, cross-harness memory: a git repository of markdown files, a derived SQLite FTS5 index, and optional sync to a hosted hub. The authoritative design is `docs/design/wkp-hub-design-v0.1.md`. Milestones and exit criteria are in `docs/plan/milestones.md`. Every task is a GitHub issue with acceptance criteria that CI can check.

Read in this order before starting any task: this file, `docs/design/wkp-hub-design-v0.1.md` sections 3 through 6, the milestone you are working in, then the issue.

## Non-negotiable priorities (in order)

1. Latency and performance. Targets in design section 4.3. A PR that regresses the benchmark gate does not merge.
2. Security. Threat model in design section 7. Anything touching `wkp-crypto`, `wkp-git`, `wkp-hub`, sandbox rules or CI workflows needs a human co-sign (CODEOWNERS).
3. Slim core. Reuse git, SQLite, OpenSSH and OS facilities. Do not add a dependency that reimplements something one of those already does. Every new crate must be justified in the PR description and pass `cargo deny` and `cargo vet`.
4. Harness neutrality. The contract with a harness is: `wkp` on `PATH`, a hook line, stdout. No client library, no daemon requirement, no TCP port.

## Hard rules

- Rust edition 2021 or later, toolchain pinned in `rust-toolchain.toml`. `#![forbid(unsafe_code)]` in every crate except `wkp-sys`.
- No `Command::new("git")` outside `crates/wkp-git`. All git access goes through its plumbing wrapper.
- Secrets never touch argv or environment variables. Read them from the OS keystore, a `0600` file, or stdin.
- Files a harness reads (`tier0.md`, `index.db`) are written to a temp file in the same directory and renamed into place. Never write them in place.
- Agent-written memory lands in `inbox/` with `confidence: proposed`. Nothing enters Tier 0 or Tier 1 without a human-signed commit. There is a test for this (`tests/injection-corpus`); do not weaken it.
- No new on-disk formats. The store is markdown in git; the index is SQLite; the config is TOML.
- Frontmatter fields and CLI flags are a public contract once merged to `main`. Additive changes only; removals need an ADR.
- Do not edit `.github/workflows/*`, `deny.toml`, `supply-chain/` or `CODEOWNERS` in the same PR as feature code.

## Commands

```
cargo build --release              # static binary at target/release/wkp
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo deny check
cargo audit
cargo bench -p wkp-core            # criterion; compare against benches/baseline.json
```

Run all of them before opening a PR and paste the tail of the output in the PR description. CI re-runs everything regardless.

## Definition of done for a PR

- Linked issue, acceptance criteria copied into the PR description with each item checked.
- Tests for new behavior; a regression test for any bug fixed.
- No new `unsafe`, no new dependency without justification, no benchmark regression beyond the threshold in `benches/README.md`.
- Commit messages: imperative subject under 72 chars, body explains why, `Signed-off-by` trailer, and a `Co-Authored-By` trailer naming the agent and model that produced the change.
- Small PRs. One issue per PR. If an issue turns out to need two PRs, split the issue.

## Layout (target state, see design 3.3)

```
crates/wkp-core    store model, frontmatter, tiers, index, FTS5 search, merge driver
crates/wkp-crypto  age filter, keys, signing, allowed_signers        (human co-sign)
crates/wkp-git     plumbing wrapper, bundles, sync                    (human co-sign)
crates/wkp-cli     the wkp binary; wkpd is a subcommand
crates/wkp-hub     hub mode: wkp-shell, tenant mapping, indexer, control plane (human co-sign)
crates/wkp-sys     only crate allowed unsafe (bundled SQLite)
adapters/          hook templates per harness, printed by `wkp hooks`
deploy/            Containerfiles, sshd_config, systemd and launchd units
fuzz/              cargo-fuzz targets for every parser of untrusted input
tests/injection-corpus/
docs/design, docs/plan, docs/adr
```

## When the design and reality disagree

Do not silently deviate. Open an ADR in `docs/adr/` using the template, state the conflict, the options, and the recommendation, and link it from the PR. Small clarifications go into the design doc directly with a changelog line at the top.

## Things that look like shortcuts and are not allowed

- Adding a loopback HTTP server "for now".
- Storing anything in the index that is not derivable from the store.
- Calling an embedding model or network endpoint in the session-start or `wkp search` default path.
- Auto-promoting inbox items to Tier 0 to make a demo look better.
- Vendoring a Python or Node dependency.
