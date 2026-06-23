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
        echo "Run 'scripts/research-env.sh --cleanup --execute' to delete expired artifacts, then retry."
        exit 1
    elif awk -v t="$total" -v w="$WARN_GB" 'BEGIN { exit !(t >= w) }'; then
        echo "WARN: ${total} GB >= ${WARN_GB} GB. Scheduled purges would run here."
        echo "Run 'scripts/research-env.sh --cleanup' to purge expired artifacts (or '--cleanup --execute' to actually delete)."
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
            # A reference is forbidden UNLESS it is the sanctioned exclusion form
            # (`--exclude pancetta-research`, which is precisely how CI keeps the
            # local-only research crate out of `--workspace` builds) or a comment
            # explaining it. Filter those allowed forms out before flagging.
            local matches
            # `|| true`: an empty filter result (grep exit 1) is the SUCCESS
            # case here and must not trip the script's `set -e`.
            matches=$(grep -rIn "$term" "${REPO_ROOT}/.github/workflows" 2>/dev/null \
                | grep -v -- '--exclude pancetta-research' \
                | grep -vE ':[0-9]+:[[:space:]]*#' || true)
            if [ -n "$matches" ]; then
                echo "FORBIDDEN reference in .github/workflows:"
                echo "$matches" | head -5
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

