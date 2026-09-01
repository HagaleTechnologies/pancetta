# PAN-9 neural OSD soft-rank verdict

**Status:** TIER-2 A/B COMPLETE (2026-09-01) — DOCUMENTED NULL, see Branch B  
**Production default:** unchanged (`osd_depth: Some(0)`)

## What landed

- Schema-v2 capture with the exact Rust MRB permutation and per-bit syndrome counts.
- A 26-channel inference contract with a behavior-preserving zero-row migration.
- A T0/T1 corpus generator and grouped split keys.
- Differentiable expected-reprocessing-order training over the natural systematic-bit output
  indices consumed by Rust. The captured MRB permutation remains diagnostic data; using it to
  remap `[91]` targets would make the output contract circular and mismatch production indexing.
- Curated-corpus preflight plus independent depth/neural attribution controls.

This host (sophon) has the operator recording corpus and jt9 baseline cache; the A/B below ran
here per the sibling runbook.

## Decision rule

The candidate is eligible only when the depth-1 soft-rank arm, compared with the true depth-0
production baseline, has bootstrap recall `ci_low > 0`, no noise-tier false-positive regression,
and no elapsed-time gate regression. The depth-1 `|LLR|` arm attributes any gain to ordering rather
than merely enabling reprocessing. Offline metrics are model-selection evidence only.

## Branch A — ship recommendation

Not taken — see Branch B.

## Branch B — documented null (2026-09-01, superseding an earlier same-day pilot)

**Corpus:** T1 mining initially ran against `~/.pancetta/recordings` alone, immediately after
implementing the WAV-retention NAS-offload policy in the same session — most of the historical
local corpus had already moved to NAS, yielding only 194 trainable labels (see history below).
`batch103_neural_osd_corpus.rs`'s T1 tier was then extended to also scan the NAS offload share
(`/Users/Shared/maersk/offload/pancetta-wav-corpus`, `/mnt/maersk/...` on Linux) and to write only
OSD-recovered rows (previously every BP-attempted candidate was written regardless of recovery —
588 WAVs had produced 442k records for 194 labels, and the full NAS-scale equivalent exhausted
local disk before finishing). A `--max-wavs` cap was added and used deliberately (operator
direction: don't scan the entire multi-month NAS history) rather than running the corpus generator
unbounded — the run was stopped after ~3,776 WAVs, yielding **5,957 recovered records, 5,907
trainable labels**. Still roughly two orders of magnitude below the design's 10^5-10^6 target, but
enough for training to actually converge (see below) — a real, if partial, test of the hypothesis.

**Training:** `train_rank.py` on the 5,907-label corpus, `--seed 0` (default), 60 epochs, MPS
device. `selection_metric` dropped from 42.875 (epoch 1) to a minimum of 24.291 (epoch 51) and
stayed in that range — the model genuinely learned this time, unlike the 194-label pilot where it
never moved past initialization.

**Arms (hard-200 + hard-1000 + noise_1000, same host/cores, sequential):**

| Arm | Config | Elapsed (s) | noise FP |
|-----|--------|------------:|---------:|
| A | depth=0, neural=off | 1003.6 | 3 |
| B | depth=1, neural=off | 1000.3 | 4 |
| C | depth=1, neural=on, shipped weights (sha `28ad6537…`) | 1039.7 | 7 |
| D | depth=1, neural=on, candidate weights (sha `253b5560…`) | 1039.4 | 4 |

**D vs A** (depth-0 baseline): hard-200 rec Δ=+5 (95% CI [+1.0, +10.0], **significant**);
hard-1000 rec Δ=+30 (95% CI [+19.0, +43.0], **significant**). A real, substantial recall gain on
both tiers. **HARD GATE FAILURE: noise false positives 3 → 4 (+1).** Any increase disqualifies the
change outright per design spec §2 (D0), independent of the recall result.

**D vs B** (depth-only attribution control): hard-200 rec Δ=+4 (95% CI [-1.0, +10.0], not
significant); hard-1000 rec Δ=+26 (95% CI [+14.0, +39.0], **significant**). The hard-1000 gain
survives controlling for depth-1 reprocessing alone — real evidence of an ordering-specific
effect, not just "more reprocessing helps." hard-200 doesn't clear the bar on its own.

**D vs C** (shipped-weights attribution control — the decisive comparison): hard-200 rec Δ=+1
(95% CI [-2.0, +4.0], not significant); hard-1000 rec Δ=+8 (95% CI [-5.0, +21.0], not
significant). The candidate does **not** significantly outperform the already-shipped weights on
either tier — D and C are statistically indistinguishable at this sample size, even though both
clearly beat the depth-0 baseline (A).

**Elapsed gate:** passed in both measurable comparisons (D vs B +3.9%, D vs C ~0.0%, budget 20%).

**Verdict: FAIL**, but a substantive one. D vs A shows the soft-rank-ordered depth-1 reprocessing
is a real, significant improvement over the dormant depth-0 default — but that's true of the
already-shipped weights too (arm C), and D doesn't clear the required `ci_low > 0` bar against
that specific control (D vs C). Per the runbook, D vs B *and* D vs C both need to pass; D vs C
doesn't. The noise-FP regression (D vs A, and identically present in B and C — i.e., a property of
depth-1 itself, not of neural ordering) is an independent, separate disqualifier. `osd_depth`
stays `Some(0)`. The checkpoint (`training/neural_osd/rank_model.pt`, seed 0, sha `253b5560…`) is
kept as research evidence only, not exported into `pancetta-ft8/assets/`.

**What this null actually says:** neural-guided OSD ordering (this architecture, this training
objective) does not currently do better than the already-migrated/shipped weights — training a
fresh soft-rank candidate isn't buying anything the shipped model doesn't already have on this
corpus. It does *not* say the whole soft-rank approach is worthless (D clearly beats no-neural-
ordering-at-all). The noise-FP gate failure is orthogonal and appears tied to `osd_depth=1` itself
across every arm that ran at depth 1 (B: 4, C: 7, D: 4) vs depth 0 (A: 3) — worth its own
follow-up independent of the neural-weights question. A larger corpus (closer to the 10^5-10^6
target) is the natural next step to get real statistical power on the D-vs-C comparison
specifically, since that's the one still ambiguous rather than clearly failing.

### History: 2026-09-01 same-day pilot on a 194-label local-only corpus

Before the NAS-offload extension above, an earlier same-day run trained on 194 labels from 588
purely-local WAVs (most of the historical corpus had already migrated to NAS by then).
`selection_metric` never improved past epoch 1 (42.946 vs. 44.1-44.9 for the rest of training) —
the saved checkpoint was effectively the model's near-random initialization. D vs C showed a
significant hard-1000 *regression* (Δ=-20, 95% CI [-30, -11]) — an undertrained model losing to
the shipped weights, which is expected and uninformative, not a real test of the hypothesis. That
result is superseded by the run above and kept here only for the record.

## Prior evidence

W2.4 found depth 2 gained five hard-200 recalls (95% CI +1 to +10) but increased noise false
positives 1 to 12 and elapsed about 78%, so it was correctly declined. PAN-9 evaluates depth 1 and
adds the missing depth-only arm; it does not weaken those safety gates.
