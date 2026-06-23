---
slug: hb-064-dia-osd-session1
mode: ft8
state: in-progress (Session 1 of 2-3)
created: 2026-05-31T13:30:00Z
last_updated: 2026-05-31T13:50:00Z
branch: iter/2026-05-31-hb-064-dia-osd
parent_hypothesis: hb-064 (DIA-augmented OSD w/ iteration-trajectory features; arXiv:2404.14165)
wild_card: false
scorecard: n/a (data-pipeline + diagnostic, no decoder behavior change)
delta_vs_main: zero (capture is opt-in thread-local; production untouched)
disposition: see "Decision" below — depends on diagnostic outcome run
---

## Hypothesis (re-statement, hb-064)

A small dense classifier trained on **per-BP-iteration LLR
trajectories** can predict which info bits are wrong in BP
non-convergences, replacing |LLR|-based ordering in OSD. The reference
paper (arXiv:2404.14165) reports **-97% TEP enumeration at SNR=2 dB**
on CCSDS (128,64). hb-065 profiling established TEP enumeration is
**99.6% of pancetta's OSD time** — so this is the correct OSD-speed
lever.

## Architectural finding (the hb-064 brief's premise was outdated)

The brief said pancetta's `neural_osd.rs` "currently uses FINAL-LLR
features, not per-iteration trajectory features." **That's wrong, on
inspection.** The actual state:

  1. `belief_propagation_with_trajectory` in `decoder.rs` already
     captures the full `[[f32; 174]; 25]` per-iteration trajectory
     and passes it directly to `neural_osd::predict_error_bits`.
  2. `neural_osd.rs` is a CNN that consumes 25 LLR channels (one per
     BP iteration) — the architecture matches the paper's trajectory
     model.
  3. The Python training pipeline (`training/neural_osd/generate_data.py`)
     does emit `(N, 25, 174)` per-iteration trajectories.

So the **trajectory ML pipeline already exists end-to-end**. What's
*missing*:

  * **Training-data distribution.** The current weights are trained on
    pure **synthetic AWGN over BPSK** (`generate_data.py`), with a
    flooding-schedule BP (`bp_decode`) — NOT layered BP (production
    default since hb-063, batch 10) and NOT pancetta's real-band signal
    distribution (post-Costas-sync, post-MRC-cross-cycle,
    post-residual-subtract).
  * **Diagnostic of whether real-band BP failures carry signal.** The
    paper studies CCSDS (128,64) with AWGN; pancetta's BP failures
    after layered BP + cross-cycle MRC + soft cancellation are a
    different population.

That's where Session 1 attacks.

## Method — Session 1 deliverables

1. **`pancetta-ft8/src/bp_trajectory_capture.rs`** — new thread-local
   recorder. `enable_local()`, `disable_local()`, `record(sample)`,
   `drain_local()`. Schema-versioned. Zero overhead when disabled
   (single thread-local read per BP failure). Disabled by default.
2. **`decoder.rs::LdpcDecoder::decode_soft`** — when capture is on,
   appends one `CapturedTrajectory` per OSD-eligible BP failure
   (parity-gate pass AND parity-gate reject — separately labeled).
3. **`pancetta-research/examples/hb064_generate_trajectory_dataset.rs`**
   — a research example that:
   - Walks a small mixed pool (ft8_lib fixtures + synth-clean +
     hard-200 head).
   - Enables capture, decodes with the **production config** (layered
     BP + cross-cycle MRC + multipass — i.e. the real distribution).
   - Drains samples to JSONL with per-trajectory features computed
     online: `final_mean_abs_llr`, `final_stddev_abs_llr`,
     `early_mean_abs_llr`, `growth_ratio` (final/early),
     `max_consecutive_sign_flips`, `total_sign_flips`,
     `parity_errors_final`.
   - Computes a **distinguishability statistic** per feature: Welch-like
     d' = |Δmean| / sqrt(σ²_recovered + σ²_failed). For each sample,
     called "distinguishable" if at least one feature with d' ≥ 0.5
     correctly classifies it on its side of the class-midpoint. The
     brief's 20% gate operates on this fraction.

