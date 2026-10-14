# pancetta-research-scan routine

The weekly web-scan routine lives on claude.ai as cloud routine
`trig_014ruZWDKSo8Deq9jJ1kkQ9X` (RemoteTrigger), not in this repository. Its scan
method stays in the routine's own prompt. Everything after the note is written is
defined here, and the routine reads this file from `origin/main` at the start of every
run, so changing the lifecycle is a PR to this file, not a routine edit.

## Routine settings

These are the target settings after PAN-172 merges. The external routine still needs
the two operator edits below; this repository cannot apply them.

| Setting | Value |
|---|---|
| Schedule | Daily, 12:03 UTC (`3 12 * * *`). Scans on Mondays; shepherds every day. |
| Model | Sonnet 4.6 |
| Connectors | GitHub (HagaleTechnologies/pancetta), Linear (team PAN) |

## Prompt edits on claude.ai

1. Put the pointer block below at the very top of the routine's prompt.
2. Never retain an instruction that adds a `Co-Authored-By:` or `Claude-Session:` line to commits. Delete it.

Before 2026-10-12 12:03 UTC, apply the prompt edits and leave the schedule weekly.
After PAN-172 merges, change the schedule to daily and attach Linear for team PAN.
The pointer's fallback makes the first edit safe before this file is on main.

<!-- pointer-block -->
```text
Before anything else: run `git fetch origin main` and read
`git show origin/main:docs/routines/pancetta-research-scan.md`. Follow its "Lifecycle"
section exactly. It overrides every later instruction in this prompt about when to scan,
commits, branches, pull requests and code review.
If that file does not exist on origin/main: if today (UTC) is not Monday, stop now;
otherwise do the scan below, commit without any trailer lines, open the PR, and stop.
Never add a Co-Authored-By or Claude-Session line to any commit message.
```

## Lifecycle

### Modes

Every run does Shepherd first. Then, if today (UTC) is Monday and no PR titled
`Weekly research scan <today>` exists, it does Scan. On other days it stops after Shepherd.

### Scan (Mondays)

- Write exactly one file, `research/notes/web-scan-YYYY-MM-DD.md`. Draft hypothesis-bank
  entries go inside the note. Do not edit `research/hypothesis_bank.md` or anything else.
- Branch `research-scan/YYYY-MM-DD` from `origin/main`. Commit message:
  `docs(research): weekly web scan YYYY-MM-DD`. No trailer lines of any kind.
- Before every push, run
  `git log --format=%B origin/main..HEAD | grep -inE '^[[:space:]]*(co-authored-by|claude-session):'`.
  It must print nothing. If it prints, reword those commits without the trailer and check again.
- Open a ready (not draft) PR into `main` titled exactly `Weekly research scan YYYY-MM-DD`.
  Do not merge it and do not enable auto-merge. `auto-merge-trigger.yml` arms it.
- Then work through review rounds (below) in this session, waiting at most 90 minutes in
  total for Codex. Whatever is left, the next day's Shepherd run picks up.

### Shepherd (every run)

For each open PR titled `Weekly research scan …`, handle every Codex review that has
threads without a reply from you (Review rounds, Replies and resolution). Touch no other PR.

### Review rounds

Round N is the Nth distinct commit that `chatgpt-codex-connector` has reviewed on the PR.
Classify severity by substance, using the Severity section of
`docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`. This scan-only schedule is
stricter than that policy:

| Round | Push a fix commit for | Defer to the follow-up ticket |
|---|---|---|
| 1 | every reasonable finding, any severity | nothing |
| 2–3 | P0/P1 only | P2 and lower |
| 4 and later | nothing; push no commits | everything (mark P0/P1 High priority in the ticket) |

Put each round's fixes in one commit. Never push an empty commit, never comment
`@codex review`, never force-push after the first review, never use an admin merge.

### Replies and resolution

Reply in the finding's own thread. The first line of the reply must be one of these,
exactly:

<!-- disposition-examples -->
```text
Fixed in 1a2b3c4 — narrowed the claim to the standard decoder
Deferred to PAN-180 — round 2, P2
Deferral blocked: Linear unavailable — needs a maintainer
```

- `Fixed in <sha>`: the SHA of the pushed commit that fixed it, which must change the
  file the thread is on.
- `Deferred to PAN-<n>`: only after the finding is in the follow-up ticket.
- `Deferral blocked`: only when Linear can't be reached. Leave the thread open.

After a `Fixed in` or `Deferred to` reply, resolve the thread if your GitHub tools can.
If they can't, leave it: `research-scan-shepherd.yml` resolves threads whose last reply
is one of these lines from `thagale` or `catalyst-cloud-connector[bot]`. Never reply to or resolve a thread a human
opened.

### Follow-up ticket

One Linear ticket in team PAN per scan PR, titled
`[follow-up] Weekly research scan YYYY-MM-DD — deferred review findings`. Paste each
deferred finding verbatim, with file:line and the thread URL, and link the PR. Append
findings from later rounds to the same ticket.

### Landing

You never merge. Auto-merge is armed when the PR opens. `research-scan-shepherd.yml`
(twice daily) resolves threads that carry a disposition, requests a fresh Codex verdict
after deferrals (an `@codex review` comment, once per head), re-arms auto-merge after a
merge-queue removal, and reports a PR stuck for more than a day.
