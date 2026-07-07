# Decoder True-Positive Sensitivity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Raise true-positive decode recall toward `jt9 -d 3` parity (from today's 59.3% on hard-200) by fixing the evaluation harness so gains are falsifiable, then landing five sensitivity workstreams: correctness bugs, a signal-domain acceptance metric that lets OSD come back, per-candidate fine sync + matched demodulation, GFSK-faithful subtraction with real multipass, and a candidate pipeline that stops displacing weak signals.

**Architecture:** Workstream 0 makes the research harness FP-aware and SNR-calibrated (all later A/B gates depend on it). Workstream 1 is small bounded bug fixes. Workstreams 2–5 each follow the standing pattern: implement behind a default-off `Ft8Config` flag → A/B on the research harness → flip the default. Mainline (GPL) techniques are consumed only via the existing clean-room specs in `research/specs/`.

**Tech Stack:** Rust workspace (existing), `pancetta-research` eval harness, `pancetta-ft8` (`decoder.rs`, `osd.rs`, `ap.rs`, `baseband.rs`, `modulator.rs`), jt9/ft8_lib as oracles.

**Spec:** `docs/superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md` — **read it first.** Section numbers below (D0–D5) refer to its design decisions.

## Global Constraints

- **Licensing / clean room:** ft8_lib (vendored, `pancetta-ft8/vendor/ft8_lib`) is MIT — constants/techniques usable directly with citation comment. WSJT-X / wsjtr / JTDX / ft8mon / MSHV are GPL — **NO task may read their source.** Every mainline technique needed here already has a clean-room spec in `research/specs/` (cited per task). If a spec is insufficient, stop and use the Reader→spec→Implementer firewall; do not improvise from memory of GPL code.
- **Gating classes:**
  - **[HARNESS]** — `pancetta-research`/docs only, no decoder behavior change. Gate: `cargo test -p pancetta-research` + a smoke eval run.
  - **[BIT-EXACT]** — decoder refactor with unchanged decode output. Gate: `cargo test --workspace --features transmit` with unchanged decode counts on the WAV fixtures.
  - **[A/B]** — behavior change. Implement behind a default-off `Ft8Config` flag; flip only after the **standing TP gate**: bootstrap-CI recall delta on hard-200 (hard-1000 for flips claiming <1% effects) excludes zero in favor, **FP-on-noise tier = 0 new decodes**, unverified-novel count increase ≤ 2× verified-TP increase, and the speed plan's elapsed hard gate. Tasks before W0.1 lands may not flip any [A/B] default.
- **Every A/B result gets an experiment log** in `research/experiments/YYYY-MM-DD-<name>.md` (existing convention: hypothesis, config, numbers, verdict) and a scorecard history entry.
- **Config-merge rule:** any new config struct/field MUST have its `merge_with` line and a regression test in the same task (2026-07-05 bug class).
- **Subagent rules (standing):** implementers never push / never destructive git; controller pushes at batch boundaries. `cargo fmt` + `cargo clippy` before each commit.
- **Coordination with the speed overhaul** (`docs/superpowers/plans/2026-07-06-decoder-speed-overhaul.md`): W0/W1 proceed in parallel with speed Phase 1. W2–W5 stages that add per-candidate cost must be schedulable in the speed plan's anytime architecture (its Phase 2). If the speed plan's `decode_window` restructure has landed, new stages plug into it; if not, keep stage boundaries as plain functions so the restructure can absorb them. Same `decoder.rs` — rebase early, rebase often.
- **Don't execute this plan in the authoring session.** Operator will start a fresh session in the pancetta repo.

---

## Workstream 0 — Measurement trust (prerequisite for all [A/B] flips)

### Task W0.1: FP-on-noise tier [HARNESS]

Any decode on signal-free audio is a false positive. This tier is the guardrail every later flip cites.

**Files:**
- Create: `pancetta-research/src/gen_noise.rs`
- Modify: `pancetta-research/src/eval.rs` (new tier; populate `SnrBin.fp` and `false_positives_total`), `pancetta-research/src/scorecard.rs` (wire fields), `pancetta-research/src/lib.rs` (module)
- Test: `pancetta-research/tests/noise_tier_tests.rs`

**Interfaces:**
- Produces: `gen_noise::generate_noise_corpus(dir: &Path, config: &NoiseConfig) -> Result<Vec<PathBuf>>` where `NoiseConfig { count: usize, seed: u64, birdie_fraction: f32 }` — 15 s, 12 kHz mono WAVs: seeded white Gaussian noise; `birdie_fraction` of files additionally get 1–3 steady sine carriers (random freq 300–2900 Hz, random level 0 to +20 dB over noise floor) and one slowly drifting carrier (±0.5 Hz/s). Deterministic for a given seed.
- Produces: eval tier `noise_1000` → scorecard fields `false_positives_total: u32`, `noise_files_decoded: u32` (any nonzero fails `compare`).

