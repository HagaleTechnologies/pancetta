#!/usr/bin/env bash
# Retry body for .github/workflows/queue-retry-handler.yml; invoked with bash -e.
# Inputs: REPO, PR, HEAD_SHA, HEAD_SHA12, RUN_URL, GH_TOKEN, MERGE_TOKEN, RETRY_LABEL.

# Re-check the pause label immediately before mutating (Codex P1): a maintainer can
# attach queue-needs-human-review between check-pr-state's own read and this step
# running, and the later labeled-event disarm in auto-merge-trigger.yml is not
# reliable cleanup here -- that handler shares this same admission-<PR#> concurrency
# group, which retains only one pending job, so a simultaneous CI/Supply
# Chain/Wait-for-Codex failure handler can evict the disarm run entirely; deduplication
# then lets this replacement handler exit while the explicitly paused PR stays queued
# and eligible to merge. Check the live label directly rather than relying on that path.
current_labels=$(gh api --paginate "repos/${REPO}/issues/${PR}/labels" --jq '.[].name')
if grep -qxF "queue-needs-human-review" <<< "$current_labels"; then
  # Codex P1, round 9: a bare stand-down here isn't enough -- an admission run that
  # already fetched labels BEFORE the maintainer's pause attach can still arm the PR
  # if the labeled-event disarm run that should have caught it got evicted from the
  # shared admission-<PR#> concurrency group (one pending job retained) by a
  # simultaneous CI/Supply Chain/Wait-for-Codex failure handler. Disarm/dequeue and
  # verify here too, the same as auto-merge-trigger.yml's own pause-detection paths,
  # rather than trusting that something else already did.
  echo "::notice::PR #$PR carries queue-needs-human-review as of right now -- disarming/dequeuing any auto-merge a racing admission run may have already armed, rather than merely standing down."
  gh pr merge "$PR" --repo "${REPO}" --disable-auto >/dev/null 2>&1 || true
  pause_queue_state=$(gh api graphql -f query="
    query(\$owner: String!, \$name: String!, \$pr: Int!) {
      repository(owner: \$owner, name: \$name) { pullRequest(number: \$pr) { id isInMergeQueue } }
    }" -f owner="${REPO%%/*}" -f name="${REPO##*/}" -F pr="$PR" 2>/dev/null)
  pause_pr_node_id=$(echo "$pause_queue_state" | jq -r '.data.repository.pullRequest.id')
  pause_in_queue=$(echo "$pause_queue_state" | jq -r '.data.repository.pullRequest.isInMergeQueue')
  if [ "$pause_in_queue" = "true" ] && [ -n "$pause_pr_node_id" ] && [ "$pause_pr_node_id" != "null" ]; then
    gh api graphql -f query="mutation(\$id: ID!) { dequeuePullRequest(input: {id: \$id}) { clientMutationId } }" -f id="$pause_pr_node_id" >/dev/null 2>&1 || true
  fi
  pause_still_armed=$(gh pr view "$PR" --repo "${REPO}" --json autoMergeRequest --jq '.autoMergeRequest != null')
  pause_still_queued=$(gh api graphql -f query="
    query(\$owner: String!, \$name: String!, \$pr: Int!) {
      repository(owner: \$owner, name: \$name) { pullRequest(number: \$pr) { isInMergeQueue } }
    }" -f owner="${REPO%%/*}" -f name="${REPO##*/}" -F pr="$PR" 2>/dev/null | jq -r '.data.repository.pullRequest.isInMergeQueue')
  if [ "$pause_still_armed" = "true" ] || [ "$pause_still_queued" = "true" ]; then
    echo "::error::PR #$PR carries queue-needs-human-review but is STILL ARMED/QUEUED despite the disable-auto/dequeue calls -- the explicit maintainer stop cannot be relied on until this is fixed by hand."
    gh pr comment "$PR" --repo "${REPO}" --body "queue-needs-human-review is attached to this PR (commit \`$HEAD_SHA12\`), but auto-merge could not be confirmed disarmed -- a racing admission run may have armed it before this pause was observed. This needs urgent human attention." || true
    exit 1
  fi
  exit 0
fi

# Re-check the base immediately before mutating: the debounce above only re-checks
# head/auto-merge, not base -- a contributor could retarget the PR in that gap.
current_base=$(gh pr view "$PR" --repo "${REPO}" --json baseRefName --jq .baseRefName)
if [ "$current_base" != "main" ]; then
  echo "::notice::PR #$PR's base changed to '$current_base' since pr-state ran -- standing down without consuming this head's retry budget."
  exit 0
fi

# Codex P1, round 8: this handler's own trigger precondition (a completed
# merge-group run) only proves the native merge_queue rule existed at the MOMENT
# that merge-group formed -- not that it still exists NOW, potentially minutes
# later, especially during this exact rollout where the ruleset itself is being
# installed and could be reverted. Same guard as auto-merge-trigger.yml's own
# arm_auto_merge() (not shared across workflow files -- each `run:` block is its
# own bash process with no cross-file function import), re-checked live
# immediately before this call for the same reason as the base/pause-label checks
# above: gh pr merge --auto only targets the native queue when the branch actually
# requires one; otherwise it silently enables ordinary auto-merge instead.
#
# Codex P2, round 9: round 8's combined check collapsed "confirmed absent" and
# "API call itself failed" into the same exit-0 stand-down -- a transient read
# failure then reported this step as `success`, so "Mark this merge-group attempt
# as handled" recorded it as handled even though nothing was retried or handed
# off. Since the original merge-group failure already disarmed the PR, there is no
# SUBSEQUENT event to ever revisit it -- exactly the stranding shape the
# --match-head-commit failure branch below already guards against for a different
# cause. Distinguish the two: an unreadable ruleset now fails this step (so
# mark-handled withholds the dedup marker) and makes a best-effort human-handoff
# attempt, rather than silently succeeding.
if ! rules=$(gh api "repos/${REPO}/rules/branches/main" 2>/dev/null); then
  echo "::error::Could not read PR #$PR's ruleset state for main to confirm the native merge_queue rule is installed -- refusing to report success, since that would let mark-handled record this attempt as handled with no retry or handoff actually taken."
  # Codex P1 (this round): this branch runs AFTER this job has already acquired the
  # shared auto-merge-<PR#> concurrency lock, but an admission run from
  # auto-merge-trigger.yml (which shares that same group) can have already ARMED the
  # PR before this run got the lock -- GitHub retains only one PENDING run per
  # concurrency group, so that admission run's own dispatch-then-arm can complete and
  # release the lock before this retry step ever starts. The pause-label recheck
  # earlier in this same step only catches an EXISTING queue-needs-human-review label,
  # not a plain armed-but-unpaused state -- so attaching the label here without also
  # disarming/dequeuing would record a pause while leaving the PR armed/queued
  # underneath it, letting an explicitly-paused PR merge anyway. Disarm/dequeue and
  # verify first, same pattern as the pause-recheck block above.
  gh pr merge "$PR" --repo "${REPO}" --disable-auto >/dev/null 2>&1 || true
  # Codex P1 (next round): every read below must survive bash -e (this job's
  # steps run under GitHub Actions' default `bash -eo pipefail`) -- an unguarded
  # `var=$(cmd)` or `cmd1 | jq ...` that fails (a transient API error, jq choking
  # on empty/invalid input) would abort the step right here, BEFORE the
  # label-attach/handoff below ever runs, defeating the whole point of this fix:
  # the pause would never get recorded at all. Every substitution below is guarded
  # with its own `|| default=...` (or `2>/dev/null`+`// empty` inside jq), and an
  # unreadable result is treated as "still armed" (fail-closed) rather than
  # silently treated as safe, same as every other unreadable-state check in this
  # fleet's admission workflows.
  ruleset_fail_queue_state=$(gh api graphql -f query="
    query(\$owner: String!, \$name: String!, \$pr: Int!) {
      repository(owner: \$owner, name: \$name) { pullRequest(number: \$pr) { id isInMergeQueue } }
    }" -f owner="${REPO%%/*}" -f name="${REPO##*/}" -F pr="$PR" 2>/dev/null) || ruleset_fail_queue_state=""
  ruleset_fail_pr_node_id=$(printf '%s' "$ruleset_fail_queue_state" | jq -r '.data.repository.pullRequest.id // empty' 2>/dev/null) || ruleset_fail_pr_node_id=""
  ruleset_fail_in_queue=$(printf '%s' "$ruleset_fail_queue_state" | jq -r '.data.repository.pullRequest.isInMergeQueue // empty' 2>/dev/null) || ruleset_fail_in_queue=""
  if [ "$ruleset_fail_in_queue" = "true" ] && [ -n "$ruleset_fail_pr_node_id" ] && [ "$ruleset_fail_pr_node_id" != "null" ]; then
    gh api graphql -f query="mutation(\$id: ID!) { dequeuePullRequest(input: {id: \$id}) { clientMutationId } }" -f id="$ruleset_fail_pr_node_id" >/dev/null 2>&1 || true
  fi
  ruleset_fail_still_armed=$(gh pr view "$PR" --repo "${REPO}" --json autoMergeRequest --jq '.autoMergeRequest != null' 2>/dev/null) || ruleset_fail_still_armed="unreadable"
  ruleset_fail_still_queued=$(gh api graphql -f query="
    query(\$owner: String!, \$name: String!, \$pr: Int!) {
      repository(owner: \$owner, name: \$name) { pullRequest(number: \$pr) { isInMergeQueue } }
    }" -f owner="${REPO%%/*}" -f name="${REPO##*/}" -F pr="$PR" 2>/dev/null | jq -r '.data.repository.pullRequest.isInMergeQueue // empty' 2>/dev/null) || ruleset_fail_still_queued="unreadable"
  gh label create queue-needs-human-review --color b60205 \
    --description "Needs a human decision -- an automated handler could not safely proceed" \
    --repo "${REPO}" >/dev/null 2>&1 || true
  gh api --method POST "repos/${REPO}/issues/${PR}/labels" -f "labels[]=queue-needs-human-review" >/dev/null 2>&1 || true
  if [ "$ruleset_fail_still_armed" != "false" ] || [ "$ruleset_fail_still_queued" != "false" ]; then
    echo "::error::PR #$PR is still armed/queued, or its post-disarm state could not be confirmed, despite the disable-auto/dequeue calls made while recording this ruleset-read-failure pause -- the explicit stop cannot be relied on until this is fixed by hand."
    gh pr comment "$PR" --repo "${REPO}" --body "This handler could not read the repository's ruleset state (commit \`$HEAD_SHA12\`), and auto-merge could not be confirmed disarmed while recording the pause -- a racing admission run may have armed it first, or its state could not be read. This needs urgent human attention." || true
  else
    gh pr comment "$PR" --repo "${REPO}" --body "Merge-group CI failed for this revision (commit \`$HEAD_SHA12\`), and this handler could not read the repository's ruleset state to confirm it's safe to retry. Standing down without consuming this head's retry budget -- this needs a human to check ruleset/API access and retry manually if appropriate." || true
  fi
  exit 1
fi
if ! printf '%s' "$rules" | jq -e '[.[] | select(.type == "merge_queue")] | length > 0' >/dev/null 2>&1; then
  # Admission can stand down during rollout, but this failed attempt
  # must remain eligible for redelivery instead of being marked handled.
  echo "::error::Native merge_queue ruleset rule is missing on main. No retry was consumed or armed. Restore the rule, then rerun this workflow to retry this attempt."
  exit 1
fi

# --match-head-commit: without it, a race between the pr-state fetch and this call
# (a contributor pushes a new commit) would silently arm auto-merge on a
# DIFFERENT, never-tested revision.
if GH_TOKEN="$MERGE_TOKEN" gh pr merge "$PR" --repo "${REPO}" --auto --squash --match-head-commit "$HEAD_SHA"; then
  gh label create "$RETRY_LABEL" --color d4c5f9 \
    --description "This revision already used its one automatic queue retry" \
    --repo "${REPO}" >/dev/null 2>&1 || true
  gh api -X POST "repos/${REPO}/issues/${PR}/labels" -f "labels[]=$RETRY_LABEL" >/dev/null 2>&1 || true
  existing_labels=$(gh api --paginate "repos/${REPO}/issues/${PR}/labels" --jq '.[].name')
  if ! grep -qxF "$RETRY_LABEL" <<< "$existing_labels"; then
    # Recheck the head before disarming: the --match-head-commit call above
    # already proved HEAD_SHA matched at that moment, but a new push can still
    # land in the gap between that success and reaching this disarm.
    current_head_sha=$(gh pr view "$PR" --repo "${REPO}" --json headRefOid --jq .headRefOid)
    if [ "$current_head_sha" != "$HEAD_SHA" ]; then
      echo "::notice::PR #$PR's head moved to ${current_head_sha:0:12} while verifying the retry-used label -- standing down without touching auto-merge on the replacement revision."
      exit 0
    fi
    echo "::error::Retry-used label failed to attach for PR #$PR (commit $HEAD_SHA12) -- disarming and handing off instead of risking an unbounded retry loop."
    gh pr merge "$PR" --repo "${REPO}" --disable-auto >/dev/null 2>&1 || true
    # Codex P2, round 11: bare command substitutions here can trigger GitHub
    # Actions' default `bash -e` (errexit) on a transient API failure, killing the
    # script before the pause label below is ever attached -- leaving the PR in an
    # ambiguous state (disable-auto attempted but not confirmed, no durable pause
    # marker either). A separately-read indeterminate/empty GraphQL result can also
    # be silently treated as "not queued," skipping the dequeue mutation -- fail-open
    # in the same shape round 5's still_armed_or_queued() fix already closed
    # elsewhere. Guard every read explicitly and treat unreadable/indeterminate as
    # still-armed (fail closed), so this always reaches the pause-label attachment
    # below regardless of API health.
    if ! still_armed=$(gh pr view "$PR" --repo "${REPO}" --json autoMergeRequest --jq '.autoMergeRequest != null' 2>/dev/null); then
      still_armed="__UNREADABLE__"
    fi
    if ! in_queue=$(gh api graphql -f query="
      query(\$owner: String!, \$name: String!, \$pr: Int!) {
        repository(owner: \$owner, name: \$name) { pullRequest(number: \$pr) { id isInMergeQueue } }
      }" -f owner="${REPO%%/*}" -f name="${REPO##*/}" -F pr="$PR" 2>/dev/null); then
      in_queue='{}'
    fi
    pr_node_id=$(echo "$in_queue" | jq -r '.data.repository.pullRequest.id' 2>/dev/null)
    in_queue_val=$(echo "$in_queue" | jq -r '.data.repository.pullRequest.isInMergeQueue' 2>/dev/null)
    case "$in_queue_val" in true|false) : ;; *) in_queue_val="__UNREADABLE__" ;; esac
    if [ "$in_queue_val" = "true" ] || [ "$in_queue_val" = "__UNREADABLE__" ]; then
      if [ -n "$pr_node_id" ] && [ "$pr_node_id" != "null" ]; then
        gh api graphql -f query="mutation(\$id: ID!) { dequeuePullRequest(input: {id: \$id}) { clientMutationId } }" -f id="$pr_node_id" >/dev/null 2>&1 || true
      fi
      if ! in_queue_val=$(gh api graphql -f query="
        query(\$owner: String!, \$name: String!, \$pr: Int!) {
          repository(owner: \$owner, name: \$name) { pullRequest(number: \$pr) { isInMergeQueue } }
        }" -f owner="${REPO%%/*}" -f name="${REPO##*/}" -F pr="$PR" 2>/dev/null | jq -r '.data.repository.pullRequest.isInMergeQueue' 2>/dev/null); then
        in_queue_val="__UNREADABLE__"
      fi
      case "$in_queue_val" in true|false) : ;; *) in_queue_val="__UNREADABLE__" ;; esac
    fi

    # Codex P1, round 8: a "no-more-retries" comment marker is not read by any
    # workflow -- if the same unchanged head is later re-armed (including by the
    # manual command the comment below recommends) and fails again in a NEW
    # merge group, check-retried finds no queue-retry-used label for this SHA
    # (it never attached, which is WHY we're in this branch) and grants another
    # automatic retry, defeating the one-retry bound entirely. Attach the actual
    # queue-needs-human-review pause label here too -- the same durable, checkable
    # state the second-failure handoff step already uses -- so a future retry
    # attempt on this head sees it and stands down (per this step's own live
    # pause-label check above), regardless of whether the disarm/dequeue above
    # fully succeeded.
    gh label create queue-needs-human-review --color b60205 \
      --description "Needs a human decision -- an automated handler could not safely proceed" \
      --repo "${REPO}" >/dev/null 2>&1 || true
    gh api --method POST "repos/${REPO}/issues/${PR}/labels" -f "labels[]=queue-needs-human-review" >/dev/null 2>&1 || true
    pause_label_attached=$(gh api --paginate "repos/${REPO}/issues/${PR}/labels" --jq '.[].name' | grep -qxF "queue-needs-human-review" && echo true || echo false)

    NO_MORE_RETRIES_MARKER="<!-- queue-retry-handler:no-more-retries sha=$HEAD_SHA12 -->"
    if [ "$still_armed" = "true" ] || [ "$still_armed" = "__UNREADABLE__" ] || [ "$in_queue_val" = "true" ] || [ "$in_queue_val" = "__UNREADABLE__" ]; then
      echo "::error::Auto-merge is STILL ARMED/QUEUED for PR #$PR despite the disable-auto/dequeue calls -- the one-retry bound cannot be relied on until this is fixed by hand."
      gh pr comment "$PR" --repo "${REPO}" --body "Merge-group CI failed for this revision (commit \`$HEAD_SHA12\`), and the retry-tracking label could not be recorded. Attempted to disarm/dequeue to prevent an unbounded retry loop, but could NOT confirm it succeeded -- this needs urgent human attention. $NO_MORE_RETRIES_MARKER" || true
      exit 1
    elif [ "$pause_label_attached" != "true" ]; then
      echo "::error::Auto-merge was disarmed, but queue-needs-human-review could not be attached either for PR #$PR -- this head's retry budget has no durable record, risking an unbounded retry loop on a future re-arm. This needs urgent human attention."
      gh pr comment "$PR" --repo "${REPO}" --body "Merge-group CI failed for this revision (commit \`$HEAD_SHA12\`), and neither the retry-tracking label nor the human-review pause label could be recorded, despite auto-merge itself being disarmed. This needs urgent human attention -- the one-retry bound cannot be relied on for this head until this is fixed by hand. $NO_MORE_RETRIES_MARKER" || true
      exit 1
    else
      # PAN-152: admission's needs_human_is_stale() clears a pause on a later
      # head only through a bot-authored queue-needs-human-review marker. Claim
      # that provenance only when the latest labeling event is ours, applying
      # the second-failure handoff's ownership rules: a maintainer can attach
      # the label between this step's opening label read and the idempotent
      # POST above, and a marker must never make their pause clearable.
      # Prints the latest pause attachment as actor<TAB>created_at.
      latest_pause_attachment() {
        local raw
        raw=$(gh api --paginate --slurp "repos/${REPO}/issues/${PR}/timeline" 2>/dev/null) &&
          printf '%s' "$raw" | jq -er '
            if type == "array" and all(.[]; type == "array" and all(.[]; type == "object"))
            then (add // []) else error("invalid timeline pages") end |
            [.[] | select(.event == "labeled" and .label.name == "queue-needs-human-review")] |
            last |
            select(.actor.login | type == "string" and length > 0) |
            select(.created_at | type == "string") |
            select(.created_at | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")) |
            select(.created_at | . == (fromdateiso8601 | todateiso8601)) |
            [.actor.login, .created_at] | @tsv' 2>/dev/null
      }
      pause_owner=unknown
      if label_info=$(latest_pause_attachment); then
        if [ "$(cut -f1 <<< "$label_info")" = "github-actions[bot]" ]; then
          pause_owner=handler
        else
          pause_owner=external
        fi
      fi
      pause_marker="$NO_MORE_RETRIES_MARKER"
      if [ "$pause_owner" = "handler" ]; then
        pause_marker="<!-- queue-retry-handler:queue-needs-human-review sha=$HEAD_SHA12 -->"
      fi
      pause_text="Merge-group CI failed for this revision (commit \`$HEAD_SHA12\`), and the retry-tracking label could not be recorded. Disarmed and attached queue-needs-human-review rather than risk retrying unboundedly -- this needs a human: push a fix, or remove the pause and manually run \`gh pr merge --auto --squash\` once the labeling problem is resolved."
      comment_url=$(gh pr comment "$PR" --repo "${REPO}" --body "$pause_text $pause_marker" 2>/dev/null) || comment_url=""
      if [ "$pause_owner" = "unknown" ]; then
        echo "::error::Could not establish queue-needs-human-review's current owner for PR #$PR (commit $HEAD_SHA12). Keeping the pause without a staleness marker; remove it by hand once a fix is pushed."
        exit 1
      elif [ "$pause_owner" = "handler" ]; then
        comment_id=$(printf '%s' "$comment_url" | grep -oE '[0-9]+$' || echo "")
        # Codex P1 (PAN-152 review): the ownership read above is a snapshot.
        # A maintainer can remove and re-attach the pause before this comment
        # exists, and admission trusts the newest bot marker when it is at or
        # after the latest attachment, so it would make their pause clearable.
        # Re-read it now that the comment exists; on any change, or if it is
        # now unreadable, rewrite the marker to the inert one and fail. Not
        # covered: a re-attachment the timeline does not show yet, or one within
        # the same second as this comment (admission compares whole seconds).
        current_attachment=$(latest_pause_attachment) || current_attachment=""
        if [ "$current_attachment" != "$label_info" ]; then
          withdrawn_body=""
          if [ -n "$comment_id" ] &&
             gh api --method PATCH "repos/${REPO}/issues/comments/${comment_id}" -f "body=$pause_text $NO_MORE_RETRIES_MARKER" >/dev/null 2>&1; then
            withdrawn_body=$(gh api "repos/${REPO}/issues/comments/${comment_id}" --jq '.body' 2>/dev/null || echo "")
          fi
          if [ -z "$withdrawn_body" ] || [[ "$withdrawn_body" == *"$pause_marker"* ]]; then
            echo "::error::queue-needs-human-review on PR #$PR changed hands, or became unreadable, while this step recorded its provenance, and the clearable marker could not be confirmed withdrawn. Remove the queue-needs-human-review sha=$HEAD_SHA12 marker from this handler's comment by hand, or a later push can clear that pause."
            exit 1
          fi
          echo "::error::queue-needs-human-review on PR #$PR changed hands, or became unreadable, while this step recorded its provenance. Replaced the clearable marker with the inert one so a later push cannot clear that pause; it now stays until removed by hand."
          exit 1
        fi
        # Verify this exact comment, as the handoff does: without it a later
        # fix push cannot prove this pause stale.
        posted_body=""
        if [ -n "$comment_id" ]; then
          posted_body=$(gh api "repos/${REPO}/issues/comments/${comment_id}" --jq '.body' 2>/dev/null || echo "")
        fi
        if [[ "$posted_body" != *"$pause_marker"* ]]; then
          echo "::error::Could not confirm the queue-needs-human-review provenance marker was recorded for PR #$PR (commit $HEAD_SHA12) -- a later fix push cannot prove this pause stale without it. Keeping the pause and failing this step."
          exit 1
        fi
      fi
    fi
  else
    gh pr comment "$PR" --repo "${REPO}" --body "Merge-group CI failed for this revision (commit \`$HEAD_SHA12\`) -- automatically re-armed auto-merge for one retry. If this fails again for the same commit, it will NOT retry again; it needs a human to look. [Run]($RUN_URL)" || true
  fi
else
  # Codex P2, this rollout's own review: a bare log-and-exit-0 here let this step
  # report `success` (mark-handled's own `if:` then marked the whole attempt
  # handled) even though nothing was actually retried or handed off -- and since
  # the original queue failure already disarmed the PR, there is no SUBSEQUENT
  # merge-group failure to ever surface the problem again. The PR sits silently
  # stranded, with no comment and no visible check reflecting it. Cannot reliably
  # distinguish "benign race, --match-head-commit rejected a superseded head" from
  # "genuine permission/token failure" from the exit code alone, so treat both the
  # same: fail this step (mark-handled correctly withholds the dedup marker,
  # leaving this attempt eligible for a fresh look) AND post a best-effort comment,
  # since a failed internal automation run alone is not visible from the PR itself.
  echo "::error::Retry merge call failed for PR #$PR (commit $HEAD_SHA12) -- possibly superseded by a new push between pr-state's check and this call (--match-head-commit rejected the mismatch), a genuinely closed/merged PR, or a permission/token problem. Failing this step rather than silently marking the attempt handled."
  gh pr comment "$PR" --repo "${REPO}" --body "Merge-group CI failed for this revision (commit \`$HEAD_SHA12\`), and the automatic retry attempt itself also failed to re-arm auto-merge (superseded head, or a permission/token problem -- check this handler's own workflow run for details). This PR is currently stranded with auto-merge disarmed; a human needs to look. [Run]($RUN_URL)" || true
  exit 1
fi
