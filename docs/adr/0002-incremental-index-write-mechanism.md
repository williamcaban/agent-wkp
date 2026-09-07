# ADR-0002: How `wkp index` writes incremental changes to `index.db`

Status: proposed
Date: 2026-09-07
Design sections affected: 4.2 ("Atomic apply on the read path"), 4.3 (latency targets), 5.1 (change detection)

## Context

CLAUDE.md's hard rule: "Files a harness reads (`tier0.md`, `index.db`) are
written to a temp file in the same directory and renamed into place. Never
write them in place." Design 4.2 gives the same rule as "Atomic apply on the
read path": a reader must never see a torn file.

M1-3 (change detection) needs an incremental write path: after the first
full index build, most `wkp index` runs touch a handful of files out of a
much larger corpus, and re-inserting every unchanged item on every run would
reintroduce the O(corpus) cost the whole point of M1-3 is to avoid (design
5.1: this is exactly what made the old Python tool's per-file
`git hash-object` approach slow).

The implementation that follows the hard rule literally: clone the current
`index.db` into a fresh temp file with SQLite's `VACUUM INTO` (a read of the
existing file, not a write to it), apply the incremental `DELETE`/`INSERT`
statements to that copy, then `rename(2)` the copy over the original. This
is `wkp_core::index::update_index` as merged.

**The problem:** `VACUUM INTO` copies and compacts the *entire* database
file regardless of how many rows changed. Benchmarked on the 50k-item
fixture corpus (`crates/wkp-core/benches/core_benches.rs`,
`incremental_update_50k_corpus/10_changed`, `ubuntu-latest`, tmpfs, 20
samples): **461.88 ms mean**, updating only 10 of 50,000 items (a second,
separate CI run measured 383.04 ms — the exact figure moves with ordinary
shared-runner variance, see `benches/README.md`'s own documented incidents
on that; the order of magnitude does not). Design 4.3's
target for "incremental index check" is p50 < 30 ms / p95 < 100 ms — this
implementation misses it by roughly an order of magnitude at realistic
corpus scale, and the miss gets worse as the corpus grows, since the copy
cost scales with total corpus size, not change count.

This is a design-vs-reality conflict per CLAUDE.md's own instructions ("do
not silently deviate... open an ADR"), not a bug to quietly patch: the hard
rule and the latency target cannot both hold for a naive "clone the whole
file" implementation of the rule.

## Options

1. **Keep `VACUUM INTO` (current implementation). Accept the latency miss
   for now, revisit if it blocks real usage.** Simplest, most literally
   compliant with the temp-and-rename rule, zero new risk. Cost: fails
   design 4.3's own stated target by ~10x at 50k items, and the gap widens
   as stores grow — this is the milestone whose whole exit criterion is "the
   author's real store" working end to end, so a slow `wkp index` on a
   large personal store is a real user-facing problem, not a hypothetical
   one.
2. **Write `index.db` in place inside a real SQLite transaction
   (`BEGIN IMMEDIATE` / incremental `DELETE`+`INSERT` / `COMMIT`), no
   temp-and-rename for this file.** SQLite's own transaction log already
   guarantees a reader never observes a torn write and a crash mid-write
   rolls back cleanly on next open — the exact property the temp-and-rename
   rule exists to provide for a *plain* file like `tier0.md`, which has no
   such engine underneath it. Cost proportional to changed rows, not corpus
   size; should comfortably clear the 4.3 targets, pending its own
   benchmark. This is an explicit, scoped exception to the hard rule for
   `index.db` specifically: `tier0.md` (and any future flat file a harness
   reads directly) keeps the temp-and-rename treatment unconditionally,
   since a plain file has no internal atomicity to lean on. Risk: a reader
   holding the file open during a `BEGIN`/`COMMIT` window sees SQLite's own
   locking behavior (a brief write lock, not a torn read) rather than "old
   version until an atomic swap, new version after" — a real behavioral
   difference from the current promise, though not a correctness one, and
   `wkp search` opening the db file fresh per process (design 4.2: no
   daemon) limits how long any one reader holds it open.
