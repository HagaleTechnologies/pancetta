# Neural OSD — design for the next iteration

Design doc for the next neural-OSD model in `training/neural_osd/`. **This is a
plan, not an implementation** — it stops at a validated Python training reference,
an eval harness, and a drop-in integration spec. No Rust inference is wired here
(the hook already exists — see §5).

## 0. Where this stands (read first)

Neural OSD is **already shipped and live**, not greenfield:

- **Model (DIA):** a ~19,926-param CNN — `Conv1d(25→32,k3)→ReLU→Conv1d(32→16,k3)→
  ReLU→Conv1d(16→1,k1)→Linear(174→91)→sigmoid`. Input = the **25-iteration BP LLR
  trajectory** `[25 × 174]`; output = `[91]` per-info-bit "probability this bit is
  wrong."
- **Rust inference is done and default-on:** `pancetta-ft8` ships the
  `neural_osd` feature *in its default set*; `assets/neural_osd_weights.bin`
  (79,704 B packed f32) is `include_bytes!`'d by `neural_osd_weights.rs` and run by
  the hand-rolled forward pass in `neural_osd.rs::predict_error_bits`, feeding
  `osd.decode_with_features(llrs, neural_ordering: Option<&[f32;91]>)`
  (`decoder.rs:10129`). The `[91]` vector reorders which info positions OSD flips
  first (highest predicted-error-prob → tried first), which is documented to cut
  OSD-3 enumeration from ~125K trials to ~200.
- **But it is effectively dormant in production**, because production runs
  `osd_depth = Some(0)` (`docs/gap-analysis.md`): OSD-0 is a single hard-decision
  trial with **no reprocessing loop to reorder**. The neural ordering only bites at
  `osd_depth ≥ 1`. And `osd_depth ≥ 2` was shelved (Batch 73) for adding ~7,000
  false positives.

**Why every improvement since shipping has been shelved** (hb-064 S2/S3, hb-194
ensembles, model-soup):
1. **Objective mismatch.** All attempts trained per-bit classifiers (BCE, then
   focal γ=2) and selected on offline per-bit / sample-recovery metrics. Those
   **do not track production decode-rate** — the ranker is one gate inside a
   multipass/MRC pipeline, and offline winners lost the hard-200 A/B (Session 2:
   +5.3× offline sample-recovery but −135 novels / composite −0.00022; hb-194
   ensemble: +55% offline but production Δ −7 recall, not significant, +82% elapsed).
2. **Tiny labeled corpus + OOD drift.** ~545–2,029 recovered-sample labels from
   ~33 WAVs; models overfit that pool and drift on real bands.
3. **Architecture never changed** — it's the one lever never pulled.

**Thesis of this design: the model is not the bottleneck; the objective and the
data are.** Retarget training from per-bit classification to a **ranking objective
that directly minimizes OSD reprocessing order to the true codeword**, scale and
harden the corpus (hard-case mining from the `docs/gap-analysis.md` miss set +
production trajectory capture), and **gate exclusively on the production A/B**
(hard-200/1000 decode-rate with bootstrap CI + an elapsed-time hard gate), never on
offline bit-accuracy. Keep the model tiny and the I/O contract intact so it drops
into the existing Rust path with zero new inference code.

## 1. Problem framing

**Classic OSD** (`osd.rs`) works in the Most-Reliable Basis: sort the 174 codeword
bits by reliability, Gaussian-eliminate to make the 91 most-reliable independent
positions systematic, hard-decide them (OSD-0), then **reprocess** — flip the
least-reliable systematic positions in combinations of increasing order (OSD-1 =
singles, OSD-2 = pairs, OSD-3 = triples), re-encode, and accept the first candidate
that passes CRC-14. The reprocessing loop enumerates flips **in the reliability
order**; a better order finds the true codeword in **fewer trials** (early-exit),
which both cuts latency *and* reduces exposure to CRC-14 collision false positives
(every wrong trial is a chance to accept a bogus codeword).

**The model's job** is to predict a better reprocessing order than `|LLR|` from the
soft decoder state, so the true error pattern is enumerated early.

