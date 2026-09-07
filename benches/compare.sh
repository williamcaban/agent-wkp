#!/usr/bin/env bash
# Compares the most recent `cargo bench -p wkp-core` run against
# benches/baseline.json and fails if any bench's mean regressed beyond
# WKP_BENCH_THRESHOLD_PCT (default 25%). See benches/README.md.
#
# Usage:
#   cargo bench -p wkp-core && ./benches/compare.sh
#   cargo bench -p wkp-core && ./benches/compare.sh --update   # rewrite baseline.json
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BASELINE_FILE="$SCRIPT_DIR/baseline.json"
CRITERION_DIR="$REPO_ROOT/target/criterion"
THRESHOLD_PCT="${WKP_BENCH_THRESHOLD_PCT:-150}"
UPDATE=0
[[ "${1:-}" == "--update" ]] && UPDATE=1

# bench_name -> path under target/criterion/ holding new/estimates.json.
# Keep in sync with crates/wkp-core/benches/core_benches.rs.
declare -A BENCHES=(
  [fixture_corpus_generate_5k]="fixture_corpus_generate_5k"
  [fixture_corpus_generate_50k]="fixture_corpus_generate_50k/run"
  [incremental_update_50k_corpus]="incremental_update_50k_corpus/10_changed"
  [cold_search_50k_corpus]="cold_search_50k_corpus/open_and_query"
)

if [[ "$UPDATE" -eq 1 ]]; then
  tmp="$(mktemp)"
  echo "{" > "$tmp"
  first=1
  for bench_name in "${!BENCHES[@]}"; do
    estimates="$CRITERION_DIR/${BENCHES[$bench_name]}/new/estimates.json"
    [[ -f "$estimates" ]] || { echo "missing $estimates; run cargo bench -p wkp-core first" >&2; exit 1; }
    mean_ns=$(jq '.mean.point_estimate' "$estimates")
    [[ "$first" -eq 1 ]] || echo "," >> "$tmp"
    first=0
    printf '  "%s": { "mean_ns": %s }' "$bench_name" "$mean_ns" >> "$tmp"
  done
  echo "" >> "$tmp"
  echo "}" >> "$tmp"
  jq . "$tmp" > "$BASELINE_FILE"
  rm -f "$tmp"
  echo "Updated $BASELINE_FILE"
  exit 0
fi

status=0
for bench_name in "${!BENCHES[@]}"; do
  estimates="$CRITERION_DIR/${BENCHES[$bench_name]}/new/estimates.json"
  if [[ ! -f "$estimates" ]]; then
    echo "SKIP  $bench_name: no criterion output at $estimates (run 'cargo bench -p wkp-core' first)" >&2
    continue
  fi
  new_mean_ns=$(jq '.mean.point_estimate' "$estimates")
  baseline_mean_ns=$(jq -r --arg name "$bench_name" '.[$name].mean_ns // empty' "$BASELINE_FILE")
  if [[ -z "$baseline_mean_ns" ]]; then
    echo "NEW   $bench_name: ${new_mean_ns}ns, no baseline entry yet (run with --update to record one)"
    continue
  fi
  limit_ns=$(echo "$baseline_mean_ns * (1 + $THRESHOLD_PCT / 100)" | bc -l)
  pct=$(echo "scale=1; ($new_mean_ns - $baseline_mean_ns) / $baseline_mean_ns * 100" | bc -l)
  if (($(echo "$new_mean_ns > $limit_ns" | bc -l))); then
    echo "FAIL  $bench_name: ${new_mean_ns}ns vs baseline ${baseline_mean_ns}ns (+${pct}%, threshold +${THRESHOLD_PCT}%)"
    status=1
  else
    echo "OK    $bench_name: ${new_mean_ns}ns vs baseline ${baseline_mean_ns}ns (${pct}%)"
  fi
done

exit $status
