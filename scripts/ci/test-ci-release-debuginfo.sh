#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
root=$PWD
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
for tool in gcc objcopy readelf; do command -v "$tool" >/dev/null; done
mkdir -p "$work/target/demo-target/release"
bin="$work/target/demo-target/release/pancetta"
printf 'int helper(int x){return x*3;} int main(void){return helper(14);}\n' > "$work/demo.c"
gcc -g -O0 "$work/demo.c" -o "$bin"
output=$(cd "$work" && bash "$root/scripts/split-debuginfo.sh" "$bin" pancetta-v1.2.3-demo-target)
test "$output" = pancetta-v1.2.3-demo-target.debug
rc=0
"$bin" || rc=$?
test "$rc" -eq 42
readelf -S "$bin" > "$work/sections"
if grep -q '\.debug_' "$work/sections"; then echo 'Unexpected content' >&2; exit 1; fi
readelf -p .gnu_debuglink "$bin" > "$work/link"
grep -qF pancetta-v1.2.3-demo-target.debug "$work/link"
readelf -S "$work/pancetta-v1.2.3-demo-target.debug" > "$work/debug-sections"
grep -q '\.debug_info' "$work/debug-sections"
echo 'PASS: ELF split retains DWARF, debuglink and executable behavior'
ruby -ryaml <<'RUBY'
steps = YAML.load_file('.github/workflows/release.yml').fetch('jobs').fetch('build').fetch('steps')
build = steps.find { |s| s['name'] == 'Build release binary' }
{'CARGO_PROFILE_RELEASE_DEBUG' => '1', 'CARGO_PROFILE_RELEASE_STRIP' => 'none', 'CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO' => 'off'}.each do |key, value|
  raise "Missing env #{key}" unless build.fetch('env')[key] == value
end
split = steps.index { |s| s.fetch('run', '').include?('scripts/split-debuginfo.sh') }
package = steps.index { |s| s['name'] == 'Package' }
gate = steps.index { |s| s.fetch('name', '').start_with?('Decode fixture verification') }
raise 'Split must follow gates and precede package' unless gate < split && split < package
raise 'Package must use bash' unless steps[package]['shell'] == 'bash'
upload = steps.find { |s| s.fetch('uses', '').start_with?('actions/upload-artifact@') }
['pancetta-*.debug', 'pancetta-*.dSYM.tar.gz'].each do |glob|
  raise "Missing symbol glob #{glob}" unless upload.fetch('with').fetch('path').lines.map(&:strip).include?(glob)
end
puts 'PASS: release debuginfo env, split ordering and upload wiring'
RUBY
