# Hypothesis Bank

last_updated: 2026-05-20T17:00:00Z
current_focus_mode: ft8
wild_card_ratio_target: 0.20
wild_cards_run: 0
exploitation_run: 0
current_ratio: 0.0

## Active (ranked by score)

### hb-001 — Multi-pass subtract-and-redecode  [PRIORITY: 0.82]
  mode: ft8
  status: pending
  priority_score: 0.82
  estimated_effort: 2 sessions
  expected_delta: +0.05 to +0.15 real decode rate on hard-200
  defensible_prior: yes
  wild_card: false
  evidence_for:
    - Memory: "5-10% of WSJT-X decode rate on identical bands" — multi-pass is the documented primary WSJT-X advantage on busy bands
    - Decoder source (decoder.rs:384): pass 2+ truncates candidate list to 40, but the full spectrogram is rebuilt from scratch each pass; signal subtraction quality is the key lever
    - subtract_with_sidelobes is called, meaning sidelobe suppression is already in place — the question is whether residual quality is good enough to surface sub-threshold signals
    - WSJT-X defaults to 3 passes; pancetta's `max_decode_passes` default is also 3 but the curated-hard-200 real decode rate is unknown (not yet in main.json)
  evidence_against:
    - Risk of compounding false positives across passes if subtraction is imperfect
    - Each pass rebuilds the full spectrogram (~O(N) FFT cost per pass)
  notes: |
    The core question: does our subtract_with_sidelobes produce a residual clean
    enough for pass 2 to find new decodes? The plan 2 scorecard only ran synth
    + fixtures — no real curated data yet. Once the curated-hard-200 baseline is
    in, this hypothesis is directly measurable. Experiment: sweep max_decode_passes
    from 1 to 4 with curated-hard-200 as the primary metric. Instrument new
    decodes-per-pass counts in the scorecard.

### hb-002 — Synth plateau investigation (1-of-6 message type)  [PRIORITY: 0.75]
  mode: ft8
  status: pending
  priority_score: 0.75
  estimated_effort: 1 session
  expected_delta: +0.02 to +0.08 synth-clean composite; potential fixture pass-rate lift
  defensible_prior: yes
  wild_card: false
  evidence_for:
    - Plan 2 main.json: SNR@50% recovery = -20.0 dB; at -18 dB only 5/6 messages decoded (83%), same 5/6 pattern at -16, -14, -12, -10 dB — one message type plateaus regardless of SNR
    - At high SNR (-10 dB) a 5/6 ceiling suggests a structural failure, not a sensitivity issue: the 6th message is just not getting through at any SNR
    - Six distinct FT8 message types exist in the synth corpus; the failing one is likely a type-1 "non-standard" or grid/suffix variant that exercises a different message parser path
    - This is a regression-free investigation: read the synth manifest, identify which message always fails, trace the path through MessageParser
  evidence_against:
    - May be a synth generation issue (wrong ground-truth in manifest) rather than a decoder bug
    - If only 1 message variant, the absolute decode-rate impact is ~1/N of the corpus
  notes: |
    First experiment: run the harness in --quick mode, capture per-message decode
    results, identify the invariant failure. Compare the failing WAV's encoded
    message against the ground truth in the synth manifest. If the ground truth
    is wrong, fix the generator. If the ground truth is right, trace the failure
    through MessageParser. This is a no-code-change investigation first; code
    change (if any) follows as hb-002b.

