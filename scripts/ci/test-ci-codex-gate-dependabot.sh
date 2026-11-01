#!/usr/bin/env bash
# PAN-249: a genuine Dependabot update must get the required "Codex review clean" check from
# wait-for-codex.yml's producer, as a completed success, through the same App-token upsert a
# real PR uses. pull_request_target runs main's copy of the workflow, so this offline test is
# the only pre-merge proof: it evaluates the shipped `if:`/`env:` expressions and runs the
# shipped create-check script against a stub `gh`.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
for tool in ruby jq shellcheck; do
  command -v "$tool" >/dev/null || { echo "FAIL: $tool is required" >&2; exit 1; }
done
ruby -ryaml -rjson -rtmpdir -rfileutils -ropen3 <<'RUBY'
MANAGED = 'Codex review clean (gate before expensive downstream jobs)'
REPO = 'HagaleTechnologies/pancetta'
FORK = 'someone/pancetta'
SHA = 'ef94c223' + ('a' * 32)
MAIN_SHA = '349f7791' + ('b' * 32)
RUN_ID = '123456'
TOKEN = 'fixture-app-token'
BOT = 'dependabot[bot]'
HUMAN = 'thagale'

# Minimal GitHub Actions expression evaluator: paths, string/number/boolean/null literals,
# ! == != && || and parentheses, with GitHub's value-returning && / || and loose equality.
# Every path must be present in the context hash, so a term the fixture does not model fails.
class Expr
  TOKEN_RE = /\G\s*(?:(&&|\|\||==|!=|!|\(|\))|'((?:[^']|'')*)'|(\d+(?:\.\d+)?)|([A-Za-z_][\w-]*(?:\.[A-Za-z_][\w-]*)*))/

  def self.evaluate(src, ctx)
    src = src.strip
    src = Regexp.last_match(1).strip if src =~ /\A\$\{\{(.*)\}\}\z/m
    raise "embedded interpolation not modelled: #{src}" if src.include?('${{')
    new(tokenize(src), ctx).run
  end

  def self.tokenize(src)
    toks = []
    pos = 0
    src = src.rstrip
    while pos < src.length
      m = TOKEN_RE.match(src, pos)
      raise "cannot tokenize at #{pos}: #{src}" unless m && m.end(0) > pos
      toks << if m[1] then [:op, m[1]]
              elsif m[2] then [:lit, m[2].gsub("''", "'")]
              elsif m[3] then [:lit, m[3].include?('.') ? m[3].to_f : m[3].to_i]
              else [:path, m[4]]
              end
      pos = m.end(0)
    end
    toks
  end

  def initialize(toks, ctx)
    @toks = toks
    @ctx = ctx
  end

  def run
    v = or_expr
    raise "trailing tokens: #{@toks.inspect}" unless @toks.empty?
    v
  end

  def self.truthy?(v)
    !(v.nil? || v == false || v == '' || v == 0 || (v.is_a?(Float) && v.nan?))
  end

  def self.num(v)
    case v
    when nil then 0
    when true then 1
    when false then 0
    when Numeric then v
    else v.strip.empty? ? 0 : (Float(v) rescue Float::NAN)
    end
  end

  def self.loose_eq(a, b)
    return a.casecmp?(b) if a.is_a?(String) && b.is_a?(String)
    return a == b if [a, b].all? { |x| x == true || x == false } || (a.nil? && b.nil?)
    x = num(a)
    y = num(b)
    !(x.is_a?(Float) && x.nan?) && !(y.is_a?(Float) && y.nan?) && x == y
  end

  def peek?(op)
    @toks.first == [:op, op]
  end

  def or_expr
    v = and_expr
    while peek?('||')
      @toks.shift
      r = and_expr
      v = Expr.truthy?(v) ? v : r
    end
    v
  end

  def and_expr
    v = eq_expr
    while peek?('&&')
      @toks.shift
      r = eq_expr
      v = Expr.truthy?(v) ? r : v
    end
    v
  end

  def eq_expr
    v = unary
    while peek?('==') || peek?('!=')
      op = @toks.shift[1]
      r = unary
      v = op == '==' ? Expr.loose_eq(v, r) : !Expr.loose_eq(v, r)
    end
    v
  end

  def unary
    if peek?('!')
      @toks.shift
      return !Expr.truthy?(unary)
    end
    primary
  end

  def primary
    kind, val = @toks.shift
    raise 'unexpected end of expression' if kind.nil?
    if [kind, val] == [:op, '(']
      v = or_expr
      raise 'missing )' unless @toks.shift == [:op, ')']
      return v
    end
    return val if kind == :lit
    raise "unexpected operator #{val}" if kind == :op
    raise "function calls are not modelled: #{val}(" if peek?('(')
    return { 'true' => true, 'false' => false, 'null' => nil }[val] if %w[true false null].include?(val)
    raise "unknown expression term: #{val}" unless @ctx.key?(val)
    @ctx[val]
  end
