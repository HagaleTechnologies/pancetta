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
command -v python3 >/dev/null || { echo "::error::python3 is required" >&2; exit 1; }

shopt -s globstar nullglob
status=0

# load <criterion-dir> <baseline|current>: fill the <kind>_files (benchmark
# ID -> estimates.json path) and <kind>_ids arrays. Glob expansion is sorted,
# so the ID list, and therefore the report, comes out in a stable order.
# Bash cannot pass arrays to a function, so the target arrays are selected
# here by kind; a nameref would only restate this and trip SC2034.
declare -A baseline_files=() current_files=()
baseline_ids=() current_ids=()
load() {
  local dir=${1%/} kind=$2 f id
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
    if [[ $kind == baseline ]]; then
      baseline_files["$id"]=$f
      baseline_ids+=("$id")
    else
      current_files["$id"]=$f
      current_ids+=("$id")
    fi
  done
}
load "$baseline_dir" baseline
load "$current_dir" current
if (( ${#baseline_ids[@]} == 0 )); then
  echo "::error::no criterion estimates under ${baseline_dir%/} (baseline)"
  status=1
fi
if (( ${#current_ids[@]} == 0 )); then
  echo "::error::no criterion estimates under ${current_dir%/} (current)"
  status=1
fi

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

# Compare one benchmark. Exit 3 for a regression or 1 for invalid estimates.
compare() {
  python3 - "$@" <<'PYTHON'
import json
import sys
from decimal import Decimal, InvalidOperation
from fractions import Fraction
from math import inf
from pathlib import Path


def reject_constant(token):
    raise ValueError(f"{token} is not a JSON number")


def mean_of(path, kind):
    try:
        document = Path(path).read_text(encoding="utf-8")
        if not document.strip(" \t\n\r"):
            raise ValueError("holds no JSON document")
        estimate = json.loads(
            document, parse_float=Decimal, parse_int=Decimal,
            parse_constant=reject_constant,
        )["mean"]["point_estimate"]
        if not isinstance(estimate, Decimal) or not 0 < float(estimate) < inf:
            raise ValueError("mean.point_estimate must be a finite positive number")
        return estimate
    except (OSError, ValueError, InvalidOperation, KeyError, TypeError, RecursionError) as error:
        reason = " ".join(str(error).splitlines())
        print(f"::error::{benchmark_id}: invalid {kind} estimates {path}: {reason}")
        return None


def format_mean(mean):
    return format(mean, ".2f").rstrip("0").rstrip(".")


benchmark_id, baseline_path, current_path, max_percent = sys.argv[1:]
baseline = mean_of(baseline_path, "baseline")
current = mean_of(current_path, "current")
if baseline is None or current is None:
    sys.exit(1)

# Keep the comparison exact, including JSON numbers in scientific notation.
# The rounded percentage below is for the report only.
regressed = Fraction(current) * 100 > Fraction(baseline) * (100 + Fraction(Decimal(max_percent)))
change = (current - baseline) * 100 / baseline
print(f"{benchmark_id}: baseline {format_mean(baseline)} ns, "
      f"current {format_mean(current)} ns ({change:+.2f}%)")
sys.exit(3 if regressed else 0)
PYTHON
}

regressed=0
compared=0
for id in "${baseline_ids[@]}"; do
  [[ -n ${current_files[$id]+set} ]] || continue
  result=0
  compare "$id" "${baseline_files[$id]}" "${current_files[$id]}" "$max_percent" || result=$?
  case $result in
    0) compared=$((compared + 1)) ;;
    3)
      echo "::error::REGRESSION $id exceeds +${max_percent}%"
      compared=$((compared + 1))
      regressed=$((regressed + 1))
      status=1
      ;;
    *) status=1 ;;
  esac
done

echo "compared $compared benchmark(s) against the baseline: $regressed above +${max_percent}%"
exit "$status"