### hb-003 — Sync candidate count sweep  [PRIORITY: 0.70]
  mode: ft8
  status: pending
  priority_score: 0.70
  estimated_effort: 1 session
  expected_delta: +0.02 to +0.06 real decode rate; potential SNR@50% improvement of 0.5-1.5 dB
  defensible_prior: yes
  wild_card: false
  evidence_for:
    - decoder.rs:35: MAX_DECODE_CANDIDATES = 100 (hard constant); pass 2+ truncates to 40 (line 385)
    - decoder.rs:63: MAX_SYNC_CANDIDATES = 100 — same cap on sync search output
    - If the 101st-ranked candidate is a real signal (possible in a busy FT8 band with 50+ simultaneous QSOs), it's silently dropped before LDPC even runs
    - WSJT-X processes more candidates on busy bands; JTDX is documented to run more candidate trials
    - Low-effort sweep: run eval with max_candidates in {50, 100, 150, 200} and record decode rate vs wall-clock time
  evidence_against:
    - Increasing candidates raises CPU per 15s slot; at 200 candidates and OSD-2 the budget timer may kick in
    - The budget tracker (decoder.rs:356) already exists precisely for this; hitting the budget could harm sensitivity on the remaining candidates
  notes: |
    Two sub-experiments: (a) raise MAX_SYNC_CANDIDATES to 150-200 without raising
    MAX_DECODE_CANDIDATES (more candidates enter NMS, best 100 still decoded);
    (b) raise both. Measure per-pass decode counts and budget-expiry rate.
    If budget expiry increases, tune the time budget alongside.

### hb-004 — AP-survival gate retune  [PRIORITY: 0.67]
  mode: ft8
  status: pending
  priority_score: 0.67
  estimated_effort: 1 session
  expected_delta: +0.01 to +0.04 composite score; lift on QSO-mode scenarios
  defensible_prior: yes
  wild_card: false
  evidence_for:
    - decoder.rs:490-493: AP decode only fires for candidates with sync_score >= 3.0 (MIN_SYNC_SCORE_FOR_AP). Comment says "Sync scores below 4.0 are likely noise" — yet the threshold is 3.0. Inconsistency suggests the threshold was tuned conservatively.
    - The memory notes (decoder_status.md) record that AP thresholds were set via manual tuning; a systematic sweep against a QSO-pattern corpus hasn't been done
    - AP levels 1-4 exist (ap.rs:182-194): Ap1 injects own call, Ap2 injects caller, Ap3 injects both, Ap4 adds i3 type. Higher AP levels at very low SNR could recover QSO exchanges missed by AP0
    - Memory (decoder_sensitivity.md): "AP decoding levels 3-4 (verify which levels are implemented)" — suggests AP3/AP4 activation rate is unknown
  evidence_against:
    - Lowering the threshold risks injecting known callsigns into noise, producing false QSO decodes (a security concern per C-1)
    - Without a curated QSO-pattern corpus, measuring AP recall improvement is indirect
  notes: |
    Sweep MIN_SYNC_SCORE_FOR_AP from 2.0 to 5.0 in 0.5 steps on curated-hard-200.
    Track: new real decodes found vs false positives introduced. The fixture tier
    acts as a regression guard. Also audit how often AP3 and AP4 actually fire
    in normal decoding runs by adding a counter to the metrics struct.

### hb-005 — OSD beta parameter + iteration sweep  [PRIORITY: 0.63]
  mode: ft8
  status: pending
  priority_score: 0.63
  estimated_effort: 1 session
  expected_delta: +0.01 to +0.05 synth sensitivity; potential fixture regression risk
  defensible_prior: yes
  wild_card: false
  evidence_for:
    - decoder.rs:115: osd_depth: Option<u8>, comment explicitly notes "OSD-2 (4,187 trials) has a high CRC-14 false positive rate without additional validation"
    - osd.rs default (line 34): max_depth = 1 ("OSD-1 is the safe default") — but main.json config shows osd_depth: 2 in practice
    - This contradiction: the module default is 1, the decoder's Default impl sets Some(2). The actual OSD depth in production is 2, not 1.
    - OSD-2 trial count (4,187) vs OSD-1 (92): ~45x more trials for OSD-2. With current parity gate ≤4, false positive rate is claimed high without "additional validation" — but that validation may not be fully implemented
    - LDPC iterations default 25; WSJT-X uses 50 iterations. More LDPC iterations before falling back to OSD could reduce the workload on OSD.
  evidence_against:
    - OSD-3 (125K trials) is mentioned in memory as a future option — but the combinatorial explosion makes it impractical without a stronger FP filter
    - Changing OSD depth could change the fixture baseline, triggering auto-regression flags
  notes: |
    Two sub-experiments: (a) compare OSD-1 vs OSD-2 vs disabled on synth corpus —
    quantify exactly how many decodes are OSD-only vs LDPC-only; (b) sweep LDPC
    iterations from 25 to 50 with OSD-1 and check whether more LDPC iterations
    substitute for OSD-2 on the hard cases. The "additional validation" comment
    suggests there's an unguarded FP path in OSD-2 worth auditing.

