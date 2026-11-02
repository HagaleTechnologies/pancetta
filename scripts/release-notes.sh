#!/usr/bin/env bash
# Internal release plumbing. Run from the repository root.
set -euo pipefail
TAG=${1:?release tag required}
CHANGELOG=${2:?changelog path required}
OUTPUT=${3:?output path required}
VERSION="${TAG#v}"
scratch=$(mktemp)
trap 'rm -f "$scratch"' EXIT
awk -v ver="$VERSION" \
  'index($0, "## [" ver "]") == 1 {flag=1; next} /^## /{flag=0} flag' \
  "$CHANGELOG" > "$scratch"
if ! grep -q '[^[:space:]]' "$scratch"; then
  echo "CHANGELOG.md has no section for ${VERSION}" >&2
  exit 1
fi
cat "$scratch" > "$OUTPUT"
{
  printf "\n---\nTagged at \`%s\`.\n\n" "${GITHUB_SHA:?tagged source SHA required}"
  cat <<'NOTES'
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
NOTES
  printf "\nBefore upgrading, stop all writers and complete the verified backup in [Upgrade and rollback](https://github.com/HagaleTechnologies/pancetta/blob/%s/docs/UPGRADING.md). The guide is included in every archive at \`docs/UPGRADING.md\`. Keep the prior binary and both generations of data for rollback.\n" "$TAG"
} >> "$OUTPUT"
