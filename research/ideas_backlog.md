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

(none yet from this backlog)

## Killed (during brainstorm; not worth a probe)

(none yet)
