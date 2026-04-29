#!/usr/bin/env bash
# scripts/check.sh — local pre-push verification.
#
# Runs every check that CI runs (when the full lane runs), in roughly the
# same order. With CI now scaled back so direct pushes to main only
# trigger fmt + clippy, this script is the actual safety net for human
# pushes — the heavy test/deny/build lanes only run in CI on PRs and the
# weekly cron. Run it before every push.
#
# Usage:
#   scripts/check.sh                # run all checks
#   scripts/check.sh --fast         # skip the slowest jobs (full test suite)
#   scripts/check.sh --fix          # `cargo fmt --all` and `cargo clippy --fix`
#                                     to auto-correct what's safe
#   scripts/check.sh --install-hook # symlink this script as the
#                                     repo's .git/hooks/pre-push and exit
#
# After --install-hook, every `git push` runs this script automatically.
# Skip it for one push with `git push --no-verify`.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

FAST=0
FIX=0
INSTALL_HOOK=0
for arg in "$@"; do
    case "$arg" in
        --fast)         FAST=1 ;;
        --fix)          FIX=1 ;;
        --install-hook) INSTALL_HOOK=1 ;;
        *) echo "Unknown flag: $arg" >&2; exit 2 ;;
    esac
done

if [[ "$INSTALL_HOOK" -eq 1 ]]; then
    hook_path=".git/hooks/pre-push"
    target="../../scripts/check.sh"
    if [[ -L "$hook_path" || -f "$hook_path" ]]; then
        existing=$(readlink "$hook_path" 2>/dev/null || true)
        if [[ "$existing" == "$target" ]]; then
            echo "pre-push hook already installed at $hook_path → $target"
            exit 0
        fi
        echo "$hook_path already exists (not pointing at $target):" >&2
        ls -l "$hook_path" >&2
        echo "Refusing to overwrite. Remove it manually if you want to replace." >&2
        exit 2
    fi
    ln -s "$target" "$hook_path"
    chmod +x "$hook_path" 2>/dev/null || true
    echo "Installed pre-push hook: $hook_path → $target"
    echo "Every \`git push\` will now run this script. Use \`git push --no-verify\` to skip."
    exit 0
fi

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
