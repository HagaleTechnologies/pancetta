#!/usr/bin/env bash
# PAN-321: guard Cargo configuration coverage and required CI routing.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
ruby -ryaml -ropen3 <<'RUBY'
jobs = YAML.load_file('.github/workflows/ci.yml').fetch('jobs')
workspace = jobs.fetch('workspace-check')
steps = workspace.fetch('steps')
commands = steps.filter_map { |s| s['run'] }
all_commands = jobs.values.flat_map { |j| j.fetch('steps', []).filter_map { |s| s['run'] } }
standalone = 'cargo test -p pancetta-ft8'
transmit = 'cargo test --workspace --exclude pancetta-research --features transmit'
no_default = 'cargo check -p pancetta-ft8 --no-default-features'
examples = 'cargo build --workspace --exclude pancetta-research --examples'
[standalone, transmit, no_default, examples].each do |cmd|
  raise "missing or repeated workspace command: #{cmd}" unless commands.count(cmd) == 1
  raise "repeated workflow command: #{cmd}" unless all_commands.count(cmd) == 1
end
[
  'cargo check -p pancetta-ft8',
  'cargo check --features transmit -p pancetta-ft8',
  'cargo check --workspace --exclude pancetta-research',
  'cargo check -p pancetta --no-default-features'
].each { |cmd| raise "redundant check: #{cmd}" if all_commands.include?(cmd) }
raise 'FT8 job still exists' if jobs.key?('ft8-tests')
raise 'standalone commands must precede examples and workspace tests' unless
  commands.index(no_default) < commands.index(standalone) &&
  commands.index(standalone) < commands.index(examples) &&
  commands.index(examples) < commands.index(transmit)
cache_jobs = jobs.select { |_, j| j.fetch('steps', []).any? { |s| s['uses'].to_s.start_with?('Swatinem/rust-cache@') } }
raise 'wrong cache jobs' unless cache_jobs.keys.sort == %w[clippy cross-platform-check workspace-check]
cache_jobs.each do |name, job|
  caches = job['steps'].select { |s| s['uses'].to_s.start_with?('Swatinem/rust-cache@') }
  raise "#{name}: wrong cache count" unless caches.length == 1
  raise "#{name}: cache must save only on main" unless
    caches.first.fetch('with').fetch('save-if') == "${{ github.ref == 'refs/heads/main' }}"
end
cache_index = steps.index { |s| s['uses'].to_s.start_with?('Swatinem/rust-cache@') }
raise 'standalone check before cache restore' unless cache_index < steps.index { |s| s['run'] == no_default }
clippy = 'cargo clippy --workspace --exclude pancetta-research --all-targets --features transmit -- -D clippy::undocumented_unsafe_blocks'
raise 'Clippy target coverage or lint changed' unless jobs['clippy']['steps'].any? { |s| s['run'] == clippy }
platform = jobs.fetch('cross-platform-check').fetch('steps')
{
  'cargo check' => "runner.os != 'Windows'",
  'cargo check --target ${{ matrix.target }} --workspace --exclude pancetta-research --all-targets --features transmit' => "runner.os == 'Windows'",
  'cargo test -p pancetta --bin pancetta -- responsibility:: mic_access::' => "runner.os == 'macOS'"
}.each do |cmd, condition|
  raise "platform coverage changed: #{cmd}" unless platform.any? { |s| s['run'] == cmd && s['if'] == condition }
end
needs = %w[clippy fmt workspace-check cross-platform-check deny codex-clean scan-trailers research-scan-tests]
raise 'aggregator dependencies changed' unless jobs.fetch('ci').fetch('needs').sort == needs.sort
raise 'aggregator must always run' unless jobs['ci']['if'] == 'always()'
raise 'aggregator no longer rejects failed/cancelled jobs' unless
  jobs['ci']['steps'].any? { |s| s['run'].to_s.include?('grep -q \'"failure"\\|"cancelled"\'') }
raise 'workspace needs changed' unless workspace['needs'] == %w[changes codex-clean]
expected_if = "always() && (needs.changes.outputs.code == 'true' || github.event_name == 'schedule' || github.event_name == 'workflow_dispatch' || (github.event_name == 'merge_group' && needs.changes.outputs.merge_group_notes_only != 'true'))"
raise 'workspace event gate changed' unless workspace['if'] == expected_if
failure = steps.first
raise 'explicit failure step changed' unless failure['name'] == 'Fail explicitly (not silently skip) if codex-clean did not pass' && !failure.key?('if')
script = failure.fetch('run')
raise 'failure step must read codex-clean result' unless script.include?('${{ needs.codex-clean.result }}')
%w[success skipped failure cancelled].each do |result|
  out, err, status = Open3.capture3('bash', '-c', script.gsub('${{ needs.codex-clean.result }}', result))
  raise "#{result}: wrong failure-step result: #{out} #{err}" unless status.success? == %w[success skipped].include?(result)
end
puts 'PASS: CI Cargo coverage, caches and routing'
RUBY
