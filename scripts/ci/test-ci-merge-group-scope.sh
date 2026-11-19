#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
ruby -ryaml -rtmpdir -rfileutils -ropen3 <<'RUBY'
jobs = YAML.load_file('.github/workflows/ci.yml').fetch('jobs')
step = jobs.fetch('changes').fetch('steps').find { |s| s['id'] == 'merge-group-scope' }
raise 'missing merge-group-scope' unless step
raise 'wrong output' unless jobs['changes']['outputs']['merge_group_notes_only'] == '${{ steps.merge-group-scope.outputs.notes_only }}'
raise 'wrong event' unless step['if'] == "github.event_name == 'merge_group'"
count = 0
Dir.mktmpdir('pan172-scope-') do |dir|
  script = File.join(dir, 'scope.sh')
  File.write(script, step.fetch('run'))
  ['bash -n', 'shellcheck -s bash'].each { |cmd| raise cmd unless system(*cmd.split, script) }
  FileUtils.mkdir_p(File.join(dir, "repo"))
  Dir.chdir(File.join(dir, "repo")) do
    def git(*args)
      out, err, status = Open3.capture3('git', *args)
      raise err unless status.success?
      out.strip
    end
    git('init', '-q'); git('config', 'user.name', 'Test'); git('config', 'user.email', 'test@example.invalid')
    FileUtils.mkdir_p('pancetta/src'); File.write('pancetta/src/x.rs', "fn main() {}\n")
    git('add', '.'); git('commit', '-qm', 'base'); base = git('rev-parse', 'HEAD')
    cases = {
      'note-only' => ['research/notes/web-scan-2026-10-12.md'],
      'two-notes' => ['research/notes/a.md', 'research/notes/b.md'],
      'rename-into-notes' => :rename,
      'note-and-lockfile' => ['research/notes/a.md', 'Cargo.lock'],
      'deny' => ['deny.toml'], 'lookalike' => ['research/notes-old/x.md'],
      'non-ascii' => ['research/notes/scan-é.md'], 'empty' => [], 'bad-sha' => :bad
    }
    cases.each do |name, paths|
      git('reset', '--hard', base)
      if paths == :rename
        FileUtils.mkdir_p('research/notes'); git('mv', 'pancetta/src/x.rs', 'research/notes/x.rs')
      elsif paths.is_a?(Array)
        paths.each { |p| FileUtils.mkdir_p(File.dirname(p)); File.write(p, "note\n") }
      end
      git('add', '--all', 'research', 'pancetta', *(['Cargo.lock', 'deny.toml'].select { |p| File.exist?(p) })) if Dir.exist?('research')
      git('add', '.')
      git('commit', '--allow-empty', '-qm', name)
      head = paths == :bad ? '0' * 40 : (name == 'empty' ? base : git('rev-parse', 'HEAD'))
      output = File.join(dir, 'output'); File.write(output, '')
      out, err, status = Open3.capture3({'BASE_SHA' => base, 'HEAD_SHA' => head, 'GITHUB_OUTPUT' => output}, 'bash', script)
      expected = %w[note-only two-notes non-ascii].include?(name)
      if name == 'bad-sha'
        raise name if status.success? || File.read(output).include?('notes_only=true')
      else
        raise "#{name}: #{out} #{err}" unless status.success? && File.read(output) == "notes_only=#{expected}\n"
      end
      count += 1
    end
  end
end
lanes = %w[clippy fmt workspace-check workspace-tests deny cross-platform-check]
# PAN-367: `verified` is changes.outputs.merge_group_verified (push-only; empty
# on every other event). Rows 1-6 reproduce the pre-PAN-367 behaviour; rows 7-11
# pin the new one: only the two heavy lanes honour the verified output, and
# schedule/workflow_dispatch ignore it entirely.
[
  ['merge_group', '', 'true',  '',      false, false, false],
  ['merge_group', '', 'false', '',      true,  true,  false],
  ['merge_group', '', '',      '',      true,  true,  false],
  ['pull_request', 'false', '', '',     false, false, false],
  ['pull_request', 'true',  '', '',     true,  true,  true],
  ['push', 'false', '', '',             false, false, false],
  ['push', 'true',  '', '',             true,  true,  false],
  ['push', 'true',  '', 'true',         true,  false, false],
  ['push', 'true',  '', 'false',        true,  true,  false],
  ['schedule', '', '', 'true',          true,  true,  true],
  ['workflow_dispatch', '', '', 'true', true,  true,  true]
].each do |event, code, notes, verified, cheap, heavy, cross|
  lanes.each do |lane|
    expr = jobs.fetch(lane).fetch('if').sub('always() &&', '')
    expr = expr.gsub("needs.changes.outputs.code == 'true'", (code == 'true').to_s)
    expr = expr.gsub("needs.changes.outputs.merge_group_verified != 'true'", (verified != 'true').to_s)
    expr = expr.gsub("needs.changes.outputs.merge_group_notes_only != 'true'", (notes != 'true').to_s)
    expr = expr.gsub(/github.event_name == '([^']+)'/) { (event == Regexp.last_match(1)).to_s }
    raise "unknown term: #{expr}" unless expr.gsub(/true|false|&&|\|\||[()!\s]/, '').empty?
    want = if lane == 'cross-platform-check'
             cross
           elsif %w[workspace-check workspace-tests].include?(lane)
             heavy
           else
             cheap
           end
    raise "#{lane}/#{event}/#{notes}/#{verified}" unless eval(expr) == want
    count += 1
  end
end
puts "PASS: #{count} cases"
RUBY
