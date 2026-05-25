---
slug: layered-bp
mode: ft8
state: graduated
created: 2026-05-25T19:00:00Z
last_updated: 2026-05-25T19:00:00Z
branch: iter/2026-05-25-batch-10
parent_hypothesis: hb-063
wild_card: false
scorecard: research/scorecards/history/2026-05-25-layered-bp.json
delta_vs_main: composite +0.001049 (0.555131 -> 0.556180); hard-200 +18 rec, hard-1000 +88 rec; -16% decode wall-clock
disposition: GRADUATE hb-063 — layered (row-sequential) BP is the production default. Biggest single-iter composite move since hb-038.
---

## Hypothesis

hb-063 (spawned from mr-003 academic-LDPC harvest 2026-05-25, ranked
#1): replace the decoder's **flooding** belief-propagation schedule
(all check nodes update from the previous sweep's messages, then all
variable nodes) with a **layered / row-sequential** schedule (update
one check node at a time and fold its new check-to-variable messages
into the variable posteriors immediately, so later checks in the same
sweep see fresher beliefs). Standard result (Hocevar 2004;
arXiv:2410.13131): ~2× convergence speed at the same frame-error
rate. Hypothesis: cut wall-clock and/or recover marginal decodes that
flooding misses within the iteration budget.

## Change

`pancetta-ft8/src/decoder.rs`:
- `Ft8Config::layered_bp: bool` (default flipped false → **true** on
  graduation).
- `LdpcDecoder.layered` field + `with_layered(bool)` builder; threaded
  through both the main decoder construction and the per-thread AP
  decoders (`ApDecodeContext.layered_bp`).
- New layered branch at the top of `belief_propagation_with_trajectory`:
  maintains a running posterior `total[]`, computes each check's
  extrinsics as `total[v] - c2v_old`, then for each outgoing edge
  updates `total[v] += new_msg - c2v_old` and stores the new message.
  Covers both SumProduct and MinSum. Early-terminates on syndrome=0 and
  records the per-sweep trajectory for the neural OSD exactly as
  flooding does.
- Unit test `test_ldpc_layered_bp_converges` (clean all-zero codeword
  stays converged; lightly corrupted codeword is corrected). Lib tests
  192 → 193.

Research `with_layered_bp` builder + `--layered-bp` eval flag for the
A/B.

## Result

### hard-200 controlled A/B (no filter, isolates the schedule)

| config            | recovered | novel | rate    | decode wall-clock |
|-------------------|----------:|------:|--------:|------------------:|
| flooding@100      |      4377 |  1787 | 0.51038 |              270s |
| layered@100       | 4395 (+18)| 1981  | 0.51248 |        226s (-16%)|
| layered@50        | 4385 (+8) | 1989  | 0.51131 |        223s (-17%)|

**layered@50 already beats flooding@100 on recall at half the
iterations** — direct confirmation of the ~2× convergence claim. The
wall-clock win comes from per-candidate early-termination firing
sooner (the 50- vs 100-iter cap rarely binds — most candidates
converge well under 50 sweeps in the layered schedule, which is why
layered@50 and layered@100 have near-identical wall-clock).
layered@100 is strictly best (max recall, same speed), so it is the
production config.

### full 5-tier, with production FP filter (the shipped reality)

| metric                     | flooding (old main) | layered (new) | Δ        |
|----------------------------|--------------------:|--------------:|---------:|
| **composite**              |            0.555131 |  **0.556180** |**+0.001049**|
| fixtures pass_rate         |                 1.0 |           1.0 |        0 |
| synth-clean @50/@90 dB     |             -20/-18 |       -20/-18 |        0 |
| hard-200 recovered         |                4376 |          4394 |      +18 |
| hard-200 novel (filtered)  |                 823 |           836 |      +13 |
| hard-1000 recovered        |               14267 |         14355 |      +88 |
| hard-1000 novel (filtered) |                2808 |          2849 |      +41 |
| wild-50                    |                   0 |             0 |        0 |

(Methodology: a flooding control on the composite tiers reproduced
the stored main.json composite to within float noise — 0.555189 vs
0.555131, the +1 recovered on hard-200 — confirming the invocation
matches and the +0.001049 is real, not a flag/filter artifact.)

## Why it wins

Layered BP converges to a valid codeword in fewer sweeps, so within
the same iteration budget it resolves marginal candidates that
flooding leaves one or two sweeps short of syndrome=0. That surfaces
+18 / +88 genuinely-recovered (jt9-matched) decodes on hard-200 /
hard-1000. The schedule also surfaces more *novel* candidates
(raw +194 on hard-200 no-filter) — the precision wall — **but the
production FP filter (hb-052/062) absorbs them**: filtered, the novel
cost is only +13 / +41, while every recovered decode survives (the
filter never drops a truth-matched callsign). Recall up, precision
essentially held — recall and precision moving the right way in the
same change, exactly as the batch-9 filter ship promised.

This is the first lever in many batches to add **real recall** rather
than just trade FPs; the precision wall finding from batches 2-8 was
"every candidate-admitting knob adds FPs with no recall" — layered BP
breaks that because it isn't admitting more candidates, it's
*decoding the existing ones better*.

## Decision

**GRADUATE.** `layered_bp` default true. main.json updated to the new
5-tier baseline (composite 0.556180). Scorecard archived to
history/2026-05-25-layered-bp.json.

## Learnings / follow-ups

- **layered@50 ≈ flooding@100** means there is now slack to cut
  `ldpc_iterations` for wall-clock if ever needed, at ~no recall cost
  — but since layered@100 is already as fast as layered@50 (early
  termination dominates) and has more recall, there's no reason to.
  Noted for the operator's MiniPC budget: headroom is comfortable.
- mr-003's #1 pick delivered. Next mr-003 candidates: hb-067 (mBP
  offset — soft win, decision pending), hb-065 (adaptive GE removal —
  needs an OSD profile first), hb-064 (DIA trajectory features —
  plan-sized).
- The neural OSD consumes the per-sweep trajectory; it was trained on
  flooding trajectories yet layered still nets positive, so the OSD is
  robust to the schedule change. A retrain on layered trajectories
  (hb-064 territory) could extract more — low priority.
</content>
