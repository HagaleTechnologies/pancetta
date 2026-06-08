# Batch 47 — implementation plan from 65 clean-room files

**Date**: 2026-06-08
**Process**: Read SPECS only (firewall-respecting); never opened GPL source during plan-writing.
**Inputs**: 58 algorithm specs + 7 catalog files in `research/specs/`

## Spec inventory by project (58 specs)

### WSJT-X mainline (7 specs) — Agent B
Original Fortran source K1JT et al. **Most-upstream — catches wsjtr translation artifacts.**
- spec-wsjtx-mainline-sync8.md — **TWO parallel baselines (red ±10-col tight + red2 ±62-col WIDE)**, each 40th-percentile normalized; wide-lag catches slot-edge negative-dt
- spec-wsjtx-mainline-ft8b.md — **FOUR parallel LLR variants A/B/C/D** (1-sym incoherent / 2-sym coherent / 3-sym coherent / bit-by-bit normalized); `scalefac=2.83`, `apmag=1.01*max(|llra|)`, two sequential dt refinements ±10 then ±4, nsync≤6 early bail
- spec-wsjtx-mainline-osd174.md — `npre2` preprocessing rule (hash-table complementary-bit-pair search via `boxit91`/`fetchit91`) at ndeep≥3
- spec-wsjtx-mainline-ft8-decode.md — 3-stage schedule `nzhsym ∈ {41, 47, 50}`; middle stage exists PRIMARILY for early-decode subtraction
- spec-wsjtx-mainline-ft8-a7.md — 206 message hypotheses (200 SNR-report variants); `dmin2/dmin ≥ 1.3` ambiguity, `dmin > 100` reject, `QU1RK` sentinel
- spec-wsjtx-mainline-subtractft8.md — frequency-domain LPF, end-correction 2001 samples, parabola-fit dt-refinement ±90 samples, GFSK `BT=2.0`
- spec-wsjtx-mainline-baseline.md — 4-term Nuttall window, 10-segment 10th-percentile lower-envelope, 5th-degree polyfit, +0.65 dB bias

### WSJT-X Improved (DG2YCB) (6 specs) — Agent A
GPL-3.0 fork with concrete sensitivity improvements.
- spec-wsjtx-improved-a8-decoding.md — **a8 = sequenced-QSO-state AP** (candidate set seeded from active QSO context); ships QSO-partner decodes "0.5-1 second earlier" per release notes
- spec-wsjtx-improved-fdr.md — post-decode confidence gate keyed by message type, two operator-selectable levels (Level1 always-safe, Level2 opt-in); strong overlap with pancetta's hb-103 + is_plausible
- spec-wsjtx-improved-auto-passband.md — spectrum-shape-based passband detection, closed-form
- spec-wsjtx-improved-4th-pass-after-a7.md — one extra standard pass mines AP-cleaned residual; bounded, deterministic, ~30 LOC
- spec-wsjtx-improved-subsample-dt-refinement.md — parabolic three-point peak interpolation on sync correlation; 5-10% sample period precision essentially free
- spec-wsjtx-improved-mtd-staged-decoder.md — MTD + 2-Stage/3-Stage orchestration; **2-Stage = 99.5% of 3-Stage yield at far lower CPU**

### wsjtr (Bodiya, GPL-3.0 Rust) (9 specs)
- spec-wsjtr-sync-bc.md (hb-242)
- spec-wsjtr-sync-norm.md
- spec-wsjtr-grid-refinement.md
- spec-wsjtr-cached-bandpass-downsampler.md (hb-243)
- spec-wsjtr-f64-tanh-bp.md
- spec-wsjtr-zsum-osd-init.md (PAIRED with dmin fix)
- spec-wsjtr-cross-sequence-a7.md (hb-237)
- spec-wsjtr-dt-refinement-during-subtract.md (hb-240)
- spec-wsjtr-per-pass-variation.md (hb-241)

