#!/usr/bin/env bash
# PAN-286: every code PR compiles x86_64-pc-windows-gnu, and a failure blocks the required CI check.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
ruby -ryaml <<'RUBY'
jobs = YAML.load_file('.github/workflows/ci.yml').fetch('jobs')
job = jobs.fetch('cross-platform-check')
legs = job.dig('strategy', 'matrix', 'include') || raise('matrix has no include list')
raise 'no windows-gnu leg' unless legs.include?('os' => 'windows-latest', 'target' => 'x86_64-pc-windows-gnu')
raise 'macos leg lost' unless legs.any? { |l| l['os'] == 'macos-latest' }
raise 'not required by ci' unless jobs.fetch('ci').fetch('needs').include?('cross-platform-check')
steps = job.fetch('steps')
on_windows = steps.select { |s| s['if'].nil? || s['if'] == "runner.os == 'Windows'" }
on_windows.each { |s| raise "#{s['name']}: multi-line run needs shell: bash on Windows" if s['run']&.include?("\n") && s['shell'] != 'bash' }
toolchain = steps.find { |s| s['uses']&.start_with?('dtolnay/rust-toolchain@') } || raise('no toolchain step')
raise 'toolchain lacks matrix target' unless toolchain.dig('with', 'targets') == '${{ matrix.target }}'
runs = on_windows.map { |s| s['run'].to_s }
raise 'no ft8_lib guard' unless runs.any? { |r| r.include?('pancetta-ft8/vendor/ft8_lib/ft8/constants.c') }
raise 'no MinGW gcc assert' unless runs.any? { |r| r.strip == 'gcc --version' }
check = runs.find { |r| r.include?('cargo check --target ${{ matrix.target }}') } || raise('no windows-gnu cargo check')
raise 'windows check must exclude pancetta-research' unless check.include?('--exclude pancetta-research')
mic = steps.find { |s| s['run'].to_s.include?('mic_access::') } || raise('mic test step missing')
raise 'mic test must be macOS-only' unless mic['if'] == "runner.os == 'macOS'"
puts 'PASS: windows-gnu PR lane (PAN-286)'
RUBY
