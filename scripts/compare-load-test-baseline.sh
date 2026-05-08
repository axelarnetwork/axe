#!/usr/bin/env bash
#
# Compare a current load-test run-set against a saved baseline.
#
# Usage:
#   scripts/compare-load-test-baseline.sh <baseline-dir> <current-dir>
#
# Both directories should contain JSON files produced by `axe load-test`,
# renamed to a stable layout (e.g. `01-sol-sol-gmp.json`). The baseline name
# determines the expected current name — comparison is by filename.
#
# Compared metrics:
#   - landing_rate            (source-side submission success)
#   - verification.success_rate (end-to-end verification success)
#   - verification.failed     (count of failed verifications)
#   - verification.stuck      (count of stuck verifications)
#
# Exit code is 0 when every baseline file has a matching current file with
# equal-or-better metrics, 1 otherwise.

set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $0 <baseline-dir> <current-dir>" >&2
    exit 2
fi

BASELINE="$1"
CURRENT="$2"

if [[ ! -d "$BASELINE" ]]; then
    echo "baseline dir not found: $BASELINE" >&2
    exit 2
fi
if [[ ! -d "$CURRENT" ]]; then
    echo "current dir not found: $CURRENT" >&2
    exit 2
fi

regression=0
shopt -s nullglob
for baseline_file in "$BASELINE"/*.json; do
    name=$(basename "$baseline_file")
    current_file="$CURRENT/$name"

    if [[ ! -f "$current_file" ]]; then
        printf "MISSING  %s (no matching current run)\n" "$name"
        regression=1
        continue
    fi

    base_landing=$(jq -r '.landing_rate // 0' "$baseline_file")
    base_verify=$(jq -r '.verification.success_rate // 0' "$baseline_file")
    base_failed=$(jq -r '.verification.failed // 0' "$baseline_file")
    base_stuck=$(jq -r '.verification.stuck // 0' "$baseline_file")

    cur_landing=$(jq -r '.landing_rate // 0' "$current_file")
    cur_verify=$(jq -r '.verification.success_rate // 0' "$current_file")
    cur_failed=$(jq -r '.verification.failed // 0' "$current_file")
    cur_stuck=$(jq -r '.verification.stuck // 0' "$current_file")

    landing_drop=$(awk -v b="$base_landing" -v c="$cur_landing" 'BEGIN { print (c < b) ? 1 : 0 }')
    verify_drop=$(awk -v b="$base_verify" -v c="$cur_verify" 'BEGIN { print (c < b) ? 1 : 0 }')
    failed_up=$(( cur_failed > base_failed ? 1 : 0 ))
    stuck_up=$(( cur_stuck > base_stuck ? 1 : 0 ))

    if (( landing_drop || verify_drop || failed_up || stuck_up )); then
        printf "REGRESS  %s\n" "$name"
        printf "    landing  baseline=%s  current=%s\n" "$base_landing" "$cur_landing"
        printf "    verify   baseline=%s  current=%s\n" "$base_verify" "$cur_verify"
        printf "    failed   baseline=%s  current=%s\n" "$base_failed" "$cur_failed"
        printf "    stuck    baseline=%s  current=%s\n" "$base_stuck"  "$cur_stuck"
        regression=1
    else
        printf "OK       %s  (landing=%s verify=%s)\n" "$name" "$cur_landing" "$cur_verify"
    fi
done

exit $regression
