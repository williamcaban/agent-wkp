# ADR-0001: Minimum supported git version

Status: accepted
Date: 2026-09-07
Design sections affected: 5.1 ("Why shell out to `git`...", "Change detection: the index stat cache, not per-file hashing")

## Context

Design 5.1 already commits to a hard dependency on `git >= 2.34` because SSH
commit signing (`gpg.format = ssh`, `gpg.ssh.allowedSignersFile`) is not
available before that release — that line is cited and not in question here.

What the design leaves as **[unverified]** is the builtin fsmonitor daemon:
"macOS and Windows since git 2.37; Linux builtin support is newer and must be
verified against the release notes of the minimum supported git version." M0
task 3 asks us to close that out, decide an actual minimum version, and make
`wkp` refuse to run below it with a clear message.

## Research

Checked directly in this session (git-scm release notes, the `git-fsmonitor--daemon`
manual page, and the `actions/runner-images` software manifests for the CI
matrix M0 task 6 will use):

- **SSH signing** (`gpg.format = ssh`): added in git **2.34.0** (November
  2021). This is the actual floor — every write in M2 depends on it.
- **Builtin fsmonitor daemon** (`git fsmonitor--daemon`, `core.fsmonitor = true`
  without a third-party hook): added in git **2.37.0** (June 2022), but that
  release's Linux backend was a stub — inotify support for the daemon did not
  land until **git 2.55** (2025). Before 2.55, `core.fsmonitor=true` on Linux
  either silently does nothing useful or must fall back to the old
  hook-script mechanism. macOS and Windows had working FSEvents/ReadDirectoryChangesW
  backends from 2.37 onward.
- Both GitHub-hosted `ubuntu-latest` (24.04) and `macos-15` runner images
  currently ship git **2.55.0** — comfortably past both floors — but a
  contributor's local Fedora, Debian, or older macOS install is not
  guaranteed to be. Fedora 43 (this session's environment) ships 2.55.0;
  Ubuntu 22.04/24.04's *distro-packaged* git (not the GitHub Actions image) is
  materially older, and RHEL/Debian stable track further behind still.

The practical consequence: if `wkp`'s minimum version were set to 2.55 to
guarantee fsmonitor works everywhere, it would exclude a large fraction of
real Linux installs for a change-detection *optimization*, not a correctness
requirement. M1 task 3 already anticipates this — its own wording is
"fsmonitor when available" — so the design's intent was always opportunistic
use, not a hard requirement. The `[unverified]` tag was about closing the gap
in the *citation*, not about raising the floor.

## Options

1. **Minimum = 2.34, fsmonitor opportunistic.** Keep the SSH-signing floor as
   the only hard gate. At runtime, `wkp` may turn on `core.fsmonitor` when the
   installed git's builtin daemon is expected to work (platform + version
   check, or simply attempt it and fall back silently), and always has the
   git index stat-cache path (`update-index --refresh` + `status --porcelain=v2`)
   as the baseline that works on every supported version. Helps slim core and
   harness neutrality: no user is turned away over an optimization. Failure
   mode: change detection on an old-git Linux box degrades from
   fsmonitor-fast to stat-cache-fast, never to full re-hash; still meets the
   4.3 latency targets per the design's own framing of fsmonitor as the
   "main lever," not the only one.
2. **Minimum = 2.55, to guarantee fsmonitor everywhere.** Simpler runtime
   logic (no capability probing), but excludes a large population of
   contributors and users on stable/LTS distros for a performance feature,
   directly against harness-neutrality and the "install with your OS
   facilities" spirit of the slim-core priority. Rejected.
3. **Minimum = 2.37, split the difference.** Gets the SSH-signing floor and
   fsmonitor's initial (macOS/Windows-only) release, but still leaves Linux
   users on 2.37–2.54 believing fsmonitor works when the daemon silently
   provides no benefit on their platform. Doesn't remove the need for a
   runtime capability check anyway (Linux still needs one up to 2.55), so it
   buys nothing over option 1 except a needlessly higher floor. Rejected.

## Decision

`wkp`'s minimum supported git version is **2.34.0**, enforced at startup by
`wkp-git::ensure_min_git_version()`: `wkp` shells out to `git --version`,
parses the `major.minor.patch` triple, and refuses to run any subcommand
(except `--version`/`-V`, which never touches git) with a clear message
naming the found and required versions when it is below the floor, git is
missing from `PATH`, or the version string can't be parsed.

fsmonitor is explicitly **not** part of the minimum-version gate. It remains
an opportunistic optimization to be wired up in M1 task 3 ("fsmonitor when
available"), where the "available" check accounts for the Linux-since-2.55
gap documented above. This ADR does not decide *how* that capability probe
is implemented (attempt-and-fall-back vs. version+platform table) — that is
left to the M1 task.

## Consequences

- `docs/design/wkp-hub-design-v0.1.md` section 5.1's `[unverified]` tag on
  the fsmonitor sentence is resolved via a changelog note pointing here,
  per the "small clarifications go into the design doc directly" rule in
  `CLAUDE.md`; the sentence itself is left otherwise intact since its
  substance (macOS/Windows since 2.37) was already correct.
- `wkp-git` gains a small, CODEOWNERS-gated, human-reviewed surface: a
  version-string parser and a startup gate. This is genuinely new logic
  (not glue), so it carries unit tests for the parser (plain version,
  platform-suffixed version like `2.39.3 (Apple Git-146)`, two-component
  version, unparseable input) rather than being trusted untested.
  `#![forbid(unsafe_code)]` continues to apply.
  M2's plumbing wrapper work builds on this module rather than replacing it.
- M1 task 3 must implement the fsmonitor capability probe honoring the
  2.55 Linux gap; a stale assumption that "fsmonitor since 2.37" is
  cross-platform would silently disable the optimization on Linux, not
  break correctness, but it should not ship as a documented-wrong comment.
- No CI gate enforces the minimum-version number itself (it's a runtime
  check against the user's environment, not the build environment), but
  `wkp-git`'s parser tests run in the normal `cargo test --workspace` gate
  from M0 task 2's CI baseline.
