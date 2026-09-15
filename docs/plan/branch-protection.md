# Branch protection for `v2-rust` (M0-2), and for `main` at cutover (issue #161)

Issue #2 asks for these settings on `v2-rust`. **Not yet applied** -- this
document is the spec for someone with repo admin rights to apply by hand
(GitHub's branch-protection API/UI, not something CI or this repo's own
code can safely do to itself). As of this writing `v2-rust` has **no**
branch protection at all (`gh api repos/<org>/agent-wkp/branches/v2-rust/protection`
returns 404).

**`main`, by contrast, already has protection configured today** -- but
against the old Python CI: a required `test (3.12)` status check (from
`.github/workflows/ci.yml`, only present on `main`) plus one approving
review, with `enforce_admins: false` (an owner can bypass the required
check; this is how PR #104 landed directly on `main` by accident on
2026-09-09 before being reverted the same day). Per ADR-0013, the
`v2-rust` -> `main` cutover (issue #161) does not happen until M6's exit
criterion holds, but when it does, **this same required-checks list below
must be applied to `main` *before* the content swap**, replacing the
`test (3.12)` check -- otherwise the swap PR itself can never satisfy a
required check that has nothing left to produce it (no `ci.yml` on the
incoming tree). Everything below is written as the target state for
whichever branch is the live one at the time it's applied.

## Required status checks

From `.github/workflows/rust-ci.yml` (every job already gates PRs today,
just not enforced as *required* yet):

- `fmt, clippy, test`
- `audit, deny, vet`
- `bench (regression gate)`
- `cross-compile (x86_64-unknown-linux-musl)`
- `cross-compile (aarch64-unknown-linux-musl)`
- `cross-compile (aarch64-apple-darwin)`
- `semgrep (rust ruleset)`
- `hub HTTPS+mTLS integration test (M5-13)`
- `hub per-tenant pod isolation test (M5-7)`

From `.github/workflows/osv-scanner.yml`:

- `scan-pr` (the PR-triggered job; `scan-scheduled` only runs on
  push/schedule, not PRs, so it can't be a PR-required check)

**Not required**: `.github/workflows/scorecard.yml`'s `Scorecard analysis`
job. Scorecard is informational (repo posture over time), not a per-PR
correctness gate -- it doesn't test this PR's own diff the way the checks
above do.

Also enable: "Require branches to be up to date before merging."

## Reviews

"Require a pull request before merging", with "Require review from Code
Owners" -- enforces `.github/CODEOWNERS`'s existing path list
(`crates/wkp-crypto/`, `crates/wkp-git/`, `crates/wkp-hub/`,
`crates/wkp-sys/`, `deploy/`, `.github/`, `deny.toml`, `supply-chain/`,
`tests/injection-corpus/`, `docs/design/`) at the platform level instead of
by convention alone.

## Other settings

- "Do not allow bypassing the above settings" for admins, per CLAUDE.md's
  own stance against `--admin`-style bypasses. (`main`'s current
  `enforce_admins: false` predates this document and should be flipped to
  `true` at the same time the required-checks list below is applied to it.)
- Disallow force pushes and branch deletion on `v2-rust` (already the case
  on `main` today: `allow_force_pushes`/`allow_deletions` both `false`).

## Why this isn't applied yet

This session has standing authorization to self-merge PRs once CI is
green, established for routine work on this branch. Applying "Require
review from Code Owners" immediately would contradict that arrangement
for every future CODEOWNERS-gated PR (`crates/wkp-hub/`, `deploy/`,
`.github/`, etc. -- most of the paths this session actually works in) --
a real, deliberate policy change, not a file edit, so it needs the human
running this repo to decide when to flip it on, not something to apply
silently as a side effect of closing out issue #2's checklist.