3. **Use SQLite's incremental backup API (`sqlite3_backup_step`) to copy
   only changed pages instead of `VACUUM INTO`'s full copy+compact, still
   into a temp file, still renamed.** Keeps the temp-and-rename guarantee
   exactly as written; `rusqlite`'s `backup` module exposes this. Cost:
   backup-by-pages still touches every page `VACUUM INTO` would for a
   B-tree/FTS5 structure with scattered writes (FTS5's internal segment
   merges can touch pages across the whole index even for a small content
   change), so the actual speedup versus `VACUUM INTO` is unproven and
   needs its own benchmark before trusting it — not evaluated in code for
   this ADR, flagged as the fallback to try if option 2 is rejected.
4. **Filesystem-level copy-on-write clone (`reflink`) instead of
   `VACUUM INTO` for the initial copy step, keeping everything else the
   same.** Near-instant on btrfs/XFS/APFS regardless of file size. Rejected
   outright: silently falls back to a full copy on ext4 (still the most
   common Linux filesystem) and requires either a new dependency or
   platform-specific `ioctl` code wkp-sys would have to own, for a gain
   that isn't portable to the filesystems most users actually run on.

## Decision

**Not yet made — this ADR is filed as `proposed`, not `accepted`, per
CLAUDE.md's instruction to flag the conflict rather than resolve it
unilaterally when it touches a hard rule.** The M1-3 PR ships option 1 (the
rule-compliant, benchmarked-slow `VACUUM INTO` implementation) as the
working default, with this ADR linked from the PR description and the
benchmark's own doc comment, so the gap is visible rather than hidden. A
human reviewer should pick between option 1 (accept the miss, revisit
later), option 2 (the recommended fix: scope the temp-and-rename rule's
"never write in place" clause to flat files, not SQLite databases, which
carry their own atomicity), or option 3 (measure the backup API before
trusting it).

**Recommendation, for the reviewer's benefit, not asserted as decided:**
option 2. The temp-and-rename rule's own stated purpose (design 4.2: "a
reader never sees a torn file") is already satisfied by SQLite's engine for
`index.db`; applying the same rule built for `tier0.md` (a plain markdown
file with no internal transactional guarantee) to a SQLite database imposes
a real, benchmarked, order-of-magnitude latency cost to protect against a
failure mode — a torn write — that the database engine underneath it
already prevents on its own.

## Consequences

- If option 2 is accepted: `CLAUDE.md`'s hard rule needs a one-line
  amendment scoping "never write in place" to flat files a harness reads
  directly (`tier0.md`, future tier files), explicitly carving out
  `index.db` as SQLite-transaction-safe instead. `wkp_core::index::update_index`
  is rewritten to open `dest` directly and wrap its changes in a
  transaction; the existing atomicity test
  (`failed_update_leaves_previous_index_db_untouched`) needs a new version
  asserting the same property (previous content survives a failed update)
  under the new mechanism, likely by forcing a mid-transaction error and
  checking the file rolls back rather than checking byte-identity of an
  unrelated file (`VACUUM INTO`'s failure mode doesn't apply once `dest` is
  opened directly).
- If option 1 stands: design 4.3's incremental-index target needs either a
  documented exception for large corpora, or a follow-up issue tracking the
  gap, so it doesn't quietly become a target nobody is measuring against
  anymore.
- Either way, `crates/wkp-core/benches/core_benches.rs`'s
  `incremental_update_50k_corpus` bench and its `benches/baseline.json`
  entry stay in the tree as the regression gate for whichever mechanism is
  chosen — the current baseline (~460 ms) exists specifically so a *further*
  regression on top of the already-known-slow path is still caught, and so
  a fix's improvement is measurable against a real prior number.
