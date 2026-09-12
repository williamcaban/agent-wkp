# Branch protection for `v2-rust` (M0-2)

Issue #2 asks for these settings on `v2-rust`. **Not yet applied** -- this
document is the spec for someone with repo admin rights to apply by hand
(GitHub's branch-protection API/UI, not something CI or this repo's own
code can safely do to itself). As of this writing `v2-rust` has **no**
branch protection at all (`gh api repos/<org>/agent-wkp/branches/v2-rust/protection`
returns 404).

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
  own stance against `--admin`-style bypasses.
- Disallow force pushes and branch deletion on `v2-rust`.

## Why this isn't applied yet

This session has standing authorization to self-merge PRs once CI is
green, established for routine work on this branch. Applying "Require
review from Code Owners" immediately would contradict that arrangement
for every future CODEOWNERS-gated PR (`crates/wkp-hub/`, `deploy/`,
`.github/`, etc. -- most of the paths this session actually works in) --
a real, deliberate policy change, not a file edit, so it needs the human
running this repo to decide when to flip it on, not something to apply
silently as a side effect of closing out issue #2's checklist.
