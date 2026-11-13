#!/usr/bin/env bash
# Run after release smoke tests, before packaging the binary.
set -euo pipefail
BIN=${1:?binary path required}
STEM=${2:?output stem required}
case "$(uname -s)" in
  Darwin*)
    dsymutil "$BIN" -o "${STEM}.dSYM"
    strip -x "$BIN"
    tar czf "${STEM}.dSYM.tar.gz" "${STEM}.dSYM"
    printf '%s\n' "${STEM}.dSYM.tar.gz"
    ;;
  *)
    objcopy --only-keep-debug "$BIN" "${STEM}.debug"
    objcopy --strip-debug --add-gnu-debuglink="${STEM}.debug" "$BIN"
    printf '%s\n' "${STEM}.debug"
    ;;
esac