end

def env_string(v)
  case v
  when nil then ''
  when Float then v == v.to_i ? v.to_i.to_s : v.to_s
  else v.to_s
  end
end

def pr_ctx(actor: HUMAN, author: HUMAN, head_repo: REPO, draft: false, event: 'pull_request_target')
  pr = event == 'pull_request_target' || event == 'pull_request'
  {
    'github.event_name' => event,
    'github.actor' => actor,
    'github.repository' => REPO,
    'github.run_id' => RUN_ID,
    'github.sha' => MAIN_SHA,
    'github.event.pull_request.draft' => pr ? draft : nil,
    'github.event.pull_request.user.login' => pr ? author : nil,
    'github.event.pull_request.head.repo.full_name' => pr ? head_repo : nil,
    'github.event.pull_request.head.sha' => pr ? SHA : nil
  }
end

wf_path = '.github/workflows/wait-for-codex.yml'
wf = YAML.load_file(wf_path)
ci = YAML.load_file('.github/workflows/ci.yml')
jobs = wf.fetch('jobs')
producer = jobs.fetch('codex-review-request')
steps = producer.fetch('steps')
step = ->(id) { steps.find { |s| s['id'] == id } or raise "missing step #{id}" }
review_step = steps.find { |s| s['name'] == 'Request a review' } or raise 'missing Request a review step'
create = step.call('create-check')

failures = []
count = 0
check = lambda do |name, &blk|
  blk.call
  count += 1
rescue StandardError => e
  failures << "#{name}: #{e.message}"
end
expect = ->(cond, msg) { raise msg unless cond }

# 1. Producer admission: every non-draft pull_request_target event enters, Dependabot included.
[
  ['human PR', pr_ctx, true],
  ['Dependabot PR', pr_ctx(actor: BOT, author: BOT), true],
  ['human push to a Dependabot branch', pr_ctx(author: BOT), true],
  ['draft human PR', pr_ctx(draft: true), false],
  ['draft Dependabot PR', pr_ctx(actor: BOT, author: BOT, draft: true), false],
  ['issue_comment', pr_ctx(event: 'issue_comment'), false],
  ['merge_group', pr_ctx(event: 'merge_group'), false],
  ['plain pull_request', pr_ctx(event: 'pull_request'), false]
].each do |label, ctx, want|
  check.call("admission/#{label}") do
    got = Expr.truthy?(Expr.evaluate(producer.fetch('if'), ctx))
    expect.call(got == want, "producer if: expected #{want}, got #{got}")
  end
end

# 2. Classification: only Dependabot's own event, on a Dependabot-authored, same-repo PR.
[BOT, HUMAN].product([BOT, HUMAN], [REPO, FORK]).each do |actor, author, head_repo|
  check.call("classification/actor=#{actor},author=#{author},head=#{head_repo}") do
    expr = create.fetch('env').fetch('IS_DEPENDABOT') { raise 'create-check env has no IS_DEPENDABOT' }
    got = env_string(Expr.evaluate(expr, pr_ctx(actor: actor, author: author, head_repo: head_repo)))
    want = (actor == BOT && author == BOT && head_repo == REPO).to_s
    expect.call(got == want, "IS_DEPENDABOT rendered #{got.inspect}, expected #{want}")
  end
