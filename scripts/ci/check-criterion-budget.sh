#!/usr/bin/env bash
# Fail when any criterion benchmark in a group has a mean at or above a budget.
# Usage: check-criterion-budget.sh <criterion-group-dir> <max-mean-ns>
set -euo pipefail
dir=${1:?usage: check-criterion-budget.sh <criterion-group-dir> <max-mean-ns>}
max=${2:?usage: check-criterion-budget.sh <criterion-group-dir> <max-mean-ns>}
shopt -s nullglob
files=("$dir"/*/new/estimates.json)
if (( ${#files[@]} == 0 )); then
  echo "::error::no criterion estimates under $dir"
  exit 1
fi
status=0
for f in "${files[@]}"; do
  name=$(basename "$(dirname "$(dirname "$f")")")
  mean=$(jq -r '.mean.point_estimate' "$f")
  echo "$name mean ${mean} ns (budget ${max} ns)"
  if ! jq -e --argjson max "$max" '.mean.point_estimate < $max' "$f" >/dev/null; then
    echo "::error::$name mean ${mean} ns is not under ${max} ns"
    status=1
  fi
done
exit "$status"
