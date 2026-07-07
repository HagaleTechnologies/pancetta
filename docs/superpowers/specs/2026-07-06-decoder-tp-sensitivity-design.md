# Decoder true-positive sensitivity overhaul: findings + design

**Date:** 2026-07-06
**Status:** Approved for planning (operator-commissioned Fable 5 architecture review, this session)
**Scope crates:** `pancetta-ft8` (primary), `pancetta-research`, `pancetta` (coordinator window
capture), `pancetta-dsp` (capture window)
**Companion plan:** `docs/superpowers/plans/2026-07-06-decoder-tp-sensitivity.md`
**Peer effort:** `docs/superpowers/specs/2026-07-06-decoder-speed-overhaul-design.md` (CPU
efficiency, separate session). This spec is about *sensitivity* — recovering true decodes the
current approach structurally cannot reach. Coordination notes in Section 9.

## 1. Executive summary

A five-domain architecture review (sync front-end, demod/LLR path, FEC stack, multi-pass
orchestration, evaluation methodology) concluded:

> The approach is credible and unusually well-instrumented, but it is **ft8_lib-shaped, not
> WSJT-X-shaped**. The decoder currently trades away the three things WSJT-X's last ~3 dB
> comes from — per-candidate fine sync, OSD, and high-fidelity subtraction — and the harness
> cannot currently see the difference between fixing that and hallucinating.

Ground truth from our own scorecards: pancetta recovers **~59.3% of what `jt9 -d 3` decodes on
hard-200** (`research/scorecards/main.json`, 2026-06-06: 5,253/8,853). The public "+11.6% vs
ft8_lib" figure is real but is measured against a weaker reference and includes pancetta-only
"novels" of which our own cross-validation estimated **~35% likely false**
(`research/experiments/2026-05-23-cross-validate-novels.md`).

Five workstreams, in priority order:

