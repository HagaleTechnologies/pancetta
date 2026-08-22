# PR review convergence policy

Applies to any iterative bot or human reviewer (Codex, CodeRabbit, human)
on a pull request. Round is counted **per reviewer** (per bot login, or per
human reviewer) — not one shared counter across all reviewers on a PR. A PR
can have more than one active reviewer, and a shared counter misclassifies
a second reviewer's genuinely-first-look findings as a late round.

**What counts as a "round":** a push, or an explicit reviewer retrigger with no new push
(e.g. a workflow rerun plus an `@`-mention comment — see `land-pr`'s Codex-retrigger
sequence), that produces a new *remote* review response from that reviewer. A local
review-gate run (e.g. `codex exec review --uncommitted`, or a repo's override command — see
"Local review gate" below) does **not** increment the round counter — only the remote
reviewer's response does.

## Schedule

- **Rounds 1-5:** fix everything reasonable, P1 and P2 alike. No ticket
  filing yet.
- **Rounds 6-15:** fix P1s inline as usual. For genuinely new (or reopened)
  P2-or-lower findings, don't fix inline — file or append to a single
  follow-up ticket for this PR, with the finding pasted verbatim (reviewer's
  exact wording, file:line). Reply on the review thread linking the ticket,
  then resolve the thread.
- **Rounds 16-25:** ticket both P1 and P2 findings by default, same
  verbatim-finding/single-ticket mechanics — UNLESS the P1 is critical
  (security vulnerability, data loss/corruption, broken build/tests) or
  blocks the PR's own stated purpose outright. "Blocks the PR's own stated
  purpose" means: check the finding against the PR's own plan/research doc
  or ticket description — the PR literally cannot do the thing it exists to
  do without this fix, not merely "this would be more correct." When
  genuinely unsure whether a P1 clears that bar, fix it inline rather than
  ticket it.
- **Round 25 is a hard stop:** escalate to a human rather than opening a
  26th round unilaterally.