end

# 3. Same App token, same main-only environment, metadata only.
check.call('static/environment, token and metadata-only job') do
  expect.call(producer['name'] != MANAGED, 'producer job name must differ from the managed check name')
  expect.call(producer['environment'] == { 'name' => 'codex-review', 'deployment' => false }, 'producer environment changed')
  on = wf['on'] || wf[true]
  expect.call(on.fetch('pull_request_target').fetch('branches') == ['main'], 'pull_request_target must stay main-only')
  app = step.call('app-token')
  expect.call(app['uses'].to_s.start_with?('actions/create-github-app-token@'), 'app-token must mint with create-github-app-token')
  expect.call(app.dig('with', 'private-key') == '${{ secrets.HAGALE_AGENT_APP_PRIVATE_KEY }}', 'app-token key source changed')
  [pr_ctx, pr_ctx(actor: BOT, author: BOT)].each do |ctx|
    expect.call(Expr.truthy?(Expr.evaluate(app.fetch('if'), ctx)), "app-token skipped for #{ctx['github.actor']}")
  end
  expect.call(steps.index(app) < steps.index(create), 'app-token must run before create-check')
  expect.call(create.dig('env', 'GH_TOKEN') == '${{ steps.app-token.outputs.token }}', 'create-check must authenticate with the minted App token')
  expect.call(steps.none? { |s| s['uses'].to_s.start_with?('actions/checkout') }, 'producer must not check out PR code')
end

# Stale-head gating on create-check.
check.call('static/stale head does not publish') do
  base = { 'github.event_name' => 'pull_request_target' }
  stale = Expr.evaluate(create.fetch('if'), base.merge('steps.head-check.outputs.stale' => 'true'))
  live = Expr.evaluate(create.fetch('if'), base.merge('steps.head-check.outputs.stale' => nil))
  expect.call(!Expr.truthy?(stale) && Expr.truthy?(live), 'create-check must skip a stale head and run a live one')
end

# 4-6. Execute the shipped create-check script against a strict stub gh.
lookup_call = ['api', '--paginate', "repos/#{REPO}/commits/#{SHA}/check-runs"]
post = lambda do |status, conclusion|
  ['api', '-X', 'POST', "repos/#{REPO}/check-runs", '-f', "name=#{MANAGED}", '-f', "head_sha=#{SHA}",
   '-f', "status=#{status}", *(conclusion ? ['-f', "conclusion=#{conclusion}"] : []),
   '-f', "external_id=#{RUN_ID}", '--jq', '.id']
end
patch = lambda do |status, conclusion|
  ['api', '-X', 'PATCH', "repos/#{REPO}/check-runs/777", '-f', "status=#{status}",
   *(conclusion ? ['-f', "conclusion=#{conclusion}"] : []), '-f', "external_id=#{RUN_ID}", '--jq', '.id']
