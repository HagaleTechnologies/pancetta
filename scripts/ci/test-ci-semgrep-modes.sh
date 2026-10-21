#!/usr/bin/env bash
# PAN-107: a skipped Semgrep scan must never report the same success as a
# completed one. This tests the workflow's mode-routing structure offline: the
# semgrep binary itself is stubbed, so no network, token, or registry access
# happens here. The end-to-end scan behaviour of the vendored ruleset is
# exercised for real by the semgrep/local-ruleset job on every token-absent
# run (fork PRs, Dependabot), and was validated when the ruleset was vendored.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
ruby -ryaml -rtmpdir -rfileutils -ropen3 <<'RUBY'
jobs = YAML.load_file('.github/workflows/semgrep.yml').fetch('jobs')
mode = jobs.fetch('semgrep-mode')
platform = jobs.fetch('semgrep')
local = jobs.fetch('semgrep-local')

def strip_expr(s)
  s.to_s.gsub('${{ ', '').gsub(' }}', '')
end

# Routing preconditions: the `secrets` context is NOT available in a
# job-level `if:` (GitHub fails the workflow with "Unrecognized named-value";
# actionlint flags it), so a decider job must export the mode as an output
# and the scan jobs must route on `needs`.
jobs.each do |key, job|
  if_expr = strip_expr(job['if'])
  if if_expr.include?('secrets.')
    raise "job #{key} uses the secrets context in a job-level if:, which GitHub does not allow"
  end
end
raise 'decider output not wired' \
  unless strip_expr(mode['outputs']['mode']) == 'steps.decide.outputs.mode'
raise 'platform job must wait on the decider' unless Array(platform['needs']).include?('semgrep-mode')
raise 'local job must wait on the decider' unless Array(local['needs']).include?('semgrep-mode')
raise 'platform job must run only in platform mode' \
  unless strip_expr(platform['if']).include?("needs.semgrep-mode.outputs.mode == 'platform'")
raise 'local job must run only in local mode' \
  unless strip_expr(local['if']).include?("needs.semgrep-mode.outputs.mode == 'local'")
raise 'platform check name must stay semgrep/ci (Semgrep platform convention)' \
  unless platform['name'] == 'semgrep/ci'
raise 'local check must be named so a reader can tell it from the platform scan' \
  unless local['name'] == 'semgrep/local-ruleset'

# No step in this workflow may paper over a skip with exit 0.
jobs.each_value do |job|
  Array(job['steps']).each do |step|
    next unless step['run']
    if step['run'].match?(/^\s*exit 0\s*$/)
      raise "silent skip (bare exit 0) in job #{job['name']}"
    end
  end
end

# House rule: every uses: ref is a full commit SHA.
jobs.each_value do |job|
  Array(job['steps']).each do |step|
    if step['uses'] && !step['uses'].match?(/@[0-9a-f]{40}/)
      raise "unpinned action: #{step['uses']}"
    end
  end
end

# The local scan must use the vendored in-repo ruleset and fail on findings.
local_run = local['steps'].find { |s| s['run'] }.fetch('run')
raise 'local scan must scan the vendored config' \
  unless local_run.include?('.semgrep/secrets.yaml')
raise 'local scan must exit nonzero on findings (--error)' \
  unless local_run.include?('--error')
raise 'local scan must not run the token-dependent platform scan' \
  if local_run.include?('semgrep ci')

# The vendored ruleset must exist and be a nonempty semgrep rules file.
rules = YAML.load_file('.semgrep/secrets.yaml')
unless rules.is_a?(Hash) && rules['rules'].is_a?(Array) && !rules['rules'].empty?
  raise '.semgrep/secrets.yaml is not a nonempty semgrep rules file'
end