### hb-006 — LLR normalization target tuning  [PRIORITY: 0.58]
  mode: ft8
  status: pending
  priority_score: 0.58
  estimated_effort: 1 session
  expected_delta: +0.01 to +0.04 SNR@50% synth-clean
  defensible_prior: yes
  wild_card: false
  evidence_for:
    - decoder.rs:57: LLR_TARGET_VARIANCE = 24.0, comment "matches ft8_lib's ftx_normalize_logl". This is correct at initialization, but the normalization target may need tuning for different channel conditions (multipath, Doppler)
    - LDPC sum-product performance is sensitive to LLR scaling: over-scaled LLRs cause BP to converge too aggressively to wrong codewords; under-scaled LLRs slow convergence
    - The target variance of 24.0 is empirically matched to AWGN; the Doppler/multipath synth tier may benefit from a different value
    - WSJT-X has noise floor estimation that feeds back into LLR normalization dynamically
  evidence_against:
    - If ft8_lib uses 24.0 and we match it, changing this could diverge from reference behavior
    - Harder to validate correctness without a clean theoretical derivation
  notes: |
    Sweep LLR_TARGET_VARIANCE from 16.0 to 36.0 in steps of 4. Measure synth-clean
    and synth-doppler SNR@50% separately (the doppler tier benefits most if the
    hypothesis is correct). Also check whether per-candidate LLR renormalization
    (after spectrogram-based extraction) could improve the consistency.

### hb-007 — MIN_SYNC_SCORE threshold sweep  [PRIORITY: 0.56]
  mode: ft8
  status: pending
  priority_score: 0.56
  estimated_effort: 1 session
  expected_delta: +0.01 to +0.04 real decode rate; FP risk if threshold too low
  defensible_prior: yes
  wild_card: false
  evidence_for:
    - decoder.rs:60: MIN_SYNC_SCORE = 3.0. This is the Costas correlation threshold below which a candidate is silently dropped before LDPC runs.
    - On the curated hard-200 corpus (busy band conditions), some real signals may have sync scores just below 3.0 due to interference from adjacent signals
    - ft8_lib uses a similar threshold but the exact value differs; WSJT-X's jt9 binary may accept lower-scoring candidates that it then validates with CRC
    - Lowering to 2.0 could surface marginal candidates; LDPC + CRC act as the real filter
  evidence_against:
    - Lower threshold → more candidates → longer decode time; budget tracker may kick in
    - More noise candidates → higher LDPC failure rate → wasted CPU without benefit
  notes: |
    Sweep MIN_SYNC_SCORE from 1.5 to 4.0 in steps of 0.5. Track: new unique decodes
    found vs new false positives per step, and wall-clock time vs decode count.
    Budget impact: if at 1.5 we exceed the 2000ms budget, try with a higher max-
    time budget as a separate sub-experiment to separate budget from threshold effects.

### hb-008 — NMS radius parameter sweep  [PRIORITY: 0.52]
  mode: ft8
  status: pending
  priority_score: 0.52
  estimated_effort: 1 session
  expected_delta: +0.01 to +0.03 real decode rate on dense-band recordings
  defensible_prior: yes
  wild_card: false
  evidence_for:
    - decoder.rs:69: NMS_TIME_RADIUS = 4 * TIME_OSR = 8 time steps; NMS_FREQ_RADIUS = 2 bins
    - Non-maximum suppression with too-large a radius may merge distinct candidates from closely spaced signals (e.g., two QSOs 12.5 Hz apart — 2 bins)
    - On a busy 40m or 20m FT8 band, signal density can exceed 1 signal per 25 Hz passband
    - Tightening NMS_FREQ_RADIUS from 2 to 1 could separate candidates that are currently merged into one
  evidence_against:
    - Too-small NMS radius → duplicate candidates for the same signal → wasted LDPC budget
    - TIME_OSR=2 means NMS_TIME_RADIUS=8 corresponds to 4 symbols, which is already fairly tight
  notes: |
    Sweep NMS_FREQ_RADIUS in {1, 2, 3} and NMS_TIME_RADIUS in {4, 6, 8} time steps.
    Focus on the curated-hard-200 tier which contains real busy-band recordings.
    Measure candidate count per WAV vs decode count (if candidates spike without
    decode improvement, NMS is too permissive).

