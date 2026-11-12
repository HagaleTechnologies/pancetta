#!/usr/bin/env bash
# PAN-336: Windows releases carry service scripts and validate them before shipping.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
for file in install-pancetta-service.ps1 uninstall-pancetta-service.ps1 README.md; do
    test -f "packaging/windows/$file"
done
if grep -Ein '(^|[;[:space:]])(Remove-Item|rmdir|rd[[:space:]])' packaging/windows/uninstall-pancetta-service.ps1; then
    echo 'Uninstall must preserve all files' >&2
    exit 1
fi
grep -Fq 'install-pancetta-service.ps1' docs/RUNBOOK.md
ruby -ryaml <<'RUBY'
steps = YAML.load_file('.github/workflows/release.yml').fetch('jobs').fetch('build').fetch('steps')
package = steps.find { |s| s['name'] == 'Package' } || raise('Package step missing')
raise 'Package needs bash' unless package['shell'] == 'bash'
run = package.fetch('run')
windows = run.scan(/if \[ "\$\{\{ runner.os \}\}" = "Windows" \]; then\n(.*?)^\s*else/m).flatten.join("\n")
raise 'Windows packaging copy missing' unless windows.include?('cp -r packaging/windows "$NAME/windows"')
raise 'PowerShell parse command missing' unless windows.include?('powershell -NoProfile -Command')
raise 'Parser missing' unless windows.include?('[System.Management.Automation.Language.Parser]::ParseFile')
raise 'Parse errors must fail packaging' unless windows.include?('if ($errs)') && windows.include?('exit 1')
raise 'Both scripts must be parsed' unless windows.include?('foreach ($f in "install","uninstall")') && windows.include?('packaging/windows/$f-pancetta-service.ps1')
%w[install uninstall].each do |verb|
  path = "$NAME/windows/#{verb}-pancetta-service.ps1"
  raise "Archive verification missing: #{path}" unless windows.lines.any? { |l| l.include?('7z l "$ARCHIVE"') && l.include?('grep -F') && l.include?(path) }
end
raise 'Parse must precede zip' unless windows.index('ParseFile') < windows.index('7z a')
puts 'PASS: Windows release service packaging (PAN-336)'
RUBY
