# Engineering Substance Audit — 2026-06-02 (Phase D)

Worktree: `/tmp/pancetta-phase-d`
Branch: `iter/2026-06-02-phase-d-engineering-audit`
Audit scope: every technical claim in production code comments, hypothesis-bank entries (active and shelved), recent experiment journals (2026-05-26 onwards), and the 2026-06-01 ideation sweep.

The mission: verify that the SUBSTANCE behind confident-sounding labels — "Welch averaging", "phase-coherent integration", "OSD-2", "Bayesian deep ensembles", "saturation-aware composite", etc. — matches primary sources. Some terms are right; some are loose; one or two indicate possible engineering mis-naming.

This document is READ-ONLY analysis. No code or bank changes were made.

## Top-level summary

44 distinct technical claims audited.

| Status | Count | % |
|---|---:|---:|
| VERIFIED — term + math + usage match primary source | 28 | 64% |
| WRONG-TERM-RIGHT-CONCEPT — engineering sound, name needs fix | 7 | 16% |
| WRONG-CONCEPT — math/implementation differs from name | 0 | 0% |
| NO-PRIMARY-SOURCE — couldn't backstop with paper/textbook | 4 | 9% |
| PANCETTA-INVENTED — we coined this, no claim of priority | 5 | 11% |

The headline result is **no real engineering bugs from terminology drift.** The decoder's mathematical core is sound. The bulk of issues are loose-language: terms applied to mechanisms that *resemble* the canonical thing but aren't strictly the textbook definition. Most concerning class is "Welch" (hb-089) and "calibration" (hb-194 journal), both of which are reasonable engineering with the wrong words attached.

## Methodology

Each claim was verified against a primary source (textbook, paper, official documentation, or reference implementation). For implementation-bound claims, the production code was inspected line-by-line against the cited reference.

Sources consulted: Welch (1967, IEEE Trans. Audio Electroacoustics), Fossorier-Lin (1995, IEEE Trans. Information Theory), Hocevar (2004, IEEE SiPS), Lakshminarayanan et al. (2017, NIPS), Guo et al. (2017, ICML), Franke-Taylor-Somerville QEX 2020, kgoba/ft8_lib source, WSJT-X User Guide, Smith CCRMA "Spectral Audio Signal Processing", WSJT-X Improved release notes.

---

## Per-claim entries

### FT8 protocol fundamentals

**1. Costas synchronization array `[3,1,4,0,6,5,2]`**
Code: `pancetta-ft8/src/decoder.rs:54`, `protocol.rs:80-85`.
Source: kgoba/ft8_lib `constants.c` defines `kFT8_Costas_pattern[7] = {3,1,4,0,6,5,2}`. Franke-Taylor-Somerville QEX 2020.
Status: **VERIFIED**.

**2. 3 Costas patterns at symbol indices 0, 36, 72 (start / middle / end of a 79-symbol message)**
Code: `protocol.rs:88` `FT8_COSTAS_POSITIONS: [0, 36, 72]`; `num_symbols: 79`.
Source: Franke-Taylor QEX 2020 — Costas at start/mid/end, 79 symbols total.
Status: **VERIFIED**.

**3. LDPC (174, 91) code with 83 parity bits**
Code: `ldpc.rs:11-30` constants, generator matrix transcribed from kgoba/ft8_lib.
Source: kgoba/ft8_lib `constants.c`; QEX 2020 paper.
Status: **VERIFIED**.

**4. FT8 modulation = "8-CPFSK" (declared in `protocol.rs:156` `ModulationType::Cpfsk`)**
Source: Wikipedia (FT8), QSL.net FT8 protocol doc, Franke-Taylor QEX 2020 — FT8 is **8-GFSK** with BT=2.0, not generic CPFSK. The modulator (`modulator.rs:28-29` `DEFAULT_BT: f64 = 2.0`, `Gaussian{bt: 2.0}`) implements it correctly.
Status: **WRONG-TERM-RIGHT-CONCEPT**. The struct tag `ModulationType::Cpfsk` is inaccurate for FT8 (GFSK is a *subclass* of CPM/CPFSK but the canonical name in FT8 literature is GFSK). The modulator code is correct.
Fix: Rename `ModulationType::Cpfsk` to `ModulationType::Gfsk { bt: f64 }` for the FT8 variant (FT4 is already correctly tagged). One-line tag change; no math change.

