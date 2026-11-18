#!/usr/bin/env bash
# PAN-377: exercise the release identity gate and workflow wiring.
set -euo pipefail
bash -n scripts/check-build-identity.sh
shellcheck -s bash scripts/check-build-identity.sh
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
sha=$(git rev-parse --short=7 HEAD)
check() {
  printf '#!/bin/sh\nprintf "%%s\\n" %s\n' "'$1'" > "$scratch/pancetta"
  chmod +x "$scratch/pancetta"
  if [ "$2" = pass ]; then bash scripts/check-build-identity.sh "$scratch/pancetta" x86_64-unknown-linux-gnu >/dev/null
  elif bash scripts/check-build-identity.sh "$scratch/pancetta" x86_64-unknown-linux-gnu >/dev/null 2>&1; then echo 'unexpected gate pass' >&2; exit 1; fi
}
check "pancetta 0.9.6 ($sha 2026-11-18 x86_64-unknown-linux-gnu)" pass
check "pancetta 0.9.6 ($sha 2026-11-18 x86_64-unknown-linux-gnu)"$'\r' pass
check "pancetta 0.9.6 (unknown 2026-11-18 x86_64-unknown-linux-gnu)" fail
check "pancetta 0.9.6 (0000000 2026-11-18 x86_64-unknown-linux-gnu)" fail
check "pancetta 0.9.6 ($sha 2026-11-18 aarch64-unknown-linux-gnu)" fail
check 'pancetta 0.9.6' fail
check "pancetta 0.9.6 ($sha 2026-11-18 x86_64-unknown-linux-gnu)
extra" fail
ruby -ryaml -e '
r=YAML.load_file(".github/workflows/release.yml"); steps=r.fetch("jobs").values.flat_map { |j| j.fetch("steps",[]) }; names=steps.map { |s| s["name"] }; i=names.index("Smoke test — binary names its commit and target (PAN-377)"); abort "missing release gate" unless i && names.index("Build release binary") < i && i < names.index("Package"); s=steps[i]; abort "wrong release gate" unless s["shell"]=="bash" && s["run"].include?(%q{bash scripts/check-build-identity.sh "$BIN" "${{ matrix.target }}"}) && s["run"].include?(%q{BIN="target/${{ matrix.target }}/release/pancetta"})
'
ruby -ryaml -e '
r=YAML.load_file(".github/workflows/ci.yml"); steps=r.fetch("jobs").fetch("workspace-tests").fetch("steps"); i=steps.index { |s| s["name"]=="Install zsh (PAN-377 completion check)" }; j=steps.index { |s| s["name"]=="Test workspace partition" }; abort "missing zsh check" unless i && j && i<j && steps[i]["run"].include?("zsh") && steps[j].fetch("env",{})["PANCETTA_REQUIRE_ZSH"]=="1"
'
echo 'PASS: build identity release gate and CI wiring'
