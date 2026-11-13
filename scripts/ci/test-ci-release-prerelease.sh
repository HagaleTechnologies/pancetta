#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
ruby -ryaml <<'RUBY'
steps = YAML.load_file('.github/workflows/release.yml').fetch('jobs').fetch('release').fetch('steps')
run = steps.find { |s| s['name'] == 'Create draft release (operator publishes)' }.fetch('run')
['--prerelease', 'v*-?*)', '--draft', '--verify-tag', '--title', '--notes-file notes.md', '"${prerelease_args[@]}"'].each do |part|
  raise "Missing release guard or flag: #{part}" unless run.include?(part)
end
# Execute the actual block with a stub gh to prove stable/rc flag selection.
require 'tmpdir'
require 'open3'
Dir.mktmpdir do |dir|
  File.write("#{dir}/gh", "#!/bin/bash\nprintf '%s\\n' \"$@\"\n")
  File.chmod(0755, "#{dir}/gh")
  %w[v1.2.3 v1.2.3-rc1].each do |tag|
    out, err, status = Open3.capture3({'PATH' => "#{dir}:#{ENV.fetch('PATH')}", 'GITHUB_REF_NAME' => tag}, 'bash', '-c', run)
    raise err unless status.success?
    raise 'Wrong prerelease selection' unless out.lines.map(&:strip).include?('--prerelease') == tag.include?('-')
  end
end
puts 'PASS: draft rc release flags and stable flag selection'
RUBY