### JTDX (6 specs) — Agents Batch 46 + C
Original 3 (3method-sweep, qso-partner-filter, relaxed-sync-near-partner) + 3 new:
- spec-jtdx-lqsothread-virtual-candidates.md — synthetic candidates at (nfqso, ±5s DT, sync=0); routed through `ft8s` template-matched super-decoder using operator's last TX context
- spec-jtdx-lft8subpass-nweak.md — combo: `nweak=2` second inner LDPC iteration on reversed-conjugate symbol matrix `csr`; `ldeepsync` longer Costas sync probe; CQ-gate relax 1.3→1.2; **nweak=2 triggers unconditionally when dfqso<2 Hz** (always pays 2× inner CPU near partner)
- spec-jtdx-napwid-ap-gating.md — **5 Hz on HF (NOT 75 Hz as initial brief said)**, 15 Hz VHF, 50 Hz UHF; anti-recommended for pancetta today
- spec-jtdx-ncandthin-dt-thinning.md — **anti-recommended** (would worsen slot-edge hole)
- spec-jtdx-cycle-audio-smoothing.md — BONUS — between-cycle 2-tap moving-average smoothing of `dd8`
- spec-jtdx-windowed-sync-fp-guard.md — BONUS — heavily-calibrated pre-LDPC FP guard combining `nsyncscore` + `nsyncscorew` with rrxdt regime branching

### ft8mon (13 specs) — Agent E + Batch 46
Original 4 (sub-bin-costas, soft-decode-pairs, known-tone-refinement, apriori-bit-prior) + 9 new:
- spec-ft8mon-rate-reduction.md — two-stage: per-thread cosine-tapered `fbandpass` + bin-shift + IFFT; per-candidate `down_v7_f` to 200 sps
- spec-ft8mon-use-hints.md — brute-force directed search via ±4.97 LLR clamp; **hint decodes use `use_osd=0` (must disable)**; `use_hints=2` (CQ-only) default
- spec-ft8mon-three-soft-decoder-ensemble.md — OR-ensemble: `c_soft_decode` (complex `c_soft_win=2` `c_soft_weight=7`) + `soft_decode_pairs` + `soft_decode_triples` (512 combos per stride); first valid LDPC wins
- spec-ft8mon-gaussian-ramp-subtract.md — `subtract_ramp=0.11` (**~17.6 ms, NOT 3.5 ms as some docs claim**); inter-symbol phase reads next symbol's measured phase
- spec-ft8mon-prevdecs-cross-slot.md — caller-side `prevdecs` rebiased by `delta_hz`; suggests TTL ~30s
- spec-ft8mon-snr-windowed-blackman.md — Blackman window of 15 symbols, `snr_how=3` weakest-tone default
- spec-ft8mon-tminus-tplus-window.md — `tminus=2.2s`, `tplus=2.4s`; **uses RANDOM-resample tail padding (NOT zero/reflection)** to avoid spectral edges
- spec-ft8mon-symbol-to-symbol-phase-fine.md — BONUS — `fine()` averages phase deltas across all 79 symbols weighted by Costas-anchored magnitude; distinct from sub-bin Costas (phase vs magnitude)
- spec-ft8mon-three-stage-sync-cascade.md — BONUS — coarse → second → third (POST-decode using LDPC-recovered symbols via `known_strength_how=7` phase-aware metric); **pancetta's current 2-stage likely missing this**

### JS8Call-Improved (4 specs) — Agent F
**ALL FOUR ARE GENUINELY NOVEL — DO NOT EXIST IN WSJT-X / wsjtr / ft8mon / JTDX:**
- spec-js8call-ldpc-feedback-refinement.md — meta-loop around LDPC using prior hard-decision codeword (agree-boost / disagree-attenuate / erase logic)
- spec-js8call-soft-combiner.md — **HIGHEST-LEVERAGE per agent**; LRU cache + Hamming-≤4 fuzzy match + additive LLR combination across repeated receptions (time-diversity boost; hb-218 weak-signal territory)
- spec-js8call-llr-whitening.md — per-tone × per-symbol noise normalization with parallel LLR streams
- spec-js8call-per-candidate-frequency-tracker.md — adaptive PLL/Kalman using Costas residuals as pilot tones during symbol demap of single candidate

