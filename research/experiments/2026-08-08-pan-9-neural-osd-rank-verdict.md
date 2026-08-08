# PAN-9 neural OSD soft-rank verdict

**Status:** IMPLEMENTATION COMPLETE — OPERATOR T1 TRAINING AND TIER-2 A/B PENDING  
**Production default:** unchanged (`osd_depth: Some(0)`)

## What landed

- Schema-v2 capture with the exact Rust MRB permutation and per-bit syndrome counts.
- A 26-channel inference contract with a behavior-preserving zero-row migration.
- A T0/T1 corpus generator and grouped split keys.
- Differentiable expected-reprocessing-order training in the MRB basis.
- Curated-corpus preflight plus independent depth/neural attribution controls.

This host has no operator recording corpus or jt9 baseline cache. The production gate therefore
cannot be run here; `eval` now fails explicitly instead of turning missing truth into a plausible
0/0 recall result. Use the sibling A/B runbook on the operator machine.

## Decision rule

The candidate is eligible only when the depth-1 soft-rank arm, compared with the true depth-0
production baseline, has bootstrap recall `ci_low > 0`, no noise-tier false-positive regression,
and no elapsed-time gate regression. The depth-1 `|LLR|` arm attributes any gain to ordering rather
than merely enabling reprocessing. Offline metrics are model-selection evidence only.

## Branch A — ship recommendation (complete after a passing run)

Record hard-200/hard-1000 recall delta, 95% CI, novel delta, noise false positives, and elapsed
delta here. If every gate passes, recommend a follow-up ticket carrying those exact numbers to add
the configuration/rollout surface. Do not change the default in PAN-9.

## Branch B — documented null (complete after a non-passing run)

Record the same measurements and the binding failed gate here. Preserve the model/checkpoint only
as research evidence, keep `osd_depth: Some(0)`, and close PAN-9 as a measured null rather than
rationalizing an offline win. This is the expected disposition if the CI overlaps zero, any new
noise false positive appears, or elapsed regresses.

## Prior evidence

W2.4 found depth 2 gained five hard-200 recalls (95% CI +1 to +10) but increased noise false
positives 1 to 12 and elapsed about 78%, so it was correctly declined. PAN-9 evaluates depth 1 and
adds the missing depth-only arm; it does not weaken those safety gates.