A finding that was already raised in an earlier round and simply never got fixed (omitted,
or the fix didn't actually land) is still owed from that round — it does not become newly
eligible for ticketing just because it resurfaces in a later round. Ticketing is for
genuinely new or reopened findings, not a way for an incomplete earlier-round fix to escape
via a later round's looser rules.

## Severity

Classify by substance, not by the reviewer's own label — many bots use
"P2" for an ordinary real correctness bug, not a style nit. Reconcile
against: **P1** = correctness bugs, security vulnerabilities, data
loss/corruption, broken build/tests, spec/contract violations. **P2** =
style, naming, structure preferences, unverified micro-optimizations, doc
nits, subjective suggestions. A finding's own substance always wins over
its label — use an explicit tag (e.g. `critical`/`blocking` → P1,
`minor`/`nit`/`suggestion` → P2) only as a shortcut when nothing about the
finding itself contradicts that tag. When genuinely unsure, treat it as
P1 — the failure mode to avoid is silently deferring a real bug.

## Ticketing mechanics

One follow-up ticket per PR, not one per finding — append later rounds'
deferred findings to the same ticket. Title: `[follow-up] <PR title> —
deferred review findings`. Body: each finding pasted verbatim (reviewer's
exact comment text, file:line, thread URL), linked back to the PR. If
ticket filing fails (no ticketing system configured, or it's unreachable),
don't let that block the thread indefinitely — fall back to fixing the
finding inline instead, except at round 25, where fixing inline would
require a push that opens the prohibited 26th round; escalate to a human
instead of either fixing inline or filing a ticket. An optional dependency
should never become load-bearing for getting a PR unstuck.

## What this does not do

- **Does not mean P2 findings get ignored.** They land in a real ticket, not silently
  dropped — the trade is "fixed later, deliberately" instead of "fixed now, chased
  indefinitely."
- **Does not relax P1 handling before round 16.** Every round through 15 still fixes every
  P1 finding it sees; round 16+ still fixes critical/blocking P1s regardless.
- **Does not override a human reviewer's explicit CHANGES_REQUESTED.** That still needs the
  reviewer's own sign-off, not just a filed ticket.

## Local review gate

Before pushing a fix round and consuming a real reviewer round-trip, run
the equivalent review locally first and iterate until clean:

```bash
codex exec review --uncommitted
```

If this round's fixes are already committed locally (not just
staged/unstaged/untracked), `--uncommitted` won't see them — review the
actual diff about to be pushed instead (a commit-range review against the
PR's base, not just the working tree; check the reviewer's own docs for
the right invocation if `--uncommitted` doesn't cover a commit range).

If a repo's `.claude/review-policy-override.md` defines a `## Local review
gate` section with its own command, use that instead (a repo's own hard
constraints can legitimately conflict with running `codex exec review`
directly). This doesn't replace the remote round — it's still the
authoritative round counter (see "What counts as a round" above) — it just
means the remote round should usually come back clean, so a round that does
find something is genuinely new signal.

Treat local findings with the same severity/round mechanics as remote ones. This doesn't
replace the remote check — it's still required and still the authoritative round counter —
it just means the remote round should usually come back clean, so a round that does find
something is genuinely new signal, not something a first local look would have caught for
free. A local finding has no PR review thread, so the ticketing mechanics above adapt: paste
the local reviewer's finding verbatim same as any other, write `(found by local review gate,
no PR thread)` in place of the thread URL, and skip the reply-and-resolve-thread step (there
is no thread to resolve) — filing to the single follow-up ticket is what clears it.
"Iterate until clean" means clean of un-ticketed findings — once a finding is correctly
deferred per the round-based schedule above, it no longer blocks the gate even if the local
reviewer keeps re-flagging it on every re-run.

## Collision awareness

Round-based reasoning assumes one worker driving one PR through its review
loop — it breaks down the moment a second session is independently active
on the same PR. Before starting work on any round (including round 1),
check whether the remote branch has moved since your last known commit in
a way you didn't cause. Note: a `land-pr` `update-branch` call you
yourself just triggered also moves the remote branch — that's not a
collision, just don't mistake it for one if you invoked `land-pr` earlier
in the same session. `$PR_NUMBER` must be in scope for this check —
`resolve-review-feedback`'s Step 3 ("Collision check before starting any
round") is what invokes this check today and already establishes
`$PR_NUMBER` in its own Step 1. `land-pr` does not call this check itself
as of this writing (its own SKILL.md has no reference to it), though it
does establish its own `PR_NUMBER` for its polling loop, and its
`diagnose_behind` sync (below) exists specifically to keep this check
accurate for whichever caller does invoke it after `land-pr` has acted in
the same session.

```bash
BRANCH="$(git symbolic-ref --short -q HEAD || true)"
if [ -z "$BRANCH" ]; then
  echo "HEAD is detached — this check assumes a checked-out branch matching the PR's remote head." >&2
  echo "Check out that branch explicitly before running it, rather than guessing at a ref." >&2
  exit 0  # deliberate: same "pause, don't fail the caller" contract as a detected collision
fi
if [ -z "${PR_NUMBER:-}" ]; then
  echo "PR_NUMBER is not set — this check requires it (both resolve-review-feedback and land-pr establish it in their own Step 1)." >&2
  echo "Cannot verify remote state without it. Pausing rather than guessing." >&2
  exit 0  # deliberate: same "pause, don't fail the caller" contract as a detected collision
fi
FETCH_ERR="$(mktemp)"
trap 'rm -f "$FETCH_ERR"' EXIT
if ! git fetch origin "refs/pull/${PR_NUMBER}/head" --quiet 2>"$FETCH_ERR"; then
  echo "Could not fetch the PR's head ref (refs/pull/${PR_NUMBER}/head) — remote state is unverifiable: $(cat "$FETCH_ERR")" >&2
  echo "Pausing rather than treating an unreachable remote as collision-free." >&2
  exit 0  # deliberate: same "pause, don't fail the caller" contract as a detected collision
fi
REMOTE_SHA="$(git rev-parse FETCH_HEAD 2>/dev/null || echo "")"
if [ -z "$REMOTE_SHA" ]; then
  echo "Could not resolve FETCH_HEAD after a successful fetch — remote state is unverifiable." >&2
  echo "Pausing rather than treating an unreachable remote as collision-free." >&2
  exit 0  # deliberate: same "pause, don't fail the caller" contract as a detected collision
fi
LOCAL_SHA="$(git rev-parse HEAD)"
if [ "$REMOTE_SHA" != "$LOCAL_SHA" ] && ! git merge-base --is-ancestor "$REMOTE_SHA" HEAD 2>/dev/null; then
  CHERRY_OUT="$(git cherry HEAD "$REMOTE_SHA" 2>/dev/null)" || CHERRY_OUT="+ unverifiable"
  if ! echo "$CHERRY_OUT" | grep -q '^+'; then
    : # every commit unique to $REMOTE_SHA is patch-equivalent to something already in our
      # history (git cherry reports no '+' lines) — this is our own rebase/amend of content
      # we already have, not new content from someone else
  else
    RECENT_AUTHOR="$(git log -1 --format='%an <%ae> at %ad' "$REMOTE_SHA" 2>/dev/null || echo "unknown")"
    echo "The PR's remote head (${REMOTE_SHA}) is not reachable from our local history and we didn't produce it (${RECENT_AUTHOR})." >&2
    echo "Possible concurrent session on this PR. Pausing — do not race it." >&2
    exit 2  # see the exit-code contract below — distinct from 0 (clean, or couldn't verify) so an automated caller can tell them apart
  fi
fi
```

This fetches `refs/pull/${PR_NUMBER}/head` directly into `FETCH_HEAD` — no
local branch or `origin/$BRANCH` ref needed, so it works identically for
same-repo and fork PRs (a fork PR's head branch doesn't exist as
`origin/$BRANCH` at all — it lives in the contributor's fork), and is
immune to `--single-branch` clone limitations (a `--single-branch`
clone's `remote.origin.fetch` refspec may not map the current branch
name, so fetching `origin "$BRANCH"` can succeed without actually
updating the `origin/$BRANCH` ref; `git fetch origin <explicit-ref>`
bypasses the configured refspec entirely). `FETCH_HEAD` is also always
exactly the commit this fetch just retrieved, so there's no separate
"which ref did we actually just fetch" ambiguity to track.

Deliberately stateless — no baseline file. A persisted "last known remote
SHA" sounds stronger, but it has to be scoped correctly (per-branch,
per-repo-checkout, per-work-session) to avoid becoming its own source of
false positives/negatives, and gets that scoping wrong in ways that are
easy to miss (a prior session's stale baseline outliving the session that
wrote it, a slash in the branch name breaking a naive filename). The
`git cherry HEAD "$REMOTE_SHA"` check above covers the legitimate-local-
rebase/amend case without needing a baseline file either: it asks the
actual right question — "is every commit unique to the remote's head
patch-equivalent to something already in our own history, or is at least
one of them genuinely new content?" — rather than "did this exact SHA
pass through somewhere we've been," which is what an earlier version of
this check (comparing against `git reflog show HEAD`) got wrong: two
sessions sharing the same checkout (not a worktree) can leave a foreign
commit's SHA sitting in the shared reflog (e.g. via a `git pull` later
discarded with `git reset --hard`), which would make the reflog check
wrongly wave through a real collision. `git cherry` doesn't have that
failure mode since it compares content, not history-traversal.

This check has several known, accepted gaps rather than one: (1) it won't
catch a force-push that moves the PR's remote head *backward* to an
ancestor of HEAD — that produces a `REMOTE_SHA` which the ancestry check
(the `git merge-base --is-ancestor` test, before `git cherry` ever runs)
treats as "fine, no collision." That's a deliberately-adversarial action
against a PR branch, not an ordinary review-loop event, and this fleet's
own hygiene rules already discourage it ("Main moves only by PR merge",
branch isolation). If you suspect it happened, verify directly
(`gh pr view --json commits`, or compare the PR's commit count against
what you expect) rather than trusting this heuristic alone. (2) `git
cherry`'s patch-equivalence is a patch-id hash of each commit's diff
against its first parent — a rebase onto a sufficiently different base
can shift enough surrounding context to change the patch-id even for a
logically-identical change, which would surface as a `+` line and this
check would (correctly conservatively) pause on it as if it were new
content; that's a false-positive-toward-caution, not a missed collision,
consistent with this check's overall bias to pause when unsure. (3) the
check always fetches from the literal `origin` remote, assuming that's
the PR's actual base repo — in a fork-clone topology where `origin` is
itself a fork (this fleet's own "clone-topology trap"), the check could
end up comparing against the wrong repo's PR-numbering namespace
entirely; resolving that needs broader remote-resolution work and is out
of scope for this fix. (4) `git cherry` does not evaluate merge commits
at all — a foreign session's merge commit (e.g. merging base into the PR
branch and resolving real conflicts) that carries content beyond its
parents is invisible to this check if all of that merge's non-merge
parent commits are otherwise already known to us.

**Exit-code contract.** Exit 2 means a real collision was detected and the
worker paused — an automated caller can distinguish this from exit 0
(checked clean, or could not verify and paused defensively) without
parsing stderr. Both still count as "don't proceed" for a human/agent
caller; the distinction is only for a scripted wrapper that wants to log
or alert differently on an actual collision versus an inconclusive check.
Every pause-without-a-detected-collision case above — detached HEAD,
unset `PR_NUMBER`, a failed fetch, and an unresolvable `FETCH_HEAD` after
an ostensibly successful fetch — stays `exit 0`: they're "couldn't
verify, pausing defensively," not "detected a real collision." Only the
actual-collision-detected branch uses `exit 2`.

If detected, do not race it — pause and surface it (hand off, split scope
explicitly, or stand down) rather than pushing a competing fix round.

## Communication guardrails win

Filing a ticket in the repo's own tracker and replying on a PR review thread are
development-workflow actions, not "outbound communication" in the sense of no-email/
no-social-post guardrails — but if a repo's guardrail is written broadly enough to make
that genuinely ambiguous, don't guess: hand off to a human.

## Two check-in triggers that override the round math regardless of count

These are structural red flags, not schedule math, and can fire well before
round 25:

- **The fix strategy itself needs to change.** The same code region
  produces a genuinely new bug shape on 3+ consecutive rounds even though
  each individual fix was locally correct — that signals the abstraction
  itself is wrong, not that you're almost done patching it. Check in and
  consider proposing a redesign rather than another point patch.
- **A "finding" is actually a scope-expansion request in disguise, not a
  defect.** Check the finding against the PR's own plan/research doc — did
  the current behavior violate what this PR said it would do, or could
  almost anything theoretically be made more correct with enough added
  machinery? Ticket it as a fresh design-needed follow-up rather than
  expanding in-PR.

## Provenance

Adopted fleet-wide 2026-08-07 (two-tier schedule); revised 2026-08-19 to the four-tier
schedule (rounds 6-15/16-25, local-review gate, collision-awareness); canonicalized into
credenza 2026-08-21 (this file becomes the single source, synced into each repo's
`docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`); corrected 2026-08-22 (restored
several clauses a rewrite had silently dropped, fixed a false-positive bug in the
collision-awareness check); corrected again 2026-08-22 (HAG-5: collision check now fetches
`refs/pull/${PR_NUMBER}/head` instead of `origin/$BRANCH`, fixing an indefinite-pause failure
mode on fork PRs and `--single-branch` clones (the old fetch of `origin/$BRANCH` either failed
outright or silently didn't update the ref it then compared against); distinct exit 2 for a
detected collision; round-25 ticket-fallback conflict, committed-but-unpushed local-gate
coverage, and ticketed-local-P2 gate-clearing clarified; `land-pr`'s `diagnose_behind` now
syncs the local checkout after a successful `update-branch` call; rebase/self-rewrite
detection replaced reflog-SHA-membership with `git cherry` patch-equivalence after review
found the reflog version could wrongly wave through a real collision in a shared-checkout,
non-worktree scenario); corrected a third time 2026-08-22 (HAG-5: `git cherry`'s own failure
no longer fails open — its exit status is captured explicitly rather than inherited from the
downstream `grep`; added the `git cherry`-skips-merge-commits gap to the known-gaps list;
exit-code-contract prose and inline comments now cover all four `exit 0` pause sites, not just
the original two; `land-pr`'s `diagnose_behind` sync no longer silently no-ops when the
`refs/pull/${PR_NUMBER}/head` fetch itself fails). Edit
`credenza/claude/skills/resolve-review-feedback/references/convergence-policy.md`,
not a per-repo copy.
