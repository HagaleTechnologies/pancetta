#!/usr/bin/env bash
# PAN-384: pancetta-core and pancetta-ft8 must stay publishable to crates.io
# together. This runs the two-package publish dry-run (package, then build
# each .crate on its own), then packages them again and checks each .crate
# against crates.io's 10 MB upload limit. A dry-run never uploads.
#
# It refuses to run without the ft8_lib submodule: a package built from such a
# checkout would lack the C decoder (pancetta-ft8's build.rs then fails the
# verify step anyway, but this names the fix up front).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

if ! test -f pancetta-ft8/vendor/ft8_lib/ft8/constants.c; then
  echo "::error::ft8_lib submodule not checked out; run git submodule update --init"
  exit 1
fi

pkg="${CARGO_TARGET_DIR:-target}/package"
rm -rf "$pkg"

cargo publish --dry-run --allow-dirty -p pancetta-core -p pancetta-ft8
cargo package --no-verify --allow-dirty -p pancetta-core -p pancetta-ft8

shopt -s nullglob
crates=("$pkg"/pancetta-core-*.crate "$pkg"/pancetta-ft8-*.crate)
if (( ${#crates[@]} != 2 )); then
  echo "::error::expected exactly two .crate files under $pkg, found ${#crates[@]}: ${crates[*]:-none}"
  exit 1
fi
bash scripts/ci/check-crate-size.sh 10000000 "${crates[@]}"
