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
doc = 'cargo test --doc --workspace --exclude pancetta-research --features transmit'
no_default = 'cargo check -p pancetta-ft8 --no-default-features'
examples = 'cargo build --workspace --exclude pancetta-research --examples'
[standalone, doc, no_default, examples].each do |cmd|
  raise "missing or repeated workspace command: #{cmd}" unless commands.count(cmd) == 1
  raise "repeated workflow command: #{cmd}" unless all_commands.count(cmd) == 1
end
# PAN-314 moved the transmit suite to the sharded workspace-tests job; it still runs once.
transmit = 'cargo nextest run --profile ci --workspace --exclude pancetta-research --features transmit --partition count:${{ matrix.partition }}/4 --no-fail-fast'
raise 'transmit suite must run once, in workspace-tests' unless all_commands.count(transmit) == 1 &&
  jobs.fetch('workspace-tests').fetch('steps').any? { |s| s['run'] == transmit }
raise 'duplicate transmit test run' if all_commands.include?('cargo test --workspace --exclude pancetta-research --features transmit')
[
  'cargo check -p pancetta-ft8',
  'cargo check --features transmit -p pancetta-ft8',
  'cargo check --workspace --exclude pancetta-research',
  'cargo check -p pancetta --no-default-features'
].each { |cmd| raise "redundant check: #{cmd}" if all_commands.include?(cmd) }
raise 'FT8 job still exists' if jobs.key?('ft8-tests')
raise 'standalone commands must precede examples and workspace doc tests' unless
  commands.index(no_default) < commands.index(standalone) &&
  commands.index(standalone) < commands.index(examples) &&
  commands.index(examples) < commands.index(doc)
cache_jobs = jobs.select { |_, j| j.fetch('steps', []).any? { |s| s['uses'].to_s.start_with?('Swatinem/rust-cache@') } }
raise 'wrong cache jobs' unless cache_jobs.keys.sort == %w[clippy cross-platform-check research-check workspace-check workspace-tests]
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
needs = %w[clippy fmt workspace-check workspace-tests cross-platform-check deny codex-clean scan-trailers research-scan-tests]
raise 'aggregator dependencies changed' unless jobs.fetch('ci').fetch('needs').sort == needs.sort
raise 'aggregator must always run' unless jobs['ci']['if'] == 'always()'
raise 'aggregator no longer rejects failed/cancelled jobs' unless
  jobs['ci']['steps'].any? { |s| s['run'].to_s.include?('grep -q \'"failure"\\|"cancelled"\'') }
raise 'workspace needs changed' unless workspace['needs'] == %w[changes codex-clean]
# PAN-372: keep the research lane path-filtered, compile-only, and non-required.
research = jobs.fetch('research-check')
raise 'Research Check must never join the aggregator needs' if jobs.fetch('ci').fetch('needs').include?('research-check')
raise 'Research Check display name changed' unless research['name'] == 'Research Check'
raise 'Research Check must depend only on changes' unless Array(research['needs']) == %w[changes]
rc_if = "always() && (needs.changes.outputs.research == 'true' || github.event_name == 'schedule' || github.event_name == 'workflow_dispatch' || (github.event_name == 'merge_group' && needs.changes.outputs.merge_group_notes_only != 'true'))"
raise 'Research Check event gate changed' unless research['if'] == rc_if
rc_cmds = research.fetch('steps').filter_map { |s| s['run'] }
['cargo check -p pancetta-research --all-targets',
 'cargo check -p pancetta-research --all-targets --features research-eval'].each do |cmd|
  raise "Research Check lost its compile command: #{cmd}" unless rc_cmds.include?(cmd)
end
raise 'Research Check must stay compile-only (no test runs)' if
  rc_cmds.any? { |c| c.start_with?('cargo test') || c.start_with?('cargo nextest') }