end
empty_pages = [{ 'total_count' => 0, 'check_runs' => [] }, { 'total_count' => 0, 'check_runs' => [] }]
existing_pages = [
  { 'total_count' => 4, 'check_runs' => [
    { 'id' => 555, 'name' => MANAGED, 'external_id' => '99', 'status' => 'completed',
      'started_at' => '2026-10-26T05:56:10Z', 'output' => { 'title' => 'older managed run' } },
    { 'id' => 888, 'name' => 'CI', 'external_id' => '101', 'status' => 'completed',
      'started_at' => '2026-10-26T05:59:00Z', 'output' => { 'title' => 'unrelated' } }
  ] },
  { 'total_count' => 4, 'check_runs' => [
    { 'id' => 777, 'name' => MANAGED, 'external_id' => '100', 'status' => 'completed',
      'started_at' => '2026-10-26T05:57:00Z', 'output' => { 'title' => 'Codex review request setup failed' } },
    { 'id' => 1111, 'name' => MANAGED, 'external_id' => '0f8fad5b-d9cb-469f-a165-70867728950e', 'status' => 'completed',
      'started_at' => '2026-10-26T05:58:00Z', 'output' => { 'title' => nil } }
  ] }
]
stub = <<~'STUB'
  #!/usr/bin/env bash
  set -euo pipefail
  { printf '%s\x1f' "$@"; printf '\n'; } >> "$STUB_LOG"
  [ "${GH_TOKEN:-}" = "$STUB_EXPECT_TOKEN" ] || { echo "stub gh: wrong GH_TOKEN" >&2; exit 97; }
  case "$*" in
    "api --paginate repos/$STUB_REPO/commits/$STUB_HEAD/check-runs") cat "$STUB_DIR"/page*.json ;;
    "api -X POST repos/$STUB_REPO/check-runs "*) echo 4242 ;;
    "api -X PATCH repos/$STUB_REPO/check-runs/"*) echo "${4##*/}" ;;
    *) echo "stub gh: unexpected call: $*" >&2; exit 98 ;;
  esac
STUB

Dir.mktmpdir('pan249-codex-gate-') do |dir|
  script = File.join(dir, 'create-check.sh')
  run = create.fetch('run')
  raise 'create-check run must not interpolate expressions' if run.include?('${{')
  File.write(script, run)
  ['bash -n', 'shellcheck -s bash'].each do |cmd|
    check.call("lint/#{cmd}") { expect.call(system(*cmd.split, script), "#{cmd} failed on create-check") }
  end
  bin = File.join(dir, 'bin')
  FileUtils.mkdir_p(bin)
  File.write(File.join(bin, 'gh'), stub)
  File.chmod(0o755, File.join(bin, 'gh'))

  exec_create = lambda do |ctx, pages|
    work = Dir.mktmpdir('case-', dir)
    pages.each_with_index { |p, i| File.write(File.join(work, "page#{i}.json"), JSON.generate(p)) }
    log = File.join(work, 'gh.log')
    out_file = File.join(work, 'output')
    File.write(log, '')
    File.write(out_file, '')
    env = create.fetch('env').transform_values { |v| env_string(Expr.evaluate(v.to_s, ctx)) }
    env.merge!('PATH' => "#{bin}:#{ENV.fetch('PATH')}", 'GITHUB_OUTPUT' => out_file, 'STUB_LOG' => log,
               'STUB_DIR' => work, 'STUB_REPO' => REPO, 'STUB_HEAD' => SHA, 'STUB_EXPECT_TOKEN' => TOKEN)
    out, err, status = Open3.capture3(env, 'bash', '--noprofile', '--norc', '-e', script, unsetenv_others: true)
    raise "create-check exited #{status.exitstatus}: #{out} #{err}" unless status.success?
    calls = File.read(log).lines.map { |l| l.chomp.split("\x1f") }
    outputs = File.read(out_file).lines.to_h { |l| l.chomp.split('=', 2) }
    [calls, outputs]
  end

  step_ctx = lambda do |bootstrap, clean|
    { 'steps.app-token.outputs.token' => TOKEN, 'steps.bootstrap-check.outputs.bootstrap_status' => bootstrap,
      'steps.existing-label.outputs.already_clean' => clean }
  end
  review_ctx = lambda do |outputs, bootstrap, clean|
    { 'steps.create-check.outputs.check_run_id' => outputs['check_run_id'],
      'steps.create-check.outputs.conclusion' => outputs['conclusion'],
      'steps.bootstrap-check.outputs.bootstrap_status' => bootstrap,
      'steps.existing-label.outputs.already_clean' => clean }
  end

  dependabot = pr_ctx(actor: BOT, author: BOT)
  human = pr_ctx
  [
    # name, event ctx, bootstrap, already_clean, pages, expected write, check_run_id, conclusion, pre status, pre title, review requested
    ['Dependabot, new check', dependabot, 'ready', 'false', empty_pages, post.call('completed', 'success'), '4242', 'success', '', '', false],
    ['Dependabot, existing managed check', dependabot, 'ready', 'false', existing_pages, patch.call('completed', 'success'), '777', 'success', 'completed', 'Codex review request setup failed', false],
    ['human push to Dependabot branch', pr_ctx(author: BOT), 'ready', 'false', empty_pages, post.call('in_progress', nil), '4242', '', '', '', true],
    ['Dependabot fork head', pr_ctx(actor: BOT, author: BOT, head_repo: FORK), 'ready', 'false', empty_pages, post.call('in_progress', nil), '4242', '', '', '', true],
    ['human, new check', human, 'ready', 'false', empty_pages, post.call('in_progress', nil), '4242', '', '', '', true],
    ['human, existing managed check', human, 'ready', 'false', existing_pages, patch.call('in_progress', nil), '777', '', 'completed', 'Codex review request setup failed', true],
    ['human, bootstrap not ready', human, 'absent', nil, existing_pages, patch.call('completed', 'success'), '777', 'success', 'completed', 'Codex review request setup failed', false],
    ['human, already clean', human, 'ready', 'true', existing_pages, patch.call('completed', 'success'), '777', 'success', 'completed', 'Codex review request setup failed', false]
  ].each do |label, ctx, bootstrap, clean, pages, write, id, conclusion, pre_status, pre_title, review|
    check.call("create-check/#{label}") do
      calls, outputs = exec_create.call(ctx.merge(step_ctx.call(bootstrap, clean)), pages)
      expect.call(calls == [lookup_call, write], "gh calls #{calls.inspect}, expected #{[lookup_call, write].inspect}")
      want = { 'check_run_id' => id, 'conclusion' => conclusion, 'pre_existing_status' => pre_status, 'pre_existing_title' => pre_title }
      expect.call(outputs == want, "outputs #{outputs.inspect}, expected #{want.inspect}")
      # 7. The shipped "Request a review" condition, on the outputs this run captured.
      got = Expr.truthy?(Expr.evaluate(review_step.fetch('if'), review_ctx.call(outputs, bootstrap, clean)))
      expect.call(got == review, "Request a review would #{got ? 'run' : 'skip'}, expected #{review ? 'run' : 'skip'}")
    end
  end
