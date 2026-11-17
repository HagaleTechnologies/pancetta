#!/usr/bin/env bash
# PAN-384: fail when any packaged .crate is at or over a byte limit (crates.io
# refuses uploads over 10 MB).
# Usage: check-crate-size.sh <max-bytes> <file.crate>...
set -euo pipefail
max=${1:?usage: check-crate-size.sh <max-bytes> <file.crate>...}
shift
if (( $# == 0 )); then
  echo "::error::check-crate-size.sh: no .crate files given"
  exit 1
fi
status=0
for f in "$@"; do
  name=$(basename "$f")
  if [[ ! -f "$f" ]]; then
    echo "::error::$name not found at $f"
    status=1
    continue
  fi
  bytes=$(wc -c <"$f" | tr -d '[:space:]')
  echo "$name: $bytes bytes (limit $max)"
  if (( bytes >= max )); then
    echo "::error::$name is $bytes bytes, not under $max"
    status=1
  fi
done
exit "$status"
