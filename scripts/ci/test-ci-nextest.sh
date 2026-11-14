#!/usr/bin/env bash
# Offline guard for PAN-314's coverage, gating and retry policy.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
ruby -ryaml -ropen3 <<'RUBY'
jobs = YAML.load_file('.github/workflows/ci.yml').fetch('jobs')
check = jobs.fetch('workspace-check')
tests = jobs.fetch('workspace-tests')
raise 'matrix' unless tests.fetch('strategy') == {'fail-fast' => false, 'matrix' => {'partition' => [1, 2, 3, 4]}}
raise 'name' unless tests['name'] == 'Workspace Tests (${{ matrix.partition }}/4)'
%w[needs if runs-on env].each { |key| raise key unless tests[key] == check[key] }
raise 'always' unless tests.fetch('if').start_with?('always() &&')
steps = tests.fetch('steps')
check_steps = check.fetch('steps')
# These steps must retain identical setup and explicit admission failure behavior.
check_steps.take(5).each_with_index { |step, i| raise "setup #{i}" unless steps[i] == step }
toolchain = ->(ss) { ss.find { |s| s['uses'].to_s.start_with?('dtolnay/rust-toolchain@') } }
raise 'toolchain' unless toolchain.call(steps) == toolchain.call(check_steps)
cache = steps.find { |s| s['uses'] == 'Swatinem/rust-cache@v2' }
raise 'cache' unless cache.fetch('with').fetch('key') == 'workspace-tests'
install = steps.find { |s| s['name'] == 'Install nextest' }.fetch('run')
raise 'pinned prebuilt' unless install.include?('cargo-nextest-0.9.151-x86_64-unknown-linux-gnu.tar.gz') && install.include?('cargo nextest --version')
base = 'cargo nextest run --profile ci --workspace --exclude pancetta-research --features transmit'
build = steps.find { |s| s['name'] == 'Build workspace tests' }
run = steps.find { |s| s['name'] == 'Test workspace partition' }
raise 'compile' unless build['run'] == "#{base} --no-run"
raise 'partition' unless run['run'] == "#{base} --partition count:${{ matrix.partition }}/4 --no-fail-fast"
raise 'build order' unless steps.index(build) < steps.index(run)
raise 'continue-on-error' if tests['continue-on-error'] || steps.any? { |s| s['continue-on-error'] }
commands = check_steps.map { |s| s['run'] }.compact
[
  'cargo check --workspace --exclude pancetta-research',
  'cargo check -p pancetta --no-default-features',
  'cargo build --workspace --exclude pancetta-research --examples',
  'cargo test --doc --workspace --exclude pancetta-research --features transmit',
  'cargo test -p pancetta --features test-harness --test coord_sim --test tx_ptt_integration',
  'cargo clippy -p pancetta --features test-harness --test coord_sim --test tx_ptt_integration'
].each { |cmd| raise cmd unless commands.include?(cmd) }
raise 'standalone' unless jobs['ft8-tests']['steps'].any? { |s| s['run'] == 'cargo test -p pancetta-ft8' }
raise 'old workspace run' if commands.include?('cargo test --workspace --exclude pancetta-research --features transmit')
filters = jobs['changes']['steps'].find { |s| s['id'] == 'filter' }['with']['filters']
raise 'paths' unless YAML.safe_load(filters).fetch('code').include?('.config/nextest.toml')
ci = jobs.fetch('ci')
%w[workspace-check workspace-tests].each { |lane| raise lane unless ci['needs'].include?(lane) }
raise 'aggregator always' unless ci['if'] == 'always()'
failure = steps.first.fetch('run')
%w[success skipped failure cancelled].each do |result|
  _, _, status = Open3.capture3('bash', '-c', failure.gsub('${{ needs.codex-clean.result }}', result))
  raise "admission #{result}" unless status.success? == %w[success skipped].include?(result)
end
aggregate = ci.fetch('steps').first.fetch('run')
[
  [%w[success success], true], [%w[success skipped], true],
  [%w[success failure], false], [%w[success cancelled], false]
].each do |results, expected|
  require 'json'
  _, _, status = Open3.capture3('bash', '-c', aggregate.gsub('${{ toJSON(needs.*.result) }}', results.to_json))
  raise "aggregate #{results}" unless status.success? == expected
end
puts 'PASS: nextest workflow coverage and gating'
RUBY
python3 <<'PY'
import tomllib
with open(".config/nextest.toml", "rb") as f:
    config = tomllib.load(f)
ci = config["profile"]["ci"]
assert ci["retries"] == 0 and ci["flaky-result"] == "pass"
assert config["test-groups"] == {
    "real-time-budget": {"max-threads": 1},
    "hamlib": {"max-threads": 1},
}
assert ci["overrides"] == [
    {
        "filter": "package(=pancetta-ft8) & binary(=wav_decode_tests) & test(=test_decode_within_realtime_budget)",
        "test-group": "real-time-budget",
        "retries": 1,
    },
    {"filter": "package(=pancetta-hamlib)", "test-group": "hamlib"},
]
assert set(config["profile"]) == {"ci"}, "leave default/local retry behavior unchanged"
print("PASS: scoped nextest retry policy")
PY