filters = jobs.fetch('changes').fetch('steps').find { |s| s.is_a?(Hash) && s.dig('with', 'filters') }.dig('with', 'filters')
raise 'changes job lost the research filter' unless filters.is_a?(String) && filters.include?("research:\n")
raise 'changes job lost the research output' unless jobs.fetch('changes').fetch('outputs').fetch('research') == '${{ steps.filter.outputs.research }}'
%w['pancetta-research/**' 'pancetta-ft8/**' 'pancetta-qso/**' 'pancetta-dsp/**' 'Cargo.toml' 'Cargo.lock' 'rust-toolchain.toml' 'pancetta-ft8/vendor/ft8_lib' '.github/workflows/ci.yml'].each do |pat|
  raise "research filter lost a path: #{pat}" unless filters.include?(pat)
end
expected_if = "always() && ((needs.changes.outputs.code == 'true' && needs.changes.outputs.merge_group_verified != 'true') || github.event_name == 'schedule' || github.event_name == 'workflow_dispatch' || (github.event_name == 'merge_group' && needs.changes.outputs.merge_group_notes_only != 'true'))"
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

# PAN-372 remediation: the guard's sanctioned forms must excuse only their own
# occurrence. A workflow line that pairs an allowed form (a compile-only check
# command, an --exclude flag, the path-filter glob) with a forbidden research
# reference -- a research test run on the same line -- must still trip the
# guard. Each fixture writes a one-line workflow inside a disposable git repo
# and runs the shipped script against it. PAN-456 adds a rejecting row for
# every forbidden term, including the `--bin <name>` terms grep once parsed as
# options and so never matched.
repo_root=$PWD
guard_repo=$(mktemp -d)
trap 'rm -rf "$guard_repo"' EXIT
git init -q "$guard_repo"
mkdir -p "$guard_repo/.github/workflows"
guard_case() {
    local expect=$1 line=$2 got
    printf '%s\n' "$line" > "$guard_repo/.github/workflows/ci.yml"
    if (cd "$guard_repo" && bash "$repo_root/scripts/research-env.sh" --guard-ci) >/dev/null 2>&1; then
        got=accept
    else
        got=reject
    fi
    if [ "$got" != "$expect" ]; then
        echo "FAIL: guard verdict was '$got', expected '$expect': $line" >&2
        exit 1
    fi
}
# A research test run stays forbidden, alone or sharing a line with an
# allowed form.
guard_case reject "        run: cargo test -p pancetta-research --all-targets"
guard_case reject "        run: cargo check -p pancetta-research --all-targets && cargo test -p pancetta-research --all-targets"
guard_case reject "        run: cargo check -p pancetta-research --all-targets --features research-eval && cargo test -p pancetta-research"
guard_case reject "        run: cargo build --workspace --exclude pancetta-research && cargo test -p pancetta-research"
guard_case reject "        run: cargo test -p pancetta-research && echo 'pancetta-research/**'"
# The sanctioned forms stay allowed on their own lines.
guard_case accept "        run: cargo check -p pancetta-research --all-targets"
guard_case accept "        run: cargo check -p pancetta-research --all-targets --features research-eval"
guard_case accept "        run: cargo clippy --workspace --exclude pancetta-research --all-targets -- -D warnings"
guard_case accept "              - 'pancetta-research/**'"
guard_case accept "          # every Rust lane passes --exclude pancetta-research"
# PAN-456: terms that start with `--` must reach grep as a pattern, not an
# option. A research bin run is forbidden alone and next to an allowed check.
guard_case reject "        run: cargo run --bin eval"
guard_case reject "        run: cargo run --bin compare"
guard_case reject "        run: cargo run --bin leaderboard"
guard_case reject "        run: cargo run --bin curate"
guard_case reject "        run: cargo run --bin baseline"
guard_case reject "        run: cargo check -p pancetta-research --all-targets && cargo run --bin eval"
# Every other forbidden term trips the guard too, so none can fail open
# silently.
guard_case reject "        run: ./scripts/research-env.sh --status"
guard_case reject "        run: cat research/scorecards/x.md"
guard_case reject "        run: cat research/experiments/x.md"
guard_case reject "        run: cat research/baselines/x.md"
guard_case reject "        run: cat research/corpus/x.md"
# A whole-line comment naming a research bin stays allowed.
guard_case accept "          # cargo run --bin eval is local only"
echo "PASS: research guard allowances are occurrence-scoped and every forbidden term is caught"
