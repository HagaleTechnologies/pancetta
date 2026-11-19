#!/usr/bin/env bash
# PAN-369: offline tests for scripts/ci/check-criterion-regression.sh. Each
# case builds miniature Criterion trees under a scratch directory and runs the
# production script against them.
# shellcheck disable=SC2317  # case functions are invoked indirectly by name
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
gate="$here/../check-criterion-regression.sh"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

# bench <criterion-dir> <bench-id> <estimates.json body>
bench() {
  mkdir -p "$1/$2/new"
  printf '%s\n' "$3" >"$1/$2/new/estimates.json"
}
# mean <point-estimate>: a minimal Criterion estimates.json body.
mean() { printf '{"mean":{"point_estimate":%s,"standard_error":1.0}}' "$1"; }

new_case() { mktemp -d "$scratch/case.XXXXXX"; }

# run_gate <baseline-dir> <current-dir> [max_percent]: sets $code and $out.
run_gate() {
  set +e
  out=$(bash "$gate" "$@" 2>&1)
  code=$?
  set -e
}

expect_code() { # <want> <label>
  if [[ $code -ne $1 ]]; then
    echo "  $2: exit $code, want $1; output:" >&2
    printf '%s\n' "$out" | sed 's/^/    /' >&2
    return 1
  fi
}
expect_out() { # <fixed-string> <label>
  if ! grep -qF -- "$1" <<<"$out"; then
    echo "  $2: output lacks '$1'; output:" >&2
    printf '%s\n' "$out" | sed 's/^/    /' >&2
    return 1
  fi
}
expect_no_out() { # <fixed-string> <label>
  if grep -qF -- "$1" <<<"$out"; then
    echo "  $2: output unexpectedly contains '$1'" >&2
    return 1
  fi
}

passes_at_limit() {
  local d; d=$(new_case)
  bench "$d/base" basic_decode/example "$(mean 100)"
  bench "$d/cur" basic_decode/example "$(mean 115)"
  run_gate "$d/base" "$d/cur" 15
  expect_code 0 "115 vs 100" || return 1
  expect_out "basic_decode/example: baseline 100 ns, current 115 ns (+15.00%)" "115 vs 100" || return 1
  expect_no_out "REGRESSION" "115 vs 100" || return 1
  # The limit defaults to 15 % when no third argument is given.
  run_gate "$d/base" "$d/cur"
  expect_code 0 "115 vs 100, default limit" || return 1
}

fails_above_limit() {
  local d; d=$(new_case)
  bench "$d/base" basic_decode/example "$(mean 100)"
  bench "$d/cur" basic_decode/example "$(mean 115.01)"
  run_gate "$d/base" "$d/cur" 15
  expect_code 1 "115.01 vs 100" || return 1
  expect_out "basic_decode/example" "115.01 vs 100" || return 1
  expect_out "+15.01%" "115.01 vs 100" || return 1
  expect_out "REGRESSION basic_decode/example exceeds +15%" "115.01 vs 100" || return 1
}

fails_when_second_benchmark_regresses() {
  local d; d=$(new_case)
  bench "$d/base" a_first/fine "$(mean 1000)"
  bench "$d/cur" a_first/fine "$(mean 990)"
  bench "$d/base" b_second/slower "$(mean 2000)"
  bench "$d/cur" b_second/slower "$(mean 2400)"
  run_gate "$d/base" "$d/cur" 15
  expect_code 1 "second regresses" || return 1
  expect_out "a_first/fine: baseline 1000 ns, current 990 ns (-1.00%)" "first reported" || return 1
  expect_out "b_second/slower: baseline 2000 ns, current 2400 ns (+20.00%)" "second reported" || return 1
  expect_out "REGRESSION b_second/slower exceeds +15%" "second named" || return 1
  expect_no_out "REGRESSION a_first/fine" "first not flagged" || return 1
}

prints_baseline_current_and_change() {
  local d; d=$(new_case)
  bench "$d/base" basic_decode/example "$(mean 100)"
  bench "$d/cur" basic_decode/example "$(mean 116)"
  run_gate "$d/base" "$d/cur" 15
  expect_code 1 "100 -> 116" || return 1
  if ! grep -qxF "basic_decode/example: baseline 100 ns, current 116 ns (+16.00%)" <<<"$out"; then
    echo "  100 -> 116: no exact report line; output:" >&2
    printf '%s\n' "$out" | sed 's/^/    /' >&2
    return 1
  fi
}

