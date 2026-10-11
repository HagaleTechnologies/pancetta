#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
dir="$PWD/scripts/ci/research-scan-shepherd"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export PATH="$dir/stub:$PATH" STUB_DIR="$dir/stub" REPO=example/pancetta
export GH_TOKEN=test-workflow-token RESOLVE_TOKEN=test-user-token ONLY_PR=420
export REPORT_MENTION=thagale RUN_URL=https://example.invalid/run/1
for scenario in A B C D E F G H I J K L M; do
  export STUB_CASE=$scenario STUB_LOG="$tmp/$scenario.log" STUB_STATE="$tmp/$scenario"
  export GITHUB_STEP_SUMMARY="$tmp/$scenario.summary"
  mkdir -p "$STUB_STATE"
  export DRY_RUN=false RESOLVE_TOKEN=test-user-token
  [[ $scenario == I ]] && export RESOLVE_TOKEN=
  [[ $scenario == A || $scenario == L ]] && export DRY_RUN=true
  status=0
  bash "$dir/shepherd.sh" > "$tmp/$scenario.out" 2>&1 || status=$?
  expected=0; [[ $scenario == D || $scenario == H || $scenario == I || $scenario == J ]] && expected=1
  if [[ $status != "$expected" ]]; then cat "$tmp/$scenario.out"; exit 1; fi
  ruby -rjson - "$scenario" "$STUB_LOG" "$tmp/$scenario.out" <<'RUBY'
scenario, log, output = ARGV
calls = File.readlines(log).map { |l| JSON.parse(l) }
mutation = ->(c) { a=c['args']; a.include?('rerun') || a.include?('merge') || a.include?('create') || a.include?('edit') || a.include?('close') || a.any? { |x| x.include?('resolveReviewThread(') } || a.include?('-X') }
mutations = calls.select(&mutation)
find = ->(pattern) { calls.index { |c| c['args'].join(' ').include?(pattern) } || raise("missing #{pattern}") }
case scenario
when 'A', 'L'
  raise 'dry run mutated' unless mutations.empty?
  decisions = File.readlines(output).filter_map { |l| JSON.parse(l) rescue nil }
  raise 'dry run decision' unless decisions.any? { |d| d['resolve']&.length == 3 }
when 'B'
  delete = find.call('-X DELETE')
  arm = find.call('pr merge 420 --repo example/pancetta --auto --squash --match-head-commit 1a2b3c4000000000000000000000000000000000')
  comment = find.call('<!-- research-scan-shepherd:rearm sha=1a2b3c400000 -->')
  raise 'rearm order' unless delete < arm && arm < comment
  raise 'arm PAT' unless calls[arm]['token'] == 'test-user-token'
  raise 'delete GITHUB_TOKEN' unless calls[delete]['token'] == 'test-workflow-token'
when 'C', 'M'
  resolves = calls.each_index.select { |i| calls[i]['args'].join(' ').include?('resolveReviewThread(') }
  raise 'resolve count/token' unless resolves.length == 2 && resolves.all? { |i| calls[i]['token'] == 'test-user-token' }
  codex = find.call('run rerun 802'); ci = find.call('run rerun 801 --failed')
  raise 'refresh order' unless resolves.max < codex && codex < ci
  raise 'reran human review' if calls.any? { |c| c['args'][0,3] == ['run','rerun','803'] }
when 'D'
  find.call('issue create')
  find.call('research-scan-stuck')
  find.call('@thagale')
  find.call('<!-- research-scan-shepherd:status -->')
when 'F', 'G'
  raise 'stale state mutated' unless mutations.empty?
when 'H', 'I'
  raise 'lifecycle mutation after failure' if mutations.any? { |c| c['args'].include?('merge') || c['args'].include?('rerun') || c['args'].join(' ').include?('resolveReviewThread(') }
  find.call('issue create')
  find.call('<!-- research-scan-shepherd:status -->')
when 'J'
  find.call('-X PATCH repos/example/pancetta/issues/comments/91')
  find.call('issue edit 92')
  raise 'duplicate report' if calls.any? { |c| c['args'][0,2] == ['issue','create'] || c['args'].include?('POST') }
when 'K'
  find.call('issue comment 92')
  find.call('PR #420 is merged; closing.')
  find.call('issue close 92')
when 'E'
  raise 'busy mutation' unless mutations.empty?
  raise 'missing skip notice' unless File.read(output).include?('skip')
end
puts "PASS: shepherd #{scenario}"
RUBY
done