### hb-009 — Block score ranking vs sync-only ranking  [PRIORITY: 0.50]
  mode: ft8
  status: pending
  priority_score: 0.50
  estimated_effort: 1 session
  expected_delta: +0.01 to +0.03 real decode rate; possibly more on doppler tier
  defensible_prior: partial
  wild_card: false
  evidence_for:
    - decoder.rs:388-398: Re-ranks by block_score after sync search. Block score is likely a combined metric (not just Costas peak). The ranking determines which candidates get LDPC budget first.
    - If budget expires (budget.expired() check at line 368), lower-ranked candidates are skipped. Better ranking of "most likely to yield a real decode" candidates would maximize decodes-per-budget-dollar.
    - Alternative ranking strategies: confidence score from AP, correlation energy (already computed in subtract path), estimated SNR from spectrogram power
  evidence_against:
    - Block score was deliberately added as an improvement over sync-only; the marginal gain from further refinement may be small
    - Need to read the full block_score implementation to understand what it already captures
  notes: |
    Read block_score implementation fully. Hypothesis: block score doesn't yet
    incorporate the estimated signal amplitude (correlation_energy function at
    decoder.rs:619). Adding correlation energy as a weighting factor in candidate
    ranking could prioritize the strongest signals for LDPC first, improving
    decodes-per-budget on time-limited runs. Prototype: add a correlation_energy
    pass over the top-50 candidates, re-rank, measure.

### hb-010 — Spectrogram window function sweep  [PRIORITY: 0.47]
  mode: ft8
  status: pending
  priority_score: 0.47
  estimated_effort: 1 session
  expected_delta: +0.005 to +0.02 SNR@50% synth-clean
  defensible_prior: partial
  wild_card: false
  evidence_for:
    - signal_processing.rs: WindowFunction enum supports Hann (default), Hamming, Blackman, Kaiser(beta), Rectangle
    - decoder.rs:270-281: Spectrogram uses sin²(πi/N) (Hann-equivalent, normalized). ft8_lib uses the same.
    - decoder.rs:251: FftProcessor for waterfall uses Hann explicitly; the spectrogram window is separate (sin²)
    - Blackman window has better sidelobe suppression (-58 dB vs -31 dB for Hann) which could reduce inter-symbol interference in the spectrogram; at the cost of slightly wider main lobe
    - Kaiser(β=6) is close to optimal for sidelobe rejection + main lobe width tradeoff
  evidence_against:
    - ft8_lib uses Hann (sin²) and matching it was deliberate for bit-exact parity; deviating may help sensitivity but break reference parity
    - The spectrogram window affects LLR quality; wrong tradeoff could hurt LDPC convergence
  notes: |
    Experiment: parameterize the spectrogram window function (currently hardcoded
    as sin²). Sweep Hann (baseline), Blackman, Kaiser(β=5), Kaiser(β=8). Measure
    synth-clean SNR@50% and curated-hard-200 decode rate. Expect diminishing
    returns vs ft8_lib parity risk; worth a single experiment to close the question.