fails_on_missing_or_invalid_estimates() {
  local d

  d=$(new_case)
  bench "$d/base" basic_decode/example "$(mean 100)"
  mkdir -p "$d/cur"
  run_gate "$d/base" "$d/cur" 15
  expect_code 1 "empty current tree" || return 1
  expect_out "no criterion estimates under $d/cur" "empty current tree" || return 1

  d=$(new_case)
  bench "$d/base" basic_decode/example "$(mean 100)"
  bench "$d/base" multi_signal/5 "$(mean 300)"
  bench "$d/cur" basic_decode/example "$(mean 100)"
  run_gate "$d/base" "$d/cur" 15
  expect_code 1 "missing benchmark" || return 1
  expect_out "multi_signal/5: missing from the current run" "missing benchmark" || return 1

  d=$(new_case)
  bench "$d/base" basic_decode/example "$(mean 100)"
  bench "$d/cur" basic_decode/example "$(mean 100)"
  bench "$d/cur" brand_new/bench "$(mean 50)"
  run_gate "$d/base" "$d/cur" 15
  expect_code 1 "unexpected benchmark" || return 1
  expect_out "brand_new/bench: unexpected in the current run" "unexpected benchmark" || return 1

  local label body
  while IFS='|' read -r label body; do
    d=$(new_case)
    bench "$d/base" basic_decode/example "$(mean 100)"
    bench "$d/cur" basic_decode/example "$body"
    run_gate "$d/base" "$d/cur" 15
    expect_code 1 "$label" || return 1
    expect_out "basic_decode/example: invalid current estimates" "$label" || return 1
    # Validation failures are also caught on the baseline side.
    d=$(new_case)
    bench "$d/base" basic_decode/example "$body"
    bench "$d/cur" basic_decode/example "$(mean 100)"
    run_gate "$d/base" "$d/cur" 15
    expect_code 1 "$label (baseline)" || return 1
    expect_out "basic_decode/example: invalid baseline estimates" "$label (baseline)" || return 1
  done <<EOF
malformed JSON|{"mean":{"point_estimate":
missing mean field|{"median":{"point_estimate":100}}
zero mean|$(mean 0)
negative mean|$(mean -5)
non-numeric mean|$(mean '"fast"')
null mean|$(mean null)
EOF
}

usage_errors_exit_2() {
  local d; d=$(new_case)
  bench "$d/base" basic_decode/example "$(mean 100)"
  run_gate
  expect_code 2 "no arguments" || return 1
  run_gate "$d/base"
  expect_code 2 "one argument" || return 1
  run_gate "$d/base" "$d/base" abc
  expect_code 2 "non-numeric max_percent" || return 1
  run_gate "$d/base" "$d/base" 15 extra
  expect_code 2 "four arguments" || return 1
}

honours_max_percent_argument() {
  local d; d=$(new_case)
  bench "$d/base" basic_decode/example "$(mean 100)"
  bench "$d/cur" basic_decode/example "$(mean 106)"
  run_gate "$d/base" "$d/cur" 5
  expect_code 1 "106 vs 100 at 5 %" || return 1
  expect_out "REGRESSION basic_decode/example exceeds +5%" "106 vs 100 at 5 %" || return 1
  run_gate "$d/base" "$d/cur" 15
  expect_code 0 "106 vs 100 at 15 %" || return 1
}

# The weekly workflow wires this gate: Monday 12:00 UTC schedule, the
# dedicated bench runner, the exact Cargo command, and the comparator as the
# compare step's last command with nothing masking its exit status.
workflow_runs_gate_weekly() {
  local wf="$here/../../../.github/workflows/ft8-benchmark.yml"
  out=$(cat "$wf")
  expect_out "- cron: '0 12 * * 1'" "weekly schedule" || return 1
  expect_out "workflow_dispatch:" "manual dispatch" || return 1
  expect_out "runs-on: [self-hosted, linux, ft8-bench]" "bench runner" || return 1
  expect_out "default_branch" "default-branch guard" || return 1
  expect_out "set -euo pipefail" "strict shell" || return 1
  if ! grep -qxE '[[:space:]]+cargo bench -p pancetta-ft8' <<<"$out"; then
    echo "  exact cargo command: missing a bare 'cargo bench -p pancetta-ft8' line" >&2
    return 1
  fi
  local compare
  compare=$(awk '/name: Compare against the stored baseline/{on=1} on && /name: Remove Criterion output/{on=0} on' "$wf")
  local last
  last=$(grep -v '^[[:space:]]*$' <<<"$compare" | tail -n 1 | sed 's/^[[:space:]]*//')
  # shellcheck disable=SC2016  # the workflow's literal text, not an expansion
  if [[ $last != 'bash scripts/ci/check-criterion-regression.sh "$BASELINE_DIR" "$CRITERION_HOME" 15' ]]; then
    echo "  exit propagation: compare step ends with '$last'" >&2
    return 1
  fi
  if grep -E 'check-criterion-regression\.sh.*(\|\||\||;)|continue-on-error' <<<"$out" >/dev/null; then
    echo "  exit propagation: the gate's status is masked" >&2
    return 1
  fi
}

failed=0
for t in workflow_runs_gate_weekly passes_at_limit fails_above_limit fails_when_second_benchmark_regresses \
  prints_baseline_current_and_change fails_on_missing_or_invalid_estimates \
  usage_errors_exit_2 honours_max_percent_argument; do
  if "$t"; then
    echo "ok $t"
  else
    echo "FAIL $t"
    failed=1
  fi
done
exit "$failed"