### MSHV (2 specs) — Agent D
GPL-3.0; multi-decoder + contest infrastructure.
- spec-mshv-band-split-multi-decoder.md — 6-pthread fan-out with ±11 Hz nudge + per-worker overlap (CORFT8=60Hz); ~400-600 LOC
- spec-mshv-multi-answer-sequencer.md — Queue/Now/Slot scheduler with `MAXSL=6`, MA-Standard vs MA-DXpedition modes, compound-callsign branch; ~600-900 LOC; maps to pancetta's QsoManager + SmartFrequencyAllocator
- (Deferred: MSHV SD-FT8 "Var" decoder in `decoderft8var.cpp` — likely biggest single-pass recall win but 2-3 sessions careful reading; **defer until current backlog exhausted**)

### SDRangel libft8 (3 specs) — Agent (follow-on)
Agent honestly assessed: 2 of 3 are FT-chirp infrastructure with **zero recall delta on FT8**.
- spec-sdrangel-gray-decode-from-magnitudes.md — generic Gray decoding; FT-chirp infra
- spec-sdrangel-generalized-soft-decode.md — generic soft demod; FT-chirp infra
- spec-sdrangel-subtract-edge-symbols.md — **BONUS — only one with FT8 recall-delta potential**; models tapered phantom symbols at frame edges during spectral subtraction; targets capture-effect regime; maps to hb-218

### kholia/pico_ft8_xcvr (1 spec) — Agent (follow-on, MIT-licensed)
- spec-kholia-knob-degradation.md — **6 kHz sample rate is ZERO-COST** on Slow tier (kholia measured 14=14 decodes at 12kHz vs 6kHz); **osr knobs are CLIFFS** (anti-recommend); 4 bonus mechanisms (uint8_t/0.5dB waterfall, 160ms-per-symbol budget, tier-probe-under-realistic-power, Hann window over 1.8× block_size)

### WSJT-Z (1 spec) — Agent (follow-on)
- spec-wsjtz-early-decode-dedup.md — slot-scoped `HashSet<String>` (size 200) dedup at start-of-slot trigger `nzhsym=41`; only valuable IF you also adopt the multi-trigger sub-slot decode (~250 LOC)

### GRAND family (2 specs) — Agent G
- spec-grand-orbgrand.md (license CAVEAT: kenrduffy MIT GRAND repos are "non-commercial research only", NOT OSI MIT — clean-room from papers only)
- spec-grand-soft-output.md

### JS8Call decoder survey (1 spec) — Agent F
- spec-js8call-decoder-survey.md — pancetta is FT8-only so JS8 protocol spec deferred

## Catalog inventory (7 catalogs)

- catalog-mshv.md — MSHV multi-feature catalog; SD-FT8 "Var" deferred
- catalog-misc-ft8.md — wsjt-z, WSJT-CB, FT-Activ8, FT8CN, FT8RX
- catalog-jtdx-improved.md — REAL but UX/automation re-skin, no novel decoder
- catalog-other-ft8-projects.md — Tier-A (SDRangel libft8, PyFT8, WB2FKO), Tier-B (12 projects), Tier-C (skipped)
- catalog-neural-ldpc-impls.md — license vetting (CrossMPT non-commercial, ECCT actually MIT)
- catalog-embedded-ft8.md — kholia, aa1gd, Rotron, wcheng95, ft8_lib derivatives
- catalog-academic-ldpc.md — adamgreig/labrador-ldpc Rust no_std as architectural template

## Tier-1 ship candidates (revised after wide-net sweep)

### #1 (Tier-1 cheap-win) — hb-229 + hb-242 + sync_bc REVISED