- **Input features.** Primary: the existing `[25 × 174]` BP LLR trajectory (keeps
  the shipped I/O contract). Add two cheap, information-dense channels shown to
  matter for MRB error location, concatenated as extra "rows" so the tensor becomes
  `[25 + C, 174]` (or a documented new contract):
  - **final syndrome** `s = H · hard(LLR)` broadcast per variable node as the count
    of unsatisfied checks each bit participates in (a per-bit "how implicated am I
    in parity failures" feature) — this is the single most direct error-location
    signal and is absent from the current input.
  - **per-bit |LLR| at the final iteration** and **LLR sign-flip count across the
    25 iterations** (instability = unreliable). These are trajectory-derivable but
    giving them as explicit channels helps a small model.
  - *Ablate each channel* — the syndrome channel is the hypothesis most likely to
    break the plateau; report its marginal contribution.
- **Output.** Unchanged interface: `[91]` real-valued **priority scores** over the
  info positions (higher = flip earlier). The Rust hook already consumes exactly
  this as `neural_ordering`. Semantically we reinterpret it as a *ranking* score,
  not a calibrated probability — calibration is irrelevant to OSD, only the induced
  order matters.
- **Loss — the core change.** Replace BCE/focal (per-bit, order-agnostic) with a
  **ranking loss that rewards putting the true error bits ahead of the correct
  bits**, i.e. minimize the rank at which OSD reaches the true error pattern. Two
  concrete, differentiable surrogates, in preference order:
  1. **Expected-reprocessing-order (soft-rank) loss.** Let `y ∈ {0,1}^91` be the
     true error pattern (natural systematic info-bit indices wrong in the OSD-0
     hard decision — see §2). Compute a differentiable **soft rank** of each info
     position from the predicted scores (e.g. `softrank_i = Σ_j σ((s_j − s_i)/τ)`,
     the smooth count of positions scored above `i`). The loss is the sum of soft
     ranks of the **true-error** positions:
     `L = Σ_i y_i · softrank_i / (Σ_i y_i)`. Minimizing it drives every true-error
     bit toward the top of the order — the exact quantity OSD pays for (mean trials
     to solution ≈ the max rank among the `|y|` error bits). This loss **is the
     production metric's differentiable surrogate**, which is the whole point.
  2. **ListMLE / pairwise-margin fallback.** If soft-rank is unstable, use a
     pairwise hinge `Σ_{i∈err, j∉err} max(0, m − (s_i − s_j))` (push every error bit
     above every non-error bit by margin `m`). Simpler, well-behaved, same intent.
  - **Order-weighting.** OSD-w only needs the `w` error bits in the top enumerated
    set, and low-weight patterns dominate (most failures are 1–3 bit errors after
    BP). Weight the loss by `1/|y|` (above) and optionally curriculum from
    low-weight to high-weight patterns.
  - Keep a **small BCE auxiliary term** (λ≈0.1) only as a regularizer / warm-start;
    the ranking term is primary.

## 2. Training-data synthesis

The label is **which natural systematic info bits are wrong in the OSD-0 hard
decision**. This matches the existing `[91]` inference contract exactly:
`neural_ordering[i]` is consumed as the score for original info bit `i` before
the MRB is constructed. Training output slot `i` as MRB slot `i` would be both
circular (the predicted scores help determine that MRB) and incorrect at the
Rust call site. Schema-v2 still records the exact Rust MRB permutation as
diagnostic/provenance data, but it is not an output-index remapping. Two
complementary sources:

- **A. Synthetic sweep (scale + coverage).** Extend `generate_data.py`:
  - **Fix the SNR bug first:** the README documents `−28…−18 dB` but the code
    default is `(5.0, 14.0)` Eb/N0 — an entirely different regime. FT8 operates at
    roughly `−21…−5 dB` SNR-in-2500Hz; pin the sweep to the FT8-realistic band and
    document the Eb/N0↔SNR mapping. The current model may be trained on the wrong
    regime.
  - Pipeline stays: random 91 info bits → systematic encode → BPSK → AWGN → 25-iter
    sum-product BP (record trajectory) → **keep only BP failures** (converged frames
    never reach OSD). Run the real Rust OSD path and retain its `final_perm` for
    diagnostics, while labeling the stable output contract as
    `y_i = (osd0_hard[i] != true_info[i])` for natural systematic indices. Keeping this mapping
    identical across training and inference is the data-level fix for output-contract skew.
  - Sweep SNR **importance-weighted toward the marginal band** where OSD actually
    fires (BP-fails-but-recoverable), not uniform — most uniform samples are either
    trivially converged or hopeless.
- **B. Real production capture (distribution match).** The `hb-064` hook already
  records `(trajectory, OSD outcome)` from real frames when capture is enabled
  (`decoder.rs`, via `pancetta-research/examples/hb064_generate_trajectory_dataset_s3.rs`
  → `trajectories.jsonl`). This is the *only* source whose input distribution
  matches production. **The stall was caused by having too little of it (~33 WAVs).**
  Scale it: run capture across the full `~/.pancetta/recordings` corpus (6,400+ WAVs,
  many bands/times) and the curated hard tiers, to get 10⁵–10⁶ real failed-BP
  trajectories with OSD-recovered labels.
- **C. Hard-case mining from the gap-analysis miss set.** `docs/gap-analysis.md`
  identified 637 standard messages ft8_lib decodes and the native decoder misses,
  concentrated at −20…−10 dB — but that analysis showed they are mostly *sync*
  misses (no candidate generated), so **most are not OSD-reachable**. The genuinely
  OSD-relevant hard cases are the subset where a *candidate exists and BP fails but
  the codeword is recoverable*. Mine those specifically: for each miss-set WAV, dump
  the candidates whose parity-error count is within the OSD gate
  (`≤ max_parity_errors_for_osd`) and capture their trajectories + brute-force /
  deep-OSD-recovered labels as a **hard-negative-weighted** slice of the training
  set. This is the highest-value data, and the smallest — use it to *weight*, not to
  train alone (it overfits, per S2).
- **Split discipline:** split by **WAV/band/time**, never by sample, so val/test
  measure generalization to unseen conditions (the S2 OOD-drift failure was a
  sample-level split leaking WAV-correlated structure).

## 3. Architecture options + budget

Keep the model **tiny** — the whole value is cutting OSD trials, so inference must be
negligible against the 15 s slot, and the weights ship as an `include_bytes!` blob.

- **Option A — 1D-CNN (keep the existing topology), new objective.** The shipped
  `[25×174]→[91]` CNN, retrained with the §1 ranking loss and §2 data. **Lowest
  risk, zero integration cost** (same TENSOR_ORDER, same blob, same forward pass).
  *Try this first* — the hypothesis is that the objective+data, not the topology,
  were the problem. Param budget ~20K; forward pass is already the shipped ~sub-ms.
- **Option B — small self-attention over positions (the never-pulled lever).** A
  1–2 layer Transformer encoder over the 174 positions (each position a token whose
  features are the 25-iter trajectory + syndrome channels), `d_model≈32`, 2 heads →
  linear head to `[91]`. Motivation: OSD errors have **cross-bit dependencies** (the
  parity structure couples positions); attention can model "these two marginal bits
  are jointly implicated by the same failed checks," which convolution over the
  arbitrary bit index cannot. **Param budget ≤ ~60K** to keep the blob small and
  inference well under a few ms (a hand-rolled Rust attention forward is feasible at
  this size, like the existing hand-rolled CNN, so "no Python at inference" holds).
  Only pursue if Option A + new objective still underperforms the production A/B.
- **Explicitly out of scope** (documented dead-ends): weight-space model soup
  (hb-194/S3 — violates shared-basin, made every member worse) and output-space
  ensembling (works offline, 8× inference, lost the A/B). wav2vec2/foundation-model
  front-ends (hb-187) are a *different* project (learned demod, not OSD ordering) —
  not part of this design.
- **Latency ceiling.** OSD runs only on failed-BP candidates that pass the parity
  gate. Forward pass must stay ≪ the OSD trial cost it saves; at ≤60K params and
  ≤2 attention layers this is microseconds-to-low-ms per candidate. The real latency
  budget is **total slot decode ≤ 15 s across all candidates** — the eval harness
  (§4) must report wall-clock elapsed, because model-soup's flatter posterior blew
  elapsed +82% by *raising* effective enumeration depth. A better order should
  *lower* elapsed.

## 4. Eval protocol

The lesson from every shelved session: **offline metrics lie.** Eval is two-tier and
the gate is the second tier only.

- **Tier 1 — offline, for model selection during training** (fast, cheap):
  - **Mean reprocessing order to true codeword** (the primary offline metric — the
    thing the ranking loss optimizes): for each failed-BP sample, the rank at which
    OSD-in-model-order reaches the true error pattern, vs `|LLR|` order. Report the
    full distribution and the fraction solved within OSD-1/2/3 budgets.
  - **Gate-restricted recovery rate** at the production parity gate
    (`parity_errors ≤ max_parity_errors_for_osd`), the metric prior sessions used —
    kept for continuity, but **not** the ship gate.
- **Tier 2 — production A/B, the ONLY ship gate** (mirrors `docs/gap-analysis.md`
  methodology): run the real decoder on the hard-200 and hard-1000 curated tiers
  (and a broad `~/.pancetta/recordings` stride sample) with the model vs the shipped
  weights vs `|LLR|` ordering, at a **fixed `osd_depth`** and measure:
  - **decode-rate Δ** (recovered + novel) with a **bootstrap 95% CI** — ship only if
    the lower bound is > 0.
  - **false-positive Δ** (this is where `osd_depth≥2` died — the model must recover
    weak decodes *without* the FP explosion; a good order finds the true codeword
    before the CRC-collision trials).
  - **elapsed-time Δ** with a **hard gate** (reject if elapsed regresses materially,
    per the model-soup +82% lesson).
  - The decisive experiment this design enables: **does the model make a higher
    `osd_depth` net-positive?** Sweep `osd_depth ∈ {0,1,2}` with model ordering +
    early-exit and find whether any setting beats the `osd_depth=0` production
    baseline on decode-rate at acceptable FP+elapsed. If yes, that is the ship;
    if no, neural OSD stays dormant and the finding is documented (a legitimate
    null, like the confidence-floor result in the gap analysis).

## 5. Rust inference-integration plan (the hook already exists)

**No new inference code is required if the `[·]→[91]` output contract is kept.** The
integration path is mature and live:

1. **Train** (Python reference, §1–3) → `model.pt`.
2. **Export** with `training/neural_osd/export_weights.py` → flat little-endian
   packed f32 → `pancetta-ft8/assets/neural_osd_weights.bin`. The `TENSOR_ORDER`
   list and the Rust schema in `neural_osd_weights.rs` must agree byte-for-byte.
3. **Consume:** `neural_osd.rs::predict_error_bits(trajectory) -> [f32;91]` (the
   hand-rolled CNN forward) already runs under the default `neural_osd` feature and
   feeds `decode_with_features`. Deterministic, no framework, no Python at inference.
- **If the input contract changes** (adding the syndrome/|LLR| channels of §1, i.e.
  `[25+C, 174]`): update three things in lockstep — (a) the capture format and the
  Python model's first Conv/embedding dim, (b) `export_weights.py::TENSOR_ORDER` and
  the `*_LEN` schema constants in `neural_osd_weights.rs`, (c) the trajectory
  assembly + `predict_error_bits` signature in `neural_osd.rs` and its caller in
  `decoder.rs` (which must now also compute and pass the syndrome channel). Add a
  `dump_neural_weights.rs`-style sentinel round-trip test so a schema drift fails
  loudly (the existing loader already length-checks the blob).
- **If the architecture changes to attention** (Option B): the hand-rolled forward
  in `neural_osd.rs` grows an attention block (still framework-free); keep it behind
  the same feature flag and the same `[91]` output so `decode_with_features` is
  untouched. Weights still ship as one packed blob with a documented new
  `TENSOR_ORDER`.
- **Rollout:** the model is default-on but **dormant at `osd_depth=0`**. Shipping a
  new model is therefore safe (no behavior change) until/unless the §4 Tier-2 sweep
  justifies raising `osd_depth` — which is the actual product decision this whole
  effort exists to inform. Gate that behind the tier auto-classifier or an explicit
  config, never a silent default flip.

## 6. Deliverables of the *implementation* phase (not this doc)

When built (separately, per the brief — this doc stops here):
- `training/neural_osd/` — the ranking-loss trainer (new `train_rank.py` following
  the existing `train.py` conventions: argparse, `.npy`/`jsonl` loaders,
  ReduceLROnPlateau/cosine, best-checkpoint), the syndrome-channel data-gen
  extension, and the WAV/band-split logic.
- The **eval harness** — a `pancetta-research` example mirroring
  `gap_confidence_floor_sweep.rs`: run the real decoder with model vs `|LLR|`
  ordering across `osd_depth` on the hard tiers, emitting decode-rate/FP/elapsed
  with bootstrap CIs (Tier-2), plus a Python offline mean-reprocessing-order report
  (Tier-1).
- A validated model + the re-exported blob, **stopping before any `osd_depth`
  default change** — the integration spec above is the drop-in.

## Appendix — corrections to the existing tree (fix during implementation)

- `training/neural_osd/README.md` SNR table (`−28…−18`) contradicts
  `generate_data.py` default (`5…14` Eb/N0). Reconcile and document the FT8-realistic
  regime.
- `generate_data.py`'s pure-Python BP path is **decoupled** from the production
  trajectory-capture path (real trajectories come from the Rust decoder). Prefer the
  captured trajectories for training-distribution fidelity; keep synth only for
  volume/coverage, and verify the two BP implementations produce comparable
  trajectories (a silent divergence would poison synth labels).
- The former `neural_osd_weights_ensemble_si.bin` dormant S3 artifact was removed by PAN-9;
  the loader reads only `neural_osd_weights.bin`, so retaining the unused blob only caused
  confusion.
