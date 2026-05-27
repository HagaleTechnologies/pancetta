---
slug: hb-072-cq-whitelist
mode: ft8
state: graduated
created: 2026-05-26T18:50:00Z
last_updated: 2026-05-26T18:50:00Z
branch: iter/2026-05-26-batch-12
parent_hypothesis: hb-072
wild_card: false
scorecard: research/scorecards/hb072-on.json (transient, removed)
delta_vs_main: hard-200 no-filter -24 novel at +0 recall; composite unchanged
disposition: GRADUATE hb-072 — directional-CQ modifier whitelist in is_plausible. Operational precision (cold-start defense), zero recall cost.
---

## Hypothesis

hb-072 (priority 0.30, spawned 2026-05-25 from hb-058): the directional-CQ
audit (fp_format_audit, batch 10) found 77 real "CQ <modifier>" vs 55
novel — a blanket reject of directional CQ would cost 77 real decodes,
but a *whitelist* of known-valid modifiers should split them cleanly.

## Change

`pancetta-ft8/src/message.rs`:
- New helper `is_valid_cq_modifier` — accepts a CQ message's
  `special_operation` only if it matches one of:
  - Named modifiers: `DX`, continents (NA/SA/EU/AS/AF/OC), `QRP`,
    program tags (POTA/SOTA/FD/RU/TEST).
  - Numeric CQ-zone / exchange (1–3 digits).
  - Short alpha prefix / state code (1–3 letters).
- `has_plausible_payload`'s `StandardMessageType::Cq` arm now invokes
  the helper on `self.special_operation`.
- Unit test `cq_modifier_whitelist` covers ~20 real modifiers and 6
  garbage tokens. Lib tests 195 → **196**.

## Result

hard-200 no-filter (isolates the rejection from the FP-filter overlap):

| config         | recovered | novel | rate    |
|----------------|----------:|------:|--------:|
| pre-hb-072     |      4431 |  1620 | 0.51667 |
| hb-072         |      4431 | 1596 (-24) | 0.51667 |

**−24 garbage-CQ-modifier novels at +0 recall.** Recall is preserved
exactly (4431 → 4431), so the whitelist is correctly comprehensive for
the real `CQ <modifier>` traffic on this corpus.

Composite is unchanged (novels don't enter the composite, and most of
the −24 are noise callsigns the production FP filter would also catch).
main.json's hard-200 filtered novel count (845) is now slightly
overstated by ~5-10 (the small subset of garbage-modifier CQs whose
callsigns *are* in the jt9-baseline reference and so escape the
continuity filter); too small to be worth a main.json refresh.

## Why graduate (same logic as hb-058)

The production FP filter (hb-052/062) catches noise-callsign FPs but
runs **lenient at cold-start** (empty operator log). hb-072 catches the
CQ-modifier FP subset *independent of the callsign reference* — so at
cold-start it's the primary defense against this FP class, and at
warm-state it's defense-in-depth for CQs from genuinely-spotted
callsigns sending garbage modifiers.

Mirrors hb-058's graduation pattern: precision-only, zero recall cost,
eval-redundant-but-production-meaningful.

## Decision

**GRADUATE.** Lands in the same `is_plausible` post-decode gate as the
existing telemetry / contest-type rejections.

## Learnings / follow-ups

- The hb-058 audit's "77 real / 55 FP" split was the right shape; a
  conservative whitelist (named + numeric ≤3 + alpha ≤3) catches the
  garbage without dropping any real modifier on this corpus.
- If a future corpus introduces a 4+ char modifier the whitelist
  doesn't know about (e.g., a new program tag), real decodes there
  would be dropped — note for hb-073 (real-Doppler corpus) and any
  contest-period capture: re-audit the modifier distribution.
- No new spawns. The /R-suffix part of hb-058 (hb-071) is still
  available if appetite ever returns; same caveats apply (the 315
  single-/R novels on hard-200 are all 0-real on THIS corpus but real
  rovers exist on-air).
