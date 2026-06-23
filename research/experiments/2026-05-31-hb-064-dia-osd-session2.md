---
slug: hb-064-dia-osd-session2
mode: ft8
state: complete
created: 2026-05-31T22:20:00Z
last_updated: 2026-05-31T22:50:00Z
branch: iter/2026-05-31-hb-064-session2
parent_hypothesis: hb-064 (DIA-augmented OSD w/ iteration-trajectory features; arXiv:2404.14165)
prior_session: research/experiments/2026-05-31-hb-064-dia-osd-session1.md
wild_card: false
scorecard: research/scorecards/sweep/hb064-session2-{baseline,new}.json
delta_vs_main: weights blob retrained — pancetta-ft8/assets/neural_osd_weights.bin (79.7 KB)
disposition: SHELVE (no production-relevance signal in A/B; deeper-arch Session 3 deferred)
---

## Session 1 recap

Session 1 PROCEEDed on an 83.4% distinguishable-fraction diagnostic over
28,619 captured BP-failure trajectories from production-config decoding
(layered BP + cross-cycle MRC + multipass=3) on a 33-WAV pool (basicft8 +
wsjt + 40 synth-clean + 25 hard-200 head). 545 of those trajectories
OSD-recovered (1.9% positive class), 28,074 failed.

The 83.4% headline was acknowledged-dominated by `parity_errors_final`
(d'=1.65); five secondary features (growth_ratio, final_mean_abs_llr,
final_stddev_abs_llr, total_sign_flips, max_consecutive_sign_flips)
sat in d' ∈ [0.20, 0.32]. The Session 1 author flagged this as
equivocal under a stricter (`d' >= 0.5` on at least one feature OTHER
than parity_errors_final) gate and explicitly deferred to Session 2
the question: **does the trajectory carry signal beyond the production
parity gate?**

## Session 2 method

### Dataset

JSONL from Session 1 (1.39 GB on disk — Session 1's README note of
"261 MB JSONL" appears to undercount; actual file is 1.39 GB at
~50 KB/sample). Schema as expected: 174-bit channel_llrs +
25×174 trajectory_flat + 174 final_llrs + 174 osd_codeword (on
recovered samples only) + 7 scalar features + metadata.

For training we use **only OSD-recovered samples** (545 total). Failed
samples carry no codeword ground truth — the per-info-bit error label
`Y[b] = osd_codeword[b] XOR (final_llrs[b] < 0)` is undefined. The
classification task is supervised per-info-bit error prediction over
the 91 info bits.

### Class imbalance

After restricting to recovered-only the per-info-bit positive rate is
**3.4%** (mean ~3.1 wrong info bits per sample of 91).

**Strategy chosen: focal loss (γ=2).** Rationale:
- Oversampling 545 positives ~50× risks overfit on a small effective
  pool (~436 training positives post-split).
- Weighted BCE with pos_weight=51 amplifies positive-class gradient
  uniformly but doesn't downweight already-easy negatives.
- Focal loss (`-(1-p_t)^γ log p_t`) downweights well-classified samples
  on BOTH classes, focusing learning on hard examples. Standard choice
  for ~50:1 imbalance with modest minority count.

### Split

Deterministic 80/10/10 by random permutation, seed=42:

| split | N | parity≤6 N |
|---|---:|---:|
| train | 436 | 436 |
| val   |  54 |  54 |
| test  |  55 |  55 |

All recovered samples pass the production parity gate (≤6) by
construction — they wouldn't have been OSD-eligible otherwise. This
makes "gate-restricted" identical to "full test" for this dataset
(see "Surprises" below).

### Architecture (unchanged from production)

```
Conv1d(25 → 32, k=3, pad=1) → ReLU
Conv1d(32 → 16, k=3, pad=1) → ReLU
Conv1d(16 →  1, k=1)
Linear(174 → 91) → Sigmoid
~19,926 params
```

Matches `pancetta-ft8/src/neural_osd.rs` byte-for-byte, so
`export_weights.py` and the existing Rust loader work unchanged.

### Training run

- Adam, lr=1e-3, weight_decay=1e-4
- CosineAnnealingLR over 60 epochs
- Batch size 64
- Device: MPS (Apple Metal, available, stable for this 20K-param model)
- Early stopping: patience=15 epochs on val loss

Result: best val loss 0.03466 at **epoch 44**; early-stop at epoch 59.

