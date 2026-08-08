# PR review convergence policy

## Decision

Every PR review round fixes P1 findings inline. Starting with the *second*
round, any P2-or-lower finding that is genuinely new (or a previously
addressed one a reviewer reopens with fresh critique) is not fixed inline;
it is captured **verbatim** into a follow-up ticket instead, and the review
thread is resolved once the ticket exists. Round one is unrestricted: P1
and P2 alike get fixed inline on the first pass, since there's no
ticket-filing overhead worth paying before the PR has had even one look.

A P2 finding raised in round one that simply never got fixed (omitted, or
the fix didn't actually land) is still owed from round one — reappearing in
round two does not make it eligible for ticketing. Ticketing is for new
findings, not a way to let an incomplete round-one remediation dodge the
round it was actually due in.

This applies to every review round on a PR — human reviewer, `/code-review`,
bot review (Codex, CodeRabbit, etc.), or the Catalyst `phase-review` /
`review-comments` pipeline — not just one mechanism. "Round" is one shared
counter per PR, not one per reviewer: round N is the Nth time the PR
receives review feedback, from any source, that gets a push in response.
Different tooling may approximate that counter differently (e.g. the
Catalyst pipeline counts prior "address review comments" commits); the
invariant that matters is convergence — a shrinking blocker list — not
byte-exact synchronization across mechanisms.

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

Classify by substance against the P1 definition above; don't just trust a
reviewer's own label. Only defer to an explicit tag when its vocabulary
maps cleanly onto blocking-vs-not (e.g. `critical`/`blocking` → P1,
`minor`/`nit`/`suggestion` → P2). Common bot P0–P3 scales often do *not*
map cleanly this way — many tools (Codex included) use P2 for an ordinary,
still-real correctness defect, not merely a style nit. Treating every
bot-labeled "P2" as automatically deferrable would let a genuine bug slip
into a ticket instead of getting fixed. So: reconcile the label against the
finding's actual content, not just its number — a "P2" that is in fact a
correctness bug is P1 under this policy regardless of what the reviewer
called it. When genuinely unsure, treat it as P1 rather than defer it; the
failure mode to avoid is silently deferring a real bug.

## Mechanics

1. **Round 1** — address everything reasonable, P1 and P2 alike. No ticket
   filing yet.
2. **Round 2 onward** — for each finding:
   - P1 → fix inline as usual, no round cap.
   - Genuinely new (or reopened) P2-or-lower → do not fix inline. File (or
     append to) a single follow-up ticket for this PR, using the repo's
     normal ticket-authoring convention, with the finding pasted
     **verbatim** (reviewer's exact wording, file:line). Reply on the
     thread linking the ticket, then resolve the thread — it's tracked,
     not dropped.
   - A P2 carried over from round one that was never actually fixed → still
     owed inline; not eligible for ticketing (see Decision above).
3. One follow-up ticket per PR, not one per finding — append additional
   P2-or-lower findings surfaced in later rounds to that same ticket rather
   than opening a new one each round.
4. **Communication guardrails win.** If a repo's own operational guardrails
   restrict what an autonomous agent may communicate externally, reconcile
   this policy's actions against them before executing: filing a ticket in
   the repo's own tracker and replying on a PR review thread are
   development-workflow actions inside the repo's own tooling, not
   "outbound communication" in the sense guardrails like "no email/social/
   external posts" mean to restrict — but if a repo's guardrail is written
   broadly enough that this is genuinely ambiguous, don't guess: hand off
   to a human instead of auto-filing or auto-replying.

## What this does NOT do

- **Does not mean P2 findings get ignored.** They land in a real ticket,
  not silently dropped — the trade is "fixed later, deliberately" instead
  of "fixed now, chased indefinitely."
- **Does not relax P1 handling.** Every round still fixes every P1 finding
  it sees.
- **Does not apply to round one.** Convergence pressure only kicks in once
  a PR has already had one full look.
- **Does not let an incomplete round-one fix escape as a "ticket."** See
  Mechanics §2 — only genuinely new/reopened P2 findings get deferred.
- **Does not override a human reviewer's explicit CHANGES_REQUESTED.** That
  still needs the reviewer's own sign-off, not just a filed ticket.

## Rollout

Adopted fleet-wide, 2026-08-07, after this exact pattern surfaced in a
coalesce-labs/catalyst PR review thread. Implemented in the Catalyst
orchestrator's `review-comments` and `phase-review` skills (round-tracking
plus follow-up-ticket filing) and stated as house policy in every
HagaleTechnologies repo's CLAUDE.md/AGENTS.md. Where a repo's bot reviewer
(e.g. Codex) reads `AGENTS.md` specifically rather than `CLAUDE.md`, the
pointer is mirrored there too (or `AGENTS.md` is a symlink to `CLAUDE.md`,
per this fleet's standing convention) — a bot reviewer that never sees the
instructions can't follow them. Revised 2026-08-07 (same day) after the
policy's own first review round — on repos with a wiki, a pointer was also
added there per that repo's normal documentation convention.