**Steps:**
- [x] Write `gen_noise.rs` with a determinism unit test (same seed → byte-identical WAV) and a sanity test (RMS within 5% of target).
- [x] Add the eval tier: decode every noise WAV with production `Ft8Config::default()`; every returned message is an FP. Record per-file and total. Write failing integration test first (tier over 5 generated files, asserts fields populated), then implement.
- [x] Wire `false_positives_total` into `Scorecard` serialization and into the `compare` binary as a **hard gate** (any increase = FAIL, printed prominently).
- [x] Generate the real corpus: `count: 1000, seed: 20260706, birdie_fraction: 0.3` under `~/.pancetta/recordings/noise_1000/`, manifest with SHA-256s at `research/corpus/curated/noise/noise_1000.manifest.json` (same pattern as hard_200). Delete the empty `dead_band_425` / `sparse_419` leftovers or regenerate them for real.
- [x] Run the tier on current production config; record the baseline number in `research/experiments/2026-07-07-noise-tier-baseline.md` and in the scorecard. **Result: 3 FPs / 1000 WAVs — NOT 0. See log for honest root-cause discussion (plausibly CRC-14 chance-passes at the trial volume, not a harness bug); this is now the reference baseline the standing gate compares against.**
- [x] Commit.

### Task W0.2: 2500 Hz SNR calibration + real sensitivity curve [HARNESS]

**Files:**
- Modify: `pancetta-research/src/gen_synth.rs:100-111` (noise scaling), `pancetta-research/src/eval.rs:853-855` (SNR@50% interpolation), `research/corpus/synth/manifests/clean.config.json`, `pancetta-research/src/metrics.rs` (corpus-refresh offset entry)
- Test: `pancetta-research/tests/snr_calibration_tests.rs`

**Interfaces:**
- Produces: synth WAVs whose stated SNR follows the WSJT-X convention (signal power / noise power in 2500 Hz reference bandwidth). With white noise spanning 0–6000 Hz: `noise_rms = signal_rms / 10^(snr/20) * sqrt(6000.0/2500.0)` (i.e., +3.8 dB more noise than today for the same label). Cite the convention in a doc comment.
- Produces: `snr_at_50pct_recovery_db` computed by **linear interpolation** between the two bins straddling 50%, not "first bin ≥ 50%".

**Steps:**
- [x] Failing test: generate a synth WAV at label −15 dB, measure signal power (windowed, over the 79-symbol span, tone-bin sum) and noise power density × 2500 Hz; assert ratio within ±0.3 dB of label. Then fix the generator.
- [x] Regenerate `clean` manifest: SNR −24 → −14 dB in **1 dB steps**, **n = 50 distinct messages per step** (seeded), randomized base freq 400–2600 Hz and dt ∈ [−0.3, +0.3] s per file (today: fixed 1500 Hz, fixed dt — decoders must not be allowed to overfit a single grid position).
- [x] Record the corpus refresh in `refresh_offsets.json` per the `metrics.rs:78-135` convention so composite history stays comparable.
- [x] Run the jt9 oracle (`pancetta-research/src/bin/baseline.rs`, `jt9 -8 -d 3`) over the new corpus to produce the **reference curve**; store per-bin jt9 recall in the scorecard (`jt9_snr_curve`).
- [x] Run pancetta; record both curves in an experiment log. This number — pancetta SNR@50% minus jt9 SNR@50%, same corpus, same convention — is the headline sensitivity metric for the rest of the plan. **Result: pancetta SNR@50% = −19.214 dB, jt9 SNR@50% = −21.313 dB → headline gap = +2.10 dB (pancetta needs ~2.1 dB more SNR than jt9 for 50% recall). See `research/experiments/2026-07-07-snr-calibration.md`.**
- [x] Commit.

### Task W0.3: Novel-decode accounting + real RegressionFlags [HARNESS]

**Files:**
- Modify: `pancetta-research/src/eval.rs` (wire `fp_filter` classification into every real-corpus tier; compute `RegressionFlags` at `eval.rs:1856` instead of `default()`), `pancetta-research/src/fp_filter.rs` (expose classify-only mode), `pancetta-research/src/scorecard.rs` (`novels_verified: u32`, `novels_unverified: u32`), `pancetta-research/src/bin/compare.rs` (gate)
- Test: extend `pancetta-research/tests/` with a classification fixture test

**Steps:**
- [ ] Report-only wiring: every pancetta-only decode on jt9-truth tiers is classified by callsign continuity (`fp_filter.rs` logic) as verified/unverified; both counts land in the scorecard. **Do not filter decodes** — measure only.
- [ ] Compute `RegressionFlags` for real: `fixture_regression` from the fixtures tier delta, `false_positive_introduced` from W0.1's tier, `snr_curve_regression_db` from W0.2's curve vs the stored baseline. Failing test first (construct two scorecards, assert flags).
- [ ] Add the unverified-novel term to `compare`'s standing gate (≤ 2× verified-TP increase).
- [ ] Commit.

