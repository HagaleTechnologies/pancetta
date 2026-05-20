#!/usr/bin/env bash
# scripts/research-env.sh — local-only disk hygiene + CI-guard for the
# decoder research harness. Spec:
# docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md
#
# Subcommands:
#   --preflight     Check disk caps; run scheduled purges if 80-95 GB; pause at 95+ GB.
#   --audit         Print usage report; no actions.
#   --guard-ci      Scan .github/workflows/ for forbidden references; fail if found.
#   --status        (plan 3) List experiments + artifact disk usage.
#   --cleanup       (plan 3) Interactive purge of expired artifacts.
#   --pin <slug>    (plan 3) Keep artifacts past default retention.
#   --finalize <slug>  (plan 3) Rename branch scorecard to history/.
#
# Per spec, caps are:
#   - research/ in git: 500 MB
#   - ~/.pancetta/research_artifacts/ + research/corpus/synth/wavs/ +
#     training/*/data/ together: 100 GB
#
# Warn at 80 GB. Hard pause at 95 GB.

set -euo pipefail

CMD="${1:-}"
shift || true

REPO_ROOT="$(git rev-parse --show-toplevel)"
ARTIFACTS_DIR="${HOME}/.pancetta/research_artifacts"
SYNTH_WAVS_DIR="${REPO_ROOT}/research/corpus/synth/wavs"
TRAINING_DATA_GLOB="${REPO_ROOT}/training"
RESEARCH_DIR="${REPO_ROOT}/research"

WARN_GB=80
PAUSE_GB=95
REPO_CAP_MB=500

# Kilobytes of a directory. Returns 0 if dir missing.
# Uses du -sk (POSIX-compatible; works on macOS BSD du and GNU du).
dir_kb() {
    local d="$1"
    [ -d "$d" ] || { echo "0"; return; }
    du -sk "$d" 2>/dev/null | awk '{print $1}'
}

# Convert kilobytes to GB string (one decimal place).
kb_to_gb() {
    awk -v kb="$1" 'BEGIN { printf "%.1f", kb / (1024*1024) }'
}

# Sum size of training/*/data/ subdirs (one per future training dataset).
# Returns total in kilobytes.
training_data_kb() {
    local total=0
    if [ -d "$TRAINING_DATA_GLOB" ]; then
        for d in "$TRAINING_DATA_GLOB"/*/data; do
            [ -d "$d" ] || continue
            local kb
            kb=$(du -sk "$d" 2>/dev/null | awk '{print $1}')
            total=$((total + kb))
        done
    fi
    echo "$total"
}

usage_report() {
    local artifacts_kb synth_kb training_kb total_kb
    local artifacts synth training total
    artifacts_kb=$(dir_kb "$ARTIFACTS_DIR")
    synth_kb=$(dir_kb "$SYNTH_WAVS_DIR")
    training_kb=$(training_data_kb)
    total_kb=$((artifacts_kb + synth_kb + training_kb))

    artifacts=$(kb_to_gb "$artifacts_kb")
    synth=$(kb_to_gb "$synth_kb")
    training=$(kb_to_gb "$training_kb")
    total=$(kb_to_gb "$total_kb")

    echo "== Research disk usage =="
    printf "  artifacts (%s): %s GB\n" "$ARTIFACTS_DIR" "$artifacts"
    printf "  synth wavs (%s): %s GB\n" "$SYNTH_WAVS_DIR" "$synth"
    printf "  training data (%s/*/data): %s GB\n" "$TRAINING_DATA_GLOB" "$training"
    printf "  ---\n"
    printf "  total on-disk: %s GB  (warn at %d, pause at %d)\n" \
        "$total" "$WARN_GB" "$PAUSE_GB"

    # repo cap check
    local repo_mb=0
    if [ -d "$RESEARCH_DIR" ]; then
        repo_mb=$(du -sm "$RESEARCH_DIR" 2>/dev/null | awk '{print $1}')
        repo_mb=${repo_mb:-0}
    fi
    printf "  research/ in git: %s MB  (cap %d MB)\n" "$repo_mb" "$REPO_CAP_MB"

    # Return the total in GB via stdout for callers; readers use awk.
    echo "$total"
}

cmd_audit() {
    # Print the report but strip the trailing total line (last line).
    # Note: macOS BSD `head` does NOT support `head -n -1`; use `sed '$d'` instead.
    usage_report | sed '$d'
    return 0
}

cmd_preflight() {
    local report total
    report=$(usage_report)
    total=$(printf '%s\n' "$report" | tail -n 1)
    printf '%s\n' "$report" | sed '$d'
    echo

    if awk -v t="$total" -v p="$PAUSE_GB" 'BEGIN { exit !(t >= p) }'; then
        echo "STOP: ${total} GB >= ${PAUSE_GB} GB pause threshold."
        echo "Run 'scripts/research-env.sh --cleanup' (available in plan 3) and retry."
        exit 1
    elif awk -v t="$total" -v w="$WARN_GB" 'BEGIN { exit !(t >= w) }'; then
        echo "WARN: ${total} GB >= ${WARN_GB} GB. Scheduled purges would run here."
        echo "Plan 1 only reports; --cleanup arrives in plan 3."
    else
        echo "OK: ${total} GB on-disk research footprint."
    fi
}

cmd_guard_ci() {
    local hits=0
    if [ -d "${REPO_ROOT}/.github/workflows" ]; then
        local forbidden=(
            "pancetta-research"
            "research-env.sh"
            "--bin eval"
            "--bin compare"
            "--bin leaderboard"
            "--bin curate"
            "--bin baseline"
            "research/scorecards"
            "research/experiments"
            "research/baselines"
            "research/corpus"
        )
        for term in "${forbidden[@]}"; do
            if grep -rIn "$term" "${REPO_ROOT}/.github/workflows" >/dev/null 2>&1; then
                echo "FORBIDDEN reference in .github/workflows:"
                grep -rIn "$term" "${REPO_ROOT}/.github/workflows" | head -5
                hits=$((hits + 1))
            fi
        done
    fi
    if [ "$hits" -gt 0 ]; then
        echo
        echo "Research harness is local-only. See pancetta-research/README.md."
        exit 1
    fi
    echo "OK: no research references in .github/workflows"
}

case "$CMD" in
    --preflight) cmd_preflight ;;
    --audit) cmd_audit ;;
    --guard-ci) cmd_guard_ci ;;
    --status|--cleanup|--pin|--finalize)
        echo "Subcommand $CMD lands in plan 3 (iteration loop)."
        exit 0
        ;;
    -h|--help|"")
        sed -n '2,20p' "$0"
        ;;
    *)
        echo "unknown subcommand: $CMD" >&2
        exit 2
        ;;
esac
