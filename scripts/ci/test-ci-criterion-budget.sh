#!/usr/bin/env bash
# PAN-284: offline tests for check-criterion-budget.sh and the CI step that runs it.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
gate=scripts/ci/check-criterion-budget.sh
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

fixture() { # <group-dir> <bench> <mean-ns>
  mkdir -p "$1/$2/new"
  printf '{"mean":{"point_estimate":%s}}\n' "$3" >"$1/$2/new/estimates.json"
}

group="$scratch/group"
fixture "$group" W1AW 65.1
fixture "$group" EA8_G8BCG 1999.9
bash "$gate" "$group" 2000 >"$scratch/out" || { cat "$scratch/out"; echo "FAIL: means under budget rejected"; exit 1; }
grep -q 'W1AW mean 65.1 ns' "$scratch/out" || { echo "FAIL: mean not printed"; exit 1; }

fixture "$group" _N7RLK_ 2000.0
set +e
bash "$gate" "$group" 2000 >"$scratch/out" 2>&1
code=$?
set -e
[[ $code -eq 1 ]] || { cat "$scratch/out"; echo "FAIL: mean at budget exited $code, want 1"; exit 1; }
grep -q '::error::_N7RLK_ mean 2000.0 ns' "$scratch/out" || { cat "$scratch/out"; echo "FAIL: offending bench not named"; exit 1; }

mkdir -p "$scratch/empty"
set +e
bash "$gate" "$scratch/empty" 2000 >"$scratch/out" 2>&1
code=$?
set -e
[[ $code -eq 1 ]] || { echo "FAIL: empty group exited $code, want 1"; exit 1; }

if bash "$gate" >/dev/null 2>&1; then echo "FAIL: no arguments accepted"; exit 1; fi
if bash "$gate" "$group" >/dev/null 2>&1; then echo "FAIL: missing budget accepted"; exit 1; fi

ruby -ryaml <<'RUBY'
steps = YAML.load_file('.github/workflows/ci.yml').fetch('jobs').fetch('workspace-check').fetch('steps')
step = steps.find { |s| s['run'].to_s.include?('cargo bench -p pancetta-core --bench dxcc_lookup') }
raise 'workspace-check has no dxcc_lookup bench step' unless step
unless step['run'].include?('scripts/ci/check-criterion-budget.sh target/criterion/dxcc_entity_for_callsign 2000')
  raise 'bench step does not run the 2000 ns budget gate'
end
RUBY
echo "PASS: criterion budget gate cases"
