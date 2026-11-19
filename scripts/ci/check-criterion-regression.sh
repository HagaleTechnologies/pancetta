#!/usr/bin/env bash
# PAN-369: fail when any Criterion benchmark mean is more than <max_percent>
# above the same benchmark's mean in a stored baseline.
#
# Usage: check-criterion-regression.sh <baseline_criterion_dir> <current_criterion_dir> [max_percent=15]
#
# A benchmark's ID is the path of its new/estimates.json relative to the
# criterion directory, minus the trailing /new/estimates.json (for example
# basic_decode/example). Both trees must hold the same, nonempty set of IDs,
# and every .mean.point_estimate (nanoseconds) must be a finite positive
# number. A benchmark regresses when current * 100 > baseline * (100 + max).
#
# Exit status: 0 all within limit, 1 any regression or validation failure
# (every benchmark is still checked and reported), 2 usage error.
set -euo pipefail

usage() {
  echo "usage: check-criterion-regression.sh <baseline_criterion_dir> <current_criterion_dir> [max_percent=15]" >&2
  exit 2
}

(( $# == 2 || $# == 3 )) || usage
baseline_dir=$1
current_dir=$2
max_percent=${3:-15}
[[ -n $baseline_dir && -n $current_dir ]] || usage
if ! [[ $max_percent =~ ^[0-9]+([.][0-9]+)?$ ]]; then
  echo "max_percent must be a non-negative number, got '$max_percent'" >&2
  usage
fi
command -v jq >/dev/null || { echo "::error::jq is required" >&2; exit 1; }

shopt -s globstar nullglob
status=0

# load <criterion-dir> <baseline|current>: fill <kind>_files (benchmark ID ->
# estimates.json path) and <kind>_ids. Glob expansion is sorted, so the ID
# list, and therefore the report, comes out in a stable order.
declare -A baseline_files=() current_files=()
baseline_ids=() current_ids=()
load() {
  local dir=${1%/} kind=$2 f id
  local -n files=${kind}_files ids=${kind}_ids
  if [[ ! -d $dir ]]; then
    echo "::error::$kind criterion directory $dir does not exist"
    status=1
    return 0
  fi
  for f in "$dir"/**/new/estimates.json; do
    [[ -f $f ]] || continue
    id=${f#"$dir"/}
    if [[ $id == new/estimates.json ]]; then
      echo "::error::$kind estimates at $f sit at the criterion root and name no benchmark"
      status=1
      continue
    fi
    id=${id%/new/estimates.json}
    # shellcheck disable=SC2034  # files is a nameref to the caller's array
    files["$id"]=$f
    ids+=("$id")
  done
  if (( ${#ids[@]} == 0 )); then
    echo "::error::no criterion estimates under $dir ($kind)"
    status=1
  fi
}
load "$baseline_dir" baseline
load "$current_dir" current

for id in "${baseline_ids[@]}"; do
  if [[ -z ${current_files[$id]+set} ]]; then
    echo "::error::$id: missing from the current run (present in the baseline)"
    status=1
  fi
done
for id in "${current_ids[@]}"; do
  if [[ -z ${baseline_files[$id]+set} ]]; then
    echo "::error::$id: unexpected in the current run (absent from the baseline)"
    status=1
  fi
done

# mean_of <estimates.json>: print .mean.point_estimate, or fail with a reason.
mean_of() {
  jq -r '(try .mean.point_estimate catch null) as $m
    | if ($m | type) == "number" and ($m | isinfinite | not) and ($m | isnan | not) and $m > 0
      then $m
      else error("mean.point_estimate is \($m | tojson), want a finite positive number")
      end' "$1"
}

regressed=0
compared=0
for id in "${baseline_ids[@]}"; do
  [[ -n ${current_files[$id]+set} ]] || continue
  valid=1
  if ! baseline_mean=$(mean_of "${baseline_files[$id]}" 2>&1); then
    echo "::error::$id: invalid baseline estimates ${baseline_files[$id]}: ${baseline_mean//$'\n'/ }"
    valid=0
  fi
  if ! current_mean=$(mean_of "${current_files[$id]}" 2>&1); then
    echo "::error::$id: invalid current estimates ${current_files[$id]}: ${current_mean//$'\n'/ }"
    valid=0
  fi
  if (( !valid )); then
    status=1
    continue
  fi
  # Floating-point compare and format in awk: prints "<verdict>\t<line>".
  result=$(awk -v id="$id" -v b="$baseline_mean" -v c="$current_mean" -v m="$max_percent" '
    function fmt(x,  s) { s = sprintf("%.2f", x); sub(/\.?0+$/, "", s); return s }
    BEGIN {
      b += 0; c += 0; m += 0
      verdict = (c * 100 > b * (100 + m)) ? "regressed" : "ok"
      printf "%s\t%s: baseline %s ns, current %s ns (%+.2f%%)\n", verdict, id, fmt(b), fmt(c), (c - b) * 100 / b
    }')
  verdict=${result%%$'\t'*}
  echo "${result#*$'\t'}"
  compared=$((compared + 1))
  case $verdict in
    ok) ;;
    regressed)
      echo "::error::REGRESSION $id exceeds +${max_percent}%"
      regressed=$((regressed + 1))
      status=1
      ;;
    *)
      echo "::error::$id: comparison produced no verdict"
      status=1
      ;;
  esac
done

echo "compared $compared benchmark(s) against the baseline: $regressed above +${max_percent}%"
exit "$status"
