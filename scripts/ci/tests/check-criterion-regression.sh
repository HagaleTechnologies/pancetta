#!/usr/bin/env bash
# PAN-369: offline tests for scripts/ci/check-criterion-regression.sh. Each
# case builds miniature Criterion trees under a scratch directory and runs the
# production script against them.
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

  # These means are exactly 15% apart in decimal. Binary floating-point
  # cross-products round differently and can make this look above the limit.
  bench "$d/base" basic_decode/example "$(mean 100000000.004)"
  bench "$d/cur" basic_decode/example "$(mean 115000000.00460)"
  run_gate "$d/base" "$d/cur"
  expect_code 0 "exact decimal limit" || return 1
  expect_out "(+15.00%)" "exact decimal limit" || return 1

  bench "$d/cur" basic_decode/example "$(mean 1.15000000004600001e8)"
  run_gate "$d/base" "$d/cur"
  expect_code 1 "just above decimal limit" || return 1
  expect_out "REGRESSION basic_decode/example exceeds +15%" "just above decimal limit" || return 1
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
multiple JSON documents|$(mean 100) $(mean 116)
zero mean|$(mean 0)
negative mean|$(mean -5)
non-numeric mean|$(mean '"fast"')
null mean|$(mean null)
EOF

  # The workflow compares the candidate tree to itself before seeding.
  d=$(new_case)
  bench "$d/cur" basic_decode/example "$(mean 100) $(mean 116)"
  run_gate "$d/cur" "$d/cur"
  expect_code 1 "seed with multiple JSON documents" || return 1
  expect_out "basic_decode/example: invalid baseline estimates" "seed with multiple JSON documents" || return 1
  expect_out "basic_decode/example: invalid current estimates" "seed with multiple JSON documents" || return 1

  # An estimates.json holding no JSON document at all (zero-byte or
  # whitespace-only) must fail on either side, never read as a 0 ns mean:
  # an empty current run would "improve" every benchmark by 100 %, and the
  # seed self-check compares a tree with itself, so an empty baseline must
  # fail to store too.
  d=$(new_case)
  bench "$d/base" basic_decode/example "$(mean 100)"
  bench "$d/cur" basic_decode/example "$(mean 100)"
  : >"$d/cur/basic_decode/example/new/estimates.json"
  run_gate "$d/base" "$d/cur" 15
  expect_code 1 "zero-byte current estimates" || return 1
  expect_out "basic_decode/example: invalid current estimates" "zero-byte current estimates" || return 1
  expect_out "holds no JSON document" "zero-byte current estimates" || return 1
  expect_no_out "current 0 ns" "zero-byte current estimates" || return 1

  local dir_side kind body
  for dir_side in base cur; do
    case $dir_side in
      base) kind=baseline ;;
      cur) kind=current ;;
    esac
    for body in "" "   "; do
      d=$(new_case)
      bench "$d/base" basic_decode/example "$(mean 100)"
      bench "$d/cur" basic_decode/example "$(mean 100)"
      printf '%s\n' "$body" >"$d/$dir_side/basic_decode/example/new/estimates.json"
      run_gate "$d/base" "$d/cur" 15
      expect_code 1 "whitespace-only $kind estimates" || return 1
      expect_out "basic_decode/example: invalid $kind estimates" "whitespace-only $kind estimates" || return 1
      expect_no_out "current 0 ns" "whitespace-only $kind estimates" || return 1
    done
  done
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
  local last expected
  last=$(grep -v '^[[:space:]]*$' <<<"$compare" | tail -n 1 | sed 's/^[[:space:]]*//')
  # The workflow's literal text: escaped dollars name the shell variables
  # GitHub expands at run time, not ones this script expands.
  expected="bash scripts/ci/check-criterion-regression.sh \"\$BASELINE_DIR\" \"\$CRITERION_HOME\" 15"
  if [[ $last != "$expected" ]]; then
    echo "  exit propagation: compare step ends with '$last'" >&2
    return 1
  fi
  if grep -E 'check-criterion-regression\.sh.*(\|\||\||;)|continue-on-error' <<<"$out" >/dev/null; then
    echo "  exit propagation: the gate's status is masked" >&2
    return 1
  fi
}

# Each case is invoked by a literal call: an indirect dispatch through a
# variable name hides the calls from shellcheck (SC2317) and from a reader.
failed=0
if workflow_runs_gate_weekly; then echo "ok workflow_runs_gate_weekly"; else echo "FAIL workflow_runs_gate_weekly"; failed=1; fi
if passes_at_limit; then echo "ok passes_at_limit"; else echo "FAIL passes_at_limit"; failed=1; fi
if fails_above_limit; then echo "ok fails_above_limit"; else echo "FAIL fails_above_limit"; failed=1; fi
if fails_when_second_benchmark_regresses; then echo "ok fails_when_second_benchmark_regresses"; else echo "FAIL fails_when_second_benchmark_regresses"; failed=1; fi
if prints_baseline_current_and_change; then echo "ok prints_baseline_current_and_change"; else echo "FAIL prints_baseline_current_and_change"; failed=1; fi
if fails_on_missing_or_invalid_estimates; then echo "ok fails_on_missing_or_invalid_estimates"; else echo "FAIL fails_on_missing_or_invalid_estimates"; failed=1; fi
if usage_errors_exit_2; then echo "ok usage_errors_exit_2"; else echo "FAIL usage_errors_exit_2"; failed=1; fi
if honours_max_percent_argument; then echo "ok honours_max_percent_argument"; else echo "FAIL honours_max_percent_argument"; failed=1; fi
exit "$failed"
