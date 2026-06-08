# Ideas Backlog

Low-friction capture for ideas that haven't earned a full `hb-NNN` bank
entry yet. One line "what + why" each. When an idea earns a probe, it
moves to `hypothesis_bank.md` with a numbered hypothesis ID.

Categories:
- **PROBE** — single-batch bounded probe, runnable now
- **WILD** — speculative; may not have a clean mechanism but worth a try
- **RESEARCH** — go-find-out item; produces ideas, not ships
- **REVISIT** — re-test a shelved hypothesis under different conditions

---

## Open ideas

### From Batch 41 research agent (2026-06-07)

Agent surveyed ft8mon (AB1HL/rtmrtmrtmrtm), WSJT-X Improved, MSHV, JTDX,
JS8Call, SDRangel, weakmon, wsjt-devel mailing list, GRAND/ORBGRAND
literature, neural LDPC papers, GPU-OSD discussions, and more.

#### PROBE (cheap, runnable in 1 session each)

- **`ft8mon fractional sub-bin Costas search` (PROBE — HIGH PRIORITY)** —
  ft8mon does `coarse_hz_n=4` × `coarse_off_n=4` = 16 sub-positions
  per FFT bin. Targets pancetta's band-middle 1000-2000 Hz recall hole
  (scalloping signature). ~50-200 LOC.
  Source: [ft8mon ft8.cc](https://github.com/rtmrtmrtmrtm/ft8mon/blob/master/ft8.cc)
- **`ft8mon a-priori bit probability prior for LDPC` (PROBE)** —
  Hardcoded `apriori174[]` array from statistical bit-frequency
  analysis. Bias BP before first iteration. ~30 LOC.
- **`ft8mon conditional OSD gating`** (PROBE) —
  Only invoke OSD when BP terminates with ≥70/83 parity bits correct.
  Lets you raise `osd_depth` without blowing CPU. ~30 LOC.
- **`ft8mon three-stage sync refinement`** (PROBE) —
  coarse (3.5 Hz / 8 pieces) → fine (0.25 Hz / depth 3) → very-fine.
  Extends pancetta's hb-044 Costas parabolic.
- **`ft8mon rate-reduction preprocessing`** (PROBE) —
  Downsample to ~100 Hz around each candidate before LDPC. Frees CPU
  for deeper OSD/more iter. resampler in pancetta-dsp.
- **`Full AP a3-a6 decoding with QSO context`** (PROBE — extends hb-050) —
  Constrain 61-77 bits when both callsigns + expected message are
  known. WSJT-X reports up to 4 dB gain. Pancetta has hb-050 partial.
  Source: [WSJT-X AP](http://www.dx.nl/?p=1611)
- **`Hash collision callsign disambiguation`** (PROBE) —
  FT8 12-bit hash collisions (ZL2CC/PG6PEACE known). Cqdx context
  could resolve via multi-hash cache. Easy false-decode reduction.
- **`Bidirectional BP schedule`** (PROBE) —
  Run BP forward + backward, average LLRs. ~0.1-0.3 dB free gain.
  Source: BCJR-style forward-backward.

#### PROBE/PLAN (1-2 sessions)

- **`Q65-style polynomial drift compensation`** (PROBE/PLAN) —
  Linear drift search ±20 Hz/s (Q65 uses this). Pancetta's
  band-middle FPs cluster at slow drift per Batch findings.
  Source: [KA1GT Q65 drift](https://bobatkins.com/radio/Q65_step_size_drift_compensation.html)
- **`Polyphase channelizer / Chirp-Z zoom`** (PROBE/PLAN) —
  Replace front-end FFT with polyphase channelizer (no scalloping)
  or chirp-Z zoom on each candidate (0.00078 Hz resolution in a
  0.4 Hz band). Directly attacks band-middle recall hole.
  Source: [Polyphase Channelizer (GNU Radio wiki)](https://wiki.gnuradio.org/index.php/Polyphase_Channelizer)
- **`Adaptive normalized min-sum scaling`** (PROBE) —
  Standard BP uses fixed scaling. Adaptive tuning of factor per-iteration
  via lookup table. 0.2-0.3 dB lift expected.
  Source: [Adaptive normalized min-sum](https://www.researchgate.net/publication/261395477)
- **`CRC-aware BP early termination via list`** (PROBE) —
  Two-stage scheme: BP produces list → CRC filters → GRAND only on
  failures. Catches "BP almost converged but CRC flipped" cases.
  Source: [Two-Stage LDPC-CRC](https://www.ncbi.nlm.nih.gov/pmc/articles/PMC12468592/)
- **`ESPRIT/MUSIC frequency refinement on Costas symbols`** (PROBE) —
  After candidate accept, run ESPRIT on 21 Costas symbol windows
  for sub-Hz freq estimate. Works at -5 dB SNR.
  Source: [Single-Snapshot ESPRIT (arxiv 1607.01827)](https://arxiv.org/pdf/1607.01827)

#### RESEARCH (find-out, may produce more PROBE items)

- **`ORBGRAND for (174,87) short-block`** (RESEARCH→PROBE) —
  Capacity-achieving for short, high-rate codes. FT8 squarely in scope.
  Could replace/augment OSD. Hardware-friendly.
  Source: [Fine-tuning ORBGRAND (arxiv 2507.08696)](https://arxiv.org/pdf/2507.08696),
  [Guessing Decoding (arxiv 2511.12108)](https://arxiv.org/pdf/2511.12108),
  [GRAND at MIT](https://granddecoder.mit.edu/)
- **`Reshuffling-ORBGRAND for near-ML`** (RESEARCH) —
  Near-ML at modest query overhead.
  Source: [arxiv 2401.15946](https://arxiv.org/pdf/2401.15946)
- **`SGRAND as upper-bound oracle`** (RESEARCH) —
  Soft-input GRAND = ML on AWGN. Slow but quantifies headroom; tells
  us how much pancetta's BP+OSD is leaving on the table.
  Source: [SGRAND IEEE 8849297](https://ieeexplore.ieee.org/document/8849297/)
- **`GPU-accelerated OSD at order 4-6`** (RESEARCH→PLAN) —
  May 2026 wsjt-devel thread describes Radeon RX 6900 XT brute-force
  OSD order-4 with meaningful FER gain. Could prototype with CPU
  rayon parallelism first.
  Source: [wsjt-devel archive May 2026](http://www.mail-archive.com/wsjt-devel@lists.sourceforge.net/)

#### WILD (speculative, higher-effort)

- **`Neural min-sum LDPC with learned weights`** (WILD/PLAN) —
  arxiv 2406.19664 survey: learned BP matches OSD on short codes at
  5-10× lower iter count. Plan-sized.
- **`Cross-attention transformer decoder (CrossMPT)`** (WILD/RESEARCH) —
  arxiv 2507.01038. Best-in-class for 6G short codes; code-agnostic.
  Multi-plan project; cutting-edge research direction.
- **`CNN-based candidate classifier on spectrogram`** (WILD) —
  Light CNN to predict P(decodable signal present) per spectrogram
  patch. Augments Costas threshold.
- **`Soft Viterbi MLSE FSK demapper`** (WILD/PLAN) —
  Replace per-symbol LLR with SOVA over multi-symbol trellis.
  Literature shows 2.1-2.3 dB for GMSK.
- **`Extended Kalman frequency tracker inline`** (WILD/PLAN) —
  EKF tracking f/amp/phase over 79-symbol burst. Per-symbol freq
  estimate sharpens LLRs.
- **`Multi-symbol matched filter (MSMF)`** (WILD/PLAN) —
  Matched filter over N-symbol windows (e.g. N=3 = 512 templates).
  Joint over symbols breaks per-symbol ceiling.
- **`Hadamard-Viterbi joint MFSK decoding`** (WILD/RESEARCH) —
  Underwater MFSK literature. HF ionospheric closer to underwater
  than AWGN. Could pair with SOVA.
  Source: [Hadamard-Viterbi MFSK](https://www.researchgate.net/publication/365884175)

### From Batch 41 own brainstorm / search (2026-06-07)

#### PROBE (low-friction)

- **`residual_min_sync_score lowered to mimic WSJT-X 2.1→1.3` (PROBE — HIGHEST PRIORITY)** —
  WSJT-X's ft8_decode.f90 lowers sync threshold BETWEEN passes (2.1→1.3
  globally), surfacing weak signals revealed by subtraction. Pancetta has
  the knob (`Ft8Config::residual_min_sync_score: Option<f64>`, hb-082) but
  default is `None` (reuses production 3.0). This was NEVER ablated at
  the global level — hb-086 V3's relaxation was in a localized
  window only. **One value to try: `Some(1.5)` on hard-200 with
  max_decode_passes=2.** Cheap; could be the cleanest unrecognized win.
- **`WSJT-X multi-interval decoding (11.8s / 13.5s / 14.7s)` (PROBE/PLAN — HIGH PRIORITY)** —
  WSJT-X 2.2+ decodes FT8 at THREE time intervals within the 15s
  slot: 11.8s (~85% of total), 13.5s, 14.7s. Pancetta does single
  full-window decode. Bounded probe: take each WAV, truncate to
  {11.8, 13.5, 14.7s}, decode each, union TPs vs single decode.
  WSJT-X claims +10% on crowded bands. Strong candidate for next
  ship. Source: ARRL article on WSJT-X 2.2.0-rc1.
- `JTDX "QSO partner filter" auto-version` (PROBE) —
  When a high-priority DX is being chased, run a focused decode pass
  at narrow ±50 Hz window around the DX's expected freq with relaxed
  thresholds. JTDX manual feature, user-driven; pancetta could
  automate based on DXspots + autonomous-operator mode.
- `JTDX "subpass" emulation` (PROBE) —
  After the standard mp=2 pass, run a SECOND independent pass with
  DIFFERENT decode settings (e.g., min_sync_score lowered + osd_depth
  bumped + max_sync_candidates doubled), union the results. Different
  mechanism from pancetta's SIC-based multipass — "search again with
  perturbed settings" rather than "search again after subtracting".
- `WSJT-X Improved-style "STD/MTD cooperation"` (PROBE) —
  Run two independent decoder configs in parallel; merge results via
  shared hash table to reduce hash collisions. Pancetta has parallel.rs
  but not 2-decoder cooperation.
- `4th pass after a7` (PROBE) —
  WSJT-X Improved v3.1.0 shipped this. Pancetta has a7 (hb-048
  Session 2). Try adding an extra LDPC/OSD pass on residual candidates
  AFTER a7 templates fire.
- `Auto-passband baseline optimization` (PROBE) —
  WSJT-X Improved v3.0.0: filter edges auto-optimized to actual
  passband. Pancetta uses a fixed MIN_FREQ_BIN..max_freq_bin. Could
  autotune per-WAV from the spectrogram noise-floor distribution.

#### REVISIT (academic literature)

- `OSDSW (Sliding Window OSD)` (REVISIT/PROBE) —
  arXiv 2404.14165 (2024): sliding window variant of OSD as
  post-processor for LDPC (128,64). FT8 is (174,87) — comparable.
  Could pair with pancetta's neural OSD (hb-063 family).
- `Probability-Based OSD for short block codes` (RESEARCH) —
  ResearchGate 349284828 — alternative OSD ranking metric for short
  codes. Worth comparing to pancetta's current OSD.
- `Neural NMS LDPC decoder` (RESEARCH/PLAN) —
  Recent academic work shows unfolding BP iterations as a NN with
  learned weights. 0.4-0.5 dB Eb/N0 gain at FER 10^-3 for short
  LDPC. Plan-sized to implement but well-motivated.
- `Neural-enhanced OSD reliability measure` (RESEARCH/PLAN) —
  Use CNN to score bit reliability for OSD basis selection.
  Pancetta's hb-063 neural OSD work could be extended.

#### WILD (speculative)

- `CNN spectrogram FT8 detector` (WILD) —
  arXiv 2501.07337 — CNN for digital mode classification from
  spectrograms. Could repurpose: train a CNN to detect FT8 sync
  patterns as alternative/supplemental to Costas correlation.
- `Convolutional autoencoder for WAV denoising` (WILD) —
  Springer-Nature chapter: CNN-AE for radio signal denoising. Train
  AE on synthesized FT8 + noise; use as pre-decode denoiser.
- `Spectrogram-level diffusion model decoder` (WILD) —
  arXiv 2501.11229 — SIC-aided diffusion models. Could explore
  diffusion-based denoising of the FT8 spectrogram before sync.
- `Doppler tracking inline with decode` (WILD) —
  EME work uses 0.3 Hz Doppler tracking steps. Most HF doesn't have
  Doppler, but some bands (12m-10m) might benefit from per-symbol
  freq tracking.

### From own brainstorm (2026-06-07)

#### PROBE (bounded, runnable)

- `min_sync_score finer grid` (PROBE) — Batch 40 tried 1.0; try
  {1.5, 2.0, 2.5}
- `time_range sweep` (PROBE) — currently 2.0s; try {1.5, 2.5, 3.0}
- `NMS enable + tune` (PROBE) — `nms_enabled` exists; default off;
  try on with various radii
- `adaptive_ldpc_iters` (PROBE) — flag exists; default off; enable
- `High-pass filter pre-decode` (PROBE) — cut <300 Hz; removes LF
  rumble that may confuse AGC
- `60 Hz hum notch` (PROBE) — many real WAVs have power-line content;
  zero ±60/120/180 Hz
- `Sub-Hz freq search` (PROBE) — pancetta uses freq_osr=2; try 4×
  via FFT zero-pad interp around top candidates
- `Cross-correlation Costas reference` (PROBE) — full template
  cross-correlate vs sum-of-tone-energy
- `Spectrum subtraction noise reduction` (PROBE) — estimate noise
  floor, subtract
- `Spectral whitening` (PROBE) — flatten noise spectrum before sync

#### WILD (speculative)

- `Reverse decode` (WILD) — decode signal time-reversed; XOR symmetry
  may surface partial decodes
- `Sub-symbol decoding` (WILD) — split each FT8 tone into 2
  sub-symbols, decode separately, majority-vote
- `Phase-coherent end-to-end` (WILD) — keep complex spectrogram
  through LDPC LLR computation
- `Random noise injection consensus` (WILD) — decode N noisy variants
  of WAV; bits with majority vote = high confidence
- `Random-restart LDPC` (WILD) — multiple BP initializations, pick best
- `Multi-pass with shifted spectrogram` (WILD) — re-search with
  ±1-2 Hz freq shift; union TPs
- `Sympathetic decode` (WILD) — when high-confidence decode lands,
  search ±1-2 tone shifts in neighbor freq for "partner" decodes
- `Generative operator model` (WILD/PLAN) — likely "next" message
  given prior decoded callsigns + grid; use as AP context
- `Time-stretched WAV decode` (WILD) — zero-stuff each sample 2×
  (pretend 30s slot); doubles freq resolution
- `Frequency dithering` (WILD) — shift WAV by ±2 Hz; decode all
  variants; union TPs
- `Multi-WAV stitching` (WILD/PLAN) — concatenate 3 consecutive 15s
  slots; allow Costas at boundaries; competes with hb-220
- `Sympathetic decode at known QSO partner freq` (WILD) — when a
  recent decode is known to be in a QSO at freq F, prioritize the
  exchange flow around F+0

#### RESEARCH (find-out)

- `Survey ft8_lib commits since 2024-01` (RESEARCH) — what has
  kgoba/forks shipped? (web search: limited recent commit info)
- `Read JTDX subpass source code` (RESEARCH) — JTDX is GPL3;
  source available; subpass is the known mechanism
- `Read WSJT-X Improved fork source` (RESEARCH) — v3.0.0/3.1.0
  features above; need source for implementation details
- `WSJT-X dev mailing list 6-month scan` (RESEARCH) —
  https://sourceforge.net/p/wsjt/mailman/wsjt-devel
- `MAP65 multi-user detection paper` (RESEARCH) — K1JT's MUD work;
  the academic reference behind the SIC family pancetta uses
- `arXiv "FT8" recent papers` (RESEARCH) — focused literature scan
- `IEEE FT8 / weak-signal HF papers 2024-2026` (RESEARCH) —
  IEEE Xplore search
- `Compressed sensing / OMP for FT8 capture pairs` (RESEARCH) —
  could be a fundamentally new joint-decode approach

#### REVISIT

- `hb-086 V2 soft cancellation at multi-stage refit` (REVISIT) — V2
  shelved single-pass across 3 corpora; iterative refit untested
- `hb-001 multipass per-bucket` (REVISIT) — was tested overall in
  2026-05-21; now have slot-edge / mid-slot / weak buckets; mp=N may
  be differently effective per-bucket
- `hb-090 phase-coherent matched filter` (REVISIT) — was wrong-scoped
  per Batch 30; could retry with proper IQ
- `hb-115 dual-Kiwi MRC` (REVISIT) — MECHANISM-PROCEED on synth;
  needs paired-Kiwi live capture (meatspace-pending)

#### META (process-level)

- `Comparative bench: pancetta vs jt9 vs JTDX vs WSJT-X Improved`
  (RESEARCH/PLAN) — run all four on hard-200; identify pancetta-
  uniquely-missed vs pancetta-uniquely-found classes. Substantial
  setup but high information value.
- `Fresh K5ARH MiniPC capture corpus` (META/PLAN) — current baseline
  is at 60% recall on hard-200 which is mostly K5ARH already; a
  larger fresh capture would test stability.
- `Production vs research code-path diff audit` (META) — make sure
  every config knob shipped in eval is also reachable via
  coordinator runtime config

### From operator/dev observations (preserved)

(any prior backlog items)

---

## Promoted (moved to `hypothesis_bank.md` as `hb-NNN`)

### Promoted Batch 42

- `WSJT-X multi-interval decoding` → **hb-221** (measured +118 TPs on hard-200)
- `ft8mon search_both_known post-decode subtraction refinement` → **hb-222**
- `ft8mon soft_decode_pairs` → **hb-223**
- `ft8mon osd_ldpc_thresh=70 gate` → **hb-224**
- `ft8mon 2-D coarse sub-bin Costas grid` → **hb-225**
- `ft8mon inter-symbol phase-bridged subtraction reconstruction` → **hb-226**
- `ft8mon apriori174[] empirical bit prior` → **hb-227**

## Killed (during brainstorm; not worth a probe)

### Killed by Batch 42 sweep (all null or negative vs mp=2+ldpc=200 baseline)

- `residual_min_sync_score = Some(1.0/1.5/2.0/2.5)` — **all +0 TPs**.
  WSJT-X's 2.1→1.3 cascade does NOT help pancetta. Confirms hb-086 V3's
  shelve at the global level. Sole highest-priority Tier 1 prediction
  refuted by direct measurement.
- `min_sync_score = 2.0 / 2.5` — null
- `time_range = 1.5 / 2.5 / 3.0` — null
- `max_sync_candidates = 450 / 500 / 700` — NEGATIVE -8, -8, -28 TPs.
  Added candidates are all noise. Current 300 is well-tuned.
- `NMS enable` — CATASTROPHIC -1198 TPs. Suppresses too aggressively.
- `adaptive_ldpc_iters = true` — -8 TPs
- `HPF 300 Hz pre-decode` — -25 TPs
- `DC offset removal pre-decode` — flipped to -4 TPs after Batch 41
  ldpc_iters=200 ship (was +4 vs ldpc=100). Effect captured by ldpc bump.
- `combo (residual_min=1.5 + dc_remove)` — same as dc_remove alone

---

## Unified pancetta + ft8mon roadmap (added Batch 43)

After Batch 42's ft8mon research agent, the strategic landscape is
the clearest it has been across all 42 batches. The 7 new hb-NNN
entries from ft8mon stack into 3 natural mechanism families:

### Family A — Multipass amplifiers (boost mp=2's yield)
- **hb-222** post-decode `search_both_known` subtraction refinement
- **hb-226** inter-symbol phase-bridged subtraction reconstruction
- Together: ~450 LOC, ~3-4 sessions; cleaner residual → more decodes
  from residual without changing the candidate population.

### Family B — Sync improvements (find more candidates)
- **hb-221** multi-interval sliding decode window (MEASURED +118 TPs)
- **hb-225** 2-D coarse sub-bin Costas grid (band-middle scalloping)
- **hb-220** slot-edge sync expansion (negative-dt + late-dt coverage)
- Together: ~750 LOC, ~5-6 sessions; recover signals currently missed
  by sync coverage. hb-221 is measured and the cheapest.

### Family C — LDPC/OSD improvements (rescue near-converging cases)
- **hb-223** `soft_decode_pairs` independent LLR producer
- **hb-224** wider parity-gate via max_parity_errors_for_osd
- **hb-227** apriori174 empirical bit prior
- Together: ~210 LOC + 1-2 sessions corpus extraction; squeeze more
  decodes out of marginal candidates that current LDPC almost rescues.

### Family D — Plan-sized initiatives (not in ft8mon)
- **hb-218b** joint LDPC for dual-miss capture pairs (~250 truths)
- **CrossMPT transformer decoder** (academic, multi-plan)
- **Neural NMS LDPC** (academic, plan-sized)
- These are high-risk longer-term R&D, not next-batch material.

### Recommended ship order (next 3-4 batches)

**Batch 43 (current)**: hb-221 multi-interval 2-window ship
+ hb-224 parity-gate probe + Tier 1 measurement
+ 2 research agents (JTDX, MAP65) for new ideas

**Batch 44**: hb-223 `soft_decode_pairs` implementation
+ hb-225 2-D coarse sub-bin Costas (if synergy with hb-221)
+ measure stacking effect (Family B + C)

**Batch 45**: hb-222 + hb-226 as a pair (Family A multipass amplifiers)
+ measure mp=2 yield boost
+ if hb-220 slot-edge can fit, ship Session 1

**Batch 46**: hb-227 apriori174 corpus extraction + impl
+ characterize remaining frontier
+ decide on hb-218b plan kickoff vs new direction

### Ideation: areas where pancetta could LEAD ft8mon/WSJT-X

ft8mon and WSJT-X are well-tuned reference implementations. To get
beyond their performance ceiling, pancetta could explore:

- **GPU-accelerated OSD order 4-6** (Metal/wgpu) — May 2026 wsjt-devel
  thread documents Radeon RX 6900 XT giving meaningful FER curve
  improvement; M4 Metal is comparable hardware
- **Neural OSD / neural BP** — academic 2024-2026 literature shows
  0.4-0.5 dB Eb/N0 gain; could ship as Fast-tier optional
- **Cross-slot temporal coherence** — pancetta has QSO state machine;
  could feed prior-slot decoded callsigns as AP context (extends hb-050)
- **Live-paired dual-Kiwi MRC** (hb-115) — pancetta's autonomous mode
  could ingest 2 Kiwi streams for +1-3 dB sensitivity (meatspace-pending)
- **Operator-tailored AP context** — autonomous operator knows what
  it's transmitting → feeds expected callsigns as a-priori; ground-truth
  context that WSJT-X doesn't have access to in the same way

These are MEDIUM-term R&D bets. Worth keeping in the backlog but not
next-3-batch material.

### Process: when to look outward vs inward

Batches 33-40 (inward focus): rich mechanism characterization,
shelved many candidates, narrowed the frontier. Necessary but
diminishing returns.

Batches 41-42 (outward focus): web research surfaced 7+ new bank
entries from ft8mon alone. Continue investing 1-2 research agents
per batch on different sources (JTDX, WSJT-X dev, academic literature,
adjacent projects).

Suggested ratio for next 5 batches: **70% implementation/probe,
30% external research**. Adjust as research yield drops.

---

## MAP65 research finding (added Batch 43)

**MAP65 is NOT a source of new ideas for FT8 capture-effect.**
Per research agent's read of K1JT EME 2012 paper, MAP65 (despite its
"multi-decode" reputation) does:
1. Wideband (90 kHz) panoramic search
2. Polarization-matched MRC over dual-pol antenna inputs
3. **Per-candidate independent decode** (no joint MUD)

The "MUD reputation" is sociological — in 2007 wideband JT65 decoding
was revolutionary vs WSJT's 2.5 kHz window. None of MAP65's components
translate to FT8 capture-effect:
- Wideband: pancetta already enumerates all candidates per pass
- Pol-MRC: requires dual-pol antennas (HF stations are single-pol; only
  hb-115 dual-Kiwi MRC is in scope, which is meatspace-pending +1.5 dB)
- Per-candidate decode: WSJT-X + pancetta both do this

WSJT-X's iterative-subtract SIC (FT4_FT8_QEX.pdf) is the closest thing
to MUD in K1JT's codebase, and pancetta implements it equivalently
(hb-079/080 + hb-086 V1).

### Implication for hb-218b

The MAP65 research closes "is there a published reference for FT8
multi-user detection?" with: **no**. hb-218b joint LDPC remains the
only viable attack on dual-miss capture pairs. Plan-sized; high-risk.

### Preparatory baseline (NEW PROBE)

- `jt9 on pancetta's dual-miss subset` (PROBE/RESEARCH) — run
  WSJT-X reference decoder on the same ~250 dual-miss truths.
  If jt9 also fails on them, hb-218b is the only frontier left.
  If jt9 succeeds on some, pancetta has a parameter-tuning gap
  with WSJT-X.

Source: [MAP65 EME 2012 paper](https://wsjt.sourceforge.io/K1JT_EME2012.pdf),
[FT4 FT8 QEX paper](https://wsjt.sourceforge.io/FT4_FT8_QEX.pdf),
[Pfister 2008 joint LDPC academic ref](http://pfister.ee.duke.edu/papers/Pfister-jsac08.pdf)

---

## JTDX research findings (added Batch 43 — high-priority concrete ports)

Agent surveyed JTDX (jtdx-project/jtdx) source code (Fortran). Found
3 mechanisms genuinely orthogonal to pancetta's existing pipeline:

### hb-228 candidate — JTDX 3-method spectral sweep (PROBE/PROD WIRE — HIGHEST PRIORITY)

JTDX's "subpass" is NOT residual subtraction — it's 3 different
magnitude metrics fed to Costas sync from the SAME FFT:

```fortran
! ipass 1,4,7 — sqrt(re² + im²)        amplitude (favors fading recovery)
! ipass 2,5,8 — re² + im²               power-squared (boosts strong SNR)
! ipass 3,6,9 — |re| + |im|              L1 norm (robust to outliers/impulse)
```

Each gets a different `syncmin` (1.225 / 1.5 / 1.1). Different signals
in the same WAV pop under different metrics. Source: lib/sync8.f90.

- Effort: **~1 day** in Rust
- Conflict: ZERO with mp=2 or hb-086 (FFT cost amortized; just compute
  3 spectrograms from same FFT, run sync on each, union+dedup)
- Mechanism is **truly orthogonal** to pancetta's residual-subtract
  approach — not a re-discovery
- Pancetta currently uses ONE magnitude (dB-power per spectrogram path)
- Insertion point: pancetta-ft8/src/decoder.rs near `compute_spectrogram`
  + `costas_sync_search` (~line 950)

### hb-229 candidate — QSO partner band-collapse (PROD WIRE — autonomous-only)

JTDX's `nfilter` collapses search band to ±60 Hz around active partner
frequency (±290 Hz in Hound mode). Pure CPU win — same recall, less
work. Source: lib/decoder.f90.

- Effort: **~hours** (plumbing already exists via hb-091 `freq_bin_range`)
- Conflict: none
- Wire in `pancetta-qso/src/qso_manager.rs`: when QSO in-flight, pass
  `Some(±60 Hz around partner freq)` to decoder
- Enables parallel multi-stream-per-QSO architecture

### hb-230 candidate — ±3 Hz relaxed sync threshold near partner freq (TINY PROBE)

Independent of band-collapse: even in full-band sweep, candidates
within ±3 Hz of `nfqso` get relaxed `syncmin=1.1` vs per-pass threshold.
Source: lib/sync8.f90.

- Effort: **1-2 hours** scalar tweak in Costas filter
- Conflict: none
- Risk: pancetta doesn't currently have a "preferred frequency" notion;
  needs to plumb partner-freq through to Costas. Could complement hb-217
  for weak partner messages.

### Anti-recommendations (JTDX features NOT worth porting)

- **DT-distance thinning** (`ncandthin`, lib/sync8.f90 sort key
  `sync / |dt - dtcenter|`): would WORSEN pancetta's slot-edge recall
  hole (Batch 40 finding: 64% of isolated strong-misses at slot-edges).
  Confirmed anti-recommend.
- **napwid AP gating** (`abs(f1 - nfqso) > napwid`): pancetta's AP
  injection already gated by trust-set match; not in top FP buckets.

### Notable null finding

JTDX does NOT do residual subtraction between its "subpasses". The
WSJT-X-derived doc wording ("audio spectrum is searched a second time")
describes the 3-method spectral sweep. Pancetta's mp=2 (residual
subtract + re-sync) and JTDX's 3-method (multiple magnitudes, no
subtract) are **mechanistically different and orthogonal**. Stacking
both is the natural next ship.

Sources:
- [JTDX repo](https://github.com/jtdx-project/jtdx)
- [DB6LL optimal settings PDF](https://www.asahi-net.or.jp/~vj5y-tkur/ft8/optimal-decoding-settings.pdf)
- lib/sync8.f90, lib/ft8b.f90, lib/decoder.f90

---

## Brainstorm: 15 cross-domain wild ideas (added Batch 44)

Drawing from adjacent fields: GPS signal acquisition, OFDM receivers,
voice codecs, sonar processing, biometric recognition, dispersive-channel
estimation.

### WILD (each speculative; bank entry only if probe motivates)

1. **GPS-style code-aided acquisition** — use Costas pattern as a
   pseudo-code and do massively-parallel correlation across
   doppler/time bins (already what pancetta does, but the GPS literature
   has specific delay-lock-loop tracking variants worth borrowing).
2. **OFDM Schmidl-Cox correlation for FT8 sync** — adjacent symbols of
   a known sequence have a known phase relationship; cross-correlate
   sliding pairs. Could improve sync at low SNR.
3. **Voice-codec-style harmonic enhancement** — voice codecs use
   harmonic predictive filtering to clean speech. FT8 tones have no
   harmonic structure but the per-symbol tone IS periodic at the
   carrier — could exploit harmonic averaging.
4. **Sonar-style adaptive matched filter** — sonar receivers track
   slowly-varying channel impulse response and update the matched
   filter. HF ionosphere is similar slow-varying.
5. **Biometric-style template matching** — treat each FT8 callsign as
   a "fingerprint" and use ML similarity matching across slots. Could
   surface callsigns that pancetta misses individually but recurs over
   slots.
6. **Sparse linear-algebra solve** — model 79 received symbols as a
   sparse linear combination of N-tone codebook entries; use OMP
   or LASSO. Lower-bound on sparsity = LDPC codeword bound.
7. **Phase-coherent inter-symbol correlation** — compute phase
   difference between adjacent same-tone symbols. Should be constant
   for a true signal; random for noise. Could be a sync verifier.
8. **Adaptive bandpass at expected freq** — dynamic LPF around each
   sync candidate's expected freq before extracting symbols. Removes
   neighbor energy contamination.
9. **Multi-rate dyadic decomposition** — analyze signal at 6, 12, 24
   kHz sample rates simultaneously. Different artifacts at each rate
   may surface different signals.
10. **Cumulant-based detection** — 4th-order cumulants are signal-
    sensitive but noise-insensitive. Could replace |·|² in sync.
11. **Reservoir computing for sync** — train a tiny ESN to map
    spectrogram patches to sync_score. Cheap inference, learns
    band-specific noise patterns.
12. **Reinforcement learning for OSD bit-flip selection** — learn
    which bits to flip based on LLR pattern.
13. **HMM for symbol decoding** — Viterbi on a hidden-Markov model
    of FT8 symbol transitions (GFSK has memory).
14. **Karhunen-Loève transform for spectrogram denoising** — project
    onto signal-subspace eigenvectors, discard noise subspace.
15. **Empirical mode decomposition (EMD)** — separate signal modes
    via Hilbert-Huang transform. Adaptive to non-stationary HF noise.

### Lower-priority ideation (no specific mechanism)

- **Cross-band "leakage" correlation** — strong signals on adjacent
  freqs sometimes leak into target band; could be subtracted
- **Self-similar codeword detection** — FT8 has 79 symbols; certain
  positions in standard messages have predictable patterns
- **Operator-fingerprint** — same operator's signal has consistent
  carrier drift, audio levels; train per-station calibration

Most of these are PROBE-able with research examples. None are
production-ready without significant work. The list is intentionally
broad to surface cross-domain inspiration; promotion to hb-NNN
requires a concrete mechanism + probe.

---

## arXiv research findings (added Batch 44 — academic candidates)

Agent scanned arXiv 2024-2026 for FT8-adjacent decoder improvements.
Excellent finds beyond ORBGRAND/neural-LDPC already in bank:

### High-priority new academic candidates

- `hb-231 candidate — RS-ORBGRAND` (RESEARCH→PLAN)
  Reshuffling ORBGRAND minimizes expected query count. "0.1 dB from
  ML at BLER 1e-6". Query-ordering change to plain ORBGRAND — pancetta
  needs ORBGRAND baseline first. Effort: medium-low if ORBGRAND lands.
  Source: [arXiv:2401.15946](https://arxiv.org/abs/2401.15946)

- `hb-232 candidate — ORDEPT (Ordered Reliability Direct Error-Pattern Testing)`
  (RESEARCH→PLAN)
  Universal soft-decision decoder; faster than Chase-II, ORBGRAND, GCD
  per 2025 follow-on. Demonstrated on BCH(256,239), BCH(32,21),
  polar(128,116) — FT8 (174,87) in range. Effort: medium.
  Source: [arXiv:2310.12039](https://arxiv.org/abs/2310.12039),
  [arXiv:2506.20079 (2025 follow-on)](https://arxiv.org/abs/2506.20079)

- `hb-233 candidate — MP-WSD (Multipoint Code-Weight Sphere Decoding)`
  (RESEARCH→PLAN)
  Precomputes low-weight codeword list; perturb first-stage estimate
  with selected combinations, retest in Euclidean ball. Two-stage:
  fast common-case, near-ML on misses. Could compose with mp=2 as
  "round 3". Offline: enumerate low-weight (174,87) codewords. Effort: medium.
  Source: [arXiv:2602.08501](https://arxiv.org/abs/2602.08501)

- `hb-234 candidate — Soft-Output GRAND + iterative coupling`
  (RESEARCH→IMPL)
  Turns soft-input GRAND into soft-output. **Even without GRAND
  decoder shipped, the SO machinery improves hb-103 content scoring
  and any joint-message reasoning (hb-218 family)**. Low-medium effort.
  Source: [arXiv:2310.10737](https://arxiv.org/abs/2310.10737)

- `hb-235 candidate — IBA-LDPC iterative phase-tracking ↔ LDPC loop`
  (RESEARCH→PLAN — HIGH POTENTIAL)
  Models bursty differential phase noise as Wiener process with
  time-varying innovation variance; iterates between channel estimator
  and LDPC decoder. 1.4 dB BER@4e-3, 3 dB PER@1e-2 vs conventional.
  **HF ionospheric phase noise on FT8 = exactly the regime they
  model.** Pancetta has no closed-loop between phase tracking and
  LDPC. Effort: HIGH but theoretically grounded.
  Source: [arXiv:2604.07004](https://arxiv.org/abs/2604.07004)

- `hb-236 candidate — Policy-Guided MCTS for OSD bit-flip selection`
  (RESEARCH→PLAN)
  RL policy replaces OSD's Gaussian elimination. 95% search reduction
  vs non-GE OSD. Complexity-reduction angle is the value-add — if
  OSD eats budget after hb-222/223 ship, this is the cleanup tool.
  Tiny network. Effort: medium-high (RL training pipeline).
  Source: [arXiv:2511.09054](https://arxiv.org/abs/2511.09054)

### Lower-priority / wild-card academic candidates

- CrossMPT / FCrossMPT / CrossED (arXiv:2507.01038) — transformer
  decoder ensemble; cross-validates with hb-225+ family but
  high-effort training pipeline.
- TransCoder (arXiv:2511.22539) — paper explicitly says it's
  "particularly effective for longer codes" — FT8 short+high-rate
  may be exactly NOT the regime. Note-but-don't-prioritize.
- Mamba-Transformer hybrid (arXiv:2505.17834) — evolution not
  step-change; bank as wild-card peer.
- EQML via saturation (arXiv:1810.13111) — older paper but
  short-code-friendly idea worth keeping.
- BP-RNN diversity + OSD (arXiv:2206.12150) — re-cited as SOTA for
  short LDPC; overlap with bank's neural-LDPC line.

### Honest negatives (agent searched, found nothing genuinely new)

- **No FT8-specific MUD / capture-effect papers**: recent literature
  (2509.25074, 2412.01511) is asymptotic massive-MTC, not pancetta's
  corpus-scale problem
- **No new Doppler/HF-ionosphere coherent receiver work** beyond
  IBA-LDPC (hb-235 above)
- **No new GFSK soft demod** — area dominated by 1990s-2000s patents
- **No WSJT-X academic publications** — WSJT-X research is in the
  K1JT/G3WDG/G4WJS code base, not arXiv

### Recommended bank insertion priority

Per arXiv agent:
1. hb-235 (IBA-LDPC phase loop) — addresses FT8 *channel*, not code
2. hb-234 (Soft-Output GRAND) — useful pre-GRAND-decoder
3. hb-231 (RS-ORBGRAND) + ORBGRAND baseline
4. hb-232 (ORDEPT) — universal short-block decoder
5. hb-233 (MP-WSD) — composes with mp=2
6. hb-236 (P-MCTS) — OSD complexity reduction after hb-222/223

---

## WSJT-X dev agent research (added Batch 44 — wsjtr discovery + bombshells)

Agent surveyed wsjt-devel mailing list + WSJT-X Improved + Bodiya's
NEW Rust peer `wsjtr` (created Mar 2026). Headline finding:
**mainline WSJT-X has had ZERO meaningful FT8 decoder algorithm
commits in 12+ months. Active R&D is in (1) WSJT-X Improved fork,
(2) Brian Bodiya KC1WIH's `wsjtr` Rust decoder, (3) experiments on
the mailing list.**

### MAJOR: Brian Bodiya's `wsjtr` is a direct Rust peer to pancetta

- Repo: https://github.com/bodiya/wsjtr (GPLv3, Mar 2026, very recent)
- crates: ft8core, jt9r, wsjtr, wsjtr-supplement, ft8coder, ft8-engine
- crate listing: https://lib.rs/crates/ft8core
- Architecture: downsample → fine sync → soft metrics → BP/OSD hybrid → between-pass subtraction
- Has cross-sequence (A7) decoding, multi-WAV chaining for A7 context,
  configurable per-pass depth, DT refinement during subtraction

**Action**: read wsjtr source as additional reference alongside ft8mon.
Direct Rust port may be easier to extract idioms from than Fortran.

### High-priority new bank candidates

- `hb-237 candidate — Cross-sequence A7 (callsign-from-prior-window AP)`
  (PROBE/PROD — HIGHEST PRIORITY from this agent)
  Bodiya's wsjtr docs/cross_sequence_decoding.md is a 200+ line
  Rust implementation reference. Maintains `prev[Even][N]` /
  `prev[Odd][N]` decode tables; at each window, takes prior
  opposite-sequence decodes, generates 206 candidate messages
  (CALL1 CALL2 + RRR/RR73/73, reports −50 to +49), correlates.
  Acceptance criteria: dmin ≤ 100 AND dmin2/dmin ≥ 1.3.
  - WSJT-X has had this since v2.6.0 (Jun 2022)
  - Pancetta has callsign trust set (hb-062, hb-103) but NOT
    AP-driven candidate-message correlation in decoder
  - Estimated headroom: ~30% of pancetta's response-shaped misses
  - Conflict: orthogonal to FP-filter line and spectral-sweep line
  - Effort: ~1-2 sessions for cross-sequence table + correlation;
    pancetta-qso already has QSO state for related context
  Source: [wsjtr cross_sequence_decoding.md](https://github.com/bodiya/wsjtr/blob/main/docs/cross_sequence_decoding.md)

- `hb-238 candidate — OSD dmin initialization audit` (PROBE/PROD —
  HIGH PRIORITY for FP reduction)
  Bodiya found wsjtx seeds dmin to order-0's distance regardless of
  CRC; he was initializing to INFINITY and accepting any CRC-passing
  codeword → ~4.1% noise FP rate per OSD call (40 FPs / 10 windows).
  **Pancetta should audit its OSD's `dmin` init.** Single highest-
  leverage finding for FP reduction.
  - Action: read pancetta-ft8/src/ldpc.rs or wherever OSD lives;
    verify dmin is seeded from order-0's distance, not INFINITY
  - Effort: ~hours to audit + fix
  Source: [wsjtr osd-depth-enhancement.md](https://github.com/bodiya/wsjtr/blob/main/docs/osd-depth-enhancement.md)

- `hb-239 candidate — WSJT-X Improved "a8" mechanism investigation`
  (RESEARCH)
  WSJT-X Improved 3.1.0 added "a8 decoding technology" with NO public
  mechanism documentation. Likely sequenced-QSO-state AP. Requires
  direct source-read of Risse's fork.
  Effort: 1 session research-only
  Source: [WSJT-X Improved release notes](https://wsjt-x-improved.sourceforge.io/)

- `hb-240 candidate — Multi-point sub-sample DT refinement during
  subtraction` (PROBE/PROD)
  21-point at 5-sample spacing (range=50, steps=10). Bodiya measured
  net +80 decodes / 4314 = ~1.8% on crowded bands.
  Effort: ~1 session; pancetta has hb-044 parabolic but only at sync
  candidate level, not during subtraction.
  Source: [wsjtr internal_pass_enhancement.md](https://github.com/bodiya/wsjtr/blob/main/docs/internal_pass_enhancement.md)

- `hb-241 candidate — Per-pass parameter variation + skip-on-zero`
  (PROBE/PROD)
  WSJT-X varies `imetric` across passes (pass 1 = amplitude, pass 2-3
  = power) AND syncmin (1.3 ndepth>2; 2.1 ndepth≤2). Also skips pass 3
  if ndecodes==0 on prior — cheap CPU save. **Confirms pancetta's
  hb-228 3-method sweep is on the right path** — Bodiya independently
  identified "carbon-copy passes" as the limit.
  Effort: ~1 session for the skip-on-zero; the variation overlaps
  with hb-228.

### Other meaningful findings

- **Polar codes for FT8 (Logan N5RLD)** — replaced LDPC(174,91) with
  Polar(174,91), shortened from 256-bit. "~1 dB gain in certain
  conditions." K1JT's response: "requires heavy AP usage, underperforms
  in cold decoding." Not actionable for pancetta (over-the-air format
  break).
  Source: [wsjt-devel msg28544](http://www.mail-archive.com/wsjt-devel@lists.sourceforge.net/msg28544.html)

- **GPU/OpenCL OSD** (Logan N5RLD May 2026) — brute-force OSD search
  on GPU. "Modest increase in successful decodes" + "smooth-outs FER
  curve, especially near lower end." Pancetta is CPU-bound by M4
  design; not chasing unless concrete patch lands.

- **`a7-subtract.patch` + `pass4.patch`** (Bodiya, Feb 2026) — inside
  A7 decode loop, subtract each freshly decoded A7 signal from
  residual. Measured: ~0.5% additional decodes on crowded bands at
  negligible CPU. Cross-confirms pancetta's ft8mon-extracted hb-222.

- **WSJT-X Improved FDR (False Decode Reduction)** — per-message-type
  ship gate. Convergent with pancetta's Batch 32 is_plausible
  extensions (DXpedition + FreeText rejected). Confirms approach.

- **Mainline WSJT-X 3.0/3.0.1** — parallelism (up to 12 threads), NOT
  sensitivity. K1JT/K9AN are NOT pursuing structural decoder rewrites.
  Pancetta's gains will come from passes/sync/AP work.

### Strategic readout

1. **Pancetta + Bodiya's wsjtr are pursuing decoder R&D mainline
   isn't.** Competitive frontier is Improved fork + 2 Rust experiments.
2. **Pancetta's hb-218 capture-effect line is net-new in FT8
   ecosystem.** No public competitor is pursuing it.
3. **Pancetta's mp=2 + ldpc=200 ship matches DG2YCB's 99.5%-yield-at-
   2-stage claim.** On the right side of the cost/yield Pareto.
4. **The 3-method spectral sweep (pancetta's hb-228) is corroborated**
   — Bodiya independently found "carbon-copy passes" are the limit.

### Recommended bank actions

1. **Add hb-237** (cross-sequence A7) at priority 0.60 — top new
2. **Add hb-238** (OSD dmin audit) at priority 0.50 — high-leverage FP
3. **Add hb-239** (a8 investigation) at priority 0.35 — research
4. **Add hb-240** (multi-point DT refinement) at priority 0.30
5. **Add hb-241** (per-pass variation + skip-on-zero) at priority 0.25