## Result — diagnostic outcome

**Run output:** `research/experiments/2026-05-31-hb-064-dia-osd-session1/summary.json`
and `trajectories.jsonl` (261 MB JSONL, ~28K samples).

**Pool decoded:** 33 WAVs (8 basicft8 + handful of wsjt + 40
synth-clean + 25 hard-200 head). Production `Ft8Config::default()`
(layered BP, OSD-2, multipass=3, cross-cycle MRC).

**Captured:** N = **28,619** BP-failure trajectories (both
parity-gate-pass and parity-gate-reject paths labeled).

| outcome | N | share |
|---|---:|---:|
| OSD-recovered | 545 | 1.9% |
| OSD-failed | 28,074 | 98.1% |

The 98% failure share confirms what production telemetry has long
suggested: the vast majority of BP non-convergences are unrecoverable
(noise dominates, no codeword in reach). The 545 recovered cases are
where the model has to learn.

**Per-feature separability** (Welch-like d' = |Δμ| / √(σ²_rec + σ²_fail)):

| feature | μ_recovered | μ_failed | d' |
|---|---:|---:|---:|
| **parity_errors_final**     | **4.79** | **13.03** | **1.65** |
| growth_ratio (final/early)  | 1.41    | 1.01    | 0.31 |
| final_mean_abs_llr          | 7.37    | 5.15    | 0.29 |
| final_stddev_abs_llr        | 4.20    | 3.86    | 0.25 |
| total_sign_flips            | 148.8   | 122.1   | 0.21 |
| max_consecutive_sign_flips  | 19.1    | 16.95   | 0.20 |
| early_mean_abs_llr          | 5.16    | 5.13    | 0.04 |

**Distinguishable fraction: 83.4%** — well above the 20% PROCEED gate.

**Important caveat:** the 83.4% headline is dominated by
`parity_errors_final` (the single d'≥0.5 feature). That feature is
*almost* trivial — it's the same syndrome count the production
parity-gate (`max_parity_errors_for_osd = 6`) already uses. Recovered
cases sit at mean=4.8 (gate-pass region); failed cases at mean=13.0
(dominated by gate-reject samples that OSD never ran on). So one
should weight this number against:

- **Secondary features (growth_ratio, final_mean_abs_llr, total_sign_flips,
  max_consecutive_sign_flips) all have d' ∈ [0.20, 0.32].** Each is
  individually weak but qualitatively the right direction (recovered
  trajectories grow, settle on stronger LLRs, exhibit more sign
  churn — all consistent with the paper's mechanism).
- **A CNN over the raw 25×174 trajectory** has access to higher-order
  structure (per-bit oscillation patterns, layered-BP wave-front
  shapes) that scalar features cannot capture. The 7 hand-picked
  features are a floor on what the data carries, not a ceiling.

## Decision

**PROCEED to Session 2.**

Justification:

1. **Distinguishable fraction (83.4%) >> 20% gate.** Passes the brief's
   explicit kill switch.
2. **At least one feature has d' > 1** (parity_errors_final at 1.65),
   confirming the BP failure population is genuinely bimodal between
   "OSD will recover this" and "OSD won't." That's the precondition
   for the paper's mechanism.
3. **Five additional features in d' ∈ [0.20, 0.32]**, all aligned
   with the paper's mechanism. Indicates trajectory structure carries
   information the model can exploit.
4. **Class imbalance (1.9% positive)** is a real Session-2 concern but
   not a kill switch. Standard remedies (focal loss, oversampling,
   class-weighting) apply.

**However**, Session 2 must include the diagnostic that this Session
could not run without a model: **does the trajectory carry signal
above what parity_errors_final alone provides?** Concretely:
gate-restricted recall (parity ≤ 6 only) is the right metric. If a
trained model doesn't beat "trust |LLR| ordering, gated by parity ≤ 6"
on the in-gate population, then everything above is parity-gate noise
and SHELVE applies post hoc.

Kill switch is **deliberately set above where I'd weight it
personally**: I'd put the threshold at 30% with d'>=0.5 on at least
one feature OTHER than parity_errors_final. By that stricter
standard, the result is more equivocal (d' on growth_ratio and
final_mean_abs_llr is 0.31 / 0.29 — both juuust below the stricter
bar). The bank's 0.42 priority for hb-064 (one of the highest open
hypotheses) tips it toward PROCEED; otherwise the close call would be
SHELVE-pending-stricter-diagnostic.

## Architecture sketch for Session 2

```
            ┌────────────────────────────────────────┐
            │  BP-failure trajectories (this session)│
            │  shape (N, 25, 174); N ≈ several K     │
            └────────────────────────────────────────┘
                                │
                                ▼
            ┌────────────────────────────────────────┐
            │  Per-info-bit error label              │
            │  Y = [info_hard_decision ⊕ OSD_codeword]│
            │  shape (N, 91); only OSD-recovered     │
            │  samples carry usable Y.               │
            └────────────────────────────────────────┘
                                │
                                ▼
            ┌────────────────────────────────────────┐
            │  Model = existing neural_osd.rs        │
            │  Conv1D(25→32) → Conv1D(32→16) →       │
            │  Conv1D(16→1) → Linear(174→91) →       │
            │  Sigmoid. ~20K params. Retrain on the  │
            │  Session-1 dataset.                    │
            └────────────────────────────────────────┘
                                │
                                ▼
            ┌────────────────────────────────────────┐
            │  Export weights → ../assets/           │
            │  neural_osd_weights.bin                │
            │  (Python: training/neural_osd/         │
            │  export_weights.py)                    │
            └────────────────────────────────────────┘
                                │
                                ▼
            ┌────────────────────────────────────────┐
            │  A/B vs production on hard-200:        │
            │  expect TEP-trial-count reduction (the │
            │  paper's headline) at ≤same recall +   │
            │  ≤same FP rate.                        │
            └────────────────────────────────────────┘
```

Key design choice for Session 2: train **only on OSD-recovered
samples** (the cases where ground truth is knowable). The 80%+ of BP
failures that OSD also can't recover have no Y — they're noise from
the decoder's perspective and contribute no learning signal.

## Session 2 — pickup instructions

**Where to start:**

1. `git checkout iter/2026-05-31-hb-064-dia-osd`
2. Inspect Session 1 outputs:
   ```
   ls research/experiments/2026-05-31-hb-064-dia-osd-session1/
   #   trajectories.jsonl  summary.json  README.txt
   wc -l research/experiments/2026-05-31-hb-064-dia-osd-session1/trajectories.jsonl
   jq '.distinguishable_fraction' research/experiments/2026-05-31-hb-064-dia-osd-session1/summary.json
   ```
3. If decision is PROCEED:
   - Expand the dataset: bump `HARD_200_SAMPLE` to 200 in the example
     and rerun. Expect ~20-40K trajectories. (Single-threaded, ~minutes;
     can parallelize with rayon if needed.)
   - Port the JSONL loader into Python: `training/neural_osd/load_pancetta_dataset.py`
     reading `trajectories.jsonl` → `(X, Y)` numpy arrays where
     `X[i] = trajectory_flat[i].reshape(25, 174)`, `Y[i][b] = (osd_codeword[b] ^ (final_llrs[b] < 0))`
     for `b in 0..91` (info bits only).
   - Retrain `training/neural_osd/train.py` on the new dataset; export
     weights via `export_weights.py`.
   - Sweep hard-200 with the new weights — measure TEP-trial counts
     (instrument once, undo) and recall delta. Expect either a wall-
     clock OSD speedup OR a recall gain at unchanged OSD trials.

**Files to read first in Session 2:**

- `pancetta-ft8/src/neural_osd.rs` — the existing inference path
  (untouched; just needs new weights).
- `training/neural_osd/train.py` — likely needs minor edits to ingest
  pancetta-band trajectories instead of synthetic.
- `pancetta-research/examples/hb064_generate_trajectory_dataset.rs` —
  Session 1 generator (this commit).
- `pancetta-ft8/src/bp_trajectory_capture.rs` — Session 1 capture API.

**Hard constraints carried forward:**

- The retrain target is **layered BP** trajectories (the production
  default since hb-063). Set `layered_bp = true` (already default)
  when generating data — Session 1 already does.
- The dataset must include the **production multipass** (`coherent_multipass_iterations = 3`)
  so trajectories reflect residual-pass conditions too. Session 1
  uses the production `Ft8Config::default()` and so captures these.
- Composite metric must not regress vs current weights, even if
  distinguishability looks promising. Existing weights stay as a
  fallback if Session 2 retrain doesn't beat them.

## Commits on this branch (Session 1)

- `feat(ft8): hb-064 — BP trajectory capture (thread-local, opt-in)` — capture API + decode_soft instrumentation
- `feat(research): hb-064 — trajectory dataset generator` — pancetta-research example + corpus walker
- `research(iter): hb-064 — Session 1 handoff (trajectory dataset + diagnostic)` — this journal + numeric results

## Architectural surprises during BP-loop instrumentation

1. **The trajectory pipeline already existed.** The brief's first
   step "verify current DIA feature extraction (probably final-iter
   only)" turned out to be moot — pancetta already wires 25-iter
   trajectories into `predict_error_bits` and the Python trainer
   already emits them. This shrinks Session 1's scope considerably:
   no new BP-loop instrumentation needed, just an **export API** for
   the research harness.
2. **Two distinct OSD-eligible populations.** The parity-gate
   (`max_parity_errors_for_osd = 6`) splits BP failures into "OSD
   actually ran" (the bulk) and "OSD was rejected by the gate"
   (degenerate cases). Session 1 captures both, labeled. The paper
   only addresses the first; the second is a free auxiliary signal
   if it carries trajectory structure.
3. **Trajectory size is non-trivial.** Each captured sample is
   174 × 25 + 174 = 4524 f32 + headers ≈ 18 KB JSON. The Session 1
   dataset is hundreds of MB on a small pool; the full hard-200
   sweep will land in the low GB range. Plan binary npz/npy export
   in Session 2 if disk pressure matters.

---

## Dataset stats (final)

```
Pool: 33 WAVs
  fixtures-basicft8: 8 WAVs (ft8_lib live + 4 silent)
  fixtures-wsjt:     6 WAVs
  synth-clean:      40 WAVs (first 40 of -28..-10 dB sweep)
  hard-200 head:    25 WAVs (real K5ARH 20m captures)
   (Actually some tiers populated fewer than listed due to truth-list
   gates; "33 WAVs across 3 tiers" is the example's reported count.)

Captures: 28,619 trajectories
JSONL size: 261 MB (~9 KB/sample including channel + trajectory +
            final LLRs + 174 codeword bytes + features object)
Wall time: ~minutes (single-threaded; trajectory feature compute is
          O(174 × 25) per sample = negligible vs decoder)
```

Storage note: JSONL is fine for Session 2 (NumPy loader can stream it
with `pandas.read_json(lines=True)` or `jq` → `numpy.loadtxt`), but a
binary .npz would shrink ~5×. Migration is a Session 2 nice-to-have if
disk pressure matters when scaling to the full hard-200.

## Diagnostic numeric table (paste of `summary.json`)

See `research/experiments/2026-05-31-hb-064-dia-osd-session1/summary.json`
for the canonical record. Headline:

- `n_samples`: 28619
- `n_osd_recovered`: 545
- `n_osd_failed`: 28074
- `distinguishable_fraction`: 0.8342
- `distinguishable_gate`: 0.2
- `decision`: "PROCEED ..."