### hb-011 — LDPC iteration count sweep (25 → 50)  [PRIORITY: 0.46]
  mode: ft8
  status: pending
  priority_score: 0.46
  estimated_effort: 1 session
  expected_delta: +0.005 to +0.02 synth sensitivity at low SNR
  defensible_prior: partial
  wild_card: false
  evidence_for:
    - decoder.rs:37: LDPC_MAX_ITERATIONS = 25. WSJT-X's jt9 uses 50 iterations by default.
    - LDPC belief propagation often converges in 10-15 iterations for strong signals; the 25→50 gap only matters for marginal signals that haven't converged by iteration 25
    - The cost is proportional: 2x iterations = ~2x LDPC time per candidate. With rayon parallelism and the budget tracker, this may be acceptable.
    - Memory (decoder_status.md): current speed is 0.37s/window release. At 50 iterations, budget timer may become a bottleneck.
  evidence_against:
    - If the LDPC code can't converge in 25 iterations on a noisy codeword, it's unlikely to converge in 50 — there may be a cycle in the Tanner graph
    - 2x LDPC cost means OSD (fallback) triggers less often; the net effect on low-SNR sensitivity is unclear
  notes: |
    Sweep ldpc_iterations in {25, 35, 50} on synth-clean corpus at -22 to -18 dB
    range (the sensitivity cliff). Track convergence rate (fraction of candidates
    that converge in ≤N iterations) as a separate metric to understand where
    the benefit actually comes from.

### hb-012 — Negative time offset extension (early-arriving DX signals)  [PRIORITY: 0.44]
  mode: ft8
  status: pending
  priority_score: 0.44
  estimated_effort: 1 session
  expected_delta: +0.01 to +0.03 real decode rate on DX recordings
  defensible_prior: partial
  wild_card: false
  evidence_for:
    - Memory (decoder_sensitivity.md): "A3: Sync search starts at t0=0 (extend for early-arriving DX signals)" — this was identified as a remaining gap after Phase A
    - FT8 messages from DX stations (long path, polar) can arrive up to 2 seconds early relative to the nominal slot start due to propagation timing
    - decoder.rs:58: time_range: 2.0 (seconds) — searches ±2s but may start at t=0 not t=-2s
    - The Spectrogram struct has a time_padding field (line 152) for negative-time search, suggesting the infrastructure exists but the range may not be fully utilized
  evidence_against:
    - If time_range already covers negative offsets, this may already be fixed
    - Extending the search window increases candidate count; budget pressure applies
  notes: |
    First: read the costas_sync_search implementation to confirm current time range.
    Check whether time_padding is set to a nonzero value in practice. If sync
    starts at t=0, extend to t=-1.0 (one symbol duration) and measure on the
    curated corpus. DX-heavy recordings (jtdx decoded more than jt9) in hard-200
    are the best test set for this.

### hb-013 — MIN_FREQ_BIN floor reduction (below 100 Hz coverage)  [PRIORITY: 0.42]
  mode: ft8
  status: pending
  priority_score: 0.42
  estimated_effort: 0.5 sessions
  expected_delta: +0.005 to +0.02 real decode rate on recordings with low-frequency signals
  defensible_prior: partial
  wild_card: false
  evidence_for:
    - decoder.rs:66: MIN_FREQ_BIN = 0 — currently set to 0, meaning full passband coverage is already enabled
    - Memory (decoder_sensitivity.md): "A4: MIN_FREQ_BIN still 16 (lower to 0 for on-air coverage below 100 Hz)" — this was a documented gap in Phase A
    - If MIN_FREQ_BIN is now 0 in the source, the gap may already be closed; but the composite score impact (real decode rate on curated corpus) hasn't been measured
  evidence_against:
    - If MIN_FREQ_BIN is already 0, this experiment is a no-op — value is in confirming the fix landed
    - Very low frequency signals (<100 Hz audio) are rare on typical FT8 operating practice
  notes: |
    Quick audit: confirm MIN_FREQ_BIN = 0 in current source (it is — decoder.rs:66).
    Then run eval on curated-hard-200 and confirm no candidates are being missed
    below 100 Hz (check per-WAV frequency distributions in the scorecard). If the
    gap is confirmed closed, document as "already fixed" and shelve hb-013.
    Effort: 0.5 sessions (mostly verification).

