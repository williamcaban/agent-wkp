#!/usr/bin/env bash
# M0-5: process-level cold-start bench for `wkp --version` (design 4.3's
# proxy for "cold start"), committed as a reproducible script rather than
# the one-off /usr/bin/time -v + manual timing loop this was originally
# measured with by hand (see benches/README.md's "Current state" section).
#
# Runs the release binary N times, records wall-clock per run in
# nanoseconds via bash's own $EPOCHREALTIME (no external timing tool
# needed), and reports p50/p95 -- matching design 4.3's own stated metric
# pair, not just a mean.
#
# Usage: benches/cold_start.sh [binary] [runs]
set -euo pipefail

BIN="${1:-target/release/wkp}"
RUNS="${2:-200}"

if [ ! -x "$BIN" ]; then
  echo "::error::$BIN does not exist or is not executable -- build it first (cargo build --release -p wkp-cli)" >&2
  exit 1
fi

durations=()
for ((i = 0; i < RUNS; i++)); do
  # $EPOCHREALTIME is SSSSSSSSSS.NNNNNN -- 6 fractional digits, i.e.
  # microsecond resolution. Stripping the dot turns it directly into an
  # integer microsecond count since the epoch; no further unit conversion
  # needed (an earlier version of this script wrongly divided by 1000 here,
  # as if this were nanoseconds, producing nonsense sub-microsecond results).
  start="${EPOCHREALTIME/./}"
  "$BIN" --version >/dev/null
  end="${EPOCHREALTIME/./}"
  durations+=($((end - start)))
done

# Sort numerically, then pick the p50/p95 index -- same nearest-rank method
# benches/compare.sh's own reasoning would use if it needed a percentile
# (it doesn't; Criterion reports its own).
IFS=$'\n' sorted=($(sort -n <<<"${durations[*]}"))
unset IFS

p50_idx=$((RUNS * 50 / 100))
p95_idx=$((RUNS * 95 / 100))
p50_us=${sorted[p50_idx]}
p95_us=${sorted[p95_idx]}

echo "cold_start_wkp_version: runs=$RUNS p50=${p50_us}us p95=${p95_us}us"

# Machine-readable line for benches/compare.sh to parse.
echo "COLD_START_P50_US=$p50_us"
echo "COLD_START_P95_US=$p95_us"
