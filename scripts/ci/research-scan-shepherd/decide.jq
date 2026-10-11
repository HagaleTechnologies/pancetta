include "disposition";
include "eligibility";
def codex_login: . == "chatgpt-codex-connector" or . == "chatgpt-codex-connector[bot]";
def thread_blocker($kind): {kind: $kind, thread_id: .id, url: .url};
def disposition($s):
  . as $t
  | if (.opener | codex_login | not) then {blocker: thread_blocker("human-thread-open")}
    elif ($s.config.actors | index($t.latest.author)) == null then
      {blocker: thread_blocker("thread-needs-disposition")}
    else (.latest.body | disposition_kind) as $kind
    | if $kind == "fixed" then
        ($t.latest.body | fixed_prefix) as $prefix
        | [$s.pr.commits[] | select(startswith($prefix))] as $matches
        | if ($matches | length) == 1 and (($s.commit_files[$matches[0]] // []) | index($t.path)) != null
          then {resolve: {thread_id: $t.id, kind: "fixed", sha: $matches[0]}}
          else {blocker: thread_blocker("thread-bad-disposition")} end
      elif $kind == "deferred" then
        {resolve: {thread_id: $t.id, kind: "deferred",
          ticket: ($t.latest.body | capture("^Deferred to (?<ticket>PAN-[0-9]+)").ticket)}}
      elif $kind == "blocked" then {blocker: thread_blocker("deferral-blocked")}
      else {blocker: thread_blocker("thread-needs-disposition")} end
    end;
. as $s
| ($s | ineligible_reasons) as $reasons
| {number: $s.pr.number, eligible: ($reasons | length == 0), ineligible_reasons: $reasons,
   codex_rounds: ($s.codex_review_commits | unique | length), resolve: [],
   rerun_codex_verdict: false, rerun_ci_failed_jobs: false, clear_pause: false,
   arm: false, blockers: [], report: false}
| if .eligible | not then . else
    [$s.threads[] | select(.isResolved | not) | disposition($s)] as $dispositions
    | .resolve = [$dispositions[].resolve // empty]
    | .blockers = [$dispositions[].blocker // empty]
    | . as $d
    | ([$s.required_checks[] | select(.name == "Codex review clean (gate before expensive downstream jobs)")][0] // {}) as $codex
    | ([$s.required_checks[] | select(.name == "CI")][0] // {}) as $ci
    | .rerun_codex_verdict = ($codex.conclusion == "failure" and
        (($s.codex_run.attempt // 1) < 2) and
        $codex.title == "Codex raised new concerns" and
        all($s.threads[] | select((.isResolved | not) and (.opener | codex_login));
          .id as $id | any($d.resolve[]; .thread_id == $id)))
    | .rerun_ci_failed_jobs = ($ci.conclusion == "failure" and
        (($s.ci_run.attempt // 1) < 2) and
        ($s.ci_failed_jobs | length > 0) and
        ($s.ci_failed_jobs - ["Codex-clean label gate (waits for wait-for-codex.yml's verdict)", "CI"] | length == 0) and
        (($s.pr.labels | index("codex-clean:" + $s.pr.headRefOid[0:12])) != null or .rerun_codex_verdict))
    | . as $d
    | .blockers += [$s.required_checks[]
        | select(.conclusion != null)
        | select(.conclusion as $c | ["success", "skipped", "neutral"] | index($c) == null)
        | select((.name == $codex.name and $d.rerun_codex_verdict or .name == "CI" and $d.rerun_ci_failed_jobs) | not)
        | {kind: "check-failing", name}]
    | .blockers += [
        if $s.pause.present then
          if $s.pause.attached_by != "github-actions[bot]" then {kind: "maintainer-pause"}
          elif $s.rearm_count_for_head >= $s.config.rearm_budget then {kind: "rearm-budget-exhausted"}
          else empty end else empty end,
        if $s.pr.mergeable == "CONFLICTING" then {kind: "merge-conflict"} else empty end,
        if $s.merge_queue_rule_present | not then {kind: "merge-queue-rule-missing"} else empty end,
        if .codex_rounds > ($s.config.round_cap + 1) then {kind: "round-cap-exceeded"} else empty end]
    | .arm = (($s.pr.armed | not) and ($s.pr.inMergeQueue | not) and
        $s.merge_queue_rule_present and $s.pr.mergeable != "CONFLICTING" and
        (($s.pause.present | not) or ($s.pause.attached_by == "github-actions[bot]" and
          $s.rearm_count_for_head < $s.config.rearm_budget)))
    | .clear_pause = (.arm and $s.pause.present)
    | .report = (($s.now | fromdateiso8601) - ($s.pr.createdAt | fromdateiso8601) >
        $s.config.stuck_after_hours * 3600 and ($s.pr.inMergeQueue | not) and (.blockers | length > 0))
  end