### hb-014 — Neural OSD confidence gating (parity threshold sweep)  [PRIORITY: 0.41]
  mode: ft8
  status: pending
  priority_score: 0.41
  estimated_effort: 1 session
  expected_delta: +0.005 to +0.02 composite; possible FP reduction
  defensible_prior: partial
  wild_card: false
  evidence_for:
    - Memory (decoder_status.md): "OSD depth: 2 (safe default). Parity gate: ≤4" — parity gate limits OSD to candidates with ≤4 unsatisfied parity checks
    - History shows: 139.5% → 123.7% was "entirely removing false decodes, not losing real ones" — the parity gate was tightened at cost of some false decodes
    - A neural classifier on parity check patterns (which checks failed) could more precisely gate OSD than a raw count threshold
    - The existing neural OSD (training/neural_osd/) provides a precedent for the ML-in-decode pattern
  evidence_against:
    - Neural OSD was already trained with DIA model (20K params); adding a second neural stage compounds latency
    - May be better framed as "improve the existing neural OSD model" rather than a second gate
  notes: |
    Simpler angle: sweep the parity gate threshold from ≤3 to ≤6 on the synth
    corpus to understand the sensitivity vs FP tradeoff curve, before committing
    to a neural gate. A parity gate of ≤5 (wider) may recover some real decodes
    that were caught by the tightening from 139.5% to 123.7%. Check: how many
    of those recovered decodes would have been real vs noise on the curated corpus.

### hb-015 — Doppler-resilient sync search (phase-coherent integration)  [PRIORITY: 0.38]
  mode: ft8
  status: pending
  priority_score: 0.38
  estimated_effort: 2 sessions
  expected_delta: +0.01 to +0.04 synth-doppler SNR@50%
  defensible_prior: partial
  wild_card: false
  evidence_for:
    - The Doppler synth tier exists (referenced in composite weights) but the composite score currently has no Doppler data (snr_50pct_synth_doppler is absent from main.json)
    - FT8 Doppler resilience: ±1 Hz/s drift over a 12.64s window shifts the signal by ~12 Hz — more than one tone bin. The spectrogram with FREQ_OSR=2 helps, but a drift-aware sync that integrates across a frequency ramp would be more robust
    - JTDX is documented to outperform WSJT-X specifically on Doppler-distorted paths (polar/satellite) — this is a known gap for all decoders including pancetta
  evidence_against:
    - Doppler-resilient integration is algorithmically complex; 2-session estimate may be optimistic
    - The Doppler synth tier WAVs aren't generated yet — the experiment's denominator (baseline decodes on Doppler WAVs) is unknown
  notes: |
    Prerequisites: generate Doppler synth corpus first (this is blocked on T16/T17
    infrastructure work). Hypothesis: add a secondary sync pass with linear frequency
    drift compensation (chirp-Z transform or frequency-tracking DFT). Measure
    improvement specifically on synth-doppler tier. May interact positively with hb-001
    (multi-pass) since Doppler-distorted signals are harder to subtract cleanly.

### hb-016 — Residual energy early-stop for multi-pass  [PRIORITY: 0.36]
  mode: ft8
  status: pending
  priority_score: 0.36
  estimated_effort: 0.5 sessions
  expected_delta: +0.01 to +0.02 throughput (CPU saved); neutral real decode rate
  defensible_prior: partial
  wild_card: false
  evidence_for:
    - decoder.rs:525-529: Signal subtraction happens before each additional pass, but there's no check on whether the residual still contains signal energy worth decoding
    - If pass 2 produces 0 new decodes (line 513: "if pass_unique.is_empty() { break }"), the loop already stops — but only after running the full spectrogram + sync search on an empty residual
    - Adding a residual energy check (compare noise floor estimate before and after subtraction) before computing the spectrogram on pass 2+ would save CPU on clean-channel recordings where pass 1 recovers everything
  evidence_against:
    - The existing empty-pass-break (line 513) already handles the most common case; the spectrogram cost per pass is fixed and relatively small
    - Residual energy estimation adds complexity for marginal CPU savings
  notes: |
    Synergizes with hb-001 (multi-pass). Implement after hb-001 has landed and
    multi-pass decode rates are measured. If pass 2+ routinely finds 0 new decodes
    on synth-clean (it probably does at high SNR), this optimization saves real
    wall-clock time. Use noise.rs estimate_noise_floor_db as the energy probe.

