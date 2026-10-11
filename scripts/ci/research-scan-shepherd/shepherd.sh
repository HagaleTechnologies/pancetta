#!/usr/bin/env bash
# PAN-172. Executes only trusted main's code; PR titles/replies are API data, never shell.
set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
ONLY_PR=${ONLY_PR:-}
DRY_RUN=${DRY_RUN:-false}
REPORT_MENTION=${REPORT_MENTION:-thagale}
RUN_URL=${RUN_URL:-}
RESOLVE_TOKEN=${RESOLVE_TOKEN:-}
for arg in "$@"; do
  case "$arg" in --dry-run) DRY_RUN=true ;; *) echo "Unknown argument: $arg" >&2; exit 2 ;; esac
done
validate_inputs() {
  [[ -z $ONLY_PR || $ONLY_PR =~ ^[0-9]+$ ]] || return 1
  [[ $DRY_RUN == true || $DRY_RUN == false || -z $DRY_RUN ]] || return 1
  [[ ${REPO:-} =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || return 1
  [[ $REPORT_MENTION =~ ^[A-Za-z0-9-]+$ ]] || return 1
}
validate_inputs || { echo 'Invalid shepherd input' >&2; exit 2; }
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
CONFIG='{"actors":["thagale","catalyst-cloud-connector[bot]"],"title_regex":"^Weekly research scan [0-9]{4}-[0-9]{2}-[0-9]{2}$","rearm_budget":2,"stuck_after_hours":24,"round_cap":3}'
# Return 0 for quiet, 1 for busy, 2 for an unreadable producer. Never treat an API
# failure as an empty queue. Check again before mutations, not just at startup.
quiescent() {
  local workflow status runs
  for workflow in auto-merge-trigger.yml queue-retry-handler.yml; do
    for status in in_progress queued; do
      runs=$(gh run list --repo "$REPO" --workflow "$workflow" --status "$status" --json databaseId --limit 1) || return 2
      jq -e 'type == "array"' <<< "$runs" >/dev/null || return 2
      if [[ $(jq 'length' <<< "$runs") != 0 ]]; then
        echo "::notice::Producer $workflow is $status; skip this pass." >&2
        return 1
      fi
    done
  done
}
list_candidates() {
  if [[ -n $ONLY_PR ]]; then printf '%s\n' "$ONLY_PR"; else
    gh pr list --repo "$REPO" --state open --search 'in:title "Weekly research scan"' --json number --limit 100 | jq -r '.[].number'
  fi
}
# Pagination is required for threads, changed files, timelines, comments and checks.
# The bounded GraphQL connections fail closed if truncated, rather than deciding on
# incomplete author/round/commit evidence. No checked-out PR content is ever read.
gather_snapshot() {
  local pr=$1 dest=$2 dir head sha prefix ci_id
  dir=$(mktemp -d "$WORK/snapshot.XXXXXX") || return 1
  gh api graphql --paginate -F query=@"$SCRIPT_DIR/pr.graphql" \
    -f owner="${REPO%%/*}" -f name="${REPO##*/}" -F pr="$pr" > "$dir/pages" || return 1
  jq -se 'length > 0 and all(.[]; (.errors == null) and
    (.data.repository.pullRequest | . != null and .state == "OPEN" and
      (.headRefOid | test("^[0-9a-f]{40}$")) and
      .labels.pageInfo.hasNextPage == false and .commits.pageInfo.hasPreviousPage == false and
      .reviews.pageInfo.hasNextPage == false))' "$dir/pages" >/dev/null || return 1
  jq -s -L "$SCRIPT_DIR" 'include "login"; .[0].data.repository.pullRequest as $p |
    {pr: ($p | {number,title,author:(.author | login),isDraft,isCrossRepository,baseRefName,
      createdAt,headRefOid,mergeable,armed:(.autoMergeRequest != null),inMergeQueue:.isInMergeQueue,
      labels:[.labels.nodes[].name],commits:[.commits.nodes[].commit.oid]}),
     codex_review_commits: [$p.reviews.nodes[] | select(.author.login == "chatgpt-codex-connector" or
       .author.login == "chatgpt-codex-connector[bot]") | .commit.oid | select(. != null)] | unique,
     threads: [.[].data.repository.pullRequest.reviewThreads.nodes[] |
       {id,isResolved,path,opener:(.opener.nodes[0].author | login),url:.latest.nodes[0].url,
        latest:(.latest.nodes[0] | {id,author:(.author | login),body})}]}' "$dir/pages" > "$dir/snapshot" || return 1
  head=$(jq -er '.pr.headRefOid' "$dir/snapshot") || return 1
  gh api "repos/$REPO/pulls/$pr/files?per_page=100" --paginate | jq -s 'add | map({path:.filename,previous_path:.previous_filename})' > "$dir/files" || return 1
  gh api "repos/$REPO/rules/branches/main" --paginate | jq -s 'add' > "$dir/rules" || return 1
  gh api "repos/$REPO/commits/$head/check-runs?per_page=100&filter=latest" --paginate | jq -s '[.[].check_runs[]]' > "$dir/checks" || return 1
  gh api "repos/$REPO/commits/$head/statuses?per_page=100" --paginate | jq -s 'add' > "$dir/statuses" || return 1
  gh api "repos/$REPO/issues/$pr/timeline?per_page=100" --paginate | jq -s 'add' > "$dir/timeline" || return 1
  gh api "repos/$REPO/issues/$pr/comments?per_page=100" --paginate | jq -s 'add' > "$dir/comments" || return 1
  gh run list --repo "$REPO" --workflow ci.yml --commit "$head" --event pull_request \
    --json databaseId,conclusion,status,headSha,attempt,createdAt --limit 1 > "$dir/ci" || return 1
  # Human reviews also create runs, but their Codex response job is skipped. Pick
  # only a Codex-triggered response; rerunning a later human review cannot refresh it.
  gh api "repos/$REPO/actions/workflows/wait-for-codex.yml/runs?event=pull_request_review&head_sha=$head&per_page=100" --paginate |
    jq -s --arg head "$head" '[.[].workflow_runs[] |
      select(.head_sha == $head and .event == "pull_request_review") |
      select(.actor.login == "chatgpt-codex-connector" or .actor.login == "chatgpt-codex-connector[bot]")] |
      sort_by(.created_at,.id) | reverse | .[:1] |
      map({databaseId:.id,conclusion,status,headSha:.head_sha,attempt:.run_attempt,createdAt:.created_at})' > "$dir/codex" || return 1
  ci_id=$(jq -r '.[0].databaseId // empty' "$dir/ci") || return 1
  printf '{"jobs":[]}' > "$dir/jobs"
  if [[ -n $ci_id ]]; then gh run view "$ci_id" --repo "$REPO" --json jobs > "$dir/jobs" || return 1; fi
  printf '{}' > "$dir/commit-files"
  jq -r -L "$SCRIPT_DIR" 'include "disposition"; .threads[] |
    select(.isResolved == false) | .latest.body | select(disposition_kind == "fixed") | fixed_prefix' \
    "$dir/snapshot" | sort -u > "$dir/prefixes" || return 1
  while IFS= read -r prefix; do
    sha=$(jq -r --arg prefix "$prefix" '[.pr.commits[] | select(startswith($prefix))] | if length == 1 then .[0] else empty end' "$dir/snapshot") || return 1
    [[ -n $sha ]] || continue
    gh api "repos/$REPO/commits/$sha?per_page=100" --paginate | jq -s '[.[].files[].filename]' > "$dir/paths" || return 1
    jq --arg sha "$sha" --slurpfile paths "$dir/paths" '.[$sha] = $paths[0]' "$dir/commit-files" > "$dir/next" || return 1
    mv "$dir/next" "$dir/commit-files" || return 1
  done < "$dir/prefixes"
  jq --arg now "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --argjson config "$CONFIG" \
    --slurpfile files "$dir/files" --slurpfile rules "$dir/rules" --slurpfile checks "$dir/checks" \
    --slurpfile statuses "$dir/statuses" --slurpfile timeline "$dir/timeline" --slurpfile comments "$dir/comments" \
    --slurpfile ci "$dir/ci" --slurpfile codex "$dir/codex" --slurpfile jobs "$dir/jobs" --slurpfile commit_files "$dir/commit-files" '
    . + {now:$now,config:$config,commit_files:$commit_files[0],
      merge_queue_rule_present:any($rules[0][]; .type == "merge_queue"),
      ci_failed_jobs:[$jobs[0].jobs[] | select(.conclusion != null and .conclusion != "success" and .conclusion != "skipped" and .conclusion != "neutral") | .name],
      ci_run:($ci[0][0] // null), codex_run:($codex[0][0] // null),
      pause:{present:(.pr.labels | index("queue-needs-human-review") != null),
        attached_by:([$timeline[0][] | select(.event == "labeled" and .label.name == "queue-needs-human-review")][-1].actor.login // null)},
      rearm_count_for_head:([ $comments[0][] | select(.user.login == "github-actions[bot]") |
        select(.body | contains("<!-- research-scan-shepherd:rearm sha=" + $head[0:12] + " -->"))] | length)}
    | .pr.files = $files[0]
    | .required_checks = [$rules[0][] | select(.type == "required_status_checks") |
        .parameters.required_status_checks[] | . as $rule |
        ([$checks[0][] | select(.name == $rule.context) |
          select($rule.integration_id == null or $rule.integration_id == -1 or .app.id == $rule.integration_id) |
          # Ignore the identically named native skipped job in favor of the managed verdict.
          select(.name != "Codex review clean (gate before expensive downstream jobs)" or
            ((.external_id // "") | test("^[0-9]+$")))] | sort_by(.started_at,.id) | last) as $check |
        ([$statuses[0][] | select(.context == $rule.context)] | sort_by(.created_at,.id) | last) as $status |
        {name:$rule.context,conclusion:($check.conclusion //
          (if $check != null then null elif $status.state == "pending" then null else $status.state end)),
         title:$check.output.title} ] | .required_checks |= unique_by(.name)' \
    --arg head "$head" "$dir/snapshot" > "$dest" || return 1
}
record_action() { printf '%s\n' "$1" >> "$WORK/actions"; }
decide() { jq -c -L "$SCRIPT_DIR" -f "$SCRIPT_DIR/decide.jq" "$1"; }
# A full fresh snapshot revalidates author, paths, pause provenance, queue rules and
# dispositions at every action boundary. Return 3 for a benign stale/busy stand-down.
refresh() {
  local pr=$1 head=$2 quiet=0
  quiescent || quiet=$?
  [[ $quiet == 0 ]] || { [[ $quiet == 1 ]] && return 3; return 1; }
  gather_snapshot "$pr" "$WORK/live" || return 1
  decide "$WORK/live" > "$WORK/live-decision" || return 1
  jq -e --arg head "$head" '.pr.headRefOid == $head' "$WORK/live" >/dev/null || return 3
  jq -e '.eligible' "$WORK/live-decision" >/dev/null || return 3
}
# Do not edit another actor's comment just because it copied our marker.
upsert_status() {
  local pr=$1 body=$2 comments id
  comments=$(gh api "repos/$REPO/issues/$pr/comments?per_page=100" --paginate | jq -s 'add') || return 1
  id=$(jq -r '[.[] | select(.user.login == "github-actions[bot]" and (.body | contains("<!-- research-scan-shepherd:status -->")))][0].id // empty' <<< "$comments") || return 1
  if [[ -n $id ]]; then
    gh api -X PATCH "repos/$REPO/issues/comments/$id" -f body="$body" >/dev/null
  else
    gh api -X POST "repos/$REPO/issues/$pr/comments" -f body="$body" >/dev/null
  fi
}
# REST, not `gh issue list`: gh renders a Bot author as `app/github-actions`, which never
# equals the REST login this workflow's own issues carry.
stuck_issues() {
  gh api "repos/$REPO/issues?state=open&labels=research-scan-stuck&per_page=100" --paginate |
    jq -s '[add[] | select(.pull_request == null and .user.login == "github-actions[bot]")]'
}
report_pr() {
  local pr=$1 decision=$2 body issues id quiet=0
  [[ $DRY_RUN != true ]] || return 0
  quiescent || quiet=$?
  [[ $quiet == 0 ]] || { [[ $quiet == 1 ]] && return 3; return 1; }
  body=$(jq -r --arg url "$RUN_URL" '.blockers | map("- " + .kind + (if .name then ": " + .name else "" end) + (if .url then " " + .url else "" end)) | join("\n") + "\n\nRun: " + $url' "$decision") || return 1
  upsert_status "$pr" "$body
<!-- research-scan-shepherd:status -->" || return 1
  # Create the label only when the lookup misses; never restyle an existing one.
  if ! gh api "repos/$REPO/labels/research-scan-stuck" >/dev/null 2>&1; then
    gh label create research-scan-stuck --repo "$REPO" --color D93F0B --description 'Weekly research scan needs a maintainer' >/dev/null || return 1
  fi
  issues=$(stuck_issues) || return 1
  id=$(jq -r --arg marker "<!-- research-scan-shepherd:issue pr=$pr -->" '[.[] | select((.body // "") | contains($marker))][0].number // empty' <<< "$issues") || return 1
  body="@$REPORT_MENTION
https://github.com/$REPO/pull/$pr

$body
<!-- research-scan-shepherd:issue pr=$pr -->"
  if [[ -n $id ]]; then
    gh issue edit "$id" --repo "$REPO" --body "$body" >/dev/null
  else
    gh issue create --repo "$REPO" --label research-scan-stuck --title "Research scan PR #$pr needs a maintainer" --body "$body" >/dev/null
  fi
}
apply() {
  local snapshot=$1 decision=$2 pr head entry id original live code ci_id codex_id attempt
  pr=$(jq -r '.pr.number' "$snapshot"); head=$(jq -r '.pr.headRefOid' "$snapshot")
  cat "$decision"
  if [[ $DRY_RUN == true ]]; then record_action "dry-run (no writes)"; return 0; fi
  while IFS= read -r entry; do
    [[ -n $RESOLVE_TOKEN ]] || { echo '::error::Missing CODEX_REVIEW_PAT' >&2; return 1; }
    refresh "$pr" "$head" || return $?
    id=$(jq -r '.thread_id' <<< "$entry")
    # Compare the whole latest comment, including its ID, so an intervening restatement
    # or changed disposition cannot inherit this snapshot's decision.
    original=$(jq -c --arg id "$id" '.threads[] | select(.id == $id) | .latest' "$snapshot")
    jq -e --arg id "$id" --argjson original "$original" 'any(.threads[]; .id == $id and .isResolved == false and .latest == $original)' "$WORK/live" >/dev/null || continue
    jq -e --argjson entry "$entry" '.resolve | index($entry) != null' "$WORK/live-decision" >/dev/null || continue
    # GraphQL variables must reach GitHub literally, hence the escaped \$id.
    live=$(gh api graphql -f query="query(\$id: ID!) { node(id: \$id) { ... on PullRequestReviewThread { isResolved latest: comments(last: 1) { nodes { id author { __typename login } body url } } } } }" -f id="$id") || return 1
    jq -e -L "$SCRIPT_DIR" --argjson original "$original" 'include "login"; .data.node | .isResolved == false and
      (.latest.nodes[0] | {id,author:(.author | login),body}) == $original' <<< "$live" >/dev/null || continue
    GH_TOKEN="$RESOLVE_TOKEN" gh api graphql -f query="mutation(\$id: ID!) { resolveReviewThread(input: {threadId: \$id}) { thread { isResolved } } }" -f id="$id" >/dev/null || return 1
    record_action "resolved $id"
  done < <(jq -c '.resolve[]' "$decision")
  if jq -e '.rerun_codex_verdict' "$decision" >/dev/null; then
    refresh "$pr" "$head" || return $?
    if jq -e '.rerun_codex_verdict' "$WORK/live-decision" >/dev/null; then
      codex_id=$(jq -r '.codex_run | select(.status == "completed") | .databaseId // empty' "$WORK/live")
      [[ -n $codex_id ]] || { echo '::error::No completed Codex response run for this head' >&2; return 1; }
      gh run rerun "$codex_id" --repo "$REPO" >/dev/null || return 1
      record_action "reran Codex verdict"
    fi
  fi
  if jq -e '.rerun_ci_failed_jobs' "$decision" >/dev/null; then
    refresh "$pr" "$head" || return $?
    if jq -e '.rerun_ci_failed_jobs' "$WORK/live-decision" >/dev/null; then
      ci_id=$(jq -r '.ci_run | select(.status == "completed") | .databaseId // empty' "$WORK/live")
      [[ -n $ci_id ]] || return 1
      gh run rerun "$ci_id" --failed --repo "$REPO" >/dev/null || return 1
      record_action "reran failed CI jobs"
    fi
  fi
  if jq -e '.arm' "$decision" >/dev/null; then
    [[ -n $RESOLVE_TOKEN ]] || { echo '::error::Missing CODEX_REVIEW_PAT' >&2; return 1; }
    refresh "$pr" "$head" || return $?
    if jq -e '.arm' "$WORK/live-decision" >/dev/null; then
      if jq -e '.clear_pause' "$WORK/live-decision" >/dev/null; then
        gh api -X DELETE "repos/$REPO/issues/$pr/labels/queue-needs-human-review" >/dev/null || return 1
        refresh "$pr" "$head" || return $?
        jq -e '.pause.present == false' "$WORK/live" >/dev/null || return 3
      fi
      jq -e '.arm' "$WORK/live-decision" >/dev/null || return 3
      GH_TOKEN="$RESOLVE_TOKEN" gh pr merge "$pr" --repo "$REPO" --auto --squash --match-head-commit "$head" >/dev/null || return 1
      record_action "armed auto-merge"
      attempt=$(jq '.rearm_count_for_head + 1' "$WORK/live")
      gh api -X POST "repos/$REPO/issues/$pr/comments" -f body="Re-armed auto-merge after a merge-queue removal (attempt $attempt/2 for ${head:0:12}). Notes-only scan PRs cannot cause a Rust merge-group failure; see docs/routines/pancetta-research-scan.md.
<!-- research-scan-shepherd:rearm sha=${head:0:12} -->" >/dev/null || return 1
    fi
  fi
  if jq -e '.report' "$decision" >/dev/null; then
    refresh "$pr" "$head" || return $?
    if jq -e '.report' "$WORK/live-decision" >/dev/null; then
      code=0; report_pr "$pr" "$WORK/live-decision" || code=$?
      [[ $code == 0 ]] || return "$code"
      record_action "reported blockers"
      return 2
    fi
  fi
}
close_resolved_issues() {
  local issues issue pr state quiet=0
  [[ $DRY_RUN != true ]] || return 0
  issues=$(stuck_issues) || return 1
  while IFS=$'\t' read -r issue pr; do
    [[ -z $ONLY_PR || $pr == "$ONLY_PR" ]] || continue
    quiescent || quiet=$?
    [[ $quiet == 0 ]] || { [[ $quiet == 1 ]] && return 3; return 1; }
    state=$(gh api "repos/$REPO/pulls/$pr") || return 1
    if [[ $(jq -r '.state' <<< "$state") == closed ]]; then
      state=$(jq -r 'if .merged then "merged" else "closed" end' <<< "$state")
      gh issue comment "$issue" --repo "$REPO" --body "PR #$pr is $state; closing." >/dev/null || return 1
      gh issue close "$issue" --repo "$REPO" >/dev/null || return 1
    fi
  done < <(jq -r '.[] |
    .number as $n | (.body // "") | capture("<!-- research-scan-shepherd:issue pr=(?<pr>[0-9]+) -->") | [$n,.pr] | @tsv' <<< "$issues")
}
quiet=0
quiescent || quiet=$?
[[ $quiet == 0 ]] || { [[ $quiet == 1 ]] && exit 0; echo '::error::Producer state unreadable' >&2; exit 1; }
list_candidates > "$WORK/candidates" || exit 1
result=0
printf '| PR | Eligibility | Rounds | Actions taken | Blockers |\n|---|---|---|---|---|\n' >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
while IFS= read -r pr; do
  [[ $pr =~ ^[0-9]+$ ]] || { result=1; continue; }
  snapshot="$WORK/$pr.snapshot"; decision="$WORK/$pr.decision"
  if gather_snapshot "$pr" "$snapshot" && decide "$snapshot" > "$decision"; then
    : > "$WORK/actions"
    code=0; apply "$snapshot" "$decision" || code=$?
    if [[ $code == 1 ]]; then
      echo "::error::PR #$pr mutation or refresh failed" >&2
      jq '.report = true | .blockers += [{kind:"mutation-failed"}]' "$decision" > "$WORK/error-decision"
      code=0; report_pr "$pr" "$WORK/error-decision" || code=$?
      [[ $code == 0 ]] || echo "::warning::PR #$pr failure report not posted (exit $code)." >&2
      cp "$WORK/error-decision" "$decision"
      record_action "mutation failed; report attempted"
      result=1
    elif [[ $code == 2 ]]; then result=1
    elif [[ $code == 3 ]]; then echo "::notice::PR #$pr changed or producer became busy; skip remaining actions."
    fi
    jq -r --rawfile actions "$WORK/actions" '[.number,
      (if .eligible then "eligible" else (.ineligible_reasons | join(", ")) end),.codex_rounds,
      ($actions | split("\n") | map(select(length > 0)) | join(", ")),
      ([.blockers[].kind]|join(", "))] | "| " + (map(tostring) | join(" | ")) + " |"' "$decision" >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
  else
    printf '| %s | unreadable | - | none | snapshot-unreadable |\n' "$pr" >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
    echo "::warning::PR #$pr snapshot-unreadable; no lifecycle mutations." >&2
    result=1
    # A failed comprehensive read may still allow a minimal age check and a report.
    # Reporting never arms, resolves, or reruns anything based on partial evidence.
    if minimal=$(gh api "repos/$REPO/pulls/$pr"); then
      if jq -e --arg now "$(date -u +%Y-%m-%dT%H:%M:%SZ)" '($now|fromdateiso8601) - (.created_at|fromdateiso8601) > 86400' <<< "$minimal" >/dev/null; then
        printf '{"blockers":[{"kind":"snapshot-unreadable"}]}' > "$WORK/error-decision"
        code=0; report_pr "$pr" "$WORK/error-decision" || code=$?
        [[ $code == 0 ]] || echo "::warning::PR #$pr snapshot-unreadable report not posted (exit $code)." >&2
      fi
    fi
  fi
done < "$WORK/candidates"
code=0; close_resolved_issues || code=$?
[[ $code == 0 || $code == 3 ]] || result=1
exit "$result"
