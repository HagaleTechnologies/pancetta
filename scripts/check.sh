#!/usr/bin/env bash
# scripts/check.sh — local pre-push verification.
#
# Runs every check that CI runs, in roughly the same order. Catch CI
# failures here, on a dev machine, before they hit the remote.
#
# Usage:
#   scripts/check.sh            # run all checks
#   scripts/check.sh --fast     # skip the slowest jobs (full test suite)
#   scripts/check.sh --fix      # `cargo fmt --all` and `cargo clippy --fix`
#                                 to auto-correct what's safe
#
# Hook this up as a pre-push git hook by symlinking:
#   ln -s ../../scripts/check.sh .git/hooks/pre-push
# or run it manually before `git push`.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

FAST=0
FIX=0
for arg in "$@"; do
    case "$arg" in
        --fast) FAST=1 ;;
        --fix)  FIX=1 ;;
        *) echo "Unknown flag: $arg" >&2; exit 2 ;;
    esac
done

run() {
    local label="$1"; shift
    printf '\n\033[1;36m== %s ==\033[0m\n' "$label"
    "$@"
}

# --- Fast lane (always runs, ~30 seconds) ----------------------------------

if [[ "$FIX" -eq 1 ]]; then
    run "fmt (fix)"     cargo fmt --all
else
    run "fmt"           cargo fmt --all -- --check
fi

# Match the CI `Clippy` job exactly — no `-D warnings` flag. The
# workspace currently carries pre-existing warnings (mostly minor:
# missing-docs, large-Result-variants) that are tracked but not gated.
# If we made `check.sh` stricter than CI, every contributor would
# trip the gap on day one. CI is the source of truth; if you want to
# drive warnings to zero, do it in CI first, then this script will
# happily inherit it.
run "clippy"            cargo clippy --workspace --features transmit

# --- Test lane (~5-10 min) -------------------------------------------------

if [[ "$FAST" -eq 0 ]]; then
    run "workspace check"   cargo check --workspace
    run "examples build"    cargo build --workspace --examples
    run "workspace tests"   cargo test --workspace --features transmit
    # pancetta-hamlib is excluded from the workspace test run (tokio
    # runtime conflicts hang the suite); needs its own --test-threads=1.
    run "hamlib tests"      cargo test -p pancetta-hamlib --lib -- --test-threads=1
fi

# --- Supply-chain lane (~5 min on cold cache) ------------------------------
#
# `cargo deny` runs the same license/advisory/source checks the CI Cargo
# Deny job runs. Install once with: `cargo install --locked cargo-deny`.

if command -v cargo-deny >/dev/null 2>&1; then
    run "cargo deny"        cargo deny check bans licenses sources advisories
else
    printf '\n\033[1;33m== cargo deny SKIPPED — install with `cargo install --locked cargo-deny` ==\033[0m\n'
fi

printf '\n\033[1;32m== all checks passed ==\033[0m\n'