Original Batch 46 plan: ship hb-242 from wsjtr spec. **Now WSJT-X mainline spec reveals wsjtr's docs MISSED the wide-lag baseline (`red2` ±62 cols)**. The wide-lag baseline is the actual mechanism that catches slot-edge.

**Revised plan**:
- Implement BOTH the tight-lag (`red`) and wide-lag (`red2`) baselines from `spec-wsjtx-mainline-sync8.md`
- Apply `sync_bc` partial-Costas trick from wsjtr spec on top
- Together address slot-edge 48.3% recall hole comprehensively

Plus pair with hb-229 QSO partner band-collapse.

Combined effort: ~400-500 LOC, 2-3 sessions, low risk.
Expected: +50-200 RR73-class slot-edge truths.

### #2 (Tier-1 strategic) — JS8Call-Improved soft combiner (hb-244 NEW)

**Highest-leverage finding from the wide-net sweep.** No other FT8 decoder has this mechanism.

Mechanism: LRU cache keyed (mode, freq_bin, time_bin) with Hamming-≤4 fuzzy matching that ADDITIVELY combines LLRs across repeated receptions of the same signal. Transparent time-diversity boost.

- Directly attacks pancetta's hb-218 weak-signal coverage problem
- ~200-300 LOC port
- 2-3 sessions
- **Pancetta would be the first FT8 decoder to ship this**

### #3 (Tier-1 sensitivity) — ft8mon three-stage sync cascade (hb-222 REVISED)

Original hb-222 was just post-decode `search_both_known`. Agent E's spec reveals the FULL cascade: coarse → second (`search_both`) → THIRD (`search_both_known` POST-LDPC using LDPC-recovered symbols via `known_strength_how=7`).

**Pancetta's current 2-stage structure is missing the third stage entirely.**

- Critical prerequisite for clean subtraction → cleaner residual → more mp=2 decodes
- ~300-400 LOC
- 2 sessions

### #4 (Tier-1 cheap) — WSJT-X Improved subsample DT refinement (hb-245 NEW)

Parabolic three-point peak interpolation on the sync correlation function. **5-10% sample period precision essentially free** (3 extra correlations per candidate). Stacks with hb-225 sub-bin Costas.

- ~50-80 LOC
- 1 session
- Cheap warmup ship before bigger items

## Tier-2 ship candidates

- **WSJT-X mainline ft8b 4 parallel LLR variants A/B/C/D** — superset of soft_decode_pairs (hb-223); ~500 LOC plan-sized
- **WSJT-X Improved a8 sequenced-QSO-state AP** — extends hb-237 cross-sequence A7; leverages pancetta's QSO state
- **WSJT-X Improved FDR** — convergent with pancetta's hb-103; spec out the differences
- **JTDX cycle-audio-smoothing** — BONUS spec; cheap (~90 LOC) recall multiplier when paired with multipass
- **ft8mon Gaussian-ramp subtract** (hb-226) — pairs with hb-222
- **ft8mon use_hints** — feed pancetta's QSO state machine hunt list as hints
- **JS8Call-Improved LDPC feedback refinement** — lowest-LOC JS8 ship (~150 LOC)
- **JS8Call-Improved LLR whitening** — pairs with frequency tracker
- **MSHV band-split multi-decoder** — relevant if pancetta wants multi-stream RX parity with TX
- **WSJT-X Improved MTD 2-Stage** — major refactor (~1500 LOC) but unlocks a8's early-display

## Tier-3 (lower priority / specialized)