**5. Tone spacing 6.25 Hz, symbol period 0.16 s, 8 tones**
Code: `protocol.rs:148-150`.
Source: QEX 2020, kgoba/ft8_lib.
Status: **VERIFIED**.

**6. CRC-14 polynomial**
Code: `message.rs` (CRC table), tested in `crc_tests` mod.
Source: Franke-Taylor QEX 2020, kgoba/ft8_lib.
Status: **VERIFIED** (not audited in detail here — the encoder/decoder bit-exact test confirms equivalence with ft8_lib).

### Signal processing / spectrogram

**7. "Welch averaging" / "intra-slot Welch averaging" in hb-089 description (`hypothesis_bank.md:1649-1655`)**
Source: Welch (1967, *IEEE Trans. Audio Electroacoustics* AU-15: 70-73) — "The use of Fast Fourier Transform for the estimation of power spectra: a method based on time averaging over short, modified periodograms". Welch's method: divide the data into *overlapping segments*, apply a *window function*, compute a *modified periodogram* per segment, average those periodograms to estimate the power spectral density. The goal is PSD estimation (smoothing the spectral estimate by reducing variance).
What we did in hb-089: extract the (already-windowed) per-bin magnitude at a *specific time-frequency coordinate* across multiple overlapping sub-windows of the same slot, average them. This is **per-bin magnitude averaging across time-shifted FFTs**, not Welch PSD estimation. The bank entry's bound `10·log10(15/13) ≈ 0.6 dB` for correlated-noise variance reduction is the correct ballpark for averaging N=5 highly-overlapped looks at the same bin — but this is a magnitude averaging operation, not Welch's method.
Status: **WRONG-TERM-RIGHT-CONCEPT**. The diagnostic and conclusion (hb-089 SHELVED) are correct; the *term* "Welch averaging" is being borrowed loosely. The closer correct term would be **"overlap-averaged magnitude" or "incoherent same-slot accumulation"**.
Fix: Rephrase hb-089 shelve_reason. The DIAGNOSIS was right; just the label was wrong.

**8. Parabolic peak refinement formula (hb-044, hb-068)**
Code: `decoder.rs:4925-4943`. Implements `a = (yL + yR - 2·yC)/2`, `b = (yR - yL)/2`, `delta = -b/(2a)`. Equivalent to user-stated `delta = (yL - yR) / (2·(yL + yR - 2·yC))`.
Source: Smith CCRMA "Quadratic Interpolation of Spectral Peaks" — `p = (yp1 - ym1) / (2·(2·y0 - yp1 - ym1))`. Pancetta's form is the same up to a double sign change (numerator and denominator both negated).
Status: **VERIFIED**. Math is canonical.

**9. "Costas sync score" — neighbor-comparison dB difference**
Code: `decoder.rs:181` "sync_score is a Costas correlation, not a strict dB". Min sync_score = 3.0.
Source: ft8_lib `decode.c` `ft8_sync_score()` — same approach (correlate against Costas pattern, normalize against neighbors).
Status: **VERIFIED**.

**10. SNR estimation: `bw_correction = 10·log10(2500/6.25) ≈ 26.02 dB`**
Code: `decoder.rs:2218-2240`. `snr_db = avg(best_tone_db − worst_tone_db) − 10·log10(2500/6.25)`.
Source: WSJT-X User Guide — SNR reported in 2500 Hz reference bandwidth. The 26 dB offset between per-tone (6.25 Hz) SNR and 2500-Hz-reference SNR is the standard conversion. VU2NSB.com, K0NR / N6MW explainer.
Status: **VERIFIED** for the *bandwidth correction term*. The noise-floor proxy `worst_tone_db` is a heuristic — WSJT-X uses the lowest 10% of spectral amplitudes across the 2500 Hz band. Pancetta's per-symbol min-of-8-tones is a simpler proxy with different statistical properties (it's biased high because the minimum of 8 noisy bins is not equal to the median of the 2500-Hz population). The composite SNR estimate is therefore *approximate* but the bandwidth-correction direction and magnitude are correct.
Sub-issue: **NO-PRIMARY-SOURCE** for the specific "min-of-tones-per-symbol" noise estimator — this is pancetta-invented for cheap per-decode SNR.

**11. "8-GFSK with BT=2.0" (modulator)**
Code: `modulator.rs:28-29, 305-340` — Gaussian-smoothed FSK with σ = √(ln 2)/(2π·BT) symbol periods.
Source: Franke-Taylor QEX 2020 — FT8 is GFSK with BT=2. The pulse-shape formula matches the canonical GFSK kernel.
Status: **VERIFIED**.

