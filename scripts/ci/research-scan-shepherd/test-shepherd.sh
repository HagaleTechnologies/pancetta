#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
dir="$PWD/scripts/ci/research-scan-shepherd"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export PATH="$dir/stub:$PATH" STUB_DIR="$dir/stub" REPO=example/pancetta
export GH_TOKEN=test-workflow-token RESOLVE_TOKEN=test-user-token CHECKS_TOKEN=test-app-token ONLY_PR=420 GITHUB_RUN_ID=555
export SETTLE_TRIES=3 SETTLE_INTERVAL=0
export REPORT_MENTION=thagale RUN_URL=https://example.invalid/run/1
# P replays B's data with only the re-arm marker comment failing; Q replays B with the
# queue handler re-pausing the PR as green checks queue it. R is armed and queued past the
# round cap. S replays C with the gate check-run PATCH refused (created by another app).
# T-V replay C racing the response job: T, a response fails the gate just after the request;
# U, the same race but Codex has already replied to the request; V, response runs never settle;
# W, V with the run-wide settle budget already spent.
for scenario in A B C D E F G H I J K L M N O P Q R S T U V W; do
  export STUB_CASE=$scenario STUB_FAIL_REARM_COMMENT='' STUB_PAUSE_ON_ARM='' STUB_FAIL_CHECK_PATCH=''
  export STUB_RACE_AFTER_REQUEST='' STUB_CODEX_ANSWERED='' STUB_ALWAYS_INFLIGHT=''
  [[ $scenario == S ]] && export STUB_CASE=C STUB_FAIL_CHECK_PATCH=1
  [[ $scenario == T ]] && export STUB_CASE=C STUB_RACE_AFTER_REQUEST=1
  [[ $scenario == U ]] && export STUB_CASE=C STUB_RACE_AFTER_REQUEST=1 STUB_CODEX_ANSWERED=1
  [[ $scenario == V ]] && export STUB_CASE=C STUB_ALWAYS_INFLIGHT=1
  export SETTLE_TRIES=3 SETTLE_BUDGET=360
  [[ $scenario == W ]] && export STUB_CASE=C STUB_ALWAYS_INFLIGHT=1 SETTLE_TRIES=100 SETTLE_BUDGET=0
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
  # Q-0068: a fresh request ("@codex review", head marker, then the budget-spending audit marker), never a rerun.
  audit = find.call('<!-- research-scan-shepherd:refresh sha=1a2b3c400000 -->')
  request = find.call('body=@codex review')
  marker = find.call('body=<!-- codex-review-request head_sha=1a2b3c4000000000000000000000000000000000 -->')
  ci = find.call('run rerun 801 --failed')
  # The stub's gate is completed/failure (check-run 3), so it is reopened first.
  reopen = find.call('-X PATCH repos/example/pancetta/check-runs/3 -f status=in_progress -f external_id=555')
  settle = ->(after) { calls.each_index.find { |i| i > after && calls[i]['args'].join(' ').include?('--workflow wait-for-codex.yml --event issue_comment') } || raise('missing settle') }
  pre = calls.each_index.find { |i| calls[i]['args'].join(' ').include?('--workflow wait-for-codex.yml --event issue_comment') }
  raise 'refresh order' unless resolves.max < pre && pre < reopen && reopen < request && request < marker &&
    marker < settle.call(marker) && settle.call(marker) < audit && audit < ci
  raise 'reopen identity' unless calls[reopen]['token'] == 'test-app-token'
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
when 'S'
  patch = find.call('-X PATCH repos/example/pancetta/check-runs/3')
  create = find.call('-X POST repos/example/pancetta/check-runs -f name=Codex review clean (gate before expensive downstream jobs) -f head_sha=1a2b3c4000000000000000000000000000000000 -f status=in_progress')
  request = find.call('body=@codex review')
  raise 'fallback order' unless patch < create && create < request
  raise 'fallback identity' unless calls[create]['token'] == 'test-app-token'
when 'T'
  reopens = calls.each_index.select { |i| calls[i]['args'].join(' ').include?('-X PATCH repos/example/pancetta/check-runs/3') }
  request = find.call('body=@codex review')
  audit = find.call('<!-- research-scan-shepherd:refresh sha=1a2b3c400000 -->')
  raise 'race not repaired' unless reopens.length == 2 && reopens[0] < request && request < reopens[1] && reopens[1] < audit
  raise 'missing race notice' unless File.read(output).include?('Codex gate failed after the request')
when 'U'
  reopens = calls.each_index.select { |i| calls[i]['args'].join(' ').include?('-X PATCH repos/example/pancetta/check-runs/3') }
  raise 'clobbered an answered verdict' unless reopens.length == 1 && reopens[0] < find.call('body=@codex review')
  find.call('<!-- research-scan-shepherd:refresh sha=1a2b3c400000 -->')
when 'V'
  find.call('body=@codex review')
  raise 'spent the budget on an unverified gate' if calls.any? { |c| c['args'].join(' ').include?('research-scan-shepherd:refresh') }
  raise 'missing settle notice' unless File.read(output).include?('response run is still pending')
when 'W'
  request = find.call('body=@codex review')
  polls = calls.each_index.count { |i| i > request && calls[i]['args'].join(' ').include?('--workflow wait-for-codex.yml --event issue_comment') }
  raise "waited past the run-wide settle budget (#{polls} polls)" unless polls == 1
  raise 'spent the budget on an unverified gate' if calls.any? { |c| c['args'].join(' ').include?('research-scan-shepherd:refresh') }
when 'E'
  raise 'busy mutation' unless mutations.empty?
  raise 'missing skip notice' unless File.read(output).include?('skip')
end
puts "PASS: shepherd #{scenario}"
RUBY
done
