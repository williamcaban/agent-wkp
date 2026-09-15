# ADR-0002: How `wkp index` writes incremental changes to `index.db`

Status: accepted
Date: 2026-09-07 (decided 2026-09-13, issue #29)
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

**Option 2, accepted 2026-09-13 (issue #29).** `index.db` is a named
exception to CLAUDE.md's temp-and-rename hard rule: `wkp_core::index::store::update_index`
now opens `dest` directly and applies deletes/upserts/edge-recomputation
inside one `BEGIN IMMEDIATE` ... `COMMIT` transaction, relying on SQLite's
own transaction log for the same "a reader never sees a torn file, a crash
rolls back cleanly" guarantee the temp-and-rename pattern exists to provide
for a plain file with no engine underneath it. `tier0.md` and any future
flat file a harness reads directly keep the unconditional temp-and-rename
treatment — this exception is scoped to `index.db` specifically, per
CLAUDE.md's updated wording.

**A second, unanticipated finding, made while verifying the fix actually
closes the gap:** removing `VACUUM INTO` alone was not sufficient. Local
benchmarking (`cargo bench -p wkp-core incremental_update_50k_corpus`,
instrumented per-phase to find where the remaining time went) showed the
in-place transaction still cost ~365-390ms for a 10-item change — barely
better than `VACUUM INTO`'s ~460ms. The actual dominant cost was
`delete_item`'s two `DELETE ... WHERE path = ?1` statements against
`items` and `items_trigram`, both FTS5 virtual tables whose `path` column
is declared `UNINDEXED` (excluded from the full-text index specifically so
it isn't tokenized/searched, which also means SQLite has no index to use
for an equality lookup against it) — each delete was a full 50,000-row
table scan (~28ms and ~17ms respectively, ×10 changed items ≈ the entire
measured cost; `paths`' own `DELETE ... WHERE path = ?1`, a real B-tree
primary key, took microseconds by comparison). This cost exists regardless
of whether the surrounding write mechanism is `VACUUM INTO`, a fresh
temp-file build, or an in-place transaction — it is intrinsic to deleting
FTS5 rows by an unindexed column, and would have limited *any* of this
ADR's four options equally once real per-row deletes were exercised at
corpus scale.

**Fix**: `insert_item` now assigns each row the same rowid `paths` (a real
table, indexed by its `TEXT PRIMARY KEY` on `path`) already auto-assigns it
via `conn.last_insert_rowid()`, using FTS5's support for explicit rowid
insertion (`INSERT INTO items (rowid, path, ...) VALUES (?1, ?2, ...)`).
`delete_item` looks up the rowid via `paths`' indexed `path` column, then
deletes from `items`/`items_trigram` by rowid — an indexed, effectively
O(1) operation — instead of by the unindexed `path` column.

**Result** (measured locally, `cargo bench -p wkp-core
incremental_update_50k_corpus`, same 50k fixture / 10-item change as the
original ~460ms measurement): **~2.9-4ms**, comfortably inside design 4.3's
p50 < 30ms / p95 < 100ms target — a ~99% reduction from the original
`VACUUM INTO` baseline. `benches/baseline.json` is updated from a real CI
run (not this local number) per this repo's own documented baseline-capture
practice.

## Consequences

- `CLAUDE.md`'s hard rule now scopes "never write in place" to flat files a
  harness reads directly (`tier0.md`, future tier files), explicitly
  carving out `index.db` as SQLite-transaction-safe instead.
  `wkp_core::index::store::update_index` opens `dest` directly and wraps
  its changes in a `BEGIN IMMEDIATE` transaction.
- The existing atomicity test (`failed_update_leaves_previous_index_db_untouched`)
  stays (a corrupt-dest failure still can't corrupt `dest` further), and a
  new test (`update_index_rolls_back_cleanly_when_it_cannot_acquire_the_write_lock`)
  forces a genuine failure to acquire `dest`'s write lock (a second
  connection holds `BEGIN IMMEDIATE` open) and confirms the previous
  content survives untouched and stays queryable — the "mid-transaction
  failure" case this ADR originally flagged as needed.
- `insert_item`/`delete_item` now depend on `paths`' auto-assigned rowid
  being explicitly mirrored onto `items`/`items_trigram` — any future code
  that inserts into those FTS5 tables directly (bypassing `insert_item`)
  must preserve this invariant or `delete_item`'s rowid-based deletes will
  silently stop finding the row to delete.
- `crates/wkp-core/benches/core_benches.rs`'s `incremental_update_50k_corpus`
  bench and its `benches/baseline.json` entry stay in the tree as the
  regression gate going forward — updated from the ~460ms `VACUUM INTO`
  baseline to the new in-place/rowid-delete number, captured from a real CI
  run per this repo's own documented baseline-capture practice.