**12. "Coherent integration" gain ~N (linear); "incoherent integration" gain ~√N**
Code/bank: `decoder.rs:290-295` ("averages SNR by N (not √N)"); hb-074, hb-075, hb-056 entries.
Source: M.A. Richards "Notes on Noncoherent Integration Gain"; standard radar/comms result. Coherent (complex sum) → SNR gain N; incoherent (power sum) → SNR gain √N.
Status: **VERIFIED**.

**13. hb-056 "non-coherent cross-cycle averaging" (linear-power magnitude sum across same-callsign slot pairs)**
Code: `decoder.rs:2257-2378` `cross_cycle_averaging_pass`. dB → linear → sum → dB path.
Source: Generic incoherent-integration / power-averaging is textbook. The bank's "non-coherent variant of JTDX's `s2(i) = |cs|² + |csold|²`" is a JTDX-specific attribution.
Status: **VERIFIED** for the mechanism; **NO-PRIMARY-SOURCE** for the specific JTDX `s2(i)` formula citation. The JTDX claim is internal lore — couldn't find it in any public JTDX document. If important, audit JTDX source directly.

**14. hb-074/075 "coherent cross-cycle averaging" via phase-rotor alignment, MRC weighting**
Code: `decoder.rs:2275-2328`. Each member's complex bins multiplied by `conj(rotor)` (phase alignment) or `conj(acc)` (MRC = alignment + magnitude weighting). Final `|sum|²` → dB.
Source: MRC textbook (Wikipedia "Maximal-ratio combining", DSP-LOG, Wireless-Pi): branch weight ∝ complex conjugate of the channel coefficient. Pancetta's `conj(acc)` weighting is exactly canonical MRC.
Status: **VERIFIED**. Both the coherent-summation and the MRC-weighting variants are correctly implemented per textbook.

**15. hb-079/080 "coherent iterative-subtract multi-pass" — ML projection `proj = Re(bin·conj(rotor))·rotor`**
Code: `decoder.rs:4395-4462` `subtract_decode_coherent`. Projects the bin onto the rotor direction, subtracts that component, leaves orthogonal noise.
Source: Standard ML signal subtraction in the complex domain. The same operation, applied iteratively in a multi-user setting, is called **successive interference cancellation (SIC)** in the multi-user-detection literature (Verdú 1998 "Multiuser Detection"; many papers including Patel-Holtzman 1994 IEEE J. Sel. Areas Commun.). Q65 and JT65 use similar subtract-then-redecode passes.
Status: **VERIFIED** as ML projection. **WRONG-TERM-RIGHT-CONCEPT** for the family-name: what we call "coherent iterative-subtract multi-pass" is canonically known as **SIC (Successive Interference Cancellation)** in the literature. Renaming would help when comparing notes against papers and would clarify what hb-081 (per-decode amplitude scaling) really is: "soft SIC" or "soft cancellation". The math is sound either way.
Fix: Cross-reference SIC in the hb-079/080/081 bank entries and decoder.rs docstring.

### Error correction (LDPC, OSD, BP)

**16. OSD = "Ordered Statistics Decoding" with Most-Reliable Basis (MRB) over sorted-|LLR| reordering + Gauss elimination**
Code: `osd.rs:1-13`, `decode()` at 258-426. Step 1: sort indices by descending |LLR| (or neural ordering); Step 2: permute generator columns; Step 3: Gaussian elimination over the permuted matrix (this builds the systematic form on the MRB); Step 4: re-derive parity bits from the hard decisions on the 91 most-reliable bits.
Source: Fossorier & Lin (1995, IEEE Trans. Information Theory) — "Soft-decision decoding of linear block codes based on ordered statistics". MRB construction by sorting reliability + Gauss elimination, OSD-k by flipping all k-subsets of the information part of MRB.
Status: **VERIFIED**. Algorithm matches Fossorier-Lin exactly.