### Task W0.4: FT4 evaluation tier [HARNESS]

**Files:**
- Modify: `pancetta-research/src/mode.rs:5-8` (add `Ft4`), `pancetta-research/src/gen_synth.rs` (FT4 generation via `pancetta-ft8` encoder+modulator with `ProtocolParams::ft4()`), `pancetta-research/src/eval.rs` (tier), `pancetta-research/src/bin/baseline.rs` (oracle)
- Test: `pancetta-research/tests/ft4_tier_tests.rs`

**Steps:**
- [ ] Determine the jt9 CLI flag for FT4 (`jt9 --help` on the installed binary); if jt9 FT4 is unavailable, use the vendored ft8_lib FT4 decode via `pancetta-ft8/src/ft8_lib_ffi.rs` as oracle and note the weaker reference in the tier metadata.
- [ ] Synth FT4 clean tier: same shape as W0.2 (1 dB steps, n=50, 2500 Hz convention, randomized freq/dt), 7.5 s slots.
- [ ] Run pancetta FT4 decode + oracle; record the first-ever FT4 baseline in an experiment log. Expect surprises — FT4 recall has never been measured (spec §2). Any FT4-specific failures found here become tickets, not silent scope creep.
- [ ] Commit.

### Task W0.5: Dead-config and doc-drift cleanup [BIT-EXACT]

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` (delete never-read `Ft8Config::time_range` (~line 184) **or** wire it into the sync sweep — decide by checking `pancetta-config`/TUI for references; fix doc drift: `max_sync_candidates` doc 100→200 at 207-212, LLR variance doc 24→32 at 9159, AP floor comment 0.50→0.55 at 4649)

**Steps:**
- [ ] Grep all crates for `time_range` consumers; delete or wire; add a compile-fail-safe test if wired.
- [ ] Fix the three doc comments. `cargo test --workspace --features transmit` unchanged. Commit.

---

## Workstream 1 — Correctness bugs

### Task W1.1: AP4 injects wrong i3 bits [A/B — but this is a bug fix; flag not required, tests are]

Spec §4. AP4 currently injects payload bits 74-76 = (0,0,0) (`ap.rs:459-462`); RR73/RRR/73 are **i3 = 1** and i3=0 messages are unconditionally rejected by `is_plausible()` — AP4 can never succeed.

**Files:**
- Modify: `pancetta-ft8/src/ap.rs:459-462`
- Test: `pancetta-ft8/tests/ap_i3_tests.rs` (new)

**Steps:**
- [ ] First, verify the bit order: read `message.rs:1395-1396` (i3 parsed from bits 74..77) and the encoder's i3 packing for an RR73 message. Write the failing test **from the encoder side**: encode `"W1ABC K1DEF RR73"`, take the 77 payload bits, assert bits 74..77 == the value AP4 injects. This test fails today (encoder says i3=1, AP4 says 0).
- [ ] Fix the injected bits to i3=1 (LSB/MSB order per the verified parse). Test passes.
- [ ] Add an end-to-end test: synthesize an RR73 signal at −18 dB (reuse `tests/test_signal_generator.rs` helpers), corrupt LLRs enough that AP0 fails but AP4's 3 extra known bits rescue it, assert decode with AP4 provenance.
- [ ] Run hard-200 A/B (expect small positive or neutral; AP4 currently fires only in `WaitingForConfirmation` QSO state, rare in corpus replay). Log the experiment either way. Commit.

### Task W1.2: FT4 AP injection ignores XOR whitening [bug fix]

Spec §4. FT4 payloads are XOR-scrambled pre-LDPC; AP injects raw bits → wrong priors at ~half the positions.

**Files:**
- Modify: `pancetta-ft8/src/ap.rs` (whiten injected bits with `FT4_XOR_SEQUENCE` when protocol is FT4 — thread `&ProtocolParams` or the xor slice into `inject_ap_llrs`)
- Test: `pancetta-ft8/tests/ap_i3_tests.rs` (extend)

**Steps:**
- [ ] Failing test: encode an FT4 message for a known call pair, take the post-XOR codeword bits at AP1's injected positions, assert `inject_ap_llrs` LLR signs match them. Fails today.
- [ ] Fix: XOR the injected callsign bits with the whitening sequence bits at the corresponding payload positions before setting LLR signs (mirror the un-XOR at `decoder.rs:4601-4619` / `par_apply_xor` at 9007).
- [ ] FT8 regression: same test with FT8 params must be unaffected. Commit.

### Task W1.3: Rectangular window on fine-FFT symbol extraction [A/B]

Spec §3. The fallback path Hann-windows a symbol-length FFT, destroying 8-FSK tone orthogonality (~1.8 dB + inter-tone leakage) on the rescue path.

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` (`extract_symbols_complex` ~6215-6296, `par_extract_symbols_complex` ~8856+; new flag `fine_fft_rect_window: bool` default false, `Ft8Config` + merge_with + test)
- Test: `pancetta-ft8/tests/decoder_refinement_tests.rs` (extend)

