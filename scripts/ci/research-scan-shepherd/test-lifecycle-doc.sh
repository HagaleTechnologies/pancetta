#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
ruby -rjson -ropen3 <<'RUBY'
doc = File.read('docs/routines/pancetta-research-scan.md')
['## Routine settings', '## Prompt edits on claude.ai', '## Lifecycle', '### Modes',
 '### Scan (Mondays)', '### Shepherd (every run)', '### Review rounds',
 '### Replies and resolution', '### Follow-up ticket', '### Landing'].each do |heading|
  raise "missing #{heading}" unless doc.lines.map(&:chomp).include?(heading)
end
examples = doc.match(/<!-- disposition-examples -->\s*```text\n(.*?)\n```/m)[1].lines.map(&:chomp)
raise 'missing examples' unless examples.length >= 3
examples.each_with_index do |line, i|
  out, err, status = Open3.capture3('jq', '-L', 'scripts/ci/research-scan-shepherd', '-R', 'include "disposition"; disposition_kind', stdin_data: line)
  raise "unparseable reply: #{err}" unless status.success? && JSON.parse(out) == %w[fixed deferred blocked][i]
end
pointer = doc.match(/<!-- pointer-block -->\s*```text\n(.*?)\n```/m)[1]
['git show origin/main:docs/routines/pancetta-research-scan.md',
 'Never add a Co-Authored-By or Claude-Session line',
 'If that file does not exist on origin/main'].each { |s| raise "missing #{s}" unless pointer.include?(s) }
raise 'title pattern' unless doc.include?('Weekly research scan YYYY-MM-DD')
config = JSON.parse(File.read('scripts/ci/research-scan-shepherd/shepherd.sh').match(/^CONFIG='([^']+)'$/)[1])
raise 'title regex' unless Regexp.new(config.fetch('title_regex')).match?('Weekly research scan 2026-10-12')
raise 'unsafe trailer instruction' if doc.lines.any? { |l| l.match?(/Co-Authored-By|Claude-Session/) && !l.match?(/Never|No /) }
puts 'PASS: lifecycle headings, disposition grammar, pointer, title and trailer policy'
RUBY