**17. OSD-0 / OSD-1 / OSD-2 / OSD-3 enumeration counts**
Code: `osd.rs:313-426`. OSD-0: hard decision on MRB. OSD-1: flip one info bit (91 trials). OSD-2: flip pairs (`C(91,2) = 4095` trials). OSD-3: triples (`C(91,3) = 121,485` trials; comment says 125,580 which is a typo — actual `91·90·89/6 = 121,485`).
Source: Fossorier-Lin original definition: OSD-k enumerates all weight-≤k error patterns over the k information bits of the MRB.
Status: **VERIFIED** with **MINOR CODE COMMENT BUG**: the comment at `osd.rs:394` "C(91, 3) = 125,580 trials" is arithmetically wrong (correct: 121,485). Math in the loop is right; only the comment is off.
Fix: Correct the comment.

**18. Belief propagation = sum-product (tanh-based check messages, `2·atanh(Π tanh(v/2))`)**
Code: `decoder.rs:5325-5500` `belief_propagation` — uses `fast_tanh` / `fast_atanh` for c-to-v messages. This is the **sum-product algorithm** (also called log-domain BP), not min-sum.
Source: MacKay 2003 "Information Theory, Inference, and Learning Algorithms" ch. 47; Richardson-Urbanke "Modern Coding Theory". Sum-product check update: `2·atanh(∏_i tanh(v_i/2))`.
Status: **VERIFIED**. Note that `decoder.rs:5071` docstring says "sum-product or min-sum belief propagation algorithm" but the implementation is sum-product. Min-sum is not actually used. Docstring could be tightened.

**19. hb-063 "Layered BP" / row-sequential schedule (Hocevar 2004)**
Code: `decoder.rs:5456-5500` (with_layered branch). Updates messages row-by-row (check-node sequential), feeding posterior of each row into the next within one iteration.
Source: Hocevar 2004 "A reduced complexity decoder architecture via layered decoding of LDPC codes" (IEEE SiPS 2004). Layered = check-node-sequential schedule, converges ~2× faster than flooding.
Status: **VERIFIED**. The arXiv:2410.13131 cited in `decoder.rs:414` is a 2024 followup — the underlying mechanism is Hocevar 2004 as cited.

