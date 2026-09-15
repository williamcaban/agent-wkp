# ADR-0013: `v2-rust` to `main` cutover timing and post-cutover branch model

Status: accepted
Date: 2026-09-13
Design sections affected: 3.3 (repo structure), 9.1/9.2 (CODEOWNERS, branch protection)

## Context

`main` is deliberately frozen `v0-python` history (M0 task 4 tagged it
`v0-python` before removing the Python tree on `v2-rust`); every milestone
since has merged to `v2-rust` instead. `milestones.md`'s own intro line
("done when its exit criterion holds on `main`") was never updated to match
that practice, which reads as an inconsistency rather than a decision.

Verified in this session:

- `main`'s current tip has **zero net content diff** from the `v2-rust`/`main`
  merge-base (`git diff 7a65e8e main --stat` is empty). A PR (#104) meant for
  `v2-rust` was accidentally merged straight to `main` on 2026-09-09 and
  reverted the same day (`backup/main-before-m5-4-fix` tag preserves the
  pre-revert state) -- net no-op, but it happened because `main`'s branch
  protection has `enforce_admins: false` (an owner can bypass required
  checks).
- `main`'s branch protection today requires the old Python CI's `test (3.12)`
  status check plus one approving review. That check can never post once
  `main`'s tree is replaced with the Rust workspace (no `ci.yml` on that
  side) -- moving `v2-rust`'s content onto `main` without first updating the
  required-checks list would deadlock every subsequent PR into `main`.
- `docs/plan/branch-protection.md` already specs the required Rust-CI checks
  and CODEOWNERS-review policy, but written against `v2-rust`, not `main`.
- M6 ("Hardening and release": Landlock/seccomp, cosign-signed reproducible
  releases with SLSA provenance and SBOM, Homebrew tap, OCI image, Scorecard
  gate, self-update signature verification) is the milestone whose own theme
  is "release" -- the natural gate for the branch that becomes the public,
  contract-bearing one per `CLAUDE.md`'s "frontmatter fields and CLI flags
  are a public contract once merged to `main`."

## Options

1. **Cut over now.** Mechanically trivial (empty diff, no conflicts) but
   ships a codebase with no signed releases, no SBOM, no sandboxing --
   directly contradicts M6's own exit criterion existing at all, and makes
   `main`'s "public contract" claim true before the hardening it implies is
   actually done.
2. **Cut over after M6's exit criterion holds** (this decision). Keeps
   `main` = frozen `v0-python` until the Rust build has the release
   hardening M6 defines. Requires reconciling the two no-op commits and
   updating `main`'s branch protection as a prerequisite step, not
   simultaneous with the content swap.
3. **Never formally cut over; keep `v2-rust` as the permanent working
   branch.** Leaves `milestones.md`'s own intro text permanently wrong and
   `main` permanently stale, with no path to `main` ever becoming the real
   default-branch content a fresh clone gets. Rejected -- undermines the
   point of `main` being the default branch at all.

## Decision

The `v2-rust` -> `main` cutover happens **after M6's exit criterion holds**,
not before. Prerequisite, sequenced as its own tracked task (**issue #161**):
reconcile the two no-op commits on `main` (a trivial merge, `git diff` is
already empty), retarget `docs/plan/branch-protection.md`'s required-checks
list from `v2-rust` to `main` and apply it before the content swap (so the
old `test (3.12)` check is never a required check with no way to satisfy
it), then merge `v2-rust`'s tree into `main` via a normal protected-branch
PR -- no force-push, no `enforce_admins` bypass.

After cutover, **`v2-rust` is retired** (deleted or archived after a grace
period) rather than kept as a permanent parallel working branch. Once
`main` carries the Rust workspace, future work branches directly from and
merges directly to `main`, matching a normal single-trunk repo -- there is
no ongoing reason for two long-lived branches once the one that was
"frozen old history" no longer differs in kind from the one under active
development.

Explicitly not decided here: the exact release version/tag main's new tip
gets, whether a grace period exists before deleting `v2-rust` (vs. deleting
it in the same PR that completes the cutover), and CODEOWNERS-required-review
enforcement's exact rollout (still flagged in `docs/plan/branch-protection.md`
as a human-timed decision, independent of this ADR).

## Consequences

- `main`'s branch protection must be updated *before* the content swap, or
  the swap PR itself cannot merge (`test (3.12)` would be a required check
  with nothing left to satisfy it).
- `docs/plan/milestones.md`'s M6 section gets a tracked task (issue #161)
  for this cutover, resolving the stale "done ... on `main`" intro wording
  as intentional-but-not-yet-true rather than an unexplained inconsistency.
- Until M6 lands, `main` continues to serve only the frozen `v0-python`
  tree; nothing about this ADR changes `v2-rust`'s day-to-day workflow.
- Issue #4's PyPI-tombstone half (yanking/retiring the old PyPI package)
  remains a separate, human-only action -- related to this cutover in
  spirit (both retire the Python-era artifacts) but not a dependency of it
  either direction.

## Addendum (2026-09-15, William): cutover no longer waits on M6's remaining tasks

**Status: this ADR's original "Decision" section above is superseded on
timing only** -- the cutover-mechanics steps it specifies (branch
protection before content swap, reconcile the two no-op commits, normal
protected-branch PR, no force-push/`enforce_admins` bypass, retire
`v2-rust` after) are unchanged and still followed exactly.

What changes: at the time of this addendum, M6's exit criterion does
**not** fully hold -- issues #171 (macOS sandboxing) and #177 (self-update
signature verification) are deferred (2026-09-14, see their own
`milestones.md` entries), and #175 (Homebrew tap) had not been started.
William directed doing the `main` cutover now rather than waiting for all
three, so that #175's Homebrew tap can be built and its release cut
against `main` as the branch a fresh clone actually gets, instead of
against soon-to-be-retired `v2-rust`.

Revised decision: `v2-rust` -> `main` proceeds now, on the original
ADR-0013 mechanics. #171, #175 and #177 become tracked work against `main`
post-cutover instead of pre-cutover gates. `milestones.md`'s M6 section is
updated to record that its own exit-criterion wording ("sandbox on macOS
verified" / "self-update verifies signatures" / "Homebrew tap") does not
yet fully hold on `main` at cutover time, same as it did not hold on
`v2-rust` before this addendum -- the cutover changes which branch that
gap is tracked against, not whether the gap exists.

Also decided at this time (see issue #4): PyPI retirement (yanking
`0.1.x`/`0.2.0`, publishing the `0.3.0` tombstone) proceeds before the
Homebrew tap's release cut, so the tap becomes the tombstone's documented
successor channel from the start rather than a channel introduced before
the old one is marked retired. PyPI's actual publish/yank steps remain
human-only (issue #4's own note: they need the PyPI account) -- this
session can prepare the tombstone source and retirement doc but not
execute the PyPI-side actions itself.
