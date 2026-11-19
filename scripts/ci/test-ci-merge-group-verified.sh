#!/usr/bin/env bash
# PAN-367: offline guard for the merge-group-verified push skip.
#
# Proves, against .github/workflows/ci.yml and a stub `gh`:
#   - the `changes` job publishes merge_group_verified / merge_group_run from
#     a push-only, continue-on-error step whose script fails CLOSED (verified
#     stays false on every error path and the step still exits 0);
#   - exactly the two heavy lanes (workspace-check, workspace-tests) gain the
#     `merge_group_verified != 'true'` term; clippy/fmt/deny/research-check
#     keep their current literals;
#   - the aggregator, workflow name, push trigger and main-alert.yml's
#     workflow_run contract stay intact, and main-alert.yml's
#     effective_success jq accepts a verified-skip push run's job list.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
for tool in ruby jq shellcheck; do
  command -v "$tool" >/dev/null || { echo "FAIL: $tool is required" >&2; exit 1; }
done
ruby -ryaml -rjson -rtmpdir -rfileutils -ropen3 <<'RUBY'
  REPO = 'HagaleTechnologies/pancetta'
  SHA = 'f76ca27b' + '0' * 32
  RUN_ID = '44446701800'
  URL = "https://github.com/#{REPO}/actions/runs/#{RUN_ID}"
  TOKEN = 'fixture-token'

  HEAVY_IF = "always() && ((needs.changes.outputs.code == 'true' && needs.changes.outputs.merge_group_verified != 'true') || github.event_name == 'schedule' || github.event_name == 'workflow_dispatch' || (github.event_name == 'merge_group' && needs.changes.outputs.merge_group_notes_only != 'true'))"
  CHEAP_IF = "always() && (needs.changes.outputs.code == 'true' || github.event_name == 'schedule' || github.event_name == 'workflow_dispatch' || (github.event_name == 'merge_group' && needs.changes.outputs.merge_group_notes_only != 'true'))"
  RESEARCH_IF = "always() && (needs.changes.outputs.research == 'true' || github.event_name == 'schedule' || github.event_name == 'workflow_dispatch' || (github.event_name == 'merge_group' && needs.changes.outputs.merge_group_notes_only != 'true'))"

  wf = YAML.load_file('.github/workflows/ci.yml')
  jobs = wf.fetch('jobs')
  changes = jobs.fetch('changes')
  steps = changes.fetch('steps')

  failures = []
  count = 0
  check = lambda do |name, &blk|
    blk.call
    count += 1
  rescue StandardError => e
    failures << "#{name}: #{e.message}"
  end
  expect = ->(cond, msg) { raise msg unless cond }

  # --- 1. structure ----------------------------------------------------------
  check.call('structure/changes outputs') do
    outputs = changes.fetch('outputs')
    expect.call(outputs['merge_group_verified'] == '${{ steps.merge-group-verified.outputs.verified }}',
                'missing/wrong merge_group_verified output')
    expect.call(outputs['merge_group_run'] == '${{ steps.merge-group-verified.outputs.run }}',
                'missing/wrong merge_group_run output')
  end
  step = steps.find { |s| s['id'] == 'merge-group-verified' }
  unless step
    # Fail fast with the real reason instead of crashing on step.fetch below.
    warn 'FAIL: structure/merge-group-verified step: missing merge-group-verified step'
    exit 1
  end
  check.call('structure/merge-group-verified step') do
    expect.call(step, 'missing merge-group-verified step')
    expect.call(step['if'] == "github.event_name == 'push'", 'step must run on push only (D5)')
    expect.call(step['continue-on-error'] == true, 'step must be continue-on-error (D4)')
    expect.call(!step.key?('uses'), 'step must add no uses: action (action-pins lane)')
    expected_env = {
      'GH_TOKEN' => '${{ secrets.GITHUB_TOKEN }}',
      'REPO' => '${{ github.repository }}',
      'SHA' => '${{ github.sha }}',
      'CACHE_INPUTS' => '${{ steps.filter.outputs.cache_inputs }}'
    }
    expect.call(step.fetch('env') == expected_env, "step env changed: #{step['env'].inspect}")
    expect.call(step.fetch('run') && !step['run'].include?('${{'), 'run must not interpolate expressions')
  end
  %w[workspace-check workspace-tests].each do |lane|
    check.call("structure/#{lane} gate") do
      expect.call(jobs.fetch(lane).fetch('if') == HEAVY_IF, "#{lane} must carry exactly the D7 gate (PAN-367)")
    end
  end
  %w[clippy fmt deny].each do |lane|
    check.call("structure/#{lane} gate") do
      expect.call(jobs.fetch(lane).fetch('if') == CHEAP_IF, "#{lane} event gate changed")
    end
  end
  check.call('structure/research-check gate') do
    expect.call(jobs.fetch('research-check').fetch('if') == RESEARCH_IF, 'research-check event gate changed')
  end
  check.call('structure/aggregator untouched') do
    ci = jobs.fetch('ci')
    expect.call(ci['if'] == 'always()', 'aggregator must keep if: always()')
    expect.call(ci.fetch('needs').sort == %w[clippy fmt workspace-check workspace-tests cross-platform-check
                                            deny codex-clean scan-trailers research-scan-tests].sort,
                'aggregator needs changed (changes must not join it)')
    expect.call(!ci.fetch('needs').include?('changes'), 'changes joined the aggregator needs (D4 forbids)')
  end
  check.call('structure/workflow identity') do
    expect.call(wf['name'] == 'CI', 'workflow renamed (Main Alert watches workflow name CI)')
    on = wf['on'] || wf[true]
    expect.call(on.fetch('push') == { 'branches' => ['main'] }, 'push trigger changed')
  end
  check.call('structure/cache_inputs filter') do
    filters_yaml = steps.find { |s| s.is_a?(Hash) && s.dig('with', 'filters') }.dig('with', 'filters')
    filters = YAML.safe_load(filters_yaml)
    expect.call(filters.fetch('cache_inputs').sort ==
                  ['**/Cargo.toml', '**/Cargo.lock', 'rust-toolchain.toml', '.cargo/**',
                   '.github/workflows/ci.yml'].sort,
                "cache_inputs filter must be exactly the five D3 paths: #{filters['cache_inputs'].inspect}")
  end
  check.call('structure/main-alert contract') do
    alert = YAML.load_file('.github/workflows/main-alert.yml')
    on = alert['on'] || alert[true]
    wr = on.fetch('workflow_run')
    expect.call(wr.fetch('workflows').include?('CI'), 'main-alert no longer watches CI')
    expect.call(wr.fetch('types') == ['completed'], 'main-alert trigger types changed')
    expect.call(wr.fetch('branches') == ['main', 'v[0-9]*'], 'main-alert branches changed')
  end

  # --- 2/3. lint and execute the shipped run: against a stub gh ---------------
  stub = <<~'STUB'
    #!/usr/bin/env bash
    set -euo pipefail
    { printf '%s\x1f' "$@"; printf '\n'; } >> "$STUB_LOG"
    [ "${GH_TOKEN:-}" = "$STUB_EXPECT_TOKEN" ] || { echo "stub gh: wrong GH_TOKEN" >&2; exit 97; }
    runs_url="api --paginate repos/$STUB_REPO/actions/workflows/ci.yml/runs?head_sha=$STUB_SHA&event=merge_group&status=success&per_page=100"
    jobs_url="api --paginate repos/$STUB_REPO/actions/runs/$STUB_RUN_ID/jobs?per_page=100&filter=latest"
    if [ "$*" = "$runs_url" ]; then
      [ -z "${STUB_FAIL_RUNS:-}" ] || { echo "stub gh: runs lookup forced failure" >&2; exit 5; }
      cat "$STUB_DIR"/runs-page*.json
    elif [ "$*" = "$jobs_url" ]; then
      [ -z "${STUB_FAIL_JOBS:-}" ] || { echo "stub gh: jobs lookup forced failure" >&2; exit 6; }
      cat "$STUB_DIR"/jobs-page*.json
    else
      echo "stub gh: unexpected call: $*" >&2; exit 98
    fi
  STUB

  runs_call = ['api', '--paginate', "repos/#{REPO}/actions/workflows/ci.yml/runs?head_sha=#{SHA}&event=merge_group&status=success&per_page=100"]
  jobs_call = ['api', '--paginate', "repos/#{REPO}/actions/runs/#{RUN_ID}/jobs?per_page=100&filter=latest"]

  mg_run = lambda do |id:, sha:, conclusion:, event: 'merge_group', created:|
    { 'id' => id, 'event' => event, 'head_sha' => sha, 'conclusion' => conclusion,
      'created_at' => created, 'html_url' => "https://github.com/#{REPO}/actions/runs/#{id}" }
  end
  good_run = mg_run.call(id: RUN_ID.to_i, sha: SHA, conclusion: 'success', created: '2026-11-19T02:46:12Z')
  other_sha = '71e705e9' + '1' * 32
  decoys = [
    mg_run.call(id: 4_444_991_363_1, sha: SHA, conclusion: 'success', event: 'push', created: '2026-11-19T03:13:29Z'),
    mg_run.call(id: 4_424_896_531_5, sha: SHA, conclusion: 'failure', created: '2026-11-15T10:00:00Z'),
    mg_run.call(id: 4_410_000_000_1, sha: other_sha, conclusion: 'success', created: '2026-11-14T10:00:00Z')
  ]
  verified_jobs = [
    { 'name' => 'Detect Code Changes', 'conclusion' => 'success' },
    { 'name' => 'Workspace Check', 'conclusion' => 'success' },
    { 'name' => 'Workspace Tests (1/4)', 'conclusion' => 'success' },
    { 'name' => 'Workspace Tests (2/4)', 'conclusion' => 'success' },
    { 'name' => 'Workspace Tests (3/4)', 'conclusion' => 'success' },
    { 'name' => 'Workspace Tests (4/4)', 'conclusion' => 'success' },
    # A skipped matrix job reports the literal template name; conclusion filters it.
    { 'name' => 'Workspace Tests (${{ matrix.partition }}/4)', 'conclusion' => 'skipped' },
    { 'name' => 'Clippy', 'conclusion' => 'success' },
    { 'name' => 'CI', 'conclusion' => 'success' }
  ]
  notes_only_jobs = [
    { 'name' => 'Detect Code Changes', 'conclusion' => 'success' },
    { 'name' => 'Workspace Check', 'conclusion' => 'skipped' },
    { 'name' => 'Workspace Tests (1/4)', 'conclusion' => 'skipped' },
    { 'name' => 'Workspace Tests (2/4)', 'conclusion' => 'skipped' },
    { 'name' => 'Workspace Tests (3/4)', 'conclusion' => 'skipped' },
    { 'name' => 'Workspace Tests (4/4)', 'conclusion' => 'skipped' },
    { 'name' => 'CI', 'conclusion' => 'success' }
  ]
  runs_page = ->(runs) { { 'total_count' => runs.length, 'workflow_runs' => runs } }
  jobs_page = ->(jobs) { { 'total_count' => jobs.length, 'jobs' => jobs } }

  Dir.mktmpdir('pan367-verified-') do |dir|
    script = File.join(dir, 'verified.sh')
    File.write(script, step.fetch('run'))
    ['bash -n', 'shellcheck -s bash'].each do |cmd|
      check.call("lint/#{cmd}") { expect.call(system(*cmd.split, script), "#{cmd} failed on the shipped run:") }
    end

    bin = File.join(dir, 'bin')
    FileUtils.mkdir_p(bin)
    File.write(File.join(bin, 'gh'), stub)
    File.chmod(0o755, File.join(bin, 'gh'))

    exec_case = lambda do |name, cache_inputs, runs_pages, jobs_pages, fail_runs: false, fail_jobs: false|
      work = Dir.mktmpdir("case-#{name}-", dir)
      runs_pages.each_with_index { |p, i| File.write(File.join(work, "runs-page#{i}.json"), p.is_a?(String) ? p : JSON.generate(p)) }
      jobs_pages.each_with_index { |p, i| File.write(File.join(work, "jobs-page#{i}.json"), p.is_a?(String) ? p : JSON.generate(p)) }
      log = File.join(work, 'gh.log')
      out_file = File.join(work, 'output')
      summary = File.join(work, 'summary')
      File.write(log, '')
      File.write(out_file, '')
      File.write(summary, '')
      env = {
        'PATH' => "#{bin}:#{ENV.fetch('PATH')}", 'GH_TOKEN' => TOKEN, 'REPO' => REPO, 'SHA' => SHA,
        'CACHE_INPUTS' => cache_inputs, 'GITHUB_OUTPUT' => out_file, 'GITHUB_STEP_SUMMARY' => summary,
        'STUB_LOG' => log, 'STUB_DIR' => work, 'STUB_REPO' => REPO, 'STUB_SHA' => SHA,
        'STUB_RUN_ID' => RUN_ID, 'STUB_EXPECT_TOKEN' => TOKEN
      }
      env['STUB_FAIL_RUNS'] = '1' if fail_runs
      env['STUB_FAIL_JOBS'] = '1' if fail_jobs
      out, err, status = Open3.capture3(env, 'bash', '--noprofile', '--norc', script)
      raise "step exited #{status.exitstatus}: #{out} #{err}" unless status.success?
      calls = File.read(log).lines.map { |l| l.chomp.split("\x1f") }
      outputs = File.read(out_file).lines.to_h { |l| l.chomp.split('=', 2) }
      [calls, outputs, File.read(summary), out]
    end

    {
      # name => [cache_inputs, runs_pages, jobs_pages, fail_runs, fail_jobs]
      'verified' => ['false', [runs_page.call([good_run] + decoys)], [jobs_page.call(verified_jobs)], false, false],
      'two-pages' => ['false', [runs_page.call(decoys), runs_page.call([good_run])],
                      [jobs_page.call(verified_jobs)], false, false],
      'notes-only-mg' => ['false', [runs_page.call([good_run])], [jobs_page.call(notes_only_jobs)], false, false],
      'no-run' => ['false', [runs_page.call(decoys)], [], false, false],
      'gh-fails' => ['false', [runs_page.call([good_run])], [jobs_page.call(verified_jobs)], true, false],
      'jobs-fails' => ['false', [runs_page.call([good_run])], [jobs_page.call(verified_jobs)], false, true],
      # AC2's unparseable-response shape: a truncated body makes jq fail.
      'bad-json' => ['false', ['{"total_count": 1, "workflow_runs": ['], [jobs_page.call(verified_jobs)], false, false],
      'cache-inputs' => ['true', [runs_page.call([good_run])], [jobs_page.call(verified_jobs)], false, false]
    }.each do |name, (cache_inputs, runs_pages, jobs_pages, fail_runs, fail_jobs)|
      check.call("case/#{name}") do
        calls, outputs, summary, out = exec_case.call(name, cache_inputs, runs_pages, jobs_pages,
                                                      fail_runs: fail_runs, fail_jobs: fail_jobs)
        case name
        when 'verified', 'two-pages'
          expect.call(calls == [runs_call, jobs_call], "gh calls #{calls.inspect}")
          expect.call(outputs == { 'verified' => 'true', 'run' => RUN_ID }, "outputs #{outputs.inspect}")
          expect.call(summary.include?("already verified in merge group #{RUN_ID} (#{URL})"),
                      "summary line missing: #{summary.inspect}")
          expect.call(out.include?("::notice::already verified in merge group #{RUN_ID} (#{URL})"),
                      "notice missing from stdout: #{out.inspect}")
        when 'notes-only-mg'
          expect.call(calls == [runs_call, jobs_call], "gh calls #{calls.inspect}")
          expect.call(outputs == { 'verified' => 'false' }, "outputs #{outputs.inspect}")
          expect.call(summary.include?('did not succeed at the heavy lanes (Workspace Check=0, Workspace Tests=0)'),
                      "heavy-lanes reason missing: #{summary.inspect}")
        when 'no-run'
          expect.call(calls == [runs_call], "gh calls #{calls.inspect}")
          expect.call(outputs == { 'verified' => 'false' }, "outputs #{outputs.inspect}")
          expect.call(summary.include?('no successful merge-group run of this SHA'),
                      "no-run reason missing: #{summary.inspect}")
        when 'gh-fails'
          expect.call(calls == [runs_call], "gh calls #{calls.inspect}")
          expect.call(outputs == { 'verified' => 'false' }, "outputs #{outputs.inspect}")
          expect.call(summary.include?('merge-group run lookup failed (fail closed)'),
                      "lookup-failed reason missing: #{summary.inspect}")
        when 'jobs-fails'
          expect.call(calls == [runs_call, jobs_call], "gh calls #{calls.inspect}")
          expect.call(outputs == { 'verified' => 'false' }, "outputs #{outputs.inspect}")
          expect.call(summary.include?("job lookup for merge-group run #{RUN_ID} failed (fail closed)"),
                      "jobs-failed reason missing: #{summary.inspect}")
        when 'bad-json'
          expect.call(calls == [runs_call], "gh calls #{calls.inspect}")
          expect.call(outputs == { 'verified' => 'false' }, "outputs #{outputs.inspect}")
          expect.call(summary.include?('could not parse the run list (fail closed)'),
                      "unparseable-response reason missing: #{summary.inspect}")
        when 'cache-inputs'
          expect.call(calls.empty?, "cache-input change must make no API call: #{calls.inspect}")
          expect.call(outputs == { 'verified' => 'false' }, "outputs #{outputs.inspect}")
          expect.call(summary.include?('cache-key inputs changed'),
                      "cache-inputs reason missing: #{summary.inspect}")
        end
      end
    end
  end

  # --- 4. Main Alert's effective_success accepts a verified-skip push run -----
  # Verbatim predicate from .github/workflows/main-alert.yml effective_success()
  # (the jq program at its lines 95-99), with RECOVERY_JOBS '{}' as configured.
  check.call('main-alert/effective_success replay') do
    predicate = <<~JQ
      [.jobs[] | select(.conclusion != "skipped")] as $ran
      | ($ran | length) > 0
        and (($req[$wf] // null) as $re
             | $re == null or any($ran[]; .conclusion == "success" and (.name | test($re))))
    JQ
    fixture = {
      'jobs' => [
        { 'name' => 'Detect Code Changes', 'conclusion' => 'success' },
        { 'name' => 'Clippy', 'conclusion' => 'success' },
        { 'name' => 'Format', 'conclusion' => 'success' },
        { 'name' => 'Workspace Tests (1/4)', 'conclusion' => 'skipped' },
        { 'name' => 'Workspace Tests (2/4)', 'conclusion' => 'skipped' },
        { 'name' => 'Workspace Tests (3/4)', 'conclusion' => 'skipped' },
        { 'name' => 'Workspace Tests (4/4)', 'conclusion' => 'skipped' },
        { 'name' => 'Workspace Check', 'conclusion' => 'skipped' },
        { 'name' => 'Cargo Deny', 'conclusion' => 'success' },
        { 'name' => 'Research Check', 'conclusion' => 'success' },
        { 'name' => 'Research scan automation tests', 'conclusion' => 'success' },
        { 'name' => 'CI', 'conclusion' => 'success' }
      ]
    }
    out, err, status = Open3.capture3('jq', '-e', '--arg', 'wf', 'CI', '--argjson', 'req', '{}',
                                      predicate, stdin_data: JSON.generate(fixture))
    expect.call(status.success? && out.strip == 'true', "effective_success was false: #{out} #{err}")
  end

  unless failures.empty?
    warn failures.map { |f| "FAIL: #{f}" }.join("\n")
    warn "#{failures.size} failed, #{count} passed"
    exit 1
  end
  puts "PASS: #{count} merge-group-verified cases (PAN-367)"
RUBY
