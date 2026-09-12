---
name: catalyst-file-ticket
description: Shape a ticket around a scannable, testable use-case before it's filed — an outcome-first title plus tiered Gherkin acceptance criteria. Guidance only; this skill does not file anything itself.
---

# Catalyst File Ticket

Turn a vague, implementation-first idea ("add a retry to the sync job") into a ticket someone can scan, understand, and test against — *before* anyone writes code. This skill only shapes the ticket; filing it into wherever this team tracks work is a separate, explicit step.

## 1. Write an outcome-first title

Not "fix the sync job" or "add retry logic" — those describe an implementation, not a result. Use:

```
<actor> should <outcome> so that <benefit>
```

Examples:

- "A tenant admin should see a failed sync retried automatically, so that a transient network blip doesn't strand their data out of date."
- "An on-call engineer should get paged only once per incident, so that a flapping check doesn't spam the rotation."

If you can't fill in `<actor>`, `<outcome>`, and `<benefit>` from what you know, that's a sign the ticket needs more context before it's ready to write — not a sign to skip the exercise.

## 2. Write tiered Gherkin acceptance criteria

Structure the criteria in tiers, from the case that must obviously work to the cases that are easy to forget:

- **Tier 1 — happy path.** The one scenario that has to work for this to be worth shipping at all.
- **Tier 2 — edge cases.** Boundary conditions, empty/zero states, concurrent or repeated attempts.
- **Tier 3 — failure handling.** What happens when a dependency is down, a permission is missing, or the input is malformed — silent failure is itself a bug worth writing a scenario for.

Each scenario follows Given/When/Then:

```gherkin
Scenario: Transient sync failure recovers automatically
  Given the sync job failed with a retryable network error
  When the next scheduled sync attempt runs
  Then the sync succeeds without any manual intervention
  And no duplicate data is written
```

This applies even to backend bugs and chores, not just user-facing features — a chore still has an actor (often "the system" or "the next engineer who reads this code") and an outcome.

## 3. Before filing

Reread the title and criteria as someone who has never seen this conversation. If a scenario needs the ticket's discussion history to make sense, tighten the wording until it doesn't.
