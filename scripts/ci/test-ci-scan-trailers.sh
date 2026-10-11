#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
ruby -ryaml -rtmpdir -rfileutils -ropen3 <<'RUBY'
jobs = YAML.load_file('.github/workflows/ci.yml').fetch('jobs')
job = jobs.fetch('scan-trailers')
raise 'wrong condition' unless job['if'] == "github.event_name == 'pull_request' && startsWith(github.event.pull_request.title, 'Weekly research scan ')"
raise 'not required' unless jobs['ci']['needs'].include?('scan-trailers')
job['steps'].each { |s| raise 'unpinned action' if s['uses'] && !s['uses'].match?(/@[0-9a-f]{40}$/) }
Dir.mktmpdir('pan172-trailers-') do |dir|
  script = File.join(dir, 'trailers.sh')
  File.write(script, job['steps'].find { |s| s['run'] }.fetch('run'))
  raise 'shell syntax' unless system('bash', '-n', script)
  raise 'shellcheck' unless system('shellcheck', '-s', 'bash', script)
  FileUtils.mkdir_p(File.join(dir, 'repo'))
  Dir.chdir(File.join(dir, 'repo')) do
    def git(*args)
      out, err, status = Open3.capture3('git', *args)
      raise err unless status.success?
      out.strip
    end
    git('init', '-q'); git('config', 'user.name', 'Test'); git('config', 'user.email', 'test@example.invalid')
    git('commit', '--allow-empty', '-qm', 'base'); base = git('rev-parse', 'HEAD')
    cases = [
      ['docs(research): weekly web scan 2026-10-12', 0],
      ["note\n\nCo-Authored-By: Claude <noreply@anthropic.com>", 1],
      ["note\n\nco-authored-by: x <y>", 1],
      ["note\n\nClaude-Session: https://claude.ai/code/session_x", 1],
      ['the phrase co-authored-by appears mid-line', 0],
      ["note\n\nSigned-off-by: A <a@b>", 0],
      ["note\n\nCo-Authored-By: x <y>", 1]
    ]
    cases.each_with_index do |(message, expected), i|
      git('reset', '--hard', base)
      git('commit', '--allow-empty', '-qm', 'clean') if i == 6
      git('commit', '--allow-empty', '-qm', message)
      head = git('rev-parse', 'HEAD')
      out, err, status = Open3.capture3({'BASE_SHA' => base, 'HEAD_SHA' => head}, 'bash', script)
      raise "case #{i}: #{out} #{err}" unless status.exitstatus == expected
      raise 'missing offending SHA' if expected == 1 && !out.include?(git('rev-parse', '--short', head))
    end
    # Failed range discovery must not produce a green guard.
    _, _, status = Open3.capture3({'BASE_SHA' => base, 'HEAD_SHA' => '0' * 40}, 'bash', script)
    raise 'invalid SHA accepted' if status.success?
    puts "PASS: #{cases.length + 1} trailer cases"
  end
end
RUBY