Dir.mktmpdir('pan107-') do |dir|
  # Stub semgrep: record argv, exit 0 unless SEMGREP_STUB_EXIT is set.
  stub = File.join(dir, 'semgrep')
  File.write(stub, <<~SH)
    #!/usr/bin/env bash
    printf '%s\\n' "$*" >> #{File.join(dir, 'argv')}
    exit "${SEMGREP_STUB_EXIT:-0}"
  SH
  File.chmod(0o755, stub)
  env = { 'PATH' => "#{dir}:#{ENV['PATH']}" }
  argv_file = File.join(dir, 'argv')

  # Decider: reads the secret via env (legal in a step), exports only the mode.
  decide_step = mode['steps'].find { |s| s['run'] }
  raise 'decider must map the secret through env' \
    unless strip_expr(decide_step['env']['SEMGREP_APP_TOKEN']) == 'secrets.SEMGREP_APP_TOKEN'
  dscript = File.join(dir, 'decide.sh')
  File.write(dscript, decide_step.fetch('run'))
  raise 'decider shell syntax' unless system('bash', '-n', dscript)

  out_file = File.join(dir, 'github_output')
  denv = env.merge('GITHUB_OUTPUT' => out_file)
  %w[tok-a1 tok-b2 tok-c3].each do |tok|
    FileUtils.rm_f(out_file)
    _o, _e, st = Open3.capture3(denv.merge('SEMGREP_APP_TOKEN' => tok), 'bash', dscript)
    raise "decider must succeed with a token (#{tok})" unless st.success?
    raise "decider must export platform mode (#{tok})" unless File.read(out_file).include?('mode=platform')
    raise "decider must not leak the token" if File.read(out_file).include?(tok)
  end
  FileUtils.rm_f(out_file)
  _o, _e, st = Open3.capture3(denv.merge('SEMGREP_APP_TOKEN' => ''), 'bash', dscript)
  raise 'decider must succeed without a token' unless st.success?
  raise 'decider must export local mode' unless File.read(out_file).include?('mode=local')

  platform_run = platform['steps'].find { |s| s['run'] }.fetch('run')
  pscript = File.join(dir, 'platform.sh')
  File.write(pscript, platform_run)
  raise 'platform step shell syntax' unless system('bash', '-n', pscript)

  # Token absent: the platform step must fail loudly, before any semgrep call.
  FileUtils.rm_f(argv_file)
  _out, err, status = Open3.capture3(env.merge('SEMGREP_APP_TOKEN' => ''), 'bash', pscript)
  raise 'platform step must fail without a token' if status.success?
  raise 'platform step must not call semgrep without a token' if File.exist?(argv_file)
  raise 'platform failure must explain itself' unless err.include?('SEMGREP_APP_TOKEN')

  # Token present: the platform step must call `semgrep ci` and pass through.
  FileUtils.rm_f(argv_file)
  _out, _err, status = Open3.capture3(env.merge('SEMGREP_APP_TOKEN' => 'tok'), 'bash', pscript)
  raise 'platform step must run semgrep ci with a token' unless status.success? && File.read(argv_file).strip == 'ci'

  # Token present but semgrep fails: the step must fail (no suppressed exit).
  _out, _err, status = Open3.capture3(env.merge('SEMGREP_APP_TOKEN' => 'tok', 'SEMGREP_STUB_EXIT' => '1'), 'bash', pscript)
  raise 'platform step must propagate a semgrep failure' if status.success?

  lscript = File.join(dir, 'local.sh')
  File.write(lscript, local_run)
  raise 'local step shell syntax' unless system('bash', '-n', lscript)

  # Clean tree: local scan calls semgrep scan with the vendored config, exits 0.
  FileUtils.rm_f(argv_file)
  _out, _err, status = Open3.capture3(env, 'bash', lscript)
  raise 'local step must succeed on a clean scan' unless status.success?
  argv = File.read(argv_file).strip
  raise "local step wrong argv: #{argv}" \
    unless argv.include?('scan') && argv.include?('.semgrep/secrets.yaml') && argv.include?('--error')

  # Finding: exit code 1 must reach the job conclusion.
  _out, _err, status = Open3.capture3(env.merge('SEMGREP_STUB_EXIT' => '1'), 'bash', lscript)
  raise 'local step must propagate a blocking finding' if status.success?

  puts 'PASS: 11 semgrep-mode routing cases (PAN-107)'
end
RUBY
