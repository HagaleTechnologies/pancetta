#!/usr/bin/env bash
# scripts/check.sh — local pre-push verification.
#
# GATE MODEL (2026-06-28): GitHub CI (.github/workflows/ci.yml) is the
# AUTHORITATIVE gate. Its heavy lanes (full workspace tests, examples build,
# cargo-deny, cross-platform) run on every PR *and* every push to main, so
# `main` is comprehensively gated regardless of how a change lands. This script
# is therefore a FAST local PRE-FLIGHT, not a full CI mirror.
#
#   - As a pre-push HOOK (auto-detected): runs the FAST lane — fmt + clippy +
#     research-guard + `cargo check --workspace --examples` (compile/API-drift,
#     incl. the CI-excluded pancetta-research examples) + cargo-deny. ~5 min.
#     The full test RUN is left to CI (it runs on the resulting push/PR).
#   - Run MANUALLY with no args for the FULL lane (adds the workspace test run)
#     when you want local certainty before a big/risky change.
#
# The one thing CI does NOT cover is pancetta-research (local-only harness,
# excluded from CI). The always-on `cargo check --workspace --examples` here is
# its cheap drift guard — it type-checks the research example call-sites so a
# decoder-API change can't silently break them (no codegen/linking of the dozens
# of example binaries, which is what used to make this gate ~40 min).
#
# Usage:
#   scripts/check.sh                # FULL lane (fast checks + workspace test run)
#   scripts/check.sh --fast         # FAST lane only (same as the hook)
#   scripts/check.sh --full         # force FULL lane even when run as a hook
#   scripts/check.sh --fix          # `cargo fmt --all` and `cargo clippy --fix`
#   scripts/check.sh --install-hook # symlink this script as the pre-push hook
#
# After --install-hook, every `git push` runs the FAST lane automatically.
# Skip it for one push with `git push --no-verify`.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

FAST=0
FORCE_FULL=0
FIX=0
INSTALL_HOOK=0
# Hook auto-detection: when git runs this as a pre-push hook it passes the
# remote name as $1 and the URL as $2 (positional, non-flag args), with refs on
# stdin. A manual `scripts/check.sh` invocation has no positional args. So a
# positional arg ⇒ hook context ⇒ default to the FAST lane (CI is the full
# gate). `--full` overrides; `--fast` forces fast even when run manually.
HOOK_CONTEXT=0
for arg in "$@"; do
    case "$arg" in
        --fast)         FAST=1 ;;
        --full)         FORCE_FULL=1 ;;
        --fix)          FIX=1 ;;
        --install-hook) INSTALL_HOOK=1 ;;
        --*)            echo "Unknown flag: $arg" >&2; exit 2 ;;
        *)              HOOK_CONTEXT=1 ;;  # positional arg ⇒ pre-push hook context
    esac
done
# In hook context, default to FAST unless the operator forced --full.
if [[ "$HOOK_CONTEXT" -eq 1 && "$FORCE_FULL" -eq 0 ]]; then
    FAST=1
fi
# --full always wins (e.g. a manual `scripts/check.sh --full`).
if [[ "$FORCE_FULL" -eq 1 ]]; then
    FAST=0
fi

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
    echo "Every \`git push\` now runs the FAST lane (~5 min); CI is the full gate."
    echo "Run \`scripts/check.sh\` (no args) for the full local lane. \`git push --no-verify\` skips."
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

run "research guard-ci" ./scripts/research-env.sh --guard-ci

# --- Compile / drift lane (always; cheap with `check`) ----------------------
#
# `cargo check --workspace --all-targets` type-checks every crate's lib/bins AND
# all example AND **test** targets — including the CI-excluded pancetta-research
# examples — so a struct/API change that breaks an example OR an integration-test
# call-site is caught here. (Was `--examples`, which compiled examples but NOT the
# `tests/` dirs — a `QsoManagerConfig` field added 2026-06-28 broke an integration
# test that the examples-only check missed and only CI caught; `--all-targets`
# closes that gap.) We `check` (not `build`/`test`): no codegen/linking of the
# dozens of example/test binaries and no test RUN (that's the FULL lane / CI).
run "compile all-targets (check)" cargo check --workspace --all-targets

# --- Test lane (FULL only; CI runs this on every PR + main push) ------------
#
# The workspace test RUN is the expensive part. CI runs it authoritatively on
# every PR and every push to main, so the hook (FAST) skips it; run the script
# manually (no args / --full) when you want local certainty.

if [[ "$FAST" -eq 0 ]]; then
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
