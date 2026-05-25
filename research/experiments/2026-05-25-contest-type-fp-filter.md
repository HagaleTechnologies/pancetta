---
slug: contest-type-fp-filter
mode: ft8
state: graduated
created: 2026-05-25T20:00:00Z
last_updated: 2026-05-25T20:00:00Z
branch: iter/2026-05-25-batch-10
parent_hypothesis: hb-058
wild_card: false
scorecard: n/a (precision-only; composite unchanged — novels don't enter the composite)
delta_vs_main: hard-200 no-filter -429 novel at +0 recall; filtered eval 0 delta (redundant with continuity filter); composite unchanged
disposition: GRADUATE a focused hb-058 — reject contest-only message types (RTTYRoundup/FieldDay/Contest) in is_plausible. /R and directional-CQ parts spawned as careful follow-ups.
---

## Hypothesis

hb-058 (mr-002 JTDX harvest 2026-05-25): JTDX added Feb-2022 commits to
"filter out /R false decodes", "better filtering ARRL Field Day
messages", and "filter out directional CQ false decodes". Port the
post-LDPC-CRC sanity rules to pancetta to cut false positives.

## Instrument before implement (mr-007)

pancetta already has a thorough `is_plausible` (telemetry + freetext
noise rejected, both-portable `/` rejected, payload validated) AND a
live callsign-continuity FP filter (hb-052/062). So I first measured
which novels actually fall into hb-058's categories, and whether
filtering them would cost real decodes. New tool:
`pancetta-research/examples/fp_format_audit.rs` (decode hard-200,
classify each decode novel-vs-recovered by message_type + suffix +
CQ-modifier).

hard-200 tally (recovered / novel):

| category      | recovered | novel | verdict                          |
|---------------|----------:|------:|----------------------------------|
| RTTYRoundup   |         0 |   335 | safe to reject                   |
| FieldDay      |         0 |    83 | safe to reject                   |
| Contest       |         0 |    15 | safe to reject                   |
| DXpedition    |         0 |    88 | **keep** — hunt target           |
| single /R     |         0 |   315 | recall risk on-air (rovers)      |
| directional CQ|        77 |    55 | **recall trap** — keep           |
| Standard      |      4685 |  1138 | continuity filter's job          |

## Change

`pancetta-ft8/src/message.rs::is_plausible`: reject RTTYRoundup
(i3=3), FieldDay (i3=0/n3=3,4), and Contest (i3=2, EU VHF)
unconditionally — same treatment, and same rationale, as the existing
unconditional `Telemetry` rejection: pancetta is a general / DX
station, not a contest logger, and these types are a disproportionate
CRC-14 collision source. **DXpedition is deliberately kept** (real
DXpeditions are pancetta's highest-value hunt target; its FPs are left
to the continuity filter). Test `plausible_rttyroundup_passes_*`
replaced by `contest_only_types_rejected` (asserts the three reject,
DXpedition still passes). Lib tests stay 193.

## Result

| eval mode              | recovered | novel        |
|------------------------|----------:|-------------:|
| no-filter, pre-hb-058  |      4395 |         1981 |
| no-filter, hb-058      |      4395 | 1552 (-429)  |
| with FP filter, base   |      4394 |          836 |
| with FP filter, hb-058 |      4394 |  836 (0)     |

- **-429 contest-type FP novels at exactly +0 recall** (no-filter,
  isolates the rule).
- **0 delta with the eval's strict jt9-baseline filter** — redundant
  there because those FP callsigns aren't in the baseline reference,
  so the continuity filter already drops them. Composite therefore
  unchanged; main.json not regenerated.

## Why graduate anyway (it's not redundant in production)

The eval's FP filter is strict-membership against 3083 jt9-baseline
callsigns. The **production** CallsignContinuityFilter (hb-062) uses
the operator's actual log + cqdx spots + a rolling window, and at
**cold-start (empty log) it runs in lenient mode** — letting more
decodes through by design. In that regime the message-type rejection
is the *primary* guard against contest-type FPs, not a redundant one.
It's also defense-in-depth for the case the continuity filter can't
catch: a contest-format FP whose callsign happens to be a genuinely
spotted/logged station. Zero recall cost, deterministic, consistent
with the existing Telemetry policy — a clean operational precision
win for Phase 5 (fewer fake QSO attempts on contest noise).

Composite doesn't measure novels, so this won't show in the headline
metric — exactly like hb-014 and the hb-052 filter ship. The win is
on-air precision.

## Decision

**GRADUATE** the contest-type rejection. The `/R` and directional-CQ
parts of the original hb-058 are NOT shipped (recall-risky) and are
spawned as careful follow-ups.

## Learnings / follow-ups

- **hb-071 (NEW, priority 0.30):** single `/R` suffix handling. 315
  hard-200 novels, 0 real *here*, but rovers (K1ABC/R) are legitimate
  rare on-air traffic and pancetta's hunt mode may want them. Don't
  blanket-reject; consider rejecting `/R` only where structurally
  invalid, or only under cold-start lenient mode. Low priority.
- **hb-072 (NEW, priority 0.30):** directional-CQ modifier whitelist.
  77 real vs 55 FP — a blanket filter costs recall. Reject only CQ
  with a modifier outside a validated set (DX, continents, CQ zones,
  POTA/SOTA, numeric). Needs the whitelist + care. Low priority.
- The `fp_format_audit.rs` tool is reusable for any future
  message-structure FP question.
- General lesson: with the continuity filter live, message-structure
  filters look redundant in the jt9-baseline eval but still matter at
  production cold-start. Eval-redundant ≠ production-redundant.
</content>
