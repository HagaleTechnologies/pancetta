# PR review convergence policy

## Decision

Every PR review round fixes P1 findings inline. Starting with the *second*
round, any P2-or-lower finding — new or resurfaced — is not fixed inline;
it is captured **verbatim** into a follow-up ticket instead, and the review
thread is resolved once the ticket exists. Round one is unrestricted: P1
and P2 alike get fixed inline on the first pass, since there's no
ticket-filing overhead worth paying before the PR has had even one look.

This applies to every review round on a PR — human reviewer, `/code-review`,
bot review (Codex, CodeRabbit, etc.), or the Catalyst `phase-review` /
`review-comments` pipeline — not just one mechanism.

## Why

Chasing progressively finer findings across review rounds burns real time
without moving correctness forward — a PR can cycle five-plus rounds over
formatting and naming preferences while review attention drifts away from
whatever actually blocks merge. Bounding what each *subsequent* round is
allowed to demand inline gives a PR a monotonically shrinking blocker
list — it converges toward mergeable instead of oscillating.

## Severity definitions

**P1 (fix every round):** correctness bugs, security vulnerabilities, data
loss/corruption, broken build or tests, spec/contract violations — anything
that would block merge on its own merits.

**P2-and-lower (ticket after round one):** style, naming, structure/
simplification preferences, unverified micro-optimizations, documentation
nits, subjective "nice to have" suggestions.

If a bot or reviewer tags explicit severity (critical/blocking vs.
minor/nit/suggestion), defer to that tag. Otherwise use judgment against
the P1 definition above — when genuinely unsure, treat it as P1 rather than
defer it; the failure mode to avoid is silently deferring a real bug.

## Mechanics

1. **Round 1** — address everything reasonable, P1 and P2 alike. No ticket
   filing yet.
2. **Round 2 onward** — for each finding:
   - P1 → fix inline as usual, no round cap.
   - P2-or-lower → do not fix inline. File (or append to) a single
     follow-up ticket for this PR, using the repo's normal ticket-authoring
     convention, with the finding pasted **verbatim** (reviewer's exact
     wording, file:line). Reply on the thread linking the ticket, then
     resolve the thread — it's tracked, not dropped.
3. One follow-up ticket per PR, not one per finding — append additional
   P2-or-lower findings surfaced in later rounds to that same ticket rather
   than opening a new one each round.

## What this does NOT do

- **Does not mean P2 findings get ignored.** They land in a real ticket,
  not silently dropped — the trade is "fixed later, deliberately" instead
  of "fixed now, chased indefinitely."
- **Does not relax P1 handling.** Every round still fixes every P1 finding
  it sees.
- **Does not apply to round one.** Convergence pressure only kicks in once
  a PR has already had one full look.
- **Does not override a human reviewer's explicit CHANGES_REQUESTED.** That
  still needs the reviewer's own sign-off, not just a filed ticket.

## Rollout

Adopted fleet-wide, 2026-08-07, after this exact pattern surfaced in a
coalesce-labs/catalyst PR review thread. Implemented in the Catalyst
orchestrator's `review-comments` and `phase-review` skills (round-tracking
plus follow-up-ticket filing) and stated as house policy in every
HagaleTechnologies repo's CLAUDE.md/AGENTS.md.
