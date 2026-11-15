#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
ruby -ryaml -e '
steps = YAML.load_file(".github/workflows/release.yml").fetch("jobs").fetch("release").fetch("steps")
step = steps.find { |s| s["name"] == "Generate aggregate SHA256SUMS" } || raise("Missing checksum step")
raise "Manifest ordering" unless steps.index(step) > steps.index { |s| s.fetch("uses", "").start_with?("actions/download-artifact@") } && steps.index(step) < steps.index { |s| s["name"] == "Create draft release (operator publishes)" }
File.write(ARGV[0], step.fetch("run"))
' "$work/manifest.sh"
mkdir "$work/dist"
printf linux > "$work/dist/pancetta-v1.2.3-x86_64-unknown-linux-gnu.tar.gz"
printf windows > "$work/dist/pancetta-v1.2.3-x86_64-pc-windows-gnu.zip"
for decoy in pancetta-v1.2.3.zip.sha256 pancetta-v1.2.3-linux.debug pancetta-v1.2.3-mac.dSYM.tar.gz; do
  printf decoy > "$work/dist/$decoy"
done
(cd "$work" && bash manifest.sh)
test "$(wc -l < "$work/dist/SHA256SUMS")" -eq 2
test "$(grep -Ec '^[0-9a-f]{64}  pancetta-[^/ ]+\.(tar.gz|zip)$' "$work/dist/SHA256SUMS")" -eq 2
if grep -Eq '\.sha256|\.debug|\.dSYM' "$work/dist/SHA256SUMS"; then echo 'Unexpected content' >&2; exit 1; fi
LC_ALL=C sort -k2 "$work/dist/SHA256SUMS" > "$work/sorted"
cmp "$work/sorted" "$work/dist/SHA256SUMS"
(cd "$work/dist" && sha256sum -c SHA256SUMS)
rm "$work/dist/pancetta-v1.2.3-x86_64-pc-windows-gnu.zip"
(cd "$work/dist" && sha256sum -c --ignore-missing SHA256SUMS)
echo 'PASS: sorted archive-only manifest, self-verification and partial downloads'
ruby -ryaml <<'RUBY'
raw = File.read('.github/workflows/release.yml')
jobs = YAML.load(raw).fetch('jobs')
build = jobs.fetch('build')
raise 'Wrong build permissions' unless build['permissions'] == {'contents' => 'read', 'id-token' => 'write', 'attestations' => 'write'}
steps = build.fetch('steps')
attest = steps.find { |s| s.fetch('uses', '').start_with?('actions/attest-build-provenance@') } || raise('Missing attestation')
raise 'Unpinned attestation' unless raw.match?(/uses: actions\/attest-build-provenance@[0-9a-f]{40} # v4\.2\.2/)
raise 'Wrong tag guard' unless attest['if'] == "startsWith(github.ref, 'refs/tags/v')"
['pancetta-*.tar.gz', 'pancetta-*.zip'].each do |glob|
  raise "Missing subject #{glob}" unless attest.fetch('with').fetch('subject-path').lines.map(&:strip).include?(glob)
end
package = steps.index { |s| s['name'] == 'Package' }
upload = steps.index { |s| s.fetch('uses', '').start_with?('actions/upload-artifact@') }
raise 'Wrong attestation order' unless package < steps.index(attest) && steps.index(attest) < upload
puts 'PASS: SHA-pinned, tag-gated per-leg attestation and least-privilege scopes'
RUBY
grep -qF 'sha256sum -c --ignore-missing SHA256SUMS' docs/UPGRADING.md
grep -qF 'shasum -a 256 -c --ignore-missing SHA256SUMS' docs/UPGRADING.md
grep -qE 'gh attestation verify .* --repo HagaleTechnologies/pancetta' docs/UPGRADING.md
for glob in '*.debug' '*.dSYM.tar.gz'; do grep -qF "$glob" docs/UPGRADING.md; done
head -n 25 .github/workflows/release.yml | grep -qF 'id-token/attestations write'
ruby <<'RUBY'
section = File.read('docs/DECISIONS/config-and-platform.md').split(/^## /).find { |s| s.start_with?('Release build provenance and pre-release marking (PAN-308,') }
raise 'Missing PAN-308 decision' unless section
['id-token: write', 'attestations: write', 'PAN-106'].each { |text| raise "Missing #{text}" unless section.include?(text) }
RUBY
echo 'PASS: operator verification commands, symbol assets and PAN-106 scope decision documented'