| # | Workstream | Expected effect | Effort |
|---|------------|-----------------|--------|
| 0 | Measurement trust: FP-on-noise tier, 2500 Hz SNR calibration, novel accounting, FT4 tier | Makes every other claim falsifiable | S–M |
| 1 | Correctness bugs: AP4 i3 bits, FT4 AP whitening, fallback Hann window, whitening units, neural-OSD sort key | Small direct TP gains; removes silent degradation | S |
| 2 | Signal-domain acceptance metric → re-enable OSD-2/3, relax post-CRC gates | Largest cheap TP gain (WSJT-X's −22…−24 dB band) | M |
| 3 | Per-candidate fine sync + matched-filter demod (wire `baseband.rs`), nsym combining | ~1.5–2.5 dB sensitivity — the big lever | L |
| 4 | GFSK-faithful time-domain subtraction + real multipass | Crowded-band capture-effect bucket | M–L |
| 5 | Candidate pipeline restructure: per-bin selection, normalized threshold, dt range | Unblocks previously-reverted sensitivity features | M |

A recurring pattern motivates Workstream 5 especially: **the right mechanisms are repeatedly
already built in-tree, then shelved behind default-off flags after A/B tests that failed for
interaction reasons** (usually flat-cap candidate displacement), not because the mechanism was
unsound. `baseband.rs` (the WSJT-X-style fine-sync front end) is built, tested, and unwired;
`costas_two_baseline` (percentile-normalized sync) is built and off; the partial B+C slot-edge
metric was reverted at −18 TP because its extra candidates displaced real ones under the
200-candidate cap (`decoder.rs:1211-1220`).

## 2. Findings: evaluation methodology (Workstream 0)

The harness reliably detects *recall* changes vs a jt9 oracle with bootstrap CIs — genuinely
better than most non-mainline decoders' evaluation. It **cannot detect false-positive
regressions**, and its synthetic SNR axis is not comparable to the field standard.

- **No false-decode measurement on noise exists.** `dead_band_425.manifest.json` and
  `sparse_419.manifest.json` have 0 entries; `SnrBin.fp` is hard-coded 0
  (`pancetta-research/src/eval.rs:849`); `false_positives_total` is declared but never
  populated (`scorecard.rs:74`). A hallucinating decoder and a better decoder score
  identically today.
- **`RegressionFlags` is theater**: written as `RegressionFlags::default()` at
  `eval.rs:1856`, computed nowhere. `fixture_regression: false` and
  `snr_curve_regression_db: 0.0` in main.json are constants, not measurements.
- **Novels are unpenalized.** `novel_decodes` carries no weight in the composite
  (`metrics.rs:7-14`). The callsign-continuity FP filter exists
  (`pancetta-research/src/fp_filter.rs`, measured "−21.7% novels at −0.02% recall") and is
  off (`fp_filter_active: false` in main.json).
- **Synth SNR is full-band (6 kHz Nyquist), not the 2500 Hz WSJT-X convention**
  (`gen_synth.rs:100-111`): `noise_rms = signal_rms / 10^(snr/20)` over the whole buffer.
  The ~3.8 dB discrepancy (10·log10(6000/2500)) means our recorded "SNR@50% = −20 dB" is
  ≈ **−16.2 dB in WSJT-X units** — likely well short of jt9's ≈ −21 dB, and we can't tell.
  Later probes got the convention right (`examples/batch62_soft_combiner_repeats.rs:66`,
  `batch98_bicm_id_gated.rs:104-106`) but the scorecard tier was never retrofitted. Steps
  are 2 dB with n=6 messages per bin — the standard sensitivity curve (0.5 dB steps,
  ≥1000 trials/point) cannot be produced.
- **FT4 decode sensitivity is entirely unmeasured**: encode round-trips only
  (`round_trip_tests.rs:484+`), no FT4 WAVs, `pancetta-research/src/mode.rs:5-8` is
  FT8-only. The decoder ships FT4 paths whose recall could be anything.
- Misc: `jtdx_decoded` is always 0 (JTDX never actually run, `scorecard.rs:96-102`);
  main.json was generated from a dirty tree on an experiment branch; the "doppler" tier is
  a multiplicative-cosine artifact, not frequency translation (`gen_synth.rs:79-97`);
  Watterson/LP1 fading is an acknowledged TODO (`synth.rs:17`).

**Design decision (D0):** Before any sensitivity work is merged, the harness gains
(a) a seeded pure-noise + birdie tier where any decode counts as a false positive, with a hard
gate; (b) 2500 Hz-referenced synth SNR at 1 dB steps with n ≥ 50 per point (0.5 dB when the
budget allows); (c) novel-decode classification via the existing continuity filter, reported
in every scorecard and gated (unverified-novel growth fails the gate); (d) computed
`RegressionFlags`; (e) a minimal FT4 synthetic tier. The standing A/B gate for all subsequent
flag-flips becomes: **ΔTP ≥ threshold with bootstrap CI excluding zero, ΔFP-on-noise = 0,
Δunverified-novels ≤ 2× ΔTP** (matching the speed-overhaul plan's gate class, extended with
the noise tier).

## 3. Findings: demodulation and fine sync (Workstream 3 — the big dB lever)

The primary decode path extracts symbol magnitudes (dB, phase discarded) directly from the
shared spectrogram: 3.125 Hz × 80 ms grid, 320 ms (2-symbol) sin² analysis window
(`decoder.rs:3390-3520`, `par_extract_symbols_from_spectrogram` at 8228).

- **Nothing in the default pipeline estimates frequency finer than 3.125 Hz.** Worst-case
  residual CFO ±1.56 Hz — a quarter of the 6.25 Hz tone spacing — eaten directly as
  scalloping loss + inter-tone leakage in the LLRs. There is no parabolic refinement on the
  frequency axis anywhere on the live path.
- The 2-symbol window integrates each symbol's neighbors (ISI), and the two TIME_OSR
  substeps are averaged **in dB** (`(db_a+db_b)/2`, `decoder.rs:8255`) — a geometric mean of
  powers. Fractional-time interpolation is also dB-domain by default
  (`sync_time_interp_linear_power: false`, 1116).
- The **fine-FFT fallback** (7226-7367) does not compensate: gated at `sync_score ≥ 3.5`
  (excluding exactly the weak candidates that need it); frequency trials only 0/±3.125 Hz
  (no sub-bin step); winner chosen by CRC-pass (cannot improve a close-but-undecodable
  trial); costs 21 full LDPC runs; and **applies a Hann window to the symbol-length FFT**
  (`decoder.rs:1616-1618` window, extraction at 6215-6296/8856+) — destroying the tone
  orthogonality a rectangular symbol window provides (~1.8 dB coherent loss + −6 dB
  leakage into adjacent tones) on the very path meant to rescue marginal candidates.
- **`baseband.rs` implements exactly the missing front end** — complex mix → Kaiser FIR →
  decimate ×60 → 200 Hz, 32 samples/symbol — and is "standalone and unwired"
  (`baseband.rs:1-16`, hb-243 Phase 1). The per-candidate frequency tracker
  (`freq_tracker.rs`, Costas-pilot drift correction) is likewise fully plumbed and off
  (`per_candidate_freq_tracker_enabled: false`, 1291).
- **No multi-symbol noncoherent combining** (WSJT-X-style nsym=2/3 Gray-hypothesis LLR
  variants, each a separate BP attempt; documented ~0.5–1 dB on stable signals). Complex
  spectrogram bins are already retained by default (`cross_cycle_coherent: true`), so
  within-transmission 2–3-symbol coherent combining is cheap to attempt.
- Rescue passes (multipass SIC rounds, joint-pair retry, cross-cycle, a7) demodulate only
  via the coarse spectrogram path — no fine-FFT fallback, no both-freq_sub trial
  (5122-5136 vs 7044-7049) — so rescue-pass candidates get a strictly weaker demodulator
  than pass-1 candidates.

**Design decision (D3):** Wire a per-candidate fine-sync + matched-demod stage: downconvert
each surviving candidate to 200 Hz complex baseband (reuse `baseband.rs`), maximize Costas
sync power over a fine dt/df grid (sub-symbol dt, sub-Hz df — clean-room technique reference:
`research/specs/spec-wsjtx-mainline-ft8b.md`, `spec-ft8mon-sub-bin-costas.md`,
`spec-wsjtx-improved-subsample-dt-refinement.md`), then extract symbols with **rectangular
one-symbol DFTs** (32-point at 200 Hz) keeping complex values. This *replaces* the 21-LDPC
fallback (one BP attempt at the refined position instead of 21 blind ones — likely cheaper
AND better; coordinate with the speed plan's stage architecture). Add nsym=2/3 noncoherent
combining LLR variants as additional BP attempts behind a flag. Drop the Hann window from any
retained symbol-FFT path. Convert dB-domain averaging/interpolation to linear power.

## 4. Findings: FEC stack (Workstream 2 + bug fixes)

BP itself is fine — layered sum-product (not min-sum), 100 iterations, per-iteration early
exit (`decoder.rs:9937`, 10441-10550) — at or above mainline quality. The losses are around it:

- **OSD is amputated**: `osd_depth: Some(0)` in production (`decoder.rs:1077`) after Batch 73
  measured ~7,000 spurious FPs for zero net TP. Root cause is not "OSD doesn't work": our OSD
  accepts the **first** CRC-14-passing candidate out of up to 121,485 flip patterns with **no
  distance/acceptance metric at all** (`osd.rs:736-769`). At CRC-14's 2⁻¹⁴ collision rate,
  4,095 order-2 trials per failed frame makes false passes a statistical certainty. The
  standard mitigations (weighted soft distance to received sequence with acceptance
  threshold; best-candidate-not-first selection; per-depth gating — see
  `research/specs/spec-wsjtx-mainline-osd174.md`, `spec-wsjtx-improved-fdr.md`) are absent.
  OSD is where WSJT-X's deep-fade (−22…−24 dB) decodes come from; this is the largest
  recoverable TP pool per unit effort.
- **OSD runs on BP-posterior LLRs** (`decoder.rs:10141-10149`), which are
  overconfident-in-the-wrong-direction after BP fails in a trapping set. Channel LLRs (or a
  swept `bp_offset_subtract`, currently 0.0) should be evaluated.
- **All FP control is message-content heuristics** (`is_plausible()`, `suspicion_score()`,
  sync-confidence floors) — none is signal-domain evidence. No re-encode-and-correlate
  check, no soft-distance gate. Note `known_coherence_score` (`decoder.rs:8085`) already
  computes a coherent re-encoded-signal score for subtraction alignment — a signal-domain
  acceptance metric can be built on it plus an LLR-domain soft distance.
- **Post-CRC confidence floor discards true decodes**: `confidence = sync_score/12` with
  floor `MIN_DECODE_CONFIDENCE = 0.41` (`decoder.rs:7186-7193`) throws away CRC-valid,
  plausibility-checked messages with sync between the 3.0 admission gate and 4.92. The
  serial twin `decode_candidate` applies **no** floor (5892) — divergent behavior.
- **AP4 injection bug**: injects i3 bits 74-76 as **0,0,0** (`ap.rs:459-462`) claiming
  RR73, but RR73/RRR/73 are **i3=1** (parse at `message.rs:1395-1396`); i3=0 is the
  FreeText/Telemetry family which `is_plausible()` unconditionally rejects. AP4 can never
  succeed and actively fights the expected message. WSJT-X-equivalent AP passes 4-6 (full
  RRR/RR73/73 message masks) are effectively absent — the highest-yield AP in a live QSO.
- **FT4 AP ignores XOR whitening**: FT4 payloads are scrambled pre-LDPC
  (`FT4_XOR_SEQUENCE`, un-XOR after decode at `decoder.rs:4601-4619`), but AP injects raw
  callsign bits — wrong priors at ~half the injected positions. AP is not gated to FT8.
- AP magnitude ±15 is injected **before** `normalize_llrs` (`decoder.rs:4583, 7593`), so
  the prior's effective strength floats with signal level. No "CQ ? ?" mask exists
  (cheap + safe given `ap_injection_survived` already verifies content). AP floor 0.55
  double-taxes given the injection-survival check.
- **Neural-OSD ordering key inverted for parity bits** (`osd.rs:523-538`): parity key
  −|LLR| under a descending sort ranks parity bits by *ascending* reliability, and the
  [0,1] CNN probability scale is incommensurable with unbounded |LLR|. Moot at depth 0;
  must be fixed before OSD re-enable. The CNN also runs per failed frame at depth 0 —
  pure cost (peer speed spec already tracks this).

**Design decision (D2):** Build one **signal-domain acceptance metric** and use it
everywhere: (a) LLR-domain weighted soft distance `d = Σ_{sign mismatch} |llr_i| / Σ|llr_i|`
plus hard-error count vs received hard decisions; (b) optional coherent re-encode correlation
reusing `known_coherence_score`. Calibrate thresholds on the Workstream-0 noise tier +
hard-200 at a target FDR (`spec-wsjtx-improved-fdr.md`). Then: OSD returns the
**best-by-distance** CRC-valid candidate with acceptance gating; A/B re-enable `osd_depth: 2`
(then 3 + npre2); evaluate channel-LLR OSD input; replace the 0.41 post-CRC sync floor and
relax the 0.55 AP floor with the acceptance metric; fix AP4/FT4-XOR/CQ-mask along the way.

## 5. Findings: subtraction and multipass (Workstream 4)

Multi-pass subtract-and-redecode — a major fraction of mainline's crowded-band yield — is
architecturally abandoned: `max_decode_passes: 1` (`decoder.rs:1061`), with the doc-comment
admitting passes 2+ "contribute essentially nothing" (186-192). Both subtraction mechanisms
are too crude to pay:

- **Dead time-domain path** (`subtract_signal` 3140, `subtract_with_sidelobes` 3274,
  unreachable at one pass): re-synthesizes **rectangular CPFSK — no GFSK BT=2.0 pulse
  shaping** — and fits **one complex amplitude for the whole 79-symbol signal**
  (3217-3241). The `subtract_with_sidelobes` ±6.25 Hz replicas at 0.135 scale are a crude
  proxy for the leakage the missing pulse shaping creates. Note **the TX modulator already
  implements `PulseShape::Gaussian { bt: 2.0 }`** (`modulator.rs:39-52`) — the faithful
  reference generator exists in-crate and is simply not used for subtraction.
- **Live spectrogram-domain SIC** (`subtract_decode_coherent` 8007): no waveform
  resynthesis; per decode, one global phase rotor from the 21 Costas symbols, projecting
  out only the single on-tone bin at the candidate's own freq_sub per symbol
  (8050-8058). Residuals left behind: off-grid leakage into the other freq_sub and
  adjacent bins; cross-symbol energy in straddling STFT frames; phase-drift residue at the
  transmission edges (no time-varying amplitude/phase estimate); below-noise-floor holes
  at subtracted cells. The spectrogram is never recomputed from audio after subtraction.
  The shelved `joint_residual_sync_relax` experiment ("surfaces noise", 1152-1156) is the
  symptom: the residual near subtracted signals is undecodable.
- Fine-path decodes with off-grid dt/df get **re-quantized to the grid** before
  subtraction (`reverse_derive_candidate` 7973) — subtraction alignment error by design.

**Design decision (D4):** Rebuild time-domain subtraction with fidelity: reference waveform
from the existing GFSK modulator at the decode's refined dt/df; **time-varying complex
amplitude/phase estimate** (low-pass-smoothed over ~symbol scale — clean-room reference:
`research/specs/spec-wsjtx-mainline-subtractft8.md`, plus existing
`spec-ft8mon-gaussian-ramp-subtract.md`); subtract in audio; **recompute the spectrogram**;
re-enable `max_decode_passes: 2-3` behind the standing A/B gate. Keep the cheap spectrogram
SIC for intra-pass use. Primary evaluation corpus: `synth_pair_200` (two-signal adversarial
ΔSNR/Δf/Δt grid) + hard-200. Depends on D3's refined dt/df for alignment quality; interacts
with the speed budget (subtraction+repass becomes an anytime stage).

## 6. Findings: sync front-end and candidate pipeline (Workstream 5)

- Grid: 3.125 Hz × 80 ms (half-symbol) — matches ft8_lib, coarser in time than mainline's
  quarter-symbol. The half-loop max inside the score kernel creates a known 2-step plateau
  emitting candidates one step early; the fix flag `costas_half_loop_disabled` is default
  OFF (1235).
- **Flat top-200 cap with NMS off** (`nms_enabled: false`, 1085; cap at 4144): every real
  signal emits a *cluster* of above-threshold cells competing for slots; on a busy band,
  strong-signal clusters exhaust the cap before a −19 dB signal's single marginal cell
  lists. This displacement mechanism is documented killing hb-242 (1211-1219) and the
  partial-BC metric (−18 TP, 1220). Mainline avoids it with per-frequency-bin peak
  selection rather than a global top-N (`spec-wsjtx-mainline-sync8.md`,
  `spec-jtdx-ncandthin-dt-thinning.md`).
- **Absolute threshold** `MIN_SYNC_SCORE = 3.0` rather than noise-relative; the
  percentile-baseline normalization is implemented (`costas_two_baseline`, 3899-4136,
  `spec-wsjtr-sync-norm.md`) and OFF.
- **dt floor is −0.16 s** (`SLIDING_FRAME_LOOKBACK_STEPS = 2`, offset at 100-103;
  `time_padding` always 0) vs mainline's ≈ −2.5 s sweep; the slot-edge bucket sits at
  **48.3% recall** by our own comment (1209-1210). Late signals whose 79-symbol span
  exceeds the buffer are excluded from the sweep entirely (`max_time_step` at 3866) even
  with 2 of 3 Costas blocks in-buffer. `WINDOW_SAMPLES` is exactly 12.64 s — zero slack
  around the transmission span; the wild-50 0/96 fiasco
  (`experiments/2026-05-23-wild-50-zero-overlap.md`) showed how capture misalignment
  dominates a tier. `Ft8Config::time_range` (default 2.0) is **dead config — never read**.
- Costas score freq-neighbors sit at ±6.25 Hz, so a strong adjacent signal drives the
  neighbor-difference score negative and suppresses a decodable weak neighbor; a steady
  carrier at the expected bin scores positively (birdies generate candidates that squeeze
  the cap).
- FT4: correct constants (4 Costas arrays, positions, XOR — `protocol.rs:118-140`), but no
  protocol-specific tuning: same absolute 3.0 threshold on a noisier 16-symbol score,
  10.4 Hz granularity vs 20.8 Hz tone spacing (same ¼-tone worst case), BT=1.0 inter-tone
  smearing unmodeled. Sensitivity parity is unverified (see Workstream 0 FT4 tier).

**Design decision (D5):** Restructure candidate selection to remove cap displacement:
per-frequency-bin peak selection (top-K per bin across time, then global cap), pathway
quotas for auxiliary candidate sources, and the percentile-normalized threshold
(`costas_two_baseline` retest). Extend the capture window (coordinator/pancetta-dsp) to
cover dt ∈ [−1.0 s, +2.5 s] with real audio (front history, not zero padding). Then
**re-run the shelved sync experiments** that the flat cap previously killed
(`costas_partial_metric`, `costas_half_loop_disabled`, `costas_two_baseline`,
`dt_history`, `relaxed_sync_near_partner`) under the new selection scheme. Delete or wire
`time_range`.

## 7. Cross-cutting cleanups (fold into workstream tasks)

- `whiten_llrs` operates on dB log-powers on the spectrogram path and linear |y| on the
  fine-FFT path — same function, dimensionally different behavior; with negative dB values
  the 1e-6 floor degenerates it to a no-op, so its measured benefit is **input-gain
  dependent** (9217-9312). Rework to linear power with a gain-invariance test.
- `normalize_llrs` variance targeting is inflated by a strong in-band interferer
  (compressing all 174 LLRs); `impulse_robust_llr` exists to fix exactly this and ships
  OFF (1340). Retest under Workstream 0 gates.
- `decode_candidate` (serial) vs `par_decode_candidate` are divergent twins (no confidence
  floor / soft combiner / BICM-ID on serial path, 5892 vs 7008+). Unify to one source of
  truth before Workstream 2 changes gates.
- Doc drift: `max_sync_candidates` doc says 100 vs actual 200 (207-212); LLR variance doc
  says 24 vs actual 32 (9159 vs 111); AP floor comment says 0.50 vs actual 0.55 (4649).
- Cross-cycle averaging groups **geometrically only** — no content check before summing
  members (7866-7915); measured +8 novels alongside +14 recovered. Add a cheap content
  guard (e.g., LLR-sign correlation between members) or subject its output to the D2
  acceptance metric.
- a7 (default OFF, correctly): emits template text **bypassing LDPC and CRC**
  (`decoder.rs:5599-5604`) with the snr7b discriminator operating at its structural
  ceiling (threshold 1.8 vs ceiling ~1.8-2.0, a7.rs:541-544), and a hard-coded fallback
  callsign bank (K1ABC, W1AW… a7.rs:211-217) that can fabricate pairings. If a7 is ever
  enabled: require real QSO context, never the fallback bank, and gate through the D2
  acceptance metric + the noise tier.

## 8. Explicit non-goals

- No new decode features beyond the six workstreams (no Watterson channel simulator beyond
  the eval-tier TODO note, no GPU, no JT65/Q65).
- No CPU-efficiency work here — that is the peer speed-overhaul spec. Where a sensitivity
  stage adds cost, it must be expressible as a stage in the speed plan's budget-governed
  anytime architecture, not a fixed cost.
- No licensing changes: WSJT-X/JTDX/wsjtr/ft8mon/MSHV remain GPL — **no task may read their
  source**. All mainline techniques referenced here go through the existing clean-room
  technique specs in `research/specs/` (already written for sync8, ft8b, osd174,
  subtractft8, a7, FDR, etc.).

## 9. Coordination with the speed-overhaul effort

Same primary file (`decoder.rs`), same season. Sequencing agreement proposed:

1. Speed Phase 1 (mechanical fixes) is orthogonal — proceed in parallel; rebase cost is low.
2. Sensitivity Workstream 0 (harness) and Workstream 1 (bug fixes) are orthogonal to the
   speed plan — proceed immediately.
3. Sensitivity Workstreams 2–5 land as **stages** in the speed plan's Phase 2 anytime
   architecture where they add per-candidate cost (fine sync, OSD depth, subtraction
   rounds). The fine-sync stage *replaces* the 21-LDPC fallback and may be net cheaper.
4. The eval-harness gates from Workstream 0 supersede/extend the speed plan's A/B gate
   definition (adds the FP-on-noise and unverified-novel terms). Both plans should cite
   the same gate.

## 10. Success criteria

- **Primary:** hard-200 recall vs `jt9 -d 3` truth rises from 59.3% with
  FP-on-noise = 0 and unverified-novel rate not increasing. Every workstream states its
  own A/B gate in the plan.
- **Secondary:** 2500 Hz-calibrated synth SNR@50% within 1 dB of jt9 on the same corpus
  (measure jt9's own curve with the same generator first); synth_pair_200 recovery up
  materially after Workstream 4; FT4 tier exists with a recorded baseline.
- **Honesty:** decoder-comparison.md headline updated to jt9-referenced numbers with
  verified-novel accounting once Workstream 0 lands.
