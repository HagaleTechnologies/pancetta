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
