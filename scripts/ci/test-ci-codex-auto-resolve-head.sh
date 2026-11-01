#!/usr/bin/env bash
# PAN-254: the Codex gate's auto-resolve loop must re-read the PR head before each
# thread and stop as soon as it has moved. Runs the real compute-verdict step body
# from wait-for-codex.yml against a stateful mock gh. Keep this file ASCII: an
# unset locale makes Ruby reject non-ASCII text in the heredoc.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
ruby -ryaml -rjson -rtmpdir -rfileutils -ropen3 <<'RUBY'
steps = YAML.load_file('.github/workflows/wait-for-codex.yml')
  .fetch('jobs').fetch('codex-review-response').fetch('steps')
step = steps.find { |s| s['id'] == 'compute-verdict' } or raise 'missing step compute-verdict'
h1 = 'a' * 40
h2 = 'b' * 40
finding = 'c' * 40
ids = %w[T1 T2 T3 T4 T5]
template = step.fetch('run')
exprs = %w[baseline_pure baseline_initiated].map { |name| "'${{ steps.baseline.outputs.#{name} }}'" }
exprs.each { |expr| raise "missing #{expr}" unless template.include?(expr) }
body_for = lambda do |thread_ids|
  body = exprs.reduce(template) { |acc, expr| acc.sub(expr, "'#{JSON.generate(thread_ids)}'") }
  raise 'unresolved expression' if body.include?('${{')
  body
end
# freshness-check is not run: its retry loop sleeps 15 s when lookups fail, and
# compute-verdict is the step that mutates threads.

# Every gh call goes through this stateful mock and is logged in order. The head
# moves from H1 to H2 on the trigger the case sets. Unexpected calls are persisted
# as failures even where the workflow ignores a command's exit code.
mock = <<~'MOCK'
  #!/usr/bin/env ruby
  require 'json'
  file = ENV.fetch('MOCK_STATE')
  s = JSON.parse(File.read(file))
  args = ARGV
  out = nil
  code = 0
  label = "codex-clean:#{s['h1'][0, 12]}"
  log = ->(kind, extra = {}) { s['log'] << {'kind' => kind}.merge(extra) }
  unexpected = lambda do |why|
    s['unexpected'] << {'why' => why, 'args' => args}
    code = 99
  end
  move = lambda do
    unless s['head'] == s['next_head']
      s['head'] = s['next_head']
      log.call('head-moved')
    end
  end
  graphql = args.length >= 4 && args[0..2] == ['api', 'graphql', '-f'] && args[3].start_with?('query=')
  if args.length == 9 && args[0..2] == ['label', 'create', label] && args[3] == '--color' &&
     args[5] == '--description' && args[7..8] == ['--repo', 'test/repo']
    log.call('label-create')
  elsif args == ['api', '--paginate', 'repos/test/repo/issues/17/comments?per_page=100']
    log.call('comments')
    out = JSON.generate([{'body' => "<!-- codex-review-request head_sha=#{s['h1']} -->",
                          'user' => {'login' => 'trusted'}}])
  elsif args == ['api', '-X', 'POST', 'repos/test/repo/issues/17/labels', '-f', "labels[]=#{label}"]
    log.call('labels-post')
    move.call if s['move_on'] == 'labels-post'
  elsif args == ['api', 'repos/test/repo/pulls/17', '--jq', '.head.sha']
    log.call('pulls')
    if s['lookup_fails']
      # Real `gh api` prints a JSON error body on stdout, even with --jq.
      $stdout.puts '{"message":"Server Error","status":"502"}'
      $stderr.puts 'gh: Server Error (HTTP 502)'
      code = 1
    else
      out = s['head']
    end
  elsif args.length == 2 && args[0] == 'api' && args[1].start_with?('repos/test/repo/compare/')
    log.call('compare', 'range' => args[1].delete_prefix('repos/test/repo/compare/'))
    out = JSON.generate('merge_base_commit' => {'sha' => s['finding']},
                        'files' => s['threads'].map { |t| {'filename' => t['path']} })
  elsif graphql && args[3].include?('reviewThreads(first: 100')
    if args[4..] == ['-f', 'owner=test', '-f', 'name=repo', '-F', 'pr=17', '-F', 'after=null']
      log.call('threads')
      nodes = s['threads'].map do |t|
        {'id' => t['id'], 'isResolved' => t['resolved'], 'path' => t['path'],
         'comments' => {'nodes' => t['comments']}}
      end
      out = JSON.generate('data' => {'repository' => {'pullRequest' => {'reviewThreads' =>
        {'pageInfo' => {'hasNextPage' => false, 'endCursor' => nil}, 'nodes' => nodes}}}})
    else
      unexpected.call('thread page arguments')
    end
  elsif graphql
    query = args[3]
    t = args[4] == '-f' && args[5].to_s.start_with?('id=') &&
        s['threads'].find { |x| x['id'] == args[5].delete_prefix('id=') }
    if !t
      unexpected.call('unknown thread')
    elsif query.include?('addPullRequestReviewThreadReply') && args.length == 8 &&
          args[6] == '-f' && args[7].start_with?('body=')
      log.call('reply', 'id' => t['id'])
      t['replies'] << args[7].delete_prefix('body=')
      t['comments'] << {'author' => {'login' => 'github-actions'}, 'createdAt' => '2026-11-01T11:30:00Z'}
      out = '{"data":{}}'
    elsif query.include?('resolveReviewThread') && args.length == 6
      if ENV['GH_TOKEN'] == 'resolve-token'
        log.call('resolve', 'id' => t['id'])
        t['resolved'] = true
        s['resolves'] += 1
        move.call if s['move_after_resolve'] == s['resolves']
        out = '{"data":{}}'
      else
        unexpected.call('resolve without RESOLVE_TOKEN')
      end
    elsif query.include?('node(id: $id)') && !query.include?('mutation') && args.length == 6
      log.call('node', 'id' => t['id'])
      out = JSON.generate('data' => {'node' => {'isResolved' => t['resolved'],
                                                'comments' => {'nodes' => t['comments']}}})
    else
      unexpected.call('graphql shape')
    end
  else
    unexpected.call('unknown call')
  end
  File.write(file, JSON.generate(s))
  puts out if code == 0 && !out.nil?
  exit code
