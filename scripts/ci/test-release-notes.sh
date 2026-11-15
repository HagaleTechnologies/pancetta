#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
cat > "$scratch/CHANGELOG.md" <<'EOF'
# Changes
## [1.2.4] - today
Adjacent newer change
## [1.2.3] - yesterday
Requested change
## [1.2.2] - earlier
Adjacent older change
EOF
export GITHUB_SHA=0123456789012345678901234567890123456789
bash scripts/release-notes.sh v1.2.3 "$scratch/CHANGELOG.md" "$scratch/notes.md" > "$scratch/stdout"
test ! -s "$scratch/stdout"
grep -qF 'Requested change' "$scratch/notes.md"
if grep -qF 'Adjacent' "$scratch/notes.md"; then echo 'Adjacent version leaked' >&2; exit 1; fi
for expected in 'Build-tested means' 'Replay-tested identifies' 'Radio-qualified requires' \
  'macOS Apple Silicon' 'Linux x86_64 GNU' 'Linux ARM64 GNU' 'Windows x86_64 MinGW' \
  'nine off-air recordings on cloud ARM64' 'not a radio test of each release' \
  'glibc >= 2.35' 'unnotarized' 'does not establish full-application replay' \
  'No Raspberry Pi or other physical ARM station qualification' \
  'https://github.com/HagaleTechnologies/pancetta/blob/v1.2.3/docs/UPGRADING.md' \
  'included in every archive' "$GITHUB_SHA"; do
  grep -qF "$expected" "$scratch/notes.md"
done
if bash scripts/release-notes.sh v9.9.9 "$scratch/CHANGELOG.md" "$scratch/missing.md" 2>"$scratch/error"; then
  echo 'Missing version succeeded' >&2; exit 1
fi
test "$(cat "$scratch/error")" = 'CHANGELOG.md has no section for 9.9.9'
echo 'PASS: release selection, evidence labels, immutable guide link and missing-version refusal'

# The archive layout must preserve README's upgrade link without a checkout.
mkdir -p "$scratch/package/docs" "$scratch/unpacked"
cp README.md "$scratch/package/"
cp docs/UPGRADING.md docs/CONFIG.md "$scratch/package/docs/"
tar czf "$scratch/package.tar.gz" -C "$scratch" package
tar xzf "$scratch/package.tar.gz" -C "$scratch/unpacked"
cmp docs/UPGRADING.md "$scratch/unpacked/package/docs/UPGRADING.md"
test -f "$scratch/unpacked/package/docs/CONFIG.md"
grep -qF '(docs/UPGRADING.md)' "$scratch/unpacked/package/README.md"
echo 'PASS: archive round trip and relative upgrade guide link'

cat > "$scratch/rc.md" <<'EOF'
## [1.2.5-rc1]
Candidate change
## [1.2.5]
Final change
EOF
bash scripts/release-notes.sh v1.2.5-rc1 "$scratch/rc.md" "$scratch/candidate.md"
test "$(head -n 1 "$scratch/candidate.md")" = '**Pre-release:** 1.2.5-rc1 is a release candidate, published as a GitHub pre-release. It may change or be withdrawn before 1.2.5 ships.'
grep -qF 'Candidate change' "$scratch/candidate.md"
if grep -qF 'Final change' "$scratch/candidate.md"; then echo 'Unexpected content' >&2; exit 1; fi
if grep -qF 'Pre-release' "$scratch/notes.md"; then echo 'Unexpected content' >&2; exit 1; fi
# Pin the entire stable output, including whitespace, without needing git history.
cat > "$scratch/baseline.md" <<'BASELINE'
Requested change

---
Tagged at `0123456789012345678901234567890123456789`.

| Platform archive | Build-tested | Replay-tested | Radio-qualified |
|---|---|---|---|
| macOS Apple Silicon | Release CI build and native-C linkage smoke | Not established by this release workflow | No recorded platform qualification |
| Linux x86_64 GNU | Release CI build and native-C linkage smoke | Not established by this release workflow | No recorded platform qualification |
| Linux ARM64 GNU | Release CI build and native-C linkage smoke | Decoder fixture replay only, nine off-air recordings on cloud ARM64 | No Raspberry Pi or other physical ARM station qualification |
| Windows x86_64 MinGW | Release CI build and native-C linkage smoke | Not established by this release workflow | Historical Windows 11 / Yaesu FTdx10 station evidence; not a radio test of each release |

Build-tested means the archive binary built and passed the release's native decoder linkage check. Replay-tested identifies the exact recorded-input test and runner; decoder fixture replay does not establish full-application replay. Radio-qualified requires a named physical station and environment. Historical station use does not qualify every new release or every radio.

Linux ARM64 archives require glibc >= 2.35, such as Raspberry Pi OS Bookworm
or newer. Bullseye-era images cannot load them. Cloud ARM decoder replay
does not qualify Raspberry Pi hardware. macOS binaries remain unnotarized;
see README's quarantine-removal instructions.

Before upgrading, stop all writers and complete the verified backup in [Upgrade and rollback](https://github.com/HagaleTechnologies/pancetta/blob/v1.2.3/docs/UPGRADING.md). The guide is included in every archive at `docs/UPGRADING.md`. Keep the prior binary and both generations of data for rollback.
BASELINE
cmp "$scratch/baseline.md" "$scratch/notes.md"
if bash scripts/release-notes.sh v1.2.6-rc1 "$scratch/rc.md" "$scratch/missing-rc.md" 2>"$scratch/error"; then
  echo 'Missing rc version succeeded' >&2; exit 1
fi
test "$(cat "$scratch/error")" = 'CHANGELOG.md has no section for 1.2.6-rc1'
echo 'PASS: pre-release notice for rc tags only, stable notes unchanged, missing rc section refused'
