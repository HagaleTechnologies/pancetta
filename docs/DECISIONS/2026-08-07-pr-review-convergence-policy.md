# PR review convergence policy

Applies to any iterative bot or human reviewer (Codex, CodeRabbit, human)
on a pull request. Round is counted **per reviewer** (per bot login, or per
human reviewer) — not one shared counter across all reviewers on a PR. A PR
can have more than one active reviewer, and a shared counter misclassifies
a second reviewer's genuinely-first-look findings as a late round.

**What counts as a "round":** one push that triggers a new *remote* review
pass from that reviewer. A local review-gate run (e.g. `codex exec review
--uncommitted`, or a repo's override command — see "Local review gate"
below) does **not** increment the round counter — only the remote
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

## Severity

Classify by substance, not by the reviewer's own label — many bots use
"P2" for an ordinary real correctness bug, not a style nit. Reconcile
against: **P1** = correctness bugs, security vulnerabilities, data
loss/corruption, broken build/tests, spec/contract violations. **P2** =
style, naming, structure preferences, unverified micro-optimizations, doc
nits, subjective suggestions. When unsure, treat it as P1.

## Ticketing mechanics

One follow-up ticket per PR, not one per finding — append later rounds'
deferred findings to the same ticket. Title: `[follow-up] <PR title> —
deferred review findings`. Body: each finding pasted verbatim (reviewer's
exact comment text, file:line, thread URL), linked back to the PR. If
ticket filing fails (no ticketing system configured, or it's unreachable),
don't let that block the thread indefinitely — fall back to fixing the
finding inline instead. An optional dependency should never become
load-bearing for getting a PR unstuck.

## Local review gate

Before pushing a fix round and consuming a real reviewer round-trip, run
the equivalent review locally first and iterate until clean:

```bash
codex exec review --uncommitted
```

If a repo's `.claude/review-policy-override.md` defines a `## Local review
gate` section with its own command, use that instead (a repo's own hard
constraints can legitimately conflict with running `codex exec review`
directly). This doesn't replace the remote round — it's still the
authoritative round counter (see "What counts as a round" above) — it just
means the remote round should usually come back clean, so a round that does
find something is genuinely new signal.

## Collision awareness

Round-based reasoning assumes one worker driving one PR through its review
loop — it breaks down the moment a second session is independently active
on the same PR. Before starting work on any round (including round 1),
check whether the remote branch has moved since your last known commit in
a way you didn't cause. Note: a `land-pr` `update-branch` call you
yourself just triggered also moves the remote branch — that's not a
collision, just don't mistake it for one if you invoked `land-pr` earlier
in the same session.

```bash
BRANCH="$(git branch --show-current)"
LOCAL_SHA="$(git rev-parse HEAD)"
git fetch origin "$BRANCH" --quiet 2>/dev/null || true
REMOTE_SHA="$(git rev-parse "origin/${BRANCH}" 2>/dev/null || echo "")"
if [ -n "$REMOTE_SHA" ] && [ "$REMOTE_SHA" != "$LOCAL_SHA" ]; then
  RECENT_AUTHOR="$(git log -1 --format='%an <%ae> at %ad' "$REMOTE_SHA" 2>/dev/null || echo "unknown")"
  echo "origin/${BRANCH} has moved since our last known commit (${RECENT_AUTHOR}) and we did not push it." >&2
  echo "Possible concurrent session on this PR. Pausing — do not race it." >&2
  exit 0  # deliberate: pausing is not a failure, so a caller checking status sees success
fi
```

If detected, do not race it — pause and surface it (hand off, split scope
explicitly, or stand down) rather than pushing a competing fix round.

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