MOCK

count = 0
Dir.mktmpdir('pan254-auto-resolve-') do |dir|
  gh = File.join(dir, 'gh')
  File.write(gh, mock)
  FileUtils.chmod(0o755, gh)
  state_path = File.join(dir, 'state.json')
  output_path = File.join(dir, 'output')
  script = File.join(dir, 'compute-verdict.sh')
  env = {'PATH' => "#{dir}:#{ENV.fetch('PATH')}", 'MOCK_STATE' => state_path,
         'GH_TOKEN' => 'github-token', 'RESOLVE_TOKEN' => 'resolve-token',
         'REPO' => 'test/repo', 'PR' => '17', 'HEAD_SHA' => h1,
         'RUN_STARTED' => '2026-11-01T11:00:00Z', 'IS_MARKED' => 'true',
         'TRUSTED_LOGIN' => 'trusted', 'GITHUB_OUTPUT' => output_path}
  # Runs compute-verdict under GitHub's default `bash -e {0}` and returns the
  # mock's final state plus the step's combined output.
  # `count` and `long_paths` size the thread list; every other key seeds the mock.
  run = lambda do |scenario|
    scenario = scenario.dup
    thread_ids = (1..(scenario.delete('count') || ids.length)).map { |n| "T#{n}" }
    dir_name = scenario.delete('long_paths') ? 'd' * 390 : 'src'
    File.write(script, body_for.call(thread_ids))
    _, err, syntax = Open3.capture3('bash', '-n', script)
    raise "syntax: #{err}" unless syntax.success?
    threads = thread_ids.each_with_index.map do |id, n|
      {'id' => id, 'path' => "#{dir_name}/f#{n}.rs", 'resolved' => false, 'replies' => [],
       'comments' => [{'author' => {'login' => 'chatgpt-codex-connector'},
                       'createdAt' => '2026-11-01T10:00:00Z', 'originalCommit' => {'oid' => finding}}]}
    end
    state = {'h1' => h1, 'head' => h1, 'next_head' => h2, 'finding' => finding, 'resolves' => 0,
             'log' => [], 'unexpected' => [], 'threads' => threads}.merge(scenario)
    File.write(state_path, JSON.generate(state))
    File.write(output_path, '')
    out, err, status = Open3.capture3(env, 'bash', '--noprofile', '--norc', '-e', script)
    output = (out + err).force_encoding(Encoding::UTF_8).scrub('?')
    result = JSON.parse(File.read(state_path))
    raise "unexpected gh calls: #{result['unexpected']}" unless result['unexpected'].empty?
    raise "compute-verdict exit #{status.exitstatus}: #{output}" unless status.exitstatus == 0
    raise 'outcome=success not written' unless File.readlines(output_path, chomp: true).include?('outcome=success')
    result.merge('output' => output)
  end
  resolved = ->(r) { r['threads'].select { |t| t['resolved'] }.map { |t| t['id'] } }
  kinds = ->(r) { r['log'].map { |e| e['kind'] } }
  lines = ->(r, prefix) { r['output'].lines.select { |l| l.start_with?(prefix) } }
  check = lambda do |name, &assertions|
    assertions.call
    count += 1
    puts "PASS: #{name}"
  end

  check.call('head moves after the second resolve: no later thread is replied to or resolved') do
    r = run.call('move_after_resolve' => 2)
    got = resolved.call(r)
    raise "resolved #{got.length}/5 (#{got.join(',')}), expected T1,T2" unless got == %w[T1 T2]
    log = kinds.call(r)
    moved = log.index('head-moved') or raise 'head never moved'
    late = log[moved..].count { |k| %w[reply resolve].include?(k) }
    raise "#{late} thread mutation(s) after the head moved" unless late == 0
    notices = lines.call(r, '::notice::')
    unless notices.length == 1 && notices[0].include?('mid-loop') && notices[0].include?(h2)
      raise "expected one mid-loop notice naming H2, got #{notices.inspect}"
    end
  end

  check.call('head moves before the first thread: nothing is compared or resolved') do
    r = run.call('move_on' => 'labels-post')
    got = resolved.call(r)
    raise "resolved #{got.length}/5 (#{got.join(',')}), expected none" unless got.empty?
    log = kinds.call(r)
    raise 'compare called after the head moved' if log.include?('compare')
    raise 'reply posted after the head moved' if log.include?('reply')
    notices = lines.call(r, '::notice::').select { |l| l.include?('mid-loop') }
    raise "expected one mid-loop notice, got #{notices.inspect}" unless notices.length == 1
  end

  check.call('head never moves: every thread resolves and the head check precedes each compare') do
    r = run.call({})
    got = resolved.call(r)
    raise "resolved #{got.length}/5 (#{got.join(',')}), expected all" unless got == ids
    replies = r['threads'].flat_map { |t| t['replies'] }
    raise "replies do not all name H1: #{replies.inspect}" unless replies.length == 5 &&
                                                               replies.all? { |b| b.include?("commit #{h1} ") }
    log = kinds.call(r)
    raise "expected 5 head lookups, got #{log.count('pulls')}" unless log.count('pulls') == 5
    compares = log.each_index.select { |i| log[i] == 'compare' }
    raise "expected 5 compares, got #{compares.length}" unless compares.length == 5
    raise "a compare ran without a head check right before it: #{log}" unless compares.all? { |i| log[i - 1] == 'pulls' }
    raise 'mid-loop message while the head held' if r['output'].include?('mid-loop')
  end

  check.call('every head lookup fails: the loop fails open and resolves every thread') do
    r = run.call('lookup_fails' => true)
    got = resolved.call(r)
    raise "resolved #{got.length}/5 (#{got.join(',')}), expected all" unless got == ids
    warnings = lines.call(r, '::warning::').select { |l| l.include?('mid-loop') }
    raise "expected 5 mid-loop warnings, got #{warnings.length}" unless warnings.length == 5
    raise 'notice on a failed lookup' unless lines.call(r, '::notice::').empty?
  end

  # 160 threads with 400-character paths give more TSV than a 64 KiB pipe buffer
  # holds, so the loop's input must not be a pipe the `break` closes on a writer.
  check.call('head moves with more threads than a pipe buffer holds: the step still exits 0') do
    r = run.call('count' => 160, 'long_paths' => true, 'move_after_resolve' => 1)
    got = resolved.call(r)
    raise "resolved #{got.length}/160 (#{got.first(3).join(',')}), expected T1" unless got == %w[T1]
  end
end
puts "PASS: #{count} codex auto-resolve head cases"
RUBY