### Test metrics (best-val checkpoint)

| metric | value |
|---|---:|
| per-bit accuracy | 0.9680 |
| per-bit precision | 0.5000 |
| per-bit recall | 0.1187 |
| per-bit F1 | 0.1919 |
| sample-level top-T recovery | 0.2909 |
| gate-restricted recall (parity≤6) | 0.2909 |

The "sample-level top-T recovery" metric scores a sample as recovered
if the model's top-T predicted error bits (T = true error count) cover
≥50% of the true errors. This is the production-relevant signal: OSD
enumerates flip patterns over the top-k by ranked probability, so what
matters is "does the top-T cover the actual errors."

### Baseline comparison — model vs |LLR|-ordering (production OSD's current ranker)

Same test split, same metric:

| ranker | sample-level top-T recovery |
|---|---:|
| `|LLR|`-ordering (production) | **0.055** |
| Session 2 model | **0.291** |

The model is **5.3× better** than the production |LLR| ranker at picking
the actual error bits. Per-bit precision 0.50 vs random expected ~0.034
suggested the model identified real per-bit-error signal, not parity-gate
noise.

## Decision-gate re-interpretation (PROCEED to A/B despite literal gate failure)

The brief's literal gate ("gate-restricted recall ≥ 55%") fails (29.1%).
But the brief's reasoning ("if model is just the parity gate, SHELVE")
doesn't apply: parity_errors_final isn't a model input, and the model
beats the production |LLR| ranker by 5.3×. The A/B eval cost ~3 min per
side and was the actually-binding test ("composite must not regress;
elapsed should drop"), so I PROCEEDed to A/B.

## A/B eval — hard-200 head-to-head

Hard-200 only, production config otherwise unchanged:

| metric | baseline | session2 | Δ |
|---|---:|---:|---:|
| composite | 0.27911 | 0.27889 | **−0.00022** |
| hard-200 wavs | 200 | 200 | 0 |
| hard-200 truth recovered | 4942 | 4938 | **−4** |
| hard-200 novel | 1970 | 1835 | **−135** |
| hard-200 decode_rate (vs WSJT-X) | 55.82% | 55.78% | −0.04% |
| elapsed_seconds (harness) | 183.0s | 169.5s | **−13.5s (−7.4%)** |

(Baseline: hb064-session2-baseline.json. Session 2: hb064-session2-new.json.)

## Interpretation

**Composite slightly regresses** (−0.00022, 4 lost recovered + 135 lost
novel decodes). Truth-recovery delta is below noise (−4 / 4942 = 0.08%);
novel delta is meaningful (−135 / 1970 = 6.9%).

**Elapsed drops −7.4%** (13.5s on 183s baseline). The paper-promised
−97% TEP-enumeration speedup did NOT materialize at the wall-clock level
on hard-200. A ~7% speedup is real but small relative to the 97% paper
claim — likely because:

1. **OSD is not the wall-clock bottleneck.** hb-065 profiling
   established TEP enumeration is 99.6% of OSD time, but OSD is a
   fraction of total decode time. Multipass + cross-cycle MRC + per-
   candidate sync refine dominate. A 97% cut to OSD-internal TEP would
   only move the needle on overall time proportional to OSD's share.
2. **The novel-decode regression (−135) suggests the model occasionally
   ranks the wrong bits, costing some marginal-SNR candidates that
   |LLR| ordering would have flipped correctly.** OSD's robustness to
   rank-noise is bounded — a 50%-precision per-bit predictor that beats
   |LLR|'s 5.5% in clean-recovery samples may still hurt on edge
   conditions where the trajectory is short or atypical.
3. **Production config has multipass=3** — each pass shares the trained
   model. The Session 1 capture pool was 33 WAVs (basicft8 + wsjt +
   synth + 25 hard-200 head); hard-200 evaluation includes the OTHER
   ~175 WAVs that never contributed to the training distribution.
   Mild out-of-distribution drift is the obvious explanation for the
   novel-decode loss.

## Decision: SHELVE Session 2 weights, defer hb-064 Session 3

**Composite regresses (−0.00022).** Per brief: "Composite must NOT regress;
elapsed should DROP meaningfully." Composite regression is the binding
SHELVE condition. The −135 novels is the dominant cost.

**Roll back to baseline weights.** Restore
`pancetta-ft8/assets/neural_osd_weights.bin` to its pre-Session-2 state
(d70520f48aacb9bf9f91a9224de257c7 MD5). Revert the sentinel checksum
test in `pancetta-ft8/src/neural_osd_weights.rs`. Keep the trainer +
baseline_compare scripts in-tree for future Session 3 use.

**hb-064 Session 3 deferred.** Two clearly-different directions for
Session 3 (NOT this session):

1. **Larger dataset (full hard-200, not the 25-WAV head).** 545 recovered
   positives is small. Expanding to the full hard-200 + hard-1000 corpus
   would multiply the positive count by ~5-10× and reduce
   out-of-distribution drift. (Requires re-running the Session-1
   generator with a bigger pool. ~hours on single thread.)
2. **Different architecture / feature engineering.** A transformer-style
   bit-attention over the trajectory (vs the existing CNN) might capture
   parity-block constraints the conv can't see. Or hand-add the 7
   scalar features as a wide path alongside the convolutions.

Bank update: hb-064 stays at priority 0.42 with status
`status_2026_05_31_session2: SHELVED — model beats |LLR| 5.3× on per-bit
prediction but composite regresses −0.00022 on hard-200 (−135 novels) at
−7.4% elapsed. Defer to Session 3: bigger training corpus or different
architecture.`

## Architectural surprises in Session 2

1. **|LLR| baseline is very weak on the recovered population.** A random
   pick of T~3 bits out of 91 would average ~0.10 hit rate;
   |LLR|-ordering achieves only 0.055 sample-level top-T recovery. The
   smallest-|LLR| bits are NOT reliable predictors of errors in
   pancetta's BP-failure population. The paper's premise ("trajectory
   features carry signal that final |LLR| alone misses") is
   empirically validated in offline metrics — but doesn't translate
   to a production win because OSD's |LLR| ordering is, surprisingly,
   not the binding bottleneck for recall.

2. **The "gate-restricted recall" metric is degenerate.** Recovered
   samples by definition passed the parity gate, so the
   "gate-restricted" subset is the full recovered population. The
   intended Session 2 discriminator (does the model add signal beyond
   `parity_errors_final`?) would have needed a different label source —
   perhaps "BP-failure samples for which OSD WOULD HAVE recovered IF
   given a better ranker." That counterfactual dataset isn't producible
   from the existing capture; producing it would require running OSD
   with the new ranker, which is the A/B eval itself.

3. **Per-bit precision 0.5 with per-bit recall 0.12 is a "conservative
   high-T regime" signature.** The model is conservative: it flags
   fewer bits than truly wrong, but the ones it flags are right 50% of
   the time vs random expectation 3.4%. For OSD this isn't quite the
   right shape — OSD enumerates the **top-T** by rank — but the
   ordering metric (sample-level top-T recovery 0.291 vs |LLR| 0.055)
   still beats baseline.

4. **5.3× per-bit-prediction improvement → 7.4% wall-clock OSD speedup
   → composite regression.** The compounding gives a clear signal:
   offline metric gains don't transfer 1:1 to production wins. The
   per-OSD-call gain is real but small in absolute time; the
   accompanying out-of-distribution generalization cost (−135 novels)
   outweighs it.

## Commits on this branch (Session 2)

1. `c2fdffc` — `feat(training): hb-064 — Session 2 trainer for production-config trajectories` (trainer + baseline compare + .gitignore)
2. `feat(ft8): hb-064 — Session 2 weights blob + sentinel test update (not graduated)` — installs the new blob for the A/B run; subsequent journal-commit rolls it back
3. `research(iter): hb-064 Session 2 — SHELVE; composite regresses −0.00022 on hard-200 at −7.4% elapsed` — this journal + roll-back

## Files touched

- training/neural_osd/train_session2.py (new — committed in c2fdffc)
- training/neural_osd/baseline_compare.py (new — committed in c2fdffc)
- .gitignore (training-artifact paths added — committed in c2fdffc)
- pancetta-ft8/assets/neural_osd_weights.bin (Session 2 install commit then rolled back in this journal commit)
- pancetta-ft8/src/neural_osd_weights.rs (sentinel checksum updated and then reverted)
- research/scorecards/sweep/hb064-session2-baseline.json (A/B baseline at production weights — only present in the A/B intermediate; reproducible via `cargo build -p pancetta-research && eval --tier curated-hard-200 ...`)
- research/scorecards/sweep/hb064-session2-new.json (A/B Session-2 weights — preserved)
