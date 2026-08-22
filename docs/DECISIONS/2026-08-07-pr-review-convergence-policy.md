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
nits, subjective suggestions. Only defer to an explicit tag when its
vocabulary maps cleanly onto blocking-vs-not (e.g. `critical`/`blocking` →
P1, `minor`/`nit`/`suggestion` → P2). When genuinely unsure, treat it as
P1 — the failure mode to avoid is silently deferring a real bug.

## Ticketing mechanics

One follow-up ticket per PR, not one per finding — append later rounds'
deferred findings to the same ticket. Title: `[follow-up] <PR title> —
deferred review findings`. Body: each finding pasted verbatim (reviewer's
exact comment text, file:line, thread URL), linked back to the PR. If
ticket filing fails (no ticketing system configured, or it's unreachable),
don't let that block the thread indefinitely — fall back to fixing the
finding inline instead. An optional dependency should never become
load-bearing for getting a PR unstuck.

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
free.

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
BRANCH="$(git symbolic-ref --short -q HEAD || true)"
if [ -z "$BRANCH" ]; then
  echo "HEAD is detached — this check assumes a checked-out branch matching the PR's remote head." >&2
  echo "Check out that branch explicitly before running it, rather than guessing at a ref." >&2
  exit 0  # deliberate: same "pause, don't fail the caller" contract as a detected collision
fi
FETCH_ERR="$(mktemp)"
trap 'rm -f "$FETCH_ERR"' EXIT
if ! git fetch origin "$BRANCH" --quiet 2>"$FETCH_ERR"; then
  echo "Could not fetch origin/${BRANCH} — remote state is unverifiable: $(cat "$FETCH_ERR")" >&2
  echo "Pausing rather than treating an unreachable remote as collision-free." >&2
  exit 0  # deliberate: same "pause, don't fail the caller" contract as a detected collision
fi
REMOTE_SHA="$(git rev-parse "origin/${BRANCH}" 2>/dev/null || echo "")"
# Compare by ancestry, not equality: HEAD is expected to run ahead of
# origin/$BRANCH once you've committed this round's local fixes but
# haven't pushed yet — that's normal mid-round state, not a collision.
# Only flag when origin/$BRANCH holds a commit your local history
# doesn't contain (i.e. it is NOT an ancestor of HEAD) — that's the
# actual signal that someone else pushed.
if [ -n "$REMOTE_SHA" ] && ! git merge-base --is-ancestor "$REMOTE_SHA" HEAD; then
  RECENT_AUTHOR="$(git log -1 --format='%an <%ae> at %ad' "$REMOTE_SHA" 2>/dev/null || echo "unknown")"
  echo "origin/${BRANCH} has a commit our local history doesn't contain (${RECENT_AUTHOR})." >&2
  echo "Possible concurrent session on this PR. Pausing — do not race it." >&2
  exit 0  # deliberate: pausing is not a failure, so a caller checking status sees success
fi
```

Deliberately stateless — no baseline file. A persisted "last known remote
SHA" sounds stronger, but it has to be scoped correctly (per-branch,
per-repo-checkout, per-work-session) to avoid becoming its own source of
false positives/negatives, and gets that scoping wrong in ways that are
easy to miss (a prior session's stale baseline outliving the session that
wrote it, a slash in the branch name breaking a naive filename). The
ancestry check above accepts one known gap in exchange for that
simplicity: it won't catch a force-push that moves `origin/$BRANCH`
*backward* to an ancestor of HEAD. That's a deliberately-adversarial
action against a PR branch, not an ordinary review-loop event, and this
fleet's own hygiene rules already discourage it ("Main moves only by PR
merge", branch isolation). If you suspect it happened, verify directly
(`gh pr view --json commits`, or compare `origin/$BRANCH`'s commit count
against what you expect) rather than trusting this heuristic alone.

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
collision-awareness check). Edit this file, not a per-repo copy.
