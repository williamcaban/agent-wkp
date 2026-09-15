# ADR-0006: Encryption filter failure policy — `required=true`, asymmetric clean/smudge behavior

Status: accepted
Date: 2026-09-08
Design sections affected: 7.2, 5.1

## Context

M4-4 (`docs/plan/milestones.md`, issue #76) registers `wkp` as a git clean/smudge filter driver for `visibility: private` content. Before writing any Rust, git's actual filter-failure protocol was rehearsed by hand (the pattern that has paid off for every prior git-plumbing task this milestone): a throwaway repo with `filter.<name>.clean`/`.smudge` set to deliberately-failing shell commands (`false`), with and without `filter.<name>.required`.

**Finding, confirmed empirically, not assumed from docs:** by git's *default* behavior (`required` unset/false), a failing `clean` filter does not abort `git add`/`git commit` — git logs a warning to stderr and silently falls back to committing the **raw, unfiltered input**. For this feature, that means: if `wkp filter clean` ever fails while processing a `visibility: private` file (missing recipients file, zero recipients, an encryption error), git would silently commit the **unencrypted plaintext** into history with only a stderr warning and exit code 0 — a severe, silent defeat of the entire feature's purpose. This is not a hypothetical; the exact commit content was verified with `git cat-file -p HEAD:a.md` after triggering the failure.

Setting `filter.<name>.required = true` fixes this: a failing `clean` now makes `git add` exit 128 and leaves the path unstaged, and the resulting `git commit` correctly excludes it — verified by hand the same way, including confirming `HEAD:a.md` genuinely does not exist after the failed attempt.

**But `required=true` has a real cost on the `smudge` side**, also confirmed by hand: a failing `smudge` on `required=true` doesn't just leave that one file un-smudged (git's default-`required` behavior for smudge failure: write the raw blob content, i.e. ciphertext, to the working tree, warn, and continue) — it aborts the **entire** `git checkout`/`git clone` operation with exit 128, including files unrelated to the failing one. For this feature, that means a device that isn't yet a registered recipient for even one private file would fail to check out *anything* — a much worse failure mode than "one file is unreadable ciphertext, everything else works."

## Options

1. **`required=true` uniformly.** Protects `clean` (the security-critical direction) but makes `smudge` failure catastrophic for the whole checkout on any device missing access to even one file — rejected as too disruptive for what design 7.2 frames as an expected, graceful-degradation scenario ("a device without a clone gets search only for shared items"; losing access to specific private content on a specific device, gracefully, is an accepted posture, not an outage).
2. **`required` unset (git's default) uniformly.** Protects `smudge` (graceful per-file degradation) but leaves `clean` able to silently leak plaintext into history on any encryption failure — rejected outright; this is the exact severe failure mode found above, and directly contradicts CLAUDE.md priority 2 (security).
3. **`required=true`, combined with `wkp filter smudge` itself never exiting non-zero.** `wkp_git.filter.wkp-crypt.required=true` is set unconditionally, so any real `clean` failure hard-blocks the commit as in option 1. Independently, `wkp filter smudge`'s own implementation is written to never propagate a decrypt failure as a process failure: on any error obtaining a device identity or decrypting (no keystore/file identity available, this device not among the ciphertext's recipients, corrupted ciphertext), it prints a clear warning to stderr and writes the **original ciphertext bytes** to stdout unchanged, then exits 0. Since `required` only changes behavior on a *non-zero* exit, and this implementation is designed to never produce one, git's harsher required-mode checkout-aborting behavior is never triggered by `smudge` at all — the checkout succeeds for every file, with private content this device can't yet decrypt simply left as visible ciphertext (a truthful, non-destructive signal, not a silent failure) instead of readable plaintext. Chosen.

## Decision

`wkp init`'s filter wiring sets `filter.wkp-crypt.required = true` in local git config, alongside `.clean`/`.smudge`. `wkp filter clean` propagates every real failure (unreadable recipients file, zero recipients, an age encryption error) as a non-zero exit — combined with `required=true`, this makes git refuse to stage or commit the file at all rather than fall back to raw plaintext. `wkp filter smudge` is written to **never** exit non-zero: every failure path (no device identity obtainable, decrypt fails) instead writes the untouched ciphertext bytes to stdout with a stderr warning and exits 0, so `required=true`'s harsher enforcement is structurally never exercised on the smudge side.

## Consequences

- A misconfigured store (no `recipients` file, or one with zero entries) cannot accidentally commit plaintext for a `visibility: private` item — `git add`/`git commit` fail loudly instead.
- A device without access to a given private file's content still gets a fully successful `git checkout`/`git clone` for everything else; that one file's working-tree content is visibly ciphertext (starts with `age-encryption.org/v1`, not readable) rather than a checkout failure or a silently-wrong result.
- This makes `wkp filter smudge`'s exit code alone an unreliable signal of "did decryption actually succeed" — a caller that needs to know must inspect the output content itself (does it still look like age ciphertext) rather than trust the exit code. `wkp_core`'s indexer (existing code, not part of this task) already treats unreadable/undecryptable content as ordinary opaque bytes, so this doesn't need new handling there, but a future task that wants to *report* "this file is still encrypted on this device" should check content, not exit status.
- Enforced by this task's own integration tests: a real failing-clean scenario (recipients file missing) must leave `HEAD` without the plaintext; a real smudge-without-identity scenario must leave the checkout fully successful with ciphertext bytes in the working tree.