**Steps:**
- [ ] Unit test proving the physics: synthesize one pure FT8 tone k at exact bin center over one symbol; with rect window assert adjacent tone bins ≤ −40 dB of the on-tone bin; with Hann assert they are ≥ −8 dB (documents the leakage being removed).
- [ ] Implement flag: when set, skip the window multiply (rectangular) in both twins.
- [ ] A/B on hard-200 + synth clean curve (this is exactly where marginal candidates live). Flip default on pass; log. Commit.

### Task W1.4: `whiten_llrs` unit consistency + gain invariance [A/B]

Spec §7. Whitening divides by dB log-powers on one path and linear |y| on the other; behavior depends on absolute input gain.

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs:9217-9312` (operate on linear power on both paths: convert dB→linear before medians, or accept a `MagnitudeDomain` arg from each caller)
- Test: `pancetta-ft8/tests/decoder_refinement_tests.rs` (extend)

**Steps:**
- [ ] Failing property test: decode a fixture WAV at gain 1.0 and gain 0.01 (pre-scaled samples); assert identical decode sets. Investigate whether it fails today (spec predicts the whitening branch flips); record finding in the test comment.
- [ ] Rework to linear power; keep the existing floor semantics on the linear scale. Gain-invariance test passes.
- [ ] A/B: whitening was accepted on a "+4 TP / −713 FP" measurement that may not survive the unit fix — re-measure, keep whichever of {fixed-on, off} wins under the standing gate. Commit.

### Task W1.5: Neural-OSD parity ordering key + depth guard [BIT-EXACT today; matters at W2.4]

**Files:**
- Modify: `pancetta-ft8/src/osd.rs:520-538` (parity bits keyed consistently with info bits: both "most reliable first" on commensurable scales — use rank-normalized keys or map CNN probability p to a pseudo-|LLR| `ln((1-p)/p)`), `pancetta-ft8/src/decoder.rs:10161-10164` (skip CNN inference entirely when `osd_depth < 1` — check the speed plan hasn't already landed this; if it has, verify and skip)
- Test: `pancetta-ft8/src/osd.rs` unit tests (extend)

**Steps:**
- [ ] Failing unit test on the sort: given known per-bit reliabilities for info+parity, assert the permutation puts genuinely-most-reliable positions (of either kind) first. Fix the key. Commit. (Decode output unchanged at depth 0 — assert fixture counts unchanged.)

### Task W1.6: Unify `decode_candidate` / `par_decode_candidate` divergent twins [BIT-EXACT]

Spec §7. The serial twin lacks the confidence floor, suspicion scrutiny, soft combiner, and BICM-ID — whichever path runs decides the gates a decode faces. W2.5 changes gates; do this first so it changes them in one place.

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` (make `decode_candidate` (~5892) delegate to the `par_` implementation, or delete it and route all callers through `par_decode_candidate`; grep callers first — cross-cycle/joint-pair/localized passes use the serial path)
- Test: existing WAV fixture counts must be **unchanged**; if unifying changes counts (because rescue passes suddenly face the 0.41 floor), stop — apply the floor consistently per current serial behavior (no floor) and leave gate changes to W2.5, so this task stays bit-exact.

**Steps:**
- [ ] Map callers, unify mechanically, `cargo test --workspace --features transmit`, fixture counts unchanged, commit.

---

## Workstream 2 — Signal-domain acceptance metric; OSD returns (D2)

### Task W2.1: Acceptance metric [BIT-EXACT — metric computed + logged, not yet gating]

**Files:**
- Create: `pancetta-ft8/src/acceptance.rs`
- Modify: `pancetta-ft8/src/lib.rs` (module), `pancetta-ft8/src/decoder.rs` (compute + attach to `DecodedMessage` as `acceptance: Option<AcceptanceScore>`; populate for every CRC-valid decode)
- Test: `pancetta-ft8/src/acceptance.rs` unit tests

**Interfaces:**
- Produces:
  ```rust
  pub struct AcceptanceScore {
      /// Fraction of |LLR| mass on bits where the codeword disagrees with
      /// the channel hard decisions: sum(|llr_i| where sign mismatch) / sum(|llr_i|).
      pub soft_distance: f32,
      /// Count of hard-decision disagreements over the 174 bits.
      pub hard_errors: u16,
      /// Optional coherent re-encode correlation (reuse known_coherence_score,
      /// decoder.rs:8085) against the candidate's spectrogram region, normalized 0..1.
      pub coherence: Option<f32>,
  }
  pub fn score(codeword: &BitSlice, channel_llrs: &[f32; 174]) -> AcceptanceScore
  ```
  **Channel** LLRs (pre-BP), not posteriors.
