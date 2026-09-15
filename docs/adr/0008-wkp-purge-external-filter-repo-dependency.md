# ADR-0008: `wkp purge` depends on an external `git-filter-repo` install, not a vendored one

Status: accepted
Date: 2026-09-09
Design sections affected: 7.6

## Context

M4-6 (`docs/plan/milestones.md`, issue #78) needs true git history erasure: removing a path from every commit that ever touched it, not just the current tree (`wkp forget`, M4-5, only ever touches the current tree). `git filter-branch` is git's own deprecated, slow, footgun-prone tool for this; `git-filter-repo` is git's own documented replacement, recommended in `git filter-branch`'s own manual page. There is no pure-Rust equivalent in this workspace's dependency tree, and `git-filter-repo` is a single, actively-maintained Python script with no packaging path into a `Cargo.toml` dependency.

## Options

1. **Vendor `git-filter-repo`'s script into this repo.** Rejected outright: CLAUDE.md's "Things that look like shortcuts and are not allowed" explicitly names "Vendoring a Python or Node dependency."
2. **Reimplement the relevant history-rewriting logic in Rust inside `wkp-git`.** Rejected: `git-filter-repo` is exactly the kind of thing CLAUDE.md's slim-core priority says to reuse rather than reimplement ("Reuse git, SQLite, OpenSSH and OS facilities... do not add a dependency that reimplements something one of those already exists"), and history rewriting has enough sharp edges (reflog, packed-refs, replace refs, submodules) that a narrower from-scratch version would be a real security/correctness liability for a comparatively rare, already-destructive operation.
3. **Require `git-filter-repo` as an external, OS-installed dependency, invoked via `git filter-repo` the same way every other `wkp-git` function shells out to git.** Chosen. This workspace already accepts `git` itself, `ssh-keygen`, and (for revoked-recipient tests) OS keystore daemons as external, not-vendored dependencies of the same shape.

## Decision

`crates/wkp-git/src/purge.rs` shells out to `git filter-repo` (which requires `git-filter-repo` on `PATH`, matching how git resolves any `git-<name>` subcommand) via the crate's existing `run_git` plumbing -- no new call site outside `crates/wkp-git`, no vendored copy in this repo. A missing installation is detected by matching git's own "is not a git command" stderr and turned into an actionable error naming the upstream project and install commands for common package managers, rather than surfacing a raw dispatch failure.

CI (a separate PR, ahead of this feature per CLAUDE.md's rule against combining a `.github/workflows/*` change with feature code) installs `git-filter-repo` via `apt-get` on the `fmt, clippy, test` job's Ubuntu runner, so this crate's own tests exercise the real tool rather than skipping or mocking it. A contributor running this workspace's tests locally without `git-filter-repo` installed will see `wkp-git`'s `purge` tests fail with that same actionable error message, not a mysterious one.

## Consequences

- `wkp purge` does not work out of the box on a machine without `git-filter-repo` installed -- this is a real, user-visible requirement, documented in the error message itself rather than only in a README a user might not read before hitting it.
- This workspace's own CI and any contributor's local dev setup both need this one extra system package; `CLAUDE.md`'s "Commands" section should eventually note it as a prerequisite alongside git itself (not done in this PR -- a documentation-only follow-up, not a functional gap).
- Hub-side purge honoring (M5: deleting unreachable objects and backups on a retention schedule after a client-side purge) inherits this same dependency if it ever needs to run `git-filter-repo` server-side too; out of scope for this task, noted here so M5 doesn't rediscover the same vendoring question from scratch.