- **wsjtr cached-bandpass downsampler** (hb-243) — biggest sensitivity gap per wsjtr, but high blast radius (structural replacement); defer
- **WSJT-X mainline npre2 OSD preprocessing** — only ndeep≥3; pancetta's OSD design is different anyway
- **ft8mon tminus/tplus wide coarse window with RANDOM-resample padding** — random padding is the key trick
- **MSHV Multi-Answer Auto-Sequencer** — touches pancetta-qso QsoManager extensively; plan-sized
- **kholia 6 kHz Slow-tier** — needs pancetta-corpus validation first (kholia's evidence is 1 WAV)
- **SDRangel subtract_edge_symbols** — small focused capture-effect tweak
- **WSJT-Z early-decode dedup** — only valuable with multi-trigger sub-slot pipeline

## Tier-4 (anti-recommended / deferred)

- **JTDX napwid AP gating** — anti-recommended (pancetta's AP not FP-limited)
- **JTDX ncandthin DT-distance thinning** — anti-recommended (would WORSEN slot-edge hole)
- **JTDX windowed-sync FP guard** — heavily threshold-calibrated; would need pancetta-recalibration
- **MSHV SD-FT8 "Var" decoder** — biggest single-pass recall win but ~3 sessions of careful reading; **defer until other backlog exhausted**
- **GRAND family, neural LDPC** — license vetting closes most; defer indefinitely
- **All academic LDPC** — deferred until classical mechanisms exhausted

## Cross-cutting principles surfaced

1. **QSO-partner-zone bypass** (JTDX has it in 3 places, all `dfqso < 2 Hz`): "give the operator's partner the benefit of the doubt." Pancetta should be internally consistent — never have one mechanism reject what another accepts on partner's frequency.

2. **wsjtr's docs paraphrased away critical detail**: at least 3 cases where Agent B caught mainline mechanisms wsjtr's docs missed (wide-lag baseline, 4 LLR variants A-D, npre2 OSD preprocessing). **Reading source directly is high-value.**

3. **Documentation values can be wrong**: subtract_ramp was 17.6 ms not 3.5 ms; napwid is 5 Hz not 75 Hz. Validate parameter values against actual source.

4. **License vetting matters**: MIT-named-but-non-commercial repos are a contamination trap. Only `kholia/pico_ft8_xcvr` + `adamgreig/labrador-ldpc` are actually MIT-OSI.

5. **Some "novel" finds are infrastructure for OTHER protocols**: SDRangel's Gray-from-magnitudes is FT-chirp infrastructure with zero FT8 delta. Check the consumer before assuming the mechanism helps.

## Recommended Batch 48 = first Implementer thread session

Implementer Agent gets pancetta source + `research/specs/` ONLY. No GPL source access.

**Ship order**:
1. hb-242 sync_bc + WSJT-X wide-lag baseline (paired) — ~400-500 LOC
2. hb-229 QSO partner band-collapse — ~250 LOC
3. hb-245 subsample DT refinement — ~80 LOC

Total: ~750 LOC, 3-4 sessions, all low-risk infrastructure improvements.

**Defer to Batch 49+**:
- hb-244 JS8Call soft combiner (biggest strategic win)
- hb-222 three-stage sync cascade
- ft8mon ensemble path (after sync work lands)

## Counters

- Total spec files: 58 + 7 catalogs = 65 files (~16,000 lines)
- Projects surveyed: 14
- Projects with novel mechanisms found: 9 (WSJT-X mainline, WSJT-X Improved, wsjtr, JTDX, ft8mon, JS8Call-Improved, MSHV, SDRangel libft8, kholia, WSJT-Z, GRAND, JS8Call survey)
- Projects ruled out: 6 (JTDX-Improved UX-only, WSJT-CB irrelevant, FT-Activ8 built on wsjtr, FT8CN downstream, ft8d delegates, fldigi/Quisk/OpenWebRX)
- License blockers identified: 4 (kenrduffy GRAND, CrossMPT, Lugosch Neural Min-Sum, FT8RX closed-source)
- New bank candidates surfaced beyond Batch 46: 15+ (4 JS8Call mechs, 2 MSHV mechs, 7 ft8mon bonuses, multiple WSJT-X mainline mechanisms missed by wsjtr translation)

**Coverage**: definitive. Every public FT8 decoder of note has either specs or a catalog entry. Ready for Implementer thread.