- Clean-room references: `research/specs/spec-wsjtx-mainline-osd174.md` (dmin semantics), `spec-wsjtx-improved-fdr.md` (threshold-by-FDR).

**Steps:**
- [ ] TDD the pure function: hand-built 174-bit cases (0 errors → distance 0; all-flipped → 1.0; one high-|LLR| flip vs many low-|LLR| flips ordering).
- [ ] Thread channel LLRs to the acceptance call site for BP, OSD, and AP decodes; attach to `DecodedMessage`. Fixture decode counts unchanged.
- [ ] Calibration run (research side): decode hard-200 + the W0.1 noise tier with `osd_depth: Some(2)` **in a research config** (not default); dump `(soft_distance, hard_errors, coherence, is_jt9_verified)` per decode to CSV via the existing candidate-dump mechanism (`decode_window_with_candidate_dump`). Pick thresholds at FDR ≤ 1% on this data; record the chosen values and the full distributions in `research/experiments/2026-07-XX-acceptance-calibration.md`.
- [ ] Commit.

### Task W2.2: OSD selects best-by-distance, not first-CRC [A/B]

**Files:**
- Modify: `pancetta-ft8/src/osd.rs` (`try_solution` ~736-769 and the order-1/2/3 loops ~608-731: collect all CRC-valid candidates, return the minimum-`soft_distance` one; add `max_soft_distance` / `max_hard_errors` acceptance params to `OsdConfig` from W2.1 calibration), `pancetta-ft8/src/decoder.rs` (pass channel LLRs into OSD for scoring)
- Test: `pancetta-ft8/tests/osd_tests.rs` (extend)

**Steps:**
- [ ] Failing test on selection: construct LLRs where an early flip pattern yields a CRC-collision codeword at large soft distance and a later pattern yields the true codeword at small distance (build by encoding a real message, adding noise, plus a known CRC-colliding pattern found by brute-force search in the test — CRC-14 collisions are easy to mine offline); assert the true codeword wins.
- [ ] Implement collect-and-rank + acceptance gate. Early-out is allowed once a candidate is below an "accept immediately" distance (calibrated), to bound cost.
- [ ] Verify: with acceptance thresholds set, re-run the Batch-73 scenario (hard-200 at `osd_depth: Some(2)`) — spurious FPs must collapse from ~7,000 toward ~0 **on the noise tier and the unverified-novel count** while retaining TPs. This is the pivotal measurement of the whole workstream; log thoroughly. Commit.

### Task W2.3: OSD input LLRs — channel vs BP-posterior [A/B experiment]

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs:10141-10149` (config enum `osd_input: BpPosterior | Channel | OffsetSubtracted(f32)`; default current behavior)
- Test: config merge test

**Steps:**
- [ ] Implement the three-way switch; A/B all three at `osd_depth 2` with W2.2 acceptance on hard-200 (+ synth curve). Keep the winner as default; log. Commit.

### Task W2.4: Re-enable OSD in production [A/B flip]

**Steps:**
- [ ] Flip `osd_depth: Some(0)` → `Some(2)` (`decoder.rs:1077`) under the standing gate (needs W0.1–W0.3 + W2.1–W2.3 + W1.5). Then evaluate `Some(3)` + `osd_npre2_preprocessing_enabled: true` — **but first** verify `npre2_residual_signature` (`osd.rs:432-465`, currently ignores two args) against `spec-wsjtx-mainline-osd174.md`; fix or delete before enabling. Wall-clock: OSD cost must ride the speed plan's budget stage, not a fixed add. Log + commit each flip separately.

### Task W2.5: Replace blunt post-CRC gates with the acceptance metric [A/B]

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` (AP0 path 7186-7201: drop `MIN_DECODE_CONFIDENCE 0.41` sync floor for decodes passing acceptance thresholds; keep suspicion scrutiny for borderline acceptance), `pancetta-ft8/src/decoder.rs:4649/7563+` (AP floor 0.55 → 0.41 when `ap_injection_survived` AND acceptance passes)
- Test: end-to-end low-sync synthetic (true signal at sync ~4.0 that CRC-decodes) accepted after change, rejected before

**Steps:**
- [ ] Implement behind `acceptance_gating_enabled: bool` (default false) replacing both floors; A/B; the expected gain is precisely the sync-3.0-to-4.92 band the sync search admits and the floor re-rejects. FP guardrails: noise tier + unverified novels. Flip on pass; log; commit.

### Task W2.6: AP coverage — CQ mask, post-normalization injection, RR73/RRR/73 full masks [A/B]

