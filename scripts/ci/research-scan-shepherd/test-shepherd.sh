#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
dir="$PWD/scripts/ci/research-scan-shepherd"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export PATH="$dir/stub:$PATH" STUB_DIR="$dir/stub" REPO=example/pancetta
export GH_TOKEN=test-workflow-token RESOLVE_TOKEN=test-user-token ONLY_PR=420
export REPORT_MENTION=thagale RUN_URL=https://example.invalid/run/1
# P replays B's data with only the re-arm marker comment failing; Q replays B with the
# queue handler re-pausing the PR as green checks queue it. R is armed and queued past the
# round cap.
for scenario in A B C D E F G H I J K L M N O P Q R; do
  export STUB_CASE=$scenario STUB_FAIL_REARM_COMMENT='' STUB_PAUSE_ON_ARM=''
  [[ $scenario == P ]] && export STUB_CASE=B STUB_FAIL_REARM_COMMENT=1
  [[ $scenario == Q ]] && export STUB_CASE=B STUB_PAUSE_ON_ARM=1
  export STUB_LOG="$tmp/$scenario.log" STUB_STATE="$tmp/$scenario"
  export GITHUB_STEP_SUMMARY="$tmp/$scenario.summary"
  mkdir -p "$STUB_STATE"
  export DRY_RUN=false RESOLVE_TOKEN=test-user-token
  [[ $scenario == I ]] && export RESOLVE_TOKEN=
  [[ $scenario == A || $scenario == L ]] && export DRY_RUN=true
  status=0
  bash "$dir/shepherd.sh" > "$tmp/$scenario.out" 2>&1 || status=$?
  expected=0; [[ $scenario == D || $scenario == H || $scenario == I || $scenario == J || $scenario == O || $scenario == P || $scenario == R ]] && expected=1
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
  raise 'rearm order' unless delete < comment && comment < arm
  raise 'arm PAT' unless calls[arm]['token'] == 'test-user-token'
  raise 'disarmed a clean arm' if calls.any? { |c| c['args'].include?('--disable-auto') }
  raise 'delete GITHUB_TOKEN' unless calls[delete]['token'] == 'test-workflow-token'
when 'C', 'M', 'N'
  resolves = calls.each_index.select { |i| calls[i]['args'].join(' ').include?('resolveReviewThread(') }
  raise 'resolve count/token' unless resolves.length == 2 && resolves.all? { |i| calls[i]['token'] == 'test-user-token' }
  # Q-0068: a fresh request (audit marker, "@codex review", head marker), never a rerun.
  audit = find.call('<!-- research-scan-shepherd:refresh sha=1a2b3c400000 -->')
  request = find.call('body=@codex review')
  marker = find.call('body=<!-- codex-review-request head_sha=1a2b3c4000000000000000000000000000000000 -->')
  ci = find.call('run rerun 801 --failed')
  raise 'refresh order' unless resolves.max < audit && audit < request && request < marker && marker < ci
  raise 'request identity' unless calls[request]['token'] == 'test-user-token' && calls[marker]['token'] == 'test-user-token'
  raise 'audit identity' unless calls[audit]['token'] == 'test-workflow-token'
  raise 'reran a Codex run' if calls.any? { |c| c['args'][0,2] == ['run','rerun'] && !c['args'].include?('--failed') }
  raise 'looked up a pull_request_review run' if calls.any? { |c| c['args'].join(' ').include?('event=pull_request_review') }
when 'D'
  find.call('label create research-scan-stuck')
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
  raise 'restyled label' if calls.any? { |c| c['args'][0,2] == ['label','create'] }
when 'K'
  find.call('issue comment 92')
  find.call('PR #420 is merged; closing.')
  find.call('issue close 92')
when 'O'
  raise 'reported an ineligible unreadable PR' unless mutations.empty?
  raise 'missing unreadable warning' unless File.read(output).include?('snapshot-unreadable')
when 'P'
  find.call('-X DELETE')
  find.call('<!-- research-scan-shepherd:rearm sha=1a2b3c400000 -->')
  raise 'armed without a recorded attempt' if calls.any? { |c| c['args'][0,2] == ['pr','merge'] }
  raise 'missing failure report' unless File.read(output).include?('mutation or refresh failed')
when 'Q'
  comment = find.call('<!-- research-scan-shepherd:rearm sha=1a2b3c400000 -->')
  arm = find.call('pr merge 420 --repo example/pancetta --auto --squash')
  disarm = find.call('pr merge 420 --repo example/pancetta --disable-auto')
  dequeue = find.call('dequeuePullRequest(')
  raise 'race order' unless comment < arm && arm < disarm && disarm < dequeue
  raise 'dequeue token' unless calls[dequeue]['args'].include?('id=PR_node_420')
  raise 'pause override not reported' unless File.read(output).include?('producer became busy')
when 'R'
  disarm = find.call('pr merge 420 --repo example/pancetta --disable-auto')
  dequeue = find.call('dequeuePullRequest(')
  report = find.call('issue create')
  raise 'round-cap order' unless disarm < dequeue && dequeue < report
  raise 'armed past the round cap' if calls.any? { |c| c['args'].include?('--auto') }
  raise 'missing round-cap blocker' unless File.read(output).include?('round-cap-exceeded')
when 'E'
  raise 'busy mutation' unless mutations.empty?
  raise 'missing skip notice' unless File.read(output).include?('skip')
end
puts "PASS: shepherd #{scenario}"
RUBY
done
