# Benchmarks

Criterion benchmarks for `wkp-core`, per design 9.5 ("Criterion benchmarks
for the operations in 4.3, with a regression threshold that fails the PR").

## Current state (M0 task 5)

Design 4.3 sets latency targets for cold `wkp search`, incremental index,
`wkp remember`, and `wkp materialize`. None of that exists yet — it lands
starting M1 (`docs/plan/milestones.md`). Benchmarking functions that don't
exist would mean either stub numbers that mean nothing, or premature
feature code that belongs in M1's own PRs, not this one.

Instead, this harness benchmarks the one real, load-bearing operation
available today: `support::generate_corpus` in
`crates/wkp-core/benches/support.rs`, which deterministically writes N
synthetic markdown+frontmatter files to disk. Every M1 bench (`wkp index`
cold and incremental, `wkp search`, `wkp materialize`) will need a fixture
corpus at realistic scale, so this is the actual shared dependency of all of
them, not a placeholder invented to have something to measure. Two sizes,
matching the "5k and 50k items" in the task:

- `fixture_corpus_generate_5k`
- `fixture_corpus_generate_50k` (fewer samples — `sample_size(10)` — since
  50k files/iteration is expensive; see `core_benches.rs`)

**When M1 lands real search/index/materialize functions**, add benches for
them here alongside (not instead of) the fixture-generation ones, reusing
the same corpora. Update `BENCHES` in `compare.sh` and re-run
`--update` to seed their baselines.

**M1-3 (change detection) added `incremental_update_50k_corpus`**: builds a
50k-item index once, then times `wkp_core::index::update_index` re-applying
a 10-item change against it — the steady-state `wkp index` scenario. This
measured ~460ms on `ubuntu-latest`, missing design 4.3's incremental-index
target (p50 < 30ms / p95 < 100ms) by roughly an order of magnitude, because
the current implementation's `VACUUM INTO` copies the whole file regardless
of change count. This is a real, documented design-vs-reality conflict, not
a regression to chase away with a threshold — see
`docs/adr/0002-incremental-index-write-mechanism.md` for the options and the
(not yet made) decision. The baseline entry for this bench exists to catch a
*further* regression on top of the already-known-slow path.

## Running

```bash
cargo bench -p wkp-core
./benches/compare.sh              # compare the run above against baseline.json
./benches/compare.sh --update     # rewrite baseline.json from the run above
```

`compare.sh` reads Criterion's `target/criterion/<bench>/new/estimates.json`
(`mean.point_estimate`, nanoseconds) for each bench listed in its `BENCHES`
map and compares it to the matching entry in `baseline.json`.

## Regression threshold

Default: **25%** over baseline, via `WKP_BENCH_THRESHOLD_PCT` (override for
one run, e.g. `WKP_BENCH_THRESHOLD_PCT=10 ./benches/compare.sh`). This is a
first cut, not a measured constant:

- It's wide enough to absorb noise from shared CI runners (no dedicated
  benchmark hardware yet), which the current fixture-generation benches
  already show some variance on even locally (single-digit percent between
  runs on this machine).
- It has not been tuned against real regressions vs. real noise, because
  there's no real operation to regress yet. **Revisit this number once M1's
  search/index/materialize benches exist** and there's enough run-to-run
  data to set it from evidence instead of a round number.

A bench with no entry in `baseline.json` is reported `NEW` and does not
fail the run — `--update` establishes a baseline for it once you're
satisfied with the numbers.

## Fixture corpus

`crates/wkp-core/benches/support.rs` is deliberately dependency-free: a
26-line inline xorshift PRNG instead of pulling in `rand`, and manual
`std::env::temp_dir()` handling instead of `tempfile` — fixture generation
for a benchmark harness doesn't need either, and CLAUDE.md requires
justifying every new dependency. `criterion` itself (dev-dependency only,
never shipped in the release binary) is the one new dependency this task
adds; its transitive dependency tree is recorded in
`supply-chain/config.toml` as `cargo vet` exemptions under the
`safe-to-run` criteria (dev/build-time only, not `safe-to-deploy`).

Corpus generation is deterministic for a given `(count, seed)` — reruns
produce byte-identical files — so benchmark numbers reflect the operation
being measured, not fixture-content variance.

## CI

`.github/workflows/rust-ci.yml`'s `bench` job runs `cargo bench -p wkp-core`
then `./benches/compare.sh` on every push/PR to `main`/`v2-rust`. A shared
GitHub-hosted runner is not dedicated benchmark hardware, so treat a
borderline `FAIL` as a prompt to re-run before assuming a real regression,
especially until M1's real operations replace these fixture-generation
placeholders.

**`baseline.json` must be captured on a GitHub-hosted runner, not a
developer machine.** The first version of this file was generated locally
and immediately failed CI: the GitHub-hosted `ubuntu-latest` runner was
~70-90% slower than the local dev box for both benches, blowing well past
even the generous 25% threshold — not a real regression, just different
hardware. To regenerate the baseline correctly, run the `bench` job's steps
in CI (or push a throwaway commit with `./benches/compare.sh --update`
added temporarily to the workflow), read the numbers from the run's log,
and commit them directly — don't `--update` from a local run and assume it
transfers.

**Second incident (2026-09-07): even a CI-captured baseline kept drifting
by 80-160% across unrelated PRs with zero bench-code changes.** Four
consecutive PRs (none touching `benches/`) failed this gate on different
`ubuntu-latest` allocations, with `fixture_corpus_generate_5k` measured at
93ms, 196ms, and 244ms for the *same* commit's code across three separate
runs. The runner-to-runner variance itself wasn't the root problem — it was
that these benches are dominated by thousands of small `fs::write` calls to
`std::env::temp_dir()`, and shared, multi-tenant cloud VMs have highly
variable disk I/O latency, far more than their CPU scheduling variance.
Fixed at the source rather than by further widening the threshold:
`bench_dir()` in `core_benches.rs` now writes to `/dev/shm` (tmpfs) when
available, falling back to `std::env::temp_dir()` only when it isn't (e.g.
local macOS development, or a CI image without a `/dev/shm` mount). This
moves the benchmark's dominant cost off the disk I/O layer entirely, which
local testing showed both faster (53ms vs. 93-244ms for the 5k corpus) and
far more consistent run-to-run.

**Third incident, same day: even with disk I/O removed, back-to-back CI
runs of the identical commit still differed by ~90%** (5k: 40.7ms then
78.9ms; 50k: 404.9ms then 796.6ms — both almost exactly 2x). Within a
single run, criterion's own confidence intervals are tight (e.g. `[40.548ms
40.674ms 40.825ms]`), so this isn't sampling noise; it's variance *between*
job runs landing on different underlying hosts, i.e. ordinary
shared-vCPU/noisy-neighbor variance on GitHub-hosted runners, for even a
lightweight, mostly-CPU-bound operation (string formatting in a loop).
There's no code-level fix for that short of dedicated benchmark hardware,
which doesn't exist for this project. The threshold moved 25% -> 60% -> a
generous **150%**, with the explicit understanding that this placeholder
fixture-generation bench is a smoke test that the harness and CI wiring
work, not a precise regression gate — see "Revisit this number once M1's
search/index/materialize benches exist" below, which is where real,
evidence-based tuning belongs.
