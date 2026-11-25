#!/usr/bin/env bash
# PAN-468: offline guard for the workspace-check step that builds pancetta-core
# and pancetta-ft8 on their declared rust-version, and for the cargo behaviour
# that step relies on to name a dependency whose own rust-version is too high.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

# 1. Workflow wiring, tied to the version the two crates declare.
ruby -ryaml <<'RUBY'
msrv = %w[pancetta-core pancetta-ft8].map do |c|
  File.read("#{c}/Cargo.toml", encoding: "UTF-8")[/^rust-version = "([^"]+)"$/, 1] or raise "#{c}/Cargo.toml declares no rust-version"
end.uniq
raise "pancetta-core and pancetta-ft8 declare different rust-versions: #{msrv}" unless msrv.size == 1
tc = "#{msrv.first}.0"

jobs = YAML.load_file('.github/workflows/ci.yml').fetch('jobs')
check = jobs.fetch('workspace-check')
steps = check.fetch('steps')
dtolnay = steps.each_index.select { |i| steps[i]['uses'].to_s.start_with?('dtolnay/rust-toolchain@') }
pinned = dtolnay.select { |i| steps[i].dig('with', 'toolchain').to_s == '1.98.0' }
old = dtolnay.select { |i| steps[i].dig('with', 'toolchain').to_s == tc }
raise "workspace-check must install #{tc} exactly once, found #{old.size}" unless old.size == 1
raise 'workspace-check lost its 1.98.0 toolchain step' unless pinned.size == 1
cache = steps.index { |s| s['uses'].to_s.start_with?('Swatinem/rust-cache@') }
raise "the #{tc} install must come after the 1.98.0 install and the rust-cache step" unless old.first > pinned.first && old.first > cache

want = [
  "cargo +#{tc} check --locked -p pancetta-core -p pancetta-ft8",
  "cargo +#{tc} check --locked -p pancetta-core -p pancetta-ft8 --all-features",
  "cargo +#{tc} check --locked -p pancetta-core -p pancetta-ft8 --no-default-features"
]
msrv_steps = steps.each_index.select { |i| steps[i]['run'].to_s.include?("cargo +#{tc} ") }
raise "workspace-check must have exactly one MSRV step, found #{msrv_steps.size}" unless msrv_steps.size == 1
step = steps[msrv_steps.first]
raise 'the MSRV step must come after the toolchain install' unless msrv_steps.first > old.first
raise "the MSRV step must run exactly: #{want}" unless step['run'].lines.map(&:strip).reject(&:empty?) == want
raise 'the MSRV step must build outside target/' unless step.dig('env', 'CARGO_TARGET_DIR') == '${{ runner.temp }}/msrv-target'
raise 'the MSRV step must fail the job' if step['continue-on-error'] || check['continue-on-error'] || step['if']

elsewhere = jobs.values.flat_map { |j| j.fetch('steps', []) }.count { |s| s['run'].to_s.include?("cargo +#{tc} ") }
raise "cargo +#{tc} must run only in the MSRV step" unless elsewhere == 1

raise 'workspace-check no longer gates on the code filter' unless check['if'].include?("needs.changes.outputs.code == 'true'")
filter_step = jobs.fetch('changes').fetch('steps').find { |s| s['id'] == 'filter' }
code = YAML.safe_load(filter_step.fetch('with').fetch('filters')).fetch('code')
%w[**/*.rs **/Cargo.toml **/Cargo.lock rust-toolchain.toml .github/workflows/ci.yml].each do |p|
  raise "the code filter is missing #{p}" unless code.include?(p)
end
RUBY

# 2. cargo refuses a dependency whose rust-version is above the running rustc,
#    and names it. 1.999 is above any real toolchain, so no 1.88 is needed here.
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/app/src" "$scratch/fixture-dep/src"
printf '[package]\nname = "fixture-dep"\nversion = "1.2.3"\nedition = "2021"\nrust-version = "1.999"\n' >"$scratch/fixture-dep/Cargo.toml"
printf '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nfixture-dep = { path = "../fixture-dep" }\n' >"$scratch/app/Cargo.toml"
: >"$scratch/fixture-dep/src/lib.rs"
: >"$scratch/app/src/lib.rs"
set +e
(cd "$scratch/app" && CARGO_TARGET_DIR="$scratch/target" cargo check --offline) >"$scratch/out" 2>&1
code=$?
set -e
[[ $code -ne 0 ]] || { cat "$scratch/out"; echo "FAIL: cargo built a dependency that requires rustc 1.999"; exit 1; }
grep -qF 'fixture-dep@1.2.3 requires rustc 1.999' "$scratch/out" || { cat "$scratch/out"; echo "FAIL: cargo did not name the dependency"; exit 1; }

echo "PASS: MSRV check wiring and dependency naming"