### hb-017 — AP2 caller pool expansion (recent-QSO callsign injection)  [PRIORITY: 0.34]
  mode: ft8
  status: pending
  priority_score: 0.34
  estimated_effort: 1 session
  expected_delta: +0.005 to +0.02 real decode rate in autonomous QSO scenarios
  defensible_prior: partial
  wild_card: false
  evidence_for:
    - ap.rs:185-193: Ap2 injects "a recent caller's callsign at bits 0-27" — the caller is "selected externally via inject_ap2_caller"
    - In practice, AP2 can inject only one caller at a time. On a busy band, there may be 5-10 callers responding to our CQ; injecting only 1 means 4-9 callers get only AP0
    - WSJT-X's AP2 mode tries multiple recent callers in sequence, effectively expanding the pool
    - The decoded_calls HashSet (decoder.rs:439) tracks already-decoded callsigns for AP2 short-circuit; expand this to also maintain a "candidate AP2 pool" of known-active callers
  evidence_against:
    - AP2 with multiple callers multiplies the candidate × caller trials (N candidates × M callers); budget impact could be significant
    - False AP2 injections (wrong callsign into wrong candidate) risk partial decodes that pass CRC by coincidence
  notes: |
    Experiment design: extend ApContext.active_qso to carry a pool of recent
    callers (Vec<String>, max 5). In par_try_ap_decode, iterate the pool for each
    AP2 candidate. Measure on a curated recording known to have multiple simultaneous
    callers (identified from the hard-200 manifest's per-WAV truth count).

### hb-018 — OSD-3 with strengthened CRC validation  [PRIORITY: 0.30]
  mode: ft8
  status: pending
  priority_score: 0.30
  estimated_effort: 3 sessions
  expected_delta: +0.005 to +0.03 synth sensitivity below -22 dB; high FP risk without mitigation
  defensible_prior: partial
  wild_card: false
  evidence_for:
    - Memory (decoder_sensitivity.md): "OSD-3 (triple bit-flip, 125K trials)" listed as a future option
    - The OSD-2 comment (decoder.rs:111-113) explicitly calls out CRC-14 FP rate as the blocker for OSD-2+ without "additional validation"
    - CRC-14 has 2^14 = 16,384 valid codewords; at 125K trials (OSD-3), random collision probability is ~0.8% — not negligible
    - Mitigation: require OSD-3 candidates to also pass a message parsability check (is the decoded 77-bit payload a valid FT8 message structure? valid callsign characters, valid grid format etc.)
  evidence_against:
    - Message parsability check adds complexity and may reject valid messages with non-standard callsign formats
    - 125K trials × N candidates × M passes = very high CPU; budget timer would need significant extension
  notes: |
    Blocking dependency: strengthen the FP filter first (hb-014 parity gate sweep,
    plus message-validity post-filter). OSD-3 is only worth attempting after we
    understand OSD-2's FP rate on the curated corpus. Do hb-005 first, use its
    learnings to decide whether OSD-3 is worth pursuing.

### hb-019 — Wild-card: disable NMS entirely, rely on LDPC dedup  [PRIORITY: wild]
  mode: ft8
  status: pending
  priority_score: 0.0
  estimated_effort: 0.5 sessions
  expected_delta: unknown; possibly +0.02 to -0.05 real decode rate
  defensible_prior: no
  wild_card: true
  evidence_for:
    - NMS is a heuristic for efficiency; LDPC + CRC provides the real validation
    - If two candidates overlap in time-frequency space, NMS may suppress the weaker one even if it would decode to a different message (adjacent-frequency QSOs)
    - Removing NMS lets all Costas peaks compete in LDPC — at the cost of more redundant decoding work
  evidence_against:
    - Without NMS, the same strong signal would generate O(radius²) redundant candidates — massive CPU waste
    - Budget timer would almost certainly expire before processing all candidates
    - Very likely a regression; purely exploratory
  notes: |
    Cheap experiment: set NMS_TIME_RADIUS = 0, NMS_FREQ_RADIUS = 0. Measure
    candidate count explosion and whether any new unique decodes appear. If
    candidate count triples with zero new decodes, NMS is vindicated. If even
    one new decode appears that was being suppressed, redesign NMS with a smaller
    radius rather than eliminating it.

### hb-020 — Wild-card: aggressive_decoding flag audit  [PRIORITY: wild]
  mode: ft8
  status: pending
  priority_score: 0.0
  estimated_effort: 0.5 sessions
  expected_delta: unknown; likely +0.01 to +0.03 with unknown FP risk
  defensible_prior: no
  wild_card: true
  evidence_for:
    - decoder.rs:100-101: `aggressive_decoding: bool` — "Enable aggressive decoding (more CPU, better weak signal performance)" — but the main.json config shows `aggressive_decoding: false`
    - The field exists in Ft8Config but the baseline has never been run with it enabled; its actual effect on the decode pipeline is unknown without reading all callsites
    - If aggressive_decoding unlocks additional code paths that were deemed too risky for production, those paths may be exactly what we want in a research context
  evidence_against:
    - May not be implemented at all (the field exists but callsites may just check it without doing anything different)
    - "Aggressive" may refer to a parameter we already tuned separately (e.g., candidate count, OSD depth)
  notes: |
    First step: grep all callsites of aggressive_decoding in pancetta-ft8/src/.
    If it does nothing, document and shelve. If it unlocks real behavior, run eval
    with it enabled and measure the actual delta. This is a "what does this flag
    actually do?" experiment.

### hb-021 — Wild-card: frequency-domain signal subtraction  [PRIORITY: wild]
  mode: ft8
  status: pending
  priority_score: 0.0
  estimated_effort: 3 sessions
  expected_delta: unknown; possibly +0.02 to +0.08 on busy bands
  defensible_prior: no
  wild_card: true
  evidence_for:
    - Current subtract_with_sidelobes operates in the time domain (reconstructs CPFSK and subtracts from audio)
    - Frequency-domain subtraction (zero out the decoded signal's bins in the spectrogram, reuse the existing spectrogram for pass 2) avoids rebuilding the spectrogram from scratch each pass
    - JTDX is rumored to use spectrogram-domain subtraction; no public source confirmation
    - If spectrogram is reused, pass 2+ is much cheaper: no re-FFT, just updated power values
  evidence_against:
    - Spectrogram-domain subtraction loses phase information that time-domain subtraction preserves — could corrupt adjacent signals
    - Building a correct frequency-domain subtract that handles the FFT windowing correctly is non-trivial; easy to introduce subtle bugs
    - Savings only matter if the spectrogram recompute is a significant fraction of pass time (need profiling data first)
  notes: |
    Prerequisite: profile where time is actually spent in a multi-pass decode (is
    it the spectrogram FFT, the LDPC iterations, or the candidate loop?). If
    spectrogram is <20% of pass time, this experiment's motivation is weak.
    High-effort wild-card; only attempt after profiling confirms the spectrogram
    is a bottleneck.

### hb-022 — Wild-card: per-candidate SNR-adaptive LDPC iteration count  [PRIORITY: wild]
  mode: ft8
  status: pending
  priority_score: 0.0
  estimated_effort: 1.5 sessions
  expected_delta: unknown; possibly +0.01 to +0.02 throughput; neutral sensitivity
  defensible_prior: no
  wild_card: true
  evidence_for:
    - All candidates currently get LDPC_MAX_ITERATIONS = 25 iterations regardless of their sync score or estimated SNR
    - High-SNR candidates converge in 5-10 iterations; low-SNR candidates may never converge in 50
    - Adaptive scheduling: give high-SNR candidates fewer iterations (free up budget); give low-SNR candidates more (spend the saved budget productively)
    - This could improve total decodes-per-budget on mixed-SNR recording
  evidence_against:
    - LDPC convergence is not monotonic in SNR — a "high sync score" candidate can still be a hard codeword
    - Complexity of per-candidate scheduling in the rayon parallel loop
  notes: |
    Prototype: use sync_score as a proxy for SNR. High score (>8.0) → 15 iterations.
    Medium (4.0-8.0) → 25 iterations. Low (<4.0) → 35 iterations. Measure total
    iterations consumed vs decodes gained. If the distribution of convergence
    iterations is already known (add a counter), this could be informed rather than heuristic.

## Shelved (kept for reference)

(empty)

## Graduated (merged to main)

(empty)