**Files:**
- Modify: `pancetta-ft8/src/ap.rs` (new `ApLevel::Cq` injecting the "CQ" token bit pattern at message-type+first-token positions; full-message masks for RRR/RR73/73 toward the QSO partner), `pancetta-ft8/src/decoder.rs` (inject AFTER `normalize_llrs`, fixed post-normalization magnitude; sequence CQ mask before AP1)
- Clean-room references: `research/specs/spec-jtdx-napwid-ap-gating.md`, `spec-ft8mon-apriori-bit-prior.md`, `spec-wsjtx-improved-a8-decoding.md`
- Test: `pancetta-ft8/tests/ap_i3_tests.rs` (extend: encode "CQ K1DEF FN42", assert mask bits match encoder output at injected positions)

**Steps:**
- [ ] TDD each mask from the encoder side (as W1.1). Injection-survival check extends naturally (decoded text must contain "CQ" / the expected RR73). A/B each level separately; log; commit. The existing `a8_qso_state_ap_enabled` template machinery (ap.rs:330-369) may subsume the RR73 case — check before duplicating.

---

## Workstream 3 — Per-candidate fine sync + matched demod (D3)

### Task W3.1: Wire `baseband.rs` per-candidate extraction [BIT-EXACT — new API, unused]

**Files:**
- Modify: `pancetta-ft8/src/baseband.rs` (public API: `extract_candidate_baseband(audio: &[f64], freq_hz: f64, start_sample: isize, pp: &ProtocolParams) -> BasebandSlice` — 200 Hz complex, 32 samples/symbol, spanning the 79-symbol window ± search margin), `pancetta-ft8/src/decoder.rs` (plumb raw audio availability to the candidate stage — the audio is already held for subtraction)
- Test: baseband.rs unit tests (extend the existing tone-recovery tests to the new API)

**Steps:**
- [ ] TDD: synthesize a clean FT8 signal at 1503.1 Hz, dt +0.12 s; extract; assert per-symbol argmax tones equal transmitted symbols. Commit. (No live-path change yet.)

### Task W3.2: Fine dt/df search on baseband [BIT-EXACT — function + tests]

**Files:**
- Create: `pancetta-ft8/src/fine_sync.rs`
- Test: `pancetta-ft8/src/fine_sync.rs` unit tests
- Clean-room references: `research/specs/spec-wsjtx-mainline-ft8b.md`, `spec-ft8mon-sub-bin-costas.md`, `spec-wsjtx-improved-subsample-dt-refinement.md`, `spec-ft8mon-symbol-to-symbol-phase-fine.md`

**Interfaces:**
- Produces: `pub fn refine(bb: &BasebandSlice, pp: &ProtocolParams) -> FineSync { dt_samples: f32, df_hz: f32, sync_power: f32 }` — maximize noncoherent Costas correlation power over dt ∈ ±half symbol in 1/16-symbol steps and df ∈ ±3.2 Hz in 0.5 Hz steps, then parabolic-refine both axes (reuse `parabolic_peak_refinement`, undamped — the ×0.3 scaling at decoder.rs:1109 exists to paper over dB-domain interpolation and must not carry over).
- Tolerance targets (test-asserted): |dt error| ≤ 10 ms, |df error| ≤ 0.2 Hz on clean signals; graceful on −18 dB AWGN (≤ 20 ms / 0.5 Hz median over 50 seeded trials).

**Steps:**
- [ ] TDD against synthetic signals with known (dt, df) drawn over the grid, clean and −18 dB. Commit.

