# ADR-0007: `wkp forget` is two operations, not one

Status: accepted
Date: 2026-09-09
Design sections affected: 7.6

## Context

M4-5 (`docs/plan/milestones.md`, issue #77) implements design 7.6's single sentence: "`wkp forget <path>` removes the file and, for private items, rotates the age recipients so old ciphertext becomes unreadable to revoked devices." Read literally, one command call does two things: delete one memory item, and revoke a *device's* access to every other still-existing private item. Those don't share an input (a file path vs. a device id), don't share an audit story (one commit removing one file vs. one commit re-encrypting many), and there is no scenario where a caller wants both at once from a single invocation -- "I am deleting this note" and "this device is no longer trusted" are different events that happen to both be filed under design 7.2/7.6's encryption story.

## Options

1. **One `wkp forget <path>` that infers intent from its argument.** Rejected: conflates two different blast radii (one file vs. every private file in the tree) behind one name, and there is no natural path-shaped argument for "revoke a device" -- it would need a special sentinel or a second, incompatible argument shape anyway.
2. **Two subcommands under one name**: `wkp forget <path>` for item deletion, `wkp forget --device <id>` for revocation, mutually exclusive. What this ADR chooses -- keeps the single `forget` verb design 7.6 uses, but the two operations are distinct code paths with distinct inputs, distinct commits, and can be tested and reasoned about independently.
3. **Two separate top-level commands** (`wkp forget` / `wkp revoke-device`). Rejected only because design 7.6 already names `forget` as the umbrella term or a future document should split it out. Not important. Kept as one entry point under 7.6's namesake.

## Decision

`wkp forget <path>` and `wkp forget --device <id>` are two distinct operations sharing one CLI verb, exactly as suggested in `milestones.md`'s own scope note for this task.

**`wkp forget <path>`**: removes one item from the current tree with a human-signed commit (`wkp-git`'s `signed_removal_commit`, the deletion counterpart to `signed_commit`). No recipient rotation. This is `wkp promote`'s identity model, but stricter: unlike promote, there is no `[promote] auto` -- style escape hatch for agent principals. Deletion is destructive in a way promotion is not (promotion just moves already-durable content sideways; forgetting removes it from the current tree entirely), so it always requires `role: human`.

**`wkp forget --device <id>`**: revokes one device's registered recipient entry and re-encrypts every currently-tracked `visibility: private` file to the resulting (smaller) recipient set, in one signed commit alongside the updated `recipients` file. Also requires `role: human` -- revoking a device's access is a security decision, not routine content maintenance. Re-encryption reuses the existing clean filter machinery (`wkp filter clean`, M4-4) rather than calling `wkp-crypto`'s `encrypt`/`decrypt` directly from this new code path: `signed_removal_commit`'s sibling, `signed_commit`, already stages new content via `git hash-object`, which applies the `*.md filter=wkp-crypt` clean filter exactly the way a real `git add` would -- so updating the `recipients` file on disk *before* staging each private file's current (plaintext, working-tree) content is sufficient to have it re-encrypted to the new set, with no new crypto call site to review. Refuses if revoking would leave the store with zero recipients at all (device or recovery) -- a real, easy-to-hit foot-gun (a single-device store revoking its only device) that would make every future private write unencryptable to anyone, not a hypothetical.

Explicitly **not decided here, deferred to M4-6 (`wkp purge`)**: this never touches already-committed history. A revoked device's ciphertext from *before* the revocation commit is still recoverable by anyone who can read the git history and still holds that device's identity -- design 7.6 itself splits this into "rotate recipients" (this task) vs. "true history erasure" (`wkp purge`), and this ADR does not blur that line.

## Consequences

- `wkp forget <path>` and `wkp forget --device <id>` are mutually exclusive in one invocation; the CLI parser rejects both being given together, and rejects neither being given.
- `wkp-git::signed_commit` gains a `signed_removal_commit` sibling (shared internal tree/commit/update-ref helper, no change to `signed_commit`'s existing signature or behavior).
- `wkp-crypto::recipients` gains a `remove` function symmetric to `append`.
- Revocation's "old ciphertext becomes unreadable" guarantee applies only to ciphertext created *after* the revocation commit going forward from that point in history -- it does not and cannot reach back into already-committed blobs. `wkp purge` (M4-6) is the only path to that stronger guarantee, and is explicitly out of scope here per the issue text.
- A store can end up with private content that predates a revocation still being decryptable by the revoked device if that device already has a stale local clone/bundle from before the revocation -- inherent to how git and age both work (neither can reach into a device that already has the bytes), not something this task's tests can meaningfully assert against; the mitigation is `wkp purge` plus revoking network/hub access separately (M5), not this command.
