#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
ruby -ryaml -rjson -rtmpdir -rfileutils -ropen3 <<'RUBY'
admission = YAML.load_file('.github/workflows/auto-merge-trigger.yml')
admission_body = admission.fetch('jobs').fetch('auto-merge').fetch('steps')
  .find { |s| s['name'] == 'Enable auto-merge' }.fetch('run')
retry_workflow = YAML.load_file('.github/workflows/queue-retry-handler.yml')
steps = retry_workflow.fetch('jobs').fetch('handle-merge-group-failure').fetch('steps')
step = ->(id) { steps.find { |s| s['id'] == id } or raise "missing step #{id}" }
handled = steps.find { |s| s['name'] == 'Mark this merge-group attempt as handled' }
raise 'missing handled step' unless handled
head = 'a' * 40
attempt_label = "queue-attempt-handled:#{'c' * 12}"
retry_label = "queue-retry-used:#{head[0, 12]}"
pause = 'queue-needs-human-review'

# Evaluate only the expression vocabulary used by these actual YAML conditions.
# Unknown syntax fails, so a workflow change cannot silently evade this adapter.
def condition(expression, values)
  expr = expression.gsub('always()', 'true')
  expr = expr.gsub(/(?:steps|needs)\.[\w-]+\.(?:outputs\.[\w-]+|conclusion)/) do |key|
    "'#{values.fetch(key)}'"
  end
  expr = expr.gsub(/'([^']*)' == '([^']*)'/) { (Regexp.last_match(1) == Regexp.last_match(2)).to_s }
  raise "unknown condition: #{expr}" unless expr.gsub(/true|false|&&|\|\||[()\s]/, '').empty?
  eval(expr)
end

def function(body, name)
  body.match(/^( *)#{Regexp.escape(name)}\(\) \{\n.*?^\1\}/m)&.[](0) or
    raise "missing function #{name}"
end

arm = %w[native_merge_queue_installed arm_auto_merge].map { |n| function(admission_body, n) }.join("\n")
staleness = function(admission_body, 'needs_human_is_stale')
count = 0
Dir.mktmpdir('pan150-queue-') do |dir|
  # Every gh call goes through this stateful mock. Unexpected calls are persisted
  # as failures even when the workflow deliberately ignores a command's exit code.
  File.write(File.join(dir, 'gh'), <<~'MOCK')
    #!/usr/bin/env ruby
    require 'json'
    file = ENV.fetch('MOCK_STATE')
    s = JSON.parse(File.read(file))
    args = ARGV
    s['calls'] << {'args' => args, 'token' => ENV.fetch('GH_TOKEN')}
    result = nil
    code = 0
    label_read = ['api', '--paginate', 'repos/test/repo/issues/17/labels', '--jq', '.[].name']
    post_label = args.length == 6 && args[0] == 'api' && %w[-X --method].include?(args[1]) &&
      args[2..4] == ['POST', 'repos/test/repo/issues/17/labels', '-f'] &&
      args[5].start_with?('labels[]=queue-')
    case args
    when ['pr', 'view', '17', '--repo', 'test/repo', '--json', 'baseRefName', '--jq', '.baseRefName']
      result = s['base']
    when ['api', 'repos/test/repo/rules/branches/main']
      code = 1 if s['rules'] == 'error'
      result = s['rules'] == 'present' ? '[{"type":"merge_queue"}]' : '[]'
    when ['api', '--paginate', '--slurp', 'repos/test/repo/issues/17/timeline'],
         ['api', '--paginate', '--slurp', 'repos/test/repo/issues/17/comments']
      kind = args.last.split('/').last
      case s["#{kind}_mode"]
      when 'error' then code = 1
      when 'invalid' then result = '{broken'
      when 'wrong-shape' then result = '{"message":"not pages"}'
      else
        # gh --paginate --slurp returns arrays of pages, including empty pages.
        pages = s[kind].each_slice(2).to_a
        result = JSON.generate(pages.empty? ? [[]] : pages)
      end
    when ['pr', 'merge', 'https://example.invalid/pr/17', '--auto', '--squash']
      code = 1 if s['merge_error']
      s['arms'] += 1 if code == 0
    when label_read
      result = s['labels'].join("\n")
    when ['pr', 'view', '17', '--repo', 'test/repo', '--json', 'headRefOid', '--jq', '.headRefOid']
      result = s['head']
    when ['pr', 'view', '17', '--repo', 'test/repo', '--json', 'autoMergeRequest', '--jq', '.autoMergeRequest != null']
      result = s['armed'].to_s
    when ['pr', 'merge', '17', '--repo', 'test/repo', '--auto', '--squash', '--match-head-commit', 'a' * 40]
      raise 'arm must use merge token' unless ENV['GH_TOKEN'] == 'merge-token'
      code = 1 if s['merge_error'] || s['head'] != 'a' * 40
      if code == 0
        s['arms'] += 1
        s['armed'] = true
        s['mutations'] << 'arm'
      end
    when ['pr', 'merge', '17', '--repo', 'test/repo', '--disable-auto']
      s['armed'] = false unless s['disarm_error']
      s['mutations'] << 'disarm'
    else
      if post_label
        label = args[5].delete_prefix('labels[]=')
        # A maintainer can attach the label before our idempotent POST arrives.
        if label == 'queue-needs-human-review' && s['human_attach_race']
          s['labels'] |= [label]
          s['timeline'] << {'event' => 'labeled', 'label' => {'name' => label},
                           'actor' => {'login' => 'maintainer'}, 'created_at' => s['now']}
          s['human_attach_race'] = false
        end
        # A POST can exit 0 without the label becoming visible on the PR.
        unless s['labels'].include?(label) || s['unattachable'].include?(label)
          s['labels'] << label
          s['timeline'] << {'event' => 'labeled', 'label' => {'name' => label},
                           'actor' => {'login' => 'github-actions[bot]'}, 'created_at' => s['now']}
        end
        s['mutations'] << "label:#{label}"
      elsif args.length == 9 && args[0..1] == ['label', 'create'] &&
            args[2].start_with?('queue-') && args[3] == '--color' &&
            args[5] == '--description' && args[7..8] == ['--repo', 'test/repo']
        # Creating the label definition does not attach it to the PR.
      elsif args.length == 7 && args[0..5] == ['pr', 'comment', '17', '--repo', 'test/repo', '--body']
        # A maintainer can remove and re-attach the pause after the ownership read
        # but before this comment exists, or the timeline can stop answering.
        if s['reattach_before_comment']
          s['timeline'] << {'event' => 'unlabeled', 'label' => {'name' => 'queue-needs-human-review'}}
          s['timeline'] << {'event' => 'labeled', 'label' => {'name' => 'queue-needs-human-review'},
                           'actor' => {'login' => 'maintainer'}, 'created_at' => s['now']}
          s['reattach_before_comment'] = false
        end
        s['timeline_mode'] = 'error' if s['timeline_error_after_comment']
        if s['post_error']
          code = 1
        else
          id = s['comments'].length + 1
          s['comments'] << {'id' => id, 'body' => args[6], 'user' => {'login' => 'github-actions[bot]'},
                           'created_at' => s['now']}
          s['mutations'] << "comment:#{id}"
          result = s['missing_comment_id'] ? '' : "https://example.invalid/pr/17#issuecomment-#{id}"
        end
      elsif args.length == 4 && args[0] == 'api' &&
            args[1].match?(%r{\Arepos/test/repo/issues/comments/[0-9]+\z}) && args[2..3] == ['--jq', '.body']
        s['mutations'] << "readback:#{args[1].split('/').last}"
        comment = s['comments'].find { |c| c['id'].to_s == args[1].split('/').last }
        raise 'readback of unknown comment' unless comment
        code = 1 if s['readback_error']
        result = s['readback_missing_marker'] ? 'unmarked body' : comment['body']
      elsif args.length == 6 && args[0..2] == ['api', '--method', 'PATCH'] &&
            args[3].match?(%r{\Arepos/test/repo/issues/comments/[0-9]+\z}) &&
            args[4] == '-f' && args[5].start_with?('body=')
        id = args[3].split('/').last
        comment = s['comments'].find { |c| c['id'].to_s == id }
        raise 'edit of unknown comment' unless comment
        if s['patch_error']
          code = 1
        else
          comment['body'] = args[5].delete_prefix('body=')
          s['mutations'] << "edit:#{id}"
        end
      elsif args[0..2] == ['api', 'graphql', '-f'] &&
            args[4..9] == ['-f', 'owner=test', '-f', 'name=repo', '-F', 'pr=17'] &&
            args.length == 10 && args[3].include?('query(') && args[3].include?('isInMergeQueue')
        result = JSON.generate({'data' => {'repository' => {'pullRequest' =>
          {'id' => 'PR_17', 'isInMergeQueue' => s['queued']}}}})
      elsif args.length == 6 && args[0..2] == ['api', 'graphql', '-f'] &&
            args[3].include?('dequeuePullRequest') && args[4..5] == ['-f', 'id=PR_17']
        s['queued'] = false unless s['dequeue_error']
        s['mutations'] << 'dequeue'
        result = '{}'
      else
        s['unexpected'] << args
        code = 99
      end
    end
    File.write(file, JSON.generate(s))
    puts result if code == 0 && !result.nil?
    exit code
  MOCK
  FileUtils.chmod(0755, File.join(dir, 'gh'))
  state_path = File.join(dir, 'state.json')
  env = {'PATH' => "#{dir}:#{ENV.fetch('PATH')}", 'MOCK_STATE' => state_path,
         'GH_TOKEN' => 'bot-token', 'MERGE_TOKEN' => 'merge-token',
         'REPO' => 'test/repo', 'PR' => '17', 'HEAD_SHA' => head, 'HEAD_SHA12' => head[0, 12],
         'MERGE_GROUP_SHA' => 'c' * 40, 'RUN_URL' => 'https://example.invalid/run/1',
         'RETRY_LABEL' => retry_label, 'GITHUB_OUTPUT' => File.join(dir, 'output')}
  fresh = lambda do |overrides = {}|
    state = {'base' => 'main', 'rules' => 'present', 'arms' => 0,
             'calls' => [], 'unexpected' => [], 'labels' => [], 'comments' => [], 'unattachable' => [],
             'timeline' => [], 'mutations' => [], 'armed' => false, 'queued' => false,
             'head' => head, 'now' => '2026-10-20T12:00:00Z'}.merge(overrides)
    File.write(state_path, JSON.generate(state))
  end
  state = -> { JSON.parse(File.read(state_path)) }
  run = lambda do |body|
    raise 'unresolved expression' if body.include?('${{')
    script = File.join(dir, 'step.sh')
    File.write(env.fetch('GITHUB_OUTPUT'), '')
    File.write(script, body)
    out, err, syntax = Open3.capture3('bash', '-n', script)
    raise "syntax: #{out}#{err}" unless syntax.success?
    out, err, status = Open3.capture3(env, 'bash', '--noprofile', '--norc', '-eo', 'pipefail', script)
    raise "unexpected gh: #{state.call['unexpected']}" unless state.call['unexpected'].empty?
    [status.exitstatus, out + err]
  end
  patch_state = lambda do |changes|
    File.write(state_path, JSON.generate(state.call.merge(changes)))
  end
  outputs = lambda do
    File.readlines(env.fetch('GITHUB_OUTPUT'), chomp: true).to_h { |line| line.split('=', 2) }
  end
  # Execute dedup, budget selection, the chosen action and mark-handled with
  # retained GitHub state, actual exit codes and the real YAML conditions.
  event = lambda do
    values = {'needs.check-pr-state.outputs.stale' => 'false',
              'steps.check-retried.outputs.already_retried' => '',
              'steps.retry.conclusion' => 'skipped', 'steps.handoff.conclusion' => 'skipped'}
    %w[dedup check-retried retry handoff].each do |id|
      current = step.call(id)
      next if current['if'] && !condition(current['if'], values)
      code, output = run.call(current.fetch('run'))
      values["steps.#{id}.conclusion"] = code == 0 ? 'success' : 'failure'
      outputs.call.each { |key, val| values["steps.#{id}.outputs.#{key}"] = val }
      # Actions implicitly skips steps after failure unless they use always().
      break if code != 0
    end
    if condition(handled.fetch('if'), values)
      body = handled.fetch('run').gsub('${{ steps.dedup.outputs.attempt_label }}',
                                      values.fetch('steps.dedup.outputs.attempt_label'))
      code, output = run.call(body)
      raise "mark handled: #{output}" unless code == 0
    end
    values
  end
  check = lambda do |name, &assertions|
    assertions.call
    count += 1
    puts "PASS: #{name}"
  end
  label_event = lambda do |actor = 'github-actions[bot]', time = '2026-10-20T12:00:00Z'|
    {'event' => 'labeled', 'label' => {'name' => pause},
     'actor' => {'login' => actor}, 'created_at' => time}
  end
  marker = lambda do |sha = head[0, 12], time = '2026-10-20T12:00:00Z', actor = 'github-actions[bot]'|
    {'id' => 1, 'user' => {'login' => actor}, 'created_at' => time,
     'body' => "<!-- queue-retry-handler:queue-needs-human-review sha=#{sha} -->"}
  end
  marker_comments = -> { state.call['comments'].select { |c| c['body'].include?('queue-retry-handler:queue-needs-human-review') } }
  handoff_state = lambda do |overrides = {}|
    fresh.call({'labels' => [retry_label, pause], 'timeline' => [label_event.call]}.merge(overrides))
  end
  # The Retry step re-arms, but its retry-used label never becomes visible.
  fallback_state = ->(o = {}) { fresh.call({'unattachable' => [retry_label]}.merge(o)) }
  failed_handoff = lambda do
    result = event.call
    raise 'handoff did not fail' unless result['steps.handoff.conclusion'] == 'failure'
    raise 'lost pause' unless state.call['labels'].include?(pause)
    raise 'failed handoff marked handled' if state.call['labels'].include?(attempt_label)
    raise 'handoff armed' unless state.call['arms'] == 0
  end
  successful_handoff = lambda do
    result = event.call
    raise 'handoff did not succeed' unless result['steps.handoff.conclusion'] == 'success'
    raise 'missing handled label' unless state.call['labels'].include?(attempt_label)
    raise 'lost pause' unless state.call['labels'].include?(pause)
    raise 'handoff armed' unless state.call['arms'] == 0
  end
  stale = lambda do |sha|
    code, = run.call("#{staleness}\nneeds_human_is_stale '#{sha}'\n")
    code == 0
  end
  check.call('complete changed workflow bodies pass Bash syntax checks and test is discovered by CI') do
    bodies = [admission_body, step.call('retry').fetch('run'), step.call('handoff').fetch('run')]
    bodies.each do |body|
      raise 'unresolved expression' if body.include?('${{')
      out, err, status = Open3.capture3('bash', '-n', stdin_data: body)
      raise "syntax: #{out}#{err}" unless status.success?
    end
    ci = YAML.load_file('.github/workflows/ci.yml').fetch('jobs')
    commands = ci.fetch('research-scan-tests').fetch('steps').filter_map { |s| s['run'] }.join("\n")
    raise 'test not discovered' unless commands.include?('scripts/ci/test-ci-*.sh') &&
                                      Dir.glob('scripts/ci/test-ci-*.sh').include?('scripts/ci/test-ci-queue-recovery.sh')
  end

  # The guarded absent-rule call already worked before PAN-150. The bare call
  # exposes the latent errexit dependency on that outer guard.
  [true, false].each do |guarded|
    %w[present absent error retargeted merge-error].each do |scenario|
      fresh.call('rules' => (%w[absent error].include?(scenario) ? scenario : 'present'),
                 'base' => (scenario == 'retargeted' ? 'release' : 'main'),
                 'merge_error' => scenario == 'merge-error')
      invocation = 'arm_auto_merge "https://example.invalid/pr/17" "$REPO" main "$PR"'
      invocation += ' || exit 1' if guarded
      code, output = run.call("#{arm}\n#{invocation}\necho reached-end\n")
      check.call("admission #{guarded ? 'guarded' : 'bare'} #{scenario}") do
        success = %w[present absent retargeted].include?(scenario)
        raise "exit #{code}: #{output}" unless (code == 0) == success
        raise 'did not reach end' unless output.include?('reached-end') == success
        raise 'wrong arm count' unless state.call['arms'] == (scenario == 'present' ? 1 : 0)
      end
    end
  end

  check.call('absent retry fails without consuming budget; restoration retries once; redelivery deduplicates') do
    fresh.call('rules' => 'absent')
    result = event.call
    raise 'absent retry succeeded' unless result['steps.retry.conclusion'] == 'failure'
    raise 'absent retry mutated state' unless state.call['mutations'].empty? && state.call['arms'] == 0
    patch_state.call('rules' => 'present')
    result = event.call
    raise 'restored retry failed' unless result['steps.retry.conclusion'] == 'success'
    raise 'missing budget/dedup' unless state.call['labels'].sort == [retry_label, attempt_label].sort
    mutations = state.call['mutations']
    raise 'budget recorded before arm' unless mutations.index('arm') < mutations.index("label:#{retry_label}")
    raise 'handled before budget' unless mutations.index("label:#{retry_label}") < mutations.index("label:#{attempt_label}")
    result = event.call
    raise 'redelivery was not deduplicated' unless result['steps.dedup.outputs.already_handled'] == 'true'
    raise 'redelivery retried' unless result['steps.retry.conclusion'] == 'skipped' && state.call['arms'] == 1
  end
  %w[present error merge-error].each do |scenario|
    check.call("retry control #{scenario}") do
      fresh.call('rules' => (scenario == 'error' ? 'error' : 'present'),
                 'merge_error' => scenario == 'merge-error')
      result = event.call
      success = scenario == 'present'
      raise 'wrong retry result' unless (result['steps.retry.conclusion'] == 'success') == success
      raise 'wrong arm count' unless state.call['arms'] == (success ? 1 : 0)
      raise 'wrong budget' unless state.call['labels'].include?(retry_label) == success
      raise 'wrong handled result' unless state.call['labels'].include?(attempt_label) == success
      unless success
        raise 'failure comment not exercised' if state.call['comments'].empty?
        raise 'wrong API-failure handoff' unless state.call['labels'].include?(pause) == (scenario == 'error')
      end
    end
  end

  check.call('interrupted handoff repairs orphan before handled; same head stays paused and new head is stale') do
    handoff_state.call('labels' => [retry_label], 'timeline' => [], 'post_error' => true,
                       'armed' => true, 'queued' => true)
    failed_handoff.call
    raise 'failed POST created marker' unless marker_comments.call.empty?
    raise 'did not disarm/dequeue' if state.call['armed'] || state.call['queued']
    patch_state.call('post_error' => false)
    successful_handoff.call
    raise 'missing repaired marker' unless marker_comments.call.length == 1
    mutations = state.call['mutations']
    raise 'marked handled before verified comment' unless mutations.index('readback:1') < mutations.index("label:#{attempt_label}")
    raise 'unchanged head stale' if stale.call(head[0, 12])
    raise 'changed head not stale' unless stale.call('b' * 12)
  end
  check.call('failed exact-comment readback rerun reuses persisted marker') do
    handoff_state.call('readback_error' => true)
    failed_handoff.call
    original = marker_comments.call
    raise 'POST did not persist' unless original.length == 1
    patch_state.call('readback_error' => false)
    successful_handoff.call
    raise 'duplicated marker' unless marker_comments.call == original
  end
  [
    ['human-owned pause', {'timeline' => [label_event.call('maintainer')]}],
    ['human attach races idempotent POST', {'labels' => [retry_label], 'timeline' => [], 'human_attach_race' => true}],
    ['human reattachment after old bot marker',
     {'timeline' => [label_event.call('github-actions[bot]', '2026-10-20T11:00:00Z'),
                     {'event' => 'unlabeled', 'label' => {'name' => pause}},
                     label_event.call('maintainer')],
      'comments' => [marker.call(head[0, 12], '2026-10-20T11:00:00Z')]}]
  ].each do |name, overrides|
    check.call(name) do
      handoff_state.call(overrides.merge('armed' => true, 'queued' => true))
      original = marker_comments.call
      successful_handoff.call
      raise 'claimed human provenance' unless marker_comments.call == original
      raise 'human pause not disarmed' if state.call['armed'] || state.call['queued']
      raise 'human pause stale' if stale.call('b' * 12)
    end
  end
  [
    ['empty comment history', []],
    ['marker predates current attachment', [marker.call(head[0, 12], '2026-10-20T11:59:59Z')]],
    ['other actor marker', [marker.call(head[0, 12], '2026-10-20T12:00:00Z', 'maintainer')]],
    ['malformed marker', [marker.call('not-a-sha')]],
    # GitHub's issue-comment schema makes the author nullable.
    ['unrelated comment with null author',
     [{'id' => 1, 'user' => nil, 'created_at' => '2026-10-20T11:30:00Z', 'body' => 'unrelated'}]]
  ].each do |name, comments|
    check.call("repair #{name}") do
      handoff_state.call('comments' => comments)
      successful_handoff.call
      raise 'did not publish repair' unless state.call['comments'].length == comments.length + 1
      raise 'repaired marker not usable' unless stale.call('b' * 12) && !stale.call(head[0, 12])
    end
  end
  [
    ['timestamp equality', [marker.call]],
    ['timestamp after attachment', [marker.call(head[0, 12], '2026-10-20T12:00:01Z')]],
    ['older SHA retained', [marker.call('d' * 12)]],
    ['multiple comment and timeline pages',
     [marker.call(head[0, 12], '2026-10-20T11:00:00Z'),
      marker.call('bad-marker'), marker.call('d' * 12)]]
  ].each do |name, comments|
    check.call("preserve valid provenance #{name}") do
      handoff_state.call('comments' => comments,
                         'timeline' => [label_event.call('maintainer', '2026-10-20T10:00:00Z'),
                                        {'event' => 'unlabeled', 'label' => {'name' => pause}},
                                        label_event.call])
      successful_handoff.call
      raise 'rewrote or duplicated valid marker' unless state.call['comments'] == comments
      recorded = comments.last['body'][/sha=([0-9a-f]{12})/, 1]
      raise 'original SHA not preserved for consumer' if stale.call(recorded)
      raise 'changed head not stale' unless stale.call('b' * 12)
    end
  end
  [
    ['timeline fetch', {'timeline_mode' => 'error'}],
    ['timeline invalid JSON', {'timeline_mode' => 'invalid'}],
    ['timeline wrong shape', {'timeline_mode' => 'wrong-shape'}],
    ['missing latest event', {'timeline' => []}],
    ['missing actor', {'timeline' => [label_event.call(nil)]}],
    ['empty actor', {'timeline' => [label_event.call('')]}],
    ['missing timestamp', {'timeline' => [label_event.call('github-actions[bot]', nil)]}],
    ['invalid timestamp', {'timeline' => [label_event.call('github-actions[bot]', 'yesterday')]}],
    ['comment fetch', {'comments_mode' => 'error'}],
    ['comments invalid JSON', {'comments_mode' => 'invalid'}],
    ['comments wrong shape', {'comments_mode' => 'wrong-shape'}]
  ].each do |name, overrides|
    check.call("unknown provenance #{name} fails after disarm") do
      handoff_state.call(overrides.merge('armed' => true, 'queued' => true))
      failed_handoff.call
      raise 'unknown provenance created marker' unless marker_comments.call.empty?
      raise 'unknown provenance skipped disarm' if state.call['armed'] || state.call['queued']
    end
  end
  %w[post_error missing_comment_id readback_missing_marker readback_error disarm_error dequeue_error].each do |failure|
    check.call("handoff #{failure} cannot mark handled") do
      handoff_state.call(failure => true, 'armed' => true, 'queued' => true)
      failed_handoff.call
      if %w[disarm_error dequeue_error].include?(failure)
        raise 'marker published before safe disarm' unless marker_comments.call.empty?
      end
    end
  end
  check.call('fallback handler-owned pause publishes verified clearable marker') do
    fallback_state.call
    result = event.call
    raise 'fallback retry failed' unless result['steps.retry.conclusion'] == 'success'
    raise 'missing pause or handled label' unless ([pause, attempt_label] - state.call['labels']).empty?
    raise 'fallback left PR armed/queued' if state.call['armed'] || state.call['queued']
    raise 'missing marker' unless marker_comments.call.length == 1
    raise 'marker not read back' unless state.call['mutations'].include?('readback:1')
    raise 'unchanged head stale' if stale.call(head[0, 12])
    raise 'changed head not stale' unless stale.call('b' * 12)
  end
  [
    ['human attach race', {'human_attach_race' => true}, 'success'],
    ['unknown owner', {'timeline_mode' => 'error'}, 'failure'],
    ['disarm unconfirmed', {'disarm_error' => true}, 'failure'],
    ['pause not visible', {'unattachable' => [retry_label, pause]}, 'failure']
  ].each do |name, overrides, conclusion|
    check.call("fallback #{name} publishes no clearable marker") do
      fallback_state.call(overrides)
      result = event.call
      raise 'wrong retry result' unless result['steps.retry.conclusion'] == conclusion
      raise 'wrong handled result' unless state.call['labels'].include?(attempt_label) == (conclusion == 'success')
      raise 'claimed clearable provenance' unless marker_comments.call.empty?
      raise 'fallback comment not posted' if state.call['comments'].empty?
      raise 'fallback pause stale' if stale.call('b' * 12)
    end
  end
  %w[post_error missing_comment_id readback_error readback_missing_marker].each do |failure|
    check.call("fallback #{failure} fails closed") do
      fallback_state.call(failure => true)
      result = event.call
      raise 'unverified marker succeeded' unless result['steps.retry.conclusion'] == 'failure'
      raise 'unverified marker marked handled' if state.call['labels'].include?(attempt_label)
      raise 'lost pause' unless state.call['labels'].include?(pause)
    end
  end
  # Codex P1 (PAN-152 review): admission trusts the newest bot marker when it is at
  # or after the latest attachment, so a marker posted after a maintainer re-attached
  # the pause would let the next push clear their pause.
  [['fallback', fallback_state, 'steps.retry.conclusion'],
   ['handoff', handoff_state, 'steps.handoff.conclusion']].each do |path, setup, key|
    [
      ['re-attachment during publication', {'reattach_before_comment' => true}],
      ['unreadable ownership after publication', {'timeline_error_after_comment' => true}]
    ].each do |name, overrides|
      check.call("#{path} #{name} withdraws the clearable marker") do
        setup.call(overrides)
        result = event.call
        raise 'raced publication succeeded' unless result[key] == 'failure'
        raise 'raced publication marked handled' if state.call['labels'].include?(attempt_label)
        raise 'lost pause' unless state.call['labels'].include?(pause)
        raise 'clearable marker left behind' unless marker_comments.call.empty?
        raise 'withdrawn comment missing' if state.call['comments'].empty?
        raise 'human pause stale' if stale.call('b' * 12)
      end
    end
    check.call("#{path} unconfirmed marker withdrawal fails closed") do
      setup.call('reattach_before_comment' => true, 'patch_error' => true)
      result = event.call
      raise 'unconfirmed withdrawal succeeded' unless result[key] == 'failure'
      raise 'unconfirmed withdrawal marked handled' if state.call['labels'].include?(attempt_label)
      raise 'lost pause' unless state.call['labels'].include?(pause)
    end
  end
  check.call('moved head takes moot path without creating provenance') do
    handoff_state.call('head' => 'b' * 40, 'labels' => [retry_label], 'timeline' => [])
    result = event.call
    raise 'moved head failed' unless result['steps.handoff.conclusion'] == 'success'
    raise 'moved head received pause' if state.call['labels'].include?(pause)
    raise 'moved head received provenance' unless marker_comments.call.empty?
    raise 'moved head disarmed or rearmed' unless state.call['mutations'] == ["label:#{attempt_label}"]
  end
end
puts "PASS: #{count} queue recovery cases"
RUBY