### Task W3.3: Matched demod stage replaces the 21-LDPC fallback [A/B]

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` (`par_decode_candidate` 7008+: behind `fine_sync_enabled: bool` default false — after spectrogram-path failure (keep that cheap first try), run W3.1+W3.2, then extract symbols by **rectangular 32-point DFTs at the refined df** (complex retained), feed the existing LLR path (dual-max default), **one** BP attempt; delete/bypass the 21-trial loop when the flag is on. Same stage for rescue passes (cross-cycle 4718+, joint-pair 5217+, localized 5674+) so rescue candidates stop getting the strictly-weaker demod.)
- Test: `pancetta-ft8/tests/decoder_refinement_tests.rs` — off-grid synthetic (df = +1.4 Hz, dt = +37 ms, −19 dB) that the spectrogram path fails and this stage decodes
- Gate: sync-score gate for the stage at the **admission** threshold 3.0, NOT 3.5 — the 3.5 gate excluded exactly the candidates needing refinement (spec §3)

**Steps:**
- [ ] Implement behind flag; off-grid test passes with flag on, fails off.
- [ ] A/B hard-200 + synth curve. Cost note for the log: replaces up to 21 LDPC runs with one sync search + one LDPC — likely net cheaper; report elapsed alongside recall. Flip on pass; log; commit. This is the plan's biggest single expected recall move (spec: ~1.5–2.5 dB class).

### Task W3.4: nsym=2/3 noncoherent combining LLR variants [A/B]

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` (behind `nsym_combining_enabled: bool`: from W3.3's complex symbols, form 2-symbol and 3-symbol noncoherent Gray-hypothesis metrics per `spec-wsjtx-mainline-ft8b.md`; each variant is an additional BP attempt after the 1-symbol attempt fails)
- Test: synthetic stable-phase signal at −20 dB where 1-symbol LLRs fail BP and the 3-symbol variant decodes (seed-searched fixture)

**Steps:**
- [ ] TDD the metric construction (unit: known symbol triplet → hypothesis energies), integrate as extra attempts, A/B (expected ~0.5–1 dB on stable signals), flip on pass, log, commit.

### Task W3.5: Linear-power substep averaging + time interpolation [A/B]

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs:8255` (substep combine in linear power), flip `sync_time_interp_linear_power: true` (1116) after A/B; revisit the ×0.3 interp damping (1109) once interpolation is linear-power

**Steps:**
- [ ] One flag (`linear_power_averaging`), A/B, flip, log, commit.

### Task W3.6: Retest `per_candidate_freq_tracker_enabled` [A/B]

Drift correction (freq_tracker.rs) was built for the fine path; under W3.3 it finally has a worthy consumer.

**Steps:**
- [ ] A/B with W3.3 on; adopt or record why not; log; commit.

---

## Workstream 4 — Subtraction fidelity + real multipass (D4)

### Task W4.1: GFSK reference synthesis for subtraction [BIT-EXACT — new function]

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` (subtraction reference from the TX modulator: `Ft8Modulator` with `PulseShape::Gaussian { bt: pp gfsk bt }` (`modulator.rs:39-52`), at the decode's **refined** dt/df from W3.3 — no grid re-quantization; delete the `reverse_derive_candidate` quantization on this path (7973)), possibly extract a shared synth helper into `pancetta-ft8/src/transmit.rs` or `modulator.rs`
- Note: modulator lives behind the `transmit` feature — either lift the pulse-shape synthesis out of the feature gate or make the improved subtraction require `transmit` (check how the workspace builds the decoder crate in production; pick the option that keeps default builds working)
- Test: unit — reference waveform spectrum vs `generate_cpfsk_iq` shows suppressed ±6.25 Hz sidelobes (quantified assert)

**Steps:**
- [ ] TDD the reference generator; commit. (Not yet called by live path.)

### Task W4.2: Time-varying complex amplitude fit + audio-domain subtract [A/B]

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` (`subtract_signal` 3140: replace the single whole-signal complex amplitude (3217-3241) with a per-block estimate — complex LS projection of audio onto the W4.1 reference over a sliding ~2-symbol window, low-pass smoothed across blocks — then subtract; keep the ±0.5 Hz/±40 ms fine alignment search but seed it from the refined dt/df)
- Clean-room references: `research/specs/spec-wsjtx-mainline-subtractft8.md`, `spec-ft8mon-gaussian-ramp-subtract.md`, `spec-sdrangel-subtract-edge-symbols.md`
- Test: `pancetta-ft8/tests/decoder_refinement_tests.rs` — synthesize A (−5 dB) overlapping B (−17 dB) 30 Hz apart; after subtracting A, assert residual power in A's tone bins ≤ noise floor + 3 dB AND B decodes on pass 2

**Steps:**
- [ ] TDD with the two-signal fixture; implement; commit behind `max_decode_passes` still 1 (dead path until W4.3 — that's fine, tests exercise it directly).

### Task W4.3: Re-enable real multipass [A/B flip]

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs:1061` (`max_decode_passes: 2`, then 3), pass loop 1927+ (spectrogram **is** recomputed from residual audio each outer pass already at 2012 — verify), budget checks inside the loop (coordinate with speed plan's `DecodeBudget` — passes 2+ must be budget-schedulable stages)

**Steps:**
- [ ] A/B on `synth_pair_200` (the two-signal adversarial grid — primary corpus for this workstream) + hard-200 + noise tier. The historical "passes 2+ contribute nothing" verdict (decoder.rs:186-192) was measured with rectangular-CPFSK subtraction; this re-measures with W4.1/W4.2. Flip on pass; log both the recall and elapsed cost; commit.

### Task W4.4: Cross-cycle grouping content guard [A/B]

Spec §7: geometric-only grouping sums different messages; measured +8 novels alongside +14 recovered.

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs:7866-7915` (`group_for_cross_cycle`: before accepting a member, require LLR-sign correlation between the member's and the group seed's demodulated soft symbols ≥ threshold (default ~0.3, calibrate on chrono_replay), config `cross_cycle_content_guard: Option<f32>`)
- Test: unit — two synthetic different-message candidates at the same grid position are NOT grouped; two same-message ones are

**Steps:**
- [ ] TDD; A/B on chrono_replay (the cross-cycle corpus) watching unverified novels specifically; flip; log; commit.

---

## Workstream 5 — Candidate pipeline restructure (D5)

### Task W5.1: Per-bin peak selection replaces the flat top-200 [A/B]

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` (`costas_sync_search_with_threshold_and_partner` 3853+/4144: group above-threshold cells by `freq_bin`; keep top-K per bin across time (K=2) plus top-1 per bin per auxiliary pathway; then global cap `max_sync_candidates`; keep exact-duplicate dedup), config `per_bin_candidate_selection: bool` default false
- Clean-room references: `research/specs/spec-wsjtx-mainline-sync8.md`, `spec-jtdx-ncandthin-dt-thinning.md`
- Test: unit — synthetic candidate list with one strong 30-cell cluster + one weak isolated cell: flat top-N at N=20 drops the weak one; per-bin selection keeps it

**Steps:**
- [ ] TDD the selection function pure (list in → list out); wire behind flag; A/B on hard-200 (displacement is a crowded-band effect — also run lid_of_band, the weak-signal-rich corpus); flip on pass; log; commit.

### Task W5.2: Percentile-normalized sync threshold retest [A/B]

`costas_two_baseline` (3899-4136, `spec-wsjtr-sync-norm.md`) was shelved under the flat cap. Retest under W5.1.

**Steps:**
- [ ] A/B `costas_two_baseline_enabled: true` with W5.1 on, hard-200 + lid_of_band + noise tier (normalization changes the FP surface). Flip or document; log; commit.

### Task W5.3: Capture window covers real dt range [coordinator change, A/B]

Spec §6: dt floor −0.16 s, `WINDOW_SAMPLES` = exactly 12.64 s, slot-edge bucket at 48.3% recall, wild-50 0/96 was capture misalignment.

**Files:**
- Investigate first: how `pancetta-dsp` → coordinator assembles the decode window (where the 12.64 s slice of the 15 s slot is cut; `pancetta/src/coordinator/`, `pancetta-dsp`)
- Modify: window assembly to hand the decoder ~14.1 s spanning slot-relative t ∈ [−0.6 s, +13.5 s] (real audio history, **not** zero padding), and `decoder.rs` sweep bounds (`max_time_step` 3866, `SLIDING_FRAME_LOOKBACK_STEPS` 91) so dt ∈ [−1.0, +2.5] s is reachable; `WINDOW_SAMPLES` consumers audited (the speed plan's profiling harness pins it — coordinate)
- Test: integration — synth WAVs with dt = −0.8 s and +2.2 s placed in a 15 s slot buffer decode; today they cannot

**Steps:**
- [ ] Investigation note first (where windows are cut; what dt distribution hard-200 truth shows — dt_history data exists); then failing integration tests; implement; A/B on hard-200 + chrono_replay (watch the slot-edge bucket recall specifically); log; commit.

### Task W5.4: Retest the shelved sync mechanisms under the new pipeline [A/B ×4]

Each was reverted/shelved under flat-cap displacement (spec §6); each is one flag flip + A/B + log:

- [ ] `costas_half_loop_disabled: true` (1235 — kills the 2-step plateau)
- [ ] `costas_partial_metric_enabled: true` (1220 — slot-edge B+C metric; pair with W5.3)
- [ ] `dt_history_enabled: true` (1200 — per-callsign DT priors)
- [ ] `relaxed_sync_near_partner_*` (1244 — set a real delta; JTDX-style partner window, `spec-jtdx-relaxed-sync-near-partner.md`)

Adopt winners; document losers in experiment logs; commit each.

---

## Final task: honesty pass on public claims

- [ ] Update `docs/decoder-comparison.md`: jt9-referenced recall as the headline, verified/unverified novel split, 2500 Hz-calibrated SNR curve figure, and the noise-tier FP number. Retire the standalone "+11.6% vs ft8_lib" framing (keep it as a secondary table). Update `benchmarks/BASELINE.md` pointers. Commit.

## Self-review notes (authoring session)

- Spec §2–§7 requirements each map to a task: D0→W0.1-5, bugs→W1.1-6, D2→W2.1-6, D3→W3.1-6, D4→W4.1-4, D5→W5.1-4, honesty→final. Cross-cutting §7 items: whitening→W1.4, twins→W1.6, cross-cycle guard→W4.4, impulse-robust retest folds into W2.5's gate work if LLR-compression shows up there (explicitly deferred otherwise), a7 remains OFF (no task enables it; its guard requirements are recorded in spec §7 should anyone try).
- Line numbers are as of commit `f9072e4` (2026-07-06); `decoder.rs` will shift under the speed overhaul — treat them as anchors, re-grep before editing.
- Interface names introduced here (`AcceptanceScore`, `fine_sync::refine`, `BasebandSlice`, flags) are consistent across tasks; anything else referenced (e.g., `known_coherence_score`, `parabolic_peak_refinement`) exists in-tree today.