# List experiments: their state (per-journal frontmatter), branch existence,
# artifact disk usage. Reads research/experiments/*.md + git for branch info.
cmd_status() {
    local exp_dir="${RESEARCH_DIR}/experiments"
    local artifacts_dir="${ARTIFACTS_DIR}/weights"
    local total=0
    local merged=0
    local shelved=0
    local in_progress=0
    local deferred=0

    echo "== Experiments =="
    if [ ! -d "$exp_dir" ]; then
        echo "  (no experiments dir yet)"
        return
    fi
    # Each experiment has a YAML frontmatter block at the top of its .md;
    # we parse just the `state:` line.
    for f in "$exp_dir"/*.md; do
        [ -f "$f" ] || continue
        # Skip placeholder .gitkeep or non-frontmatter files.
        head -1 "$f" | grep -q '^---' || continue
        total=$((total + 1))
        local slug
        slug=$(basename "$f" .md)
        local state
        state=$(awk '/^state:/ { print $2; exit }' "$f")
        local delta
        delta=$(awk '/^delta_vs_main:/ { print $2; exit }' "$f")
        local branch
        branch=$(awk '/^branch:/ { print $2; exit }' "$f")
        local branch_exists="-"
        if [ -n "$branch" ] && git -C "$REPO_ROOT" rev-parse --verify "$branch" >/dev/null 2>&1; then
            branch_exists="✓"
        fi
        local artifact_size="-"
        if [ -d "$artifacts_dir/$slug" ]; then
            artifact_size=$(du -sh "$artifacts_dir/$slug" 2>/dev/null | awk '{print $1}')
        fi
        printf "  [%s] %-40s  delta=%-10s branch=%s artifacts=%s\n" \
            "${state:-?}" "$slug" "${delta:-?}" "$branch_exists" "$artifact_size"
        case "$state" in
            merged) merged=$((merged + 1)) ;;
            shelved) shelved=$((shelved + 1)) ;;
            evaluated|implementing|planned) in_progress=$((in_progress + 1)) ;;
            deferred) deferred=$((deferred + 1)) ;;
        esac
    done
    if [ "$total" -eq 0 ]; then
        echo "  (no experiments yet — bootstrap the hypothesis bank to start)"
    else
        echo
        printf "  Total: %d   merged: %d   shelved: %d   in-progress: %d   deferred: %d\n" \
            "$total" "$merged" "$shelved" "$in_progress" "$deferred"
    fi
}

# Mark an experiment's on-disk artifacts as "pinned" — exempt from default
# retention purges. State recorded in a sidecar file under the artifact dir.
# Usage: research-env.sh --pin <slug>
cmd_pin() {
    local slug="$1"
    if [ -z "$slug" ]; then
        echo "usage: research-env.sh --pin <slug>" >&2
        exit 2
    fi
    local artifact_root="${ARTIFACTS_DIR}/weights/${slug}"
    if [ ! -d "$artifact_root" ]; then
        # Still allow pinning to "reserve" the slot for future artifacts.
        mkdir -p "$artifact_root"
        echo "(no artifacts yet for $slug; created reservation)"
    fi
    echo "pinned: $(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$artifact_root/.pinned"
    echo "Pinned $slug. Artifacts at $artifact_root will not be auto-purged."
}

# Purge expired artifacts: experiments shelved >14 days, merged-but-not-promoted
# >30 days. Pinned dirs (.pinned file present) are always preserved.
#
# This is a dry-run by default. Pass --execute to actually delete.
cmd_cleanup() {
    local execute=0
    if [ "${1:-}" = "--execute" ]; then
        execute=1
    fi
    local artifact_root="${ARTIFACTS_DIR}/weights"
    if [ ! -d "$artifact_root" ]; then
        echo "no artifacts dir; nothing to clean"
        return
    fi
    local now_epoch
    now_epoch=$(date -u +%s)
    local total_freed_kb=0
    local count=0

    for d in "$artifact_root"/*; do
        [ -d "$d" ] || continue
        local slug
        slug=$(basename "$d")
        # Skip pinned.
        if [ -f "$d/.pinned" ]; then
            continue
        fi
        # Look up the corresponding journal.
        local journal=""
        for cand in "${RESEARCH_DIR}/experiments/"*"${slug}.md"; do
            [ -f "$cand" ] || continue
            journal="$cand"
            break
        done
        if [ -z "$journal" ]; then
            # Orphan artifact — no journal. Default to keeping (operator may
            # be mid-experiment with no journal yet).
            continue
        fi
        local state
        state=$(awk '/^state:/ { print $2; exit }' "$journal")
        local last_updated
        last_updated=$(awk '/^last_updated:/ { print $2; exit }' "$journal")
        # Convert ISO timestamp to epoch. macOS BSD date uses -j -f; GNU uses -d.
        local last_epoch
        if last_epoch=$(date -j -f "%Y-%m-%dT%H:%M:%SZ" "$last_updated" "+%s" 2>/dev/null); then
            :
        elif last_epoch=$(date -d "$last_updated" "+%s" 2>/dev/null); then
            :
        else
            # Couldn't parse — skip.
            continue
        fi
        local age_days
        age_days=$(( (now_epoch - last_epoch) / 86400 ))

        local retention_days
        case "$state" in
            shelved) retention_days=14 ;;
            merged) retention_days=30 ;;
            *) continue ;;  # deferred / in-progress / planned: don't auto-purge
        esac

        if [ "$age_days" -ge "$retention_days" ]; then
            local size_kb
            size_kb=$(du -sk "$d" 2>/dev/null | awk '{print $1}')
            size_kb=${size_kb:-0}
            count=$((count + 1))
            total_freed_kb=$((total_freed_kb + size_kb))
            if [ "$execute" -eq 1 ]; then
                rm -rf "$d"
                printf "PURGED: %-50s (state=%s, age=%dd, %d KB)\n" \
                    "$slug" "$state" "$age_days" "$size_kb"
            else
                printf "WOULD PURGE: %-50s (state=%s, age=%dd, %d KB)\n" \
                    "$slug" "$state" "$age_days" "$size_kb"
            fi
        fi
    done

    local total_mb
    total_mb=$(awk -v kb="$total_freed_kb" 'BEGIN { printf "%.1f", kb / 1024 }')
    if [ "$execute" -eq 1 ]; then
        echo
        echo "Cleanup: purged $count artifact dir(s), freed ${total_mb} MB."
    else
        echo
        echo "Cleanup (dry-run): would purge $count artifact dir(s), freeing ${total_mb} MB."
        echo "Re-run with --execute to actually delete."
    fi
}

# Finalize an experiment: rename its branch scorecard to history/<date>-<slug>.json,
# update the experiment journal's `last_updated`, and (if the experiment is merged
# or shelved) mark its branch eligible for cleanup. This is called as part of the
# merge/shelve workflow.
#
# Usage: research-env.sh --finalize <slug>
cmd_finalize() {
    local slug="$1"
    if [ -z "$slug" ]; then
        echo "usage: research-env.sh --finalize <slug>" >&2
        exit 2
    fi
    local journal=""
    for cand in "${RESEARCH_DIR}/experiments/"*"${slug}.md"; do
        [ -f "$cand" ] || continue
        journal="$cand"
        break
    done
    if [ -z "$journal" ]; then
        echo "no journal found for slug '$slug' (expected research/experiments/*-${slug}.md)" >&2
        exit 2
    fi
    local state
    state=$(awk '/^state:/ { print $2; exit }' "$journal")
    local branch
    branch=$(awk '/^branch:/ { print $2; exit }' "$journal")

    # Date prefix: take the slug's leading YYYY-MM-DD if present, else use today.
    local date_prefix
    if [[ "$slug" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2} ]]; then
        date_prefix="${BASH_REMATCH[0]}"
    else
        date_prefix=$(date -u +%Y-%m-%d)
    fi
    # The journal might already have the date prefix in the slug (preferred).
    # Strip it for the scorecard target name; if the journal slug = "synth-plateau",
    # the scorecard target is "history/2026-05-20-synth-plateau.json".
    local target_slug
    target_slug="${slug#${date_prefix}-}"

    # Move branch scorecard to history/, if it exists.
    local branch_scorecard="${RESEARCH_DIR}/scorecards/${branch##*/}.json"
    local history_scorecard="${RESEARCH_DIR}/scorecards/history/${date_prefix}-${target_slug}.json"
    if [ -f "$branch_scorecard" ]; then
        mkdir -p "${RESEARCH_DIR}/scorecards/history"
        mv "$branch_scorecard" "$history_scorecard"
        echo "moved: $branch_scorecard -> $history_scorecard"
    else
        echo "(no branch scorecard at $branch_scorecard — nothing to move)"
    fi

    # Update the journal's last_updated timestamp.
    local now_iso
    now_iso=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
    # Use a portable sed in-place edit. On macOS, sed -i wants an empty backup arg.
    if sed --version >/dev/null 2>&1; then
        # GNU sed.
        sed -i "s/^last_updated:.*/last_updated: ${now_iso}/" "$journal"
    else
        # BSD sed (macOS).
        sed -i "" "s/^last_updated:.*/last_updated: ${now_iso}/" "$journal"
    fi

    echo "finalized $slug (state=$state, scorecard=$history_scorecard)"
}

case "$CMD" in
    --preflight) cmd_preflight ;;
    --audit) cmd_audit ;;
    --guard-ci) cmd_guard_ci ;;
    --status) cmd_status ;;
    --pin) cmd_pin "${1:-}" ;;
    --cleanup) cmd_cleanup "${1:-}" ;;
    --finalize) cmd_finalize "${1:-}" ;;
    -h|--help|"")
        sed -n '2,20p' "$0"
        ;;
    *)
        echo "unknown subcommand: $CMD" >&2
        exit 2
        ;;
esac