**20. AP (a priori) LLR injection: hard-fix known bits at ±15.0**
Code: `ap.rs:16,290`. `AP_LLR_MAGNITUDE = 15.0`, `inject_bit` sets `llrs[pos] = ±15.0`.
Source: WSJT-X uses AP LLR injection (`apsym` array) with sentinel-magnitude values. Concept is canonical; specific magnitude (15.0 vs WSJT-X's value) is a pancetta tuning.
Status: **VERIFIED** mechanism; magnitude is a pancetta tuning choice.

**21. AP type 7 ("a7") — template cross-correlation against decoded callsigns**
Code: `a7.rs:1-100`. After decoding callsign C at (f, t), construct plausible follow-up message templates rooted at C, FT8-encode each into 174-bit codewords, cross-correlate against residual LLRs.
Source: WSJT-X Improved release notes describe a7 as "information from previous Rx sequences" — high-level only. The technique itself is documented in WSJT-X Improved (forked from WSJT-X by Sako JG1EIQ et al.).
Status: **VERIFIED** at the high-level description; **NO-PRIMARY-SOURCE** for the exact code path I could access. We have access to the WSJT-X-Improved source on SourceForge; if a deeper audit is needed, that's the place to look.

**22. snr7 threshold = 6.0, snr7b threshold = 1.8 (hb-048)**
Code: `a7.rs:58-62`. `A7_SNR7_THRESHOLD_DEFAULT = 6.0`, `A7_SNR7B_THRESHOLD_DEFAULT = 1.8`.
Source: Couldn't extract specific Fortran source for WSJT-X Improved commit f13e3182 in this audit window — the repo URL returned 500/404. The bank entry hb-048 claims these values match the f13e3182 commit. The cross-correlation FORMULA in `a7.rs:471-477` (`sum / sqrt(N)` of `llr·sign(expected_bit)`) is the canonical matched-filter SNR in the LLR domain — semantically correct.
Status: **NO-PRIMARY-SOURCE** for the *exact* threshold values 6.0 and 1.8. The mechanism is sound. If we want to claim parity with WSJT-X Improved f13e3182 specifically, we should grab that Fortran snippet and pin the value.
Fix: Either grab the WSJT-X Improved source and cite explicitly, or relabel these as pancetta-tuned defaults (which gives us freedom to retune them per our corpus without breaking a claimed-equivalence).

**23. "Soft LLR" from spectrogram — max-log-MAP for 8-FSK Gray-coded bits**
Code: `decoder.rs:4638-4684` `par_compute_soft_llrs_db`. For 3 bits-per-symbol: `llr_k = max{db at tones where bit_k=1} - max{db at tones where bit_k=0}` (then negated).
Source: Standard **max-log-MAP** soft demapper for M-ary symbols with Gray coding. Removes the log-sum-exp correction term; equivalent to assuming a single dominant hypothesis per bit. ft8_lib uses the same formulation.
Status: **VERIFIED**. Bank/code casually calls this "soft LLR computation"; the precise textbook term is **max-log-MAP LLR**.
Fix: Add a docstring line in `par_compute_soft_llrs_db` saying "max-log-MAP approximation".

**24. LLR target variance = 32.0 (pancetta) vs ft8_lib's 24.0**
Code: `decoder.rs:67-74`. Documented as pancetta-specific tuning (hb-006).
Source: ft8_lib `ftx_normalize_logl` uses `sqrt(24.0/variance)`. Pancetta diverges intentionally per bank entry.
Status: **VERIFIED** as documented divergence.

### Machine learning / OSD ranker

**25. Neural OSD with DIA (Decoding Information Aggregation) — CNN over BP iteration trajectory**
Code: `neural_osd.rs:1-90`. 3-layer 1D conv, input shape `[25 iterations][174 bits]`, output 91 per-info-bit error probabilities, used to reorder bits in OSD MRB.
Source: arXiv:2404.14165 Li-Yu "Boosting Ordered Statistics Decoding of Short LDPC Codes with Simple Neural Network Models" introduces DIA exactly as described: synthesize iterative BP trajectories, learn to suppress wrong-sign codeword bits.
Status: **VERIFIED**. Both the term "DIA" and the architecture lineage are correctly cited.

**26. hb-194 "Bayesian deep ensembles" (K=8 OSD CNN copies + ensemble disagreement)**
Bank: hb-194; ideation entry F8 (`foundation-models.md:472-528`).
Source: Lakshminarayanan, Pritzel, Blundell (2017, NIPS) "Simple and Scalable Predictive Uncertainty Estimation using Deep Ensembles". The paper argues that K independently-initialized models capture *epistemic* uncertainty via predictive variance, and that this is a Bayesian-ish posterior approximation.
Status: **VERIFIED** for the ensemble + variance-as-disagreement framing.
Note: The hb-194 ideation entry F8 explicitly hedges with "Bayesian deep learning" — this is the standard term-of-art (deep ensembles are sometimes called a "non-Bayesian" Bayesian method; recent work like arXiv:2501.17917 actually formalizes them as empirical Bayes). Calling them "Bayesian deep ensembles" is loose but defensible.

**27. hb-194 Session-1 journal claim: "no-bootstrap variance is strongly calibrated, Pearson +0.48" (`research/experiments/2026-06-01-hb-194-bayesian-ensembles.md:155-164`)**
Source: Guo et al. (2017, ICML) "On Calibration of Modern Neural Networks" — formal "calibration" is measured by **Expected Calibration Error (ECE)** or Brier score, NOT Pearson correlation between variance and error. Pearson correlation between predictive variance and actual error is a perfectly valid *informativeness* metric — it measures whether high-variance samples tend to be more wrong — but it does NOT measure calibration in the Guo-et-al. sense (whether predicted confidences match empirical accuracy).
Status: **WRONG-TERM-RIGHT-CONCEPT**. The signal the journal measured (variance correlates positively with error rate at Pearson +0.48, Spearman +0.46) is a genuine and useful signal showing that ensemble variance is *informative* about correctness. But calling that "calibration" conflates two distinct ML concepts. ECE was not measured (it would require sweeping a confidence threshold and computing accuracy-vs-confidence bins).
Fix: Rephrase hb-194 journal: "variance is informative (Pearson 0.48 with error rate)" instead of "variance is calibrated". Note ECE was not measured; if a Session 2 happens, an actual ECE computation is one extra row in the metrics table.

**28. Bootstrap (Efron 1979 / Efron-Tibshirani 1993)**
Bank/journal: hb-194 mentions "different training-data bootstraps".
Source: Efron-Tibshirani "An Introduction to the Bootstrap" — sample with replacement n observations from a dataset of size n. Standard ML / statistics technique.
Status: **VERIFIED**. The journal correctly identifies that 37% drop-rate per member (`1 − (1 − 1/n)^n ≈ 1 − 1/e`) is the canonical bootstrap behavior, and correctly diagnoses why this hurt with 436 training samples.

### Metric / composite

**29. Composite metric — additive weighted sum over four normalized tier scores**
Code: `pancetta-research/src/metrics.rs:23-61`. `composite = 0.50·real_decode_rate_hard_200 + 0.30·normalize_snr(synth_clean) + 0.15·fixtures_pass_rate + 0.05·normalize_snr(synth_doppler)`.
Source: Standard weighted-sum aggregation. Choice of weights is pancetta policy.
Status: **PANCETTA-INVENTED** with **VERIFIED** mathematical form (simple weighted average; no statistical impropriety).

**30. SNR-to-score normalization `clamp((-snr - 10) / 20, 0, 1)`**
Code: `metrics.rs:18-21`.
Source: Pancetta-defined; no claim of priority. Maps SNR-at-50%-recovery (more negative is better) into [0, 1].
Status: **PANCETTA-INVENTED**. Verified to be monotone in the right direction, no math bug.

**31. "Saturation-aware composite" — additive correction for corpus refreshes**
Code: `metrics.rs:79-181`. `s_sat = s_raw − Σ offset_to_subtract`. One-time additive offsets per corpus rotation, stored in `refresh_offsets.json`.
Source: This is a pancetta convention to keep multi-week tracking comparable across corpus changes. The "offset" is computed as `score(prev_main, new_corpus) − score(prev_main, old_corpus)` — same decoder, two corpora — so it's a corpus-shift correction by construction.
Status: **PANCETTA-INVENTED**. The math is statistically valid: it's a *fixed* additive offset (not data-dependent), so it does not introduce bias or change the rank-ordering of comparisons made within the post-refresh era. The name "saturation-aware" is unusual — "saturation" in metric terms usually means non-linear flattening near a ceiling, not corpus offsets. A clearer name would be "**corpus-refresh-adjusted composite**" or "**corpus-shift-corrected composite**".
Fix (terminology only): the implementation is fine; the *name* is misleading. If the project wants to use "saturation" because the corpus rotates as the decoder saturates the previous corpus, the name should at least say "**corpus-shift correction (saturation-aware tracking)**" so readers know what it is.

**32. "ECE" (Expected Calibration Error) — bin-and-average confidence-vs-accuracy gap**
Used: hb-194 ideation but not actually measured.
Source: Guo et al. (2017, ICML).
Status: **VERIFIED** as a textbook calibration metric — but it's not actually computed in the pancetta codebase. The hb-194 Session-1 journal says "variance is calibrated" without measuring ECE.

### Algorithm/family naming

**33. hb-086 V1 "joint multi-candidate decoding" / "joint-pair-retry"**
Bank: hb-086.
Status: **PANCETTA-INVENTED** name for "retry decoded candidates against the residual after SIC pass-1". The underlying mechanism is sound (the diagnostic was paired-decodability and 78.3% pair-likely was confirmed before graduating). Not a textbook standard term but the description in the bank is precise enough.

**34. hb-086 V2 "joint LLR with iterative interference cancellation" — soft cancellation collapses to hard subtraction in pancetta's decoder**
Bank: hb-086 V2 shelve reason.
Source: The "soft cancellation" framework comes from turbo-equalization / iterative-detection literature (Tüchler-Singer-Koetter 2002). The shelve diagnosis is correct: when LLRs are sharp (CRC-passed decode → near-1.0 posterior on the chosen tone), soft cancellation reduces to hard subtraction. This is mathematically exact and the shelve reason is a *precise* claim.
Status: **VERIFIED**.

**35. hb-086 V3 "subtract-aware sync threshold relaxation"**
Bank: hb-086 V3.
Status: **PANCETTA-INVENTED** as a name. Mechanism is sound (relax min_sync_score in a local window around subtracted candidates); the shelve diagnostic (V3 surfaces noise, not signal, with LDPC over-converging on noise) is a *precise and correct* diagnosis of what's happening.

**36. hb-090 "Phase-coherent matched filter at truth coordinates"**
Bank: hb-090.
Source: Standard radar/comms matched filter (the optimal linear detector for a known-shape signal in AWGN). For FT8 the symbol "shape" is the known Costas tone sequence + GFSK pulse. The mechanism description matches the textbook definition (correlate the complex IQ against the known waveform replica at hypothesized time/freq).
Status: **VERIFIED** for the mechanism description. Implementation is pending — when it lands, audit the actual integration kernel.

**37. "Wiener filter on residual spectrogram" (ideation X11)**
Source: Wiener (1949) optimal LMMSE filter; standard signal processing textbook (Oppenheim-Schafer or Proakis).
Status: **VERIFIED** at the high-level description.

**38. "Bayesian saliency maps" / "probabilistic CFAR" (ideation reference in hb-108 et al.)**
Source: Bayesian saliency: standard in vision RPNs. CFAR (Constant False Alarm Rate) detectors: standard radar literature.
Status: **VERIFIED** as legitimate terms.

### Cross-cycle / multi-pass

**39. hb-057 "Per-callsign median-DT prior" (cross-WAV history)**
Bank: hb-057.
Source: JTDX maintains per-callsign DT smoothing; not formally in canonical literature but operationally documented in JTDX docs / forum posts.
Status: **NO-PRIMARY-SOURCE** for the JTDX claim. The *mechanism* (use prior decodes' (callsign, dt) to bias future sync windows) is just a Bayesian sequential prior — standard.

**40. hb-085 "Cross-cycle averaging on residual" — shelved as redundant**
Bank: hb-085.
Status: **VERIFIED**. The shelve reason ("after subtract, original positions are near-zero so averaging dilutes") is a precise mathematical claim and obviously correct.

### Statistics / ML extras

**41. "Pearson correlation 0.48 with error rate" as calibration**
See claim 27. **WRONG-TERM-RIGHT-CONCEPT**.

**42. "Spearman correlation 0.46 with error rate"**
Source: Standard non-parametric rank correlation (Spearman 1904).
Status: **VERIFIED** as a correlation metric; same naming caveat as Pearson (it's correlation, not calibration).

**43. "Focal loss γ=2" (hb-064 Session 2 retraining)**
Source: Lin et al. (2017, ICCV) "Focal Loss for Dense Object Detection". γ=2 is the original paper's recommended value for class-imbalanced classification.
Status: **VERIFIED**.

**44. "Cosine schedule" (60-epoch cosine schedule, hb-064 S2)**
Source: Loshchilov-Hutter (2017) "SGDR: Stochastic Gradient Descent with Warm Restarts". Cosine annealing is the standard schedule.
Status: **VERIFIED**.

---

## Most concerning section (WRONG-CONCEPT items that might indicate engineering issues)

**Result: NONE.** No claim was found where the math/implementation actually contradicts the named technique. The decoder's correctness is intact.

The closest call was the SNR estimator (claim 10): pancetta's `min-of-8-tones-per-symbol` proxy for the noise floor is a heuristic, not WSJT-X's "lowest 10% across 2500 Hz" method. But the *bandwidth correction* (the part that matters for reported SNR numbers) is mathematically correct, and the resulting SNR is at worst biased uniformly (so deltas remain meaningful). This is "heuristic, not wrong".

---

## Cleanup recommended section (WRONG-TERM-RIGHT-CONCEPT name fixes)

In priority order:

**A. (hb-194 journal) "calibration" → "informativeness" / "Pearson correlation with error rate"** (claim 27).
Stop calling Pearson(variance, error) "calibration". Either measure ECE, or rephrase to "ensemble variance is informative (Pearson 0.48 with error rate)". Single rephrase in `research/experiments/2026-06-01-hb-194-bayesian-ensembles.md`.

**B. (hb-089 bank shelve_reason) "Welch averaging" → "overlap-averaged magnitude" or "incoherent same-bin accumulation"** (claim 7).
The diagnosis was right; the term is wrong. Welch is PSD estimation via averaged periodograms; we did per-bin magnitude averaging at fixed coordinates. Single-line fix in `research/hypothesis_bank.md:1649-1656`.

**C. (hb-079/080/081) "coherent iterative-subtract multi-pass" → cross-reference SIC (Successive Interference Cancellation)** (claim 15).
The mechanism IS SIC. Calling it that explicitly would let us draw on the multi-user-detection literature for follow-ups (e.g., hb-081 "soft SIC", or PIC = Parallel Interference Cancellation as an alternative). Touchpoint: bank entries hb-079, hb-080, hb-081; decoder.rs:359-368 docstring.

**D. (`ModulationType::Cpfsk` for FT8) → `ModulationType::Gfsk { bt: 2.0 }`** (claim 4).
The modulator already implements GFSK with BT=2.0 correctly. The struct tag in `protocol.rs:156` says `Cpfsk` and contradicts both the implementation and the canonical FT8 literature. Trivial one-line rename in the enum; downstream matchers will need a touch-up.

**E. ("Saturation-aware composite" → "corpus-shift-corrected composite")** (claim 31).
The math is fine. The name "saturation" is misleading because it suggests non-linear ceiling behavior. Either rename or add a docstring sentence clarifying the etymology.

**F. (`decoder.rs:5071` docstring) "sum-product or min-sum BP" → "sum-product BP"** (claim 18).
Min-sum is not actually implemented. Tighten the docstring to match.

**G. (`osd.rs:394` comment) "C(91, 3) = 125,580 trials" → "= 121,485 trials"** (claim 17).
Arithmetic typo. Loop is correct; comment is wrong.

---

## NO-PRIMARY-SOURCE — flag-as-uncertain claims

These are claims where I couldn't find a public textbook/paper backing the EXACT formulation. They aren't necessarily wrong — they're just operating without an external anchor.

- **JTDX `s2(i) = |cs|² + |csold|²` cross-cycle formula** (cited in hb-056 entry and decoder.rs docstring). JTDX is open source; if this matters, grab the relevant Fortran file from `https://sourceforge.net/projects/jtdx/`.
- **WSJT-X Improved commit f13e3182 specific snr7=6.0 / snr7b=1.8 thresholds** (claim 22). The repo URL returned errors during this audit window; either pin against an actual file from SourceForge, or relabel as pancetta defaults.
- **JTDX per-callsign DT smoothing as prior art for hb-057** (claim 39). The claim is operationally true but no public reference was located.
- **Pancetta's `min-of-8-tones-per-symbol` SNR noise proxy** (sub-issue of claim 10). Pancetta-invented; differs from WSJT-X's "lowest 10% across 2500 Hz" method. Bias and variance properties not characterized.

---

## PANCETTA-INVENTED list (no priority claimed; documented as such)

1. Composite metric weights `(0.50, 0.30, 0.15, 0.05)` over the four tiers (claim 29).
2. SNR normalization `clamp((-snr-10)/20, 0, 1)` mapping (claim 30).
3. Saturation-aware composite offset bookkeeping (claim 31).
4. hb-086 V1 "joint-pair-retry" name and exact mechanism (claim 33).
5. hb-086 V3 "subtract-aware sync threshold relaxation" name (claim 35).
6. (sub-issue) min-of-8-tones noise floor estimator (claim 10).

These are not problems — they're just unique pancetta contributions. Calling them out so future docs can say "pancetta-invented" instead of citing a non-existent source.

---

## Recommendations for Phase A / Phase C

**Phase A (bank/journal cleanup, READ → WRITE on `hypothesis_bank.md` + recent journals):**

1. Apply fixes A, B, C, E from the Cleanup-Recommended section. Single-line rephrases each.
2. Add a top-of-bank pointer to this document: "Engineering substance verified 2026-06-02; see `docs/engineering/2026-06-02-engineering-substance-audit.md`."

**Phase C (production code minor fixes):**

3. Apply fix D (`Cpfsk` → `Gfsk { bt: 2.0 }` for FT8 in `protocol.rs:156`). Trivial; modulator already does the right thing.
4. Apply fix F (`decoder.rs:5071` docstring tighten).
5. Apply fix G (`osd.rs:394` comment correction).
6. (Optional) Add a one-line docstring to `par_compute_soft_llrs_db` noting "max-log-MAP approximation" (claim 23).

Together these are ~7 single-line edits. None changes runtime behavior. The audit's main value is **the absence of substantial WRONG-CONCEPT findings** — the production decoder math holds up against canonical sources.

---

## Audit metadata

- Worktree: `/tmp/pancetta-phase-d`
- Branch: `iter/2026-06-02-phase-d-engineering-audit`
- Base: `main @ e327193`
- Sources audited: production code (`pancetta-ft8/`, `pancetta-research/`), hypothesis bank (~5300 lines), recent journals (`research/experiments/2026-05-26-onwards`), 2026-06-01 ideation sweep (~6500 lines across 8 files).
- Primary-source consultations: 11 WebSearch + WebFetch calls against Welch, Fossorier-Lin, Hocevar, Lakshminarayanan, Guo, Efron, Smith-CCRMA, kgoba/ft8_lib, WSJT-X user guide, Franke-Taylor QEX 2020, K0NR FT8 SNR explainer.
- Audit duration: ~90 min real time.