end

# 7. CI keeps skipping label polling for Dependabot, and CI runs this test.
check.call('ci/label gate still skips Dependabot') do
  gate = ci.fetch('jobs').fetch('codex-clean').fetch('if')
  ctx = ->(actor) { { 'github.event_name' => 'pull_request', 'github.actor' => actor, 'github.event.pull_request.draft' => false } }
  expect.call(!Expr.truthy?(Expr.evaluate(gate, ctx.call(BOT))), 'codex-clean must skip a Dependabot PR')
  expect.call(Expr.truthy?(Expr.evaluate(gate, ctx.call(HUMAN))), 'codex-clean must still run for a human PR')
end
check.call('ci/test discovery') do
  run = ci.fetch('jobs').fetch('research-scan-tests').fetch('steps').map { |s| s['run'].to_s }.join("\n")
  expect.call(run.include?('scripts/ci/test-ci-*.sh') && run.include?('scripts/ci/*.sh'), 'CI no longer runs scripts/ci/test-ci-*.sh')
  path = 'scripts/ci/test-ci-codex-gate-dependabot.sh'
  expect.call(File.exist?(path) && File.fnmatch('scripts/ci/test-ci-*.sh', path), "#{path} is outside CI's test glob")
end

unless failures.empty?
  warn failures.map { |f| "FAIL: #{f}" }.join("\n")
  warn "#{failures.size} failed, #{count} passed"
  exit 1
end
puts "PASS: #{count} codex gate Dependabot cases (PAN-249)"
RUBY
