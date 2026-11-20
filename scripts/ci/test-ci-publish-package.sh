#!/usr/bin/env bash
# PAN-384: offline tests for check-crate-size.sh, the workspace-check step that
# runs check-publish-package.sh, the `code` paths filter that triggers it, and
# the docs that describe the crates.io posture.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
gate=scripts/ci/check-crate-size.sh
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

# 1. Size gate cases.
head -c 10 /dev/zero >"$scratch/small.crate"
bash "$gate" 100 "$scratch/small.crate" >"$scratch/out" || { cat "$scratch/out"; echo "FAIL: file under the limit rejected"; exit 1; }
grep -q 'small.crate: 10 bytes (limit 100)' "$scratch/out" || { cat "$scratch/out"; echo "FAIL: size not printed"; exit 1; }

head -c 100 /dev/zero >"$scratch/big.crate"
set +e
bash "$gate" 100 "$scratch/small.crate" "$scratch/big.crate" >"$scratch/out" 2>&1
code=$?
set -e
[[ $code -eq 1 ]] || { cat "$scratch/out"; echo "FAIL: file at the limit exited $code, want 1"; exit 1; }
grep -q '::error::big.crate is 100 bytes, not under 100' "$scratch/out" || { cat "$scratch/out"; echo "FAIL: oversized file not named"; exit 1; }

set +e
bash "$gate" 100 "$scratch/missing.crate" >"$scratch/out" 2>&1
code=$?
set -e
[[ $code -eq 1 ]] || { cat "$scratch/out"; echo "FAIL: missing file exited $code, want 1"; exit 1; }

if bash "$gate" >/dev/null 2>&1; then echo "FAIL: no arguments accepted"; exit 1; fi
if bash "$gate" 100 >/dev/null 2>&1; then echo "FAIL: a limit with no files accepted"; exit 1; fi

# 2. Workflow wiring.
ruby -ryaml <<'RUBY'
jobs = YAML.load_file('.github/workflows/ci.yml').fetch('jobs')
steps = jobs.fetch('workspace-check').fetch('steps')
idx = steps.each_index.select { |i| steps[i]['run'].to_s.strip == 'bash scripts/ci/check-publish-package.sh' }
raise "workspace-check must run check-publish-package.sh exactly once, found #{idx.size}" unless idx.size == 1
cache = steps.index { |s| s['uses'].to_s.start_with?('Swatinem/rust-cache@') }
raise 'workspace-check has no rust-cache step' unless cache
raise 'the publish dry-run step must come after the rust-cache step' unless idx.first > cache

filter_step = jobs.fetch('changes').fetch('steps').find { |s| s['id'] == 'filter' }
raise 'changes job has no paths filter step' unless filter_step
code = YAML.safe_load(filter_step.fetch('with').fetch('filters')).fetch('code')
%w[
  scripts/ci/check-publish-package.sh
  scripts/ci/check-crate-size.sh
  pancetta-ft8/README.md
  pancetta-core/README.md
  **/LICENSE-*
  **/THIRD-PARTY-NOTICES.md
  pancetta-ft8/assets/**
  pancetta-ft8/*.h
  pancetta-ft8/vendor/**
].each { |p| raise "the code filter is missing #{p}" unless code.include?(p) }
RUBY

# 3. The packaging script runs the two-package commands.
for line in \
  'cargo publish --dry-run --allow-dirty -p pancetta-core -p pancetta-ft8' \
  'cargo package --no-verify --allow-dirty -p pancetta-core -p pancetta-ft8'; do
  grep -qxF "$line" scripts/ci/check-publish-package.sh || { echo "FAIL: check-publish-package.sh lacks: $line"; exit 1; }
done

# 4. Docs describe the crates.io posture.
if grep -qF 'crates.io publishing is OUT of scope' .github/workflows/release.yml; then
  echo "FAIL: release.yml still says crates.io publishing is out of scope"; exit 1
fi
grep -qF 'docs/decoder-crate.md' .github/workflows/release.yml || { echo "FAIL: release.yml does not point to docs/decoder-crate.md"; exit 1; }
unreleased=$(awk '/^## \[Unreleased\]/{on=1; next} on && /^## \[/{exit} on' CHANGELOG.md)
grep -qF 'PAN-384' <<<"$unreleased" || { echo "FAIL: CHANGELOG.md [Unreleased] does not mention PAN-384"; exit 1; }
grep -qF 'Registry packaging (PAN-384)' AGENTS.md || { echo "FAIL: AGENTS.md lacks the Registry packaging (PAN-384) invariant"; exit 1; }
grep -qF '(PAN-384, 2026-11-17)' docs/DECISIONS/config-and-platform.md || { echo "FAIL: docs/DECISIONS/config-and-platform.md lacks the PAN-384 entry"; exit 1; }

echo "PASS: publish package gate cases"
