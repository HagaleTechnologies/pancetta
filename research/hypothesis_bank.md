# Hypothesis Bank

last_updated: 2026-05-21T01:00:00Z
current_focus_mode: ft8
wild_card_ratio_target: 0.20
wild_cards_run: 0
exploitation_run: 2
current_ratio: 0.0

## Active (ranked by score)

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

### hb-030 — subtract_with_sidelobes residual quality audit  [PRIORITY: 0.60]
  mode: ft8
  status: pending
  priority_score: 0.60
  estimated_effort: 1-2 sessions
  expected_delta: diagnostic; informs whether to redesign subtraction kernel
  defensible_prior: yes (hb-001 sweep directly motivated this — see 2026-05-21 journal)
  wild_card: false
  evidence_for:
    - hb-001 sweep (2026-05-21) measured pass-2+ multi-pass contribution at +1.2% real decode rate on hard-200, vs the +5-15% hypothesized
    - Pass 2 contribution: +43 decodes; pass 3 contribution: +3; pass 4 contribution: +1 — sharp diminishing returns after one subtraction
    - synth-clean shows IDENTICAL decode tables at max_passes ∈ {1,2,3,4} — multi-pass adds zero on clean signals
    - Either subtraction leaves artifacts that mask weak signals, or weak residual signals fall below sync threshold — current evidence can't distinguish
  evidence_against:
    - Diagnostic-only experiment; no guaranteed code change drops out
    - If the answer is "subtraction is fundamentally limited by FT8's overlapped-symbol structure" no fix is forthcoming
  notes: |
    Controlled two-signal synth experiment: generate WAV with strong signal
    at SNR=-5 dB and weak signal at SNR=-18 dB, offset 25 Hz apart in
    frequency. Decode the strong, subtract it, then measure:
    (a) Spectrogram power at the weak signal's TF cell, before and after
        subtraction — quantifies subtraction quality.
    (b) Sync score at the weak signal's TF location, before and after —
        does subtraction surface a sync candidate that pass 1 missed?
    (c) LDPC convergence rate at the weak candidate, before and after.
    If (a) shows large residual artifacts at the strong signal's harmonics
    bleeding into the weak signal's TF cell, the kernel needs sidelobe
    work. If (a) is clean but (b) doesn't improve, the issue is sync
    threshold not subtraction quality. If both improve but (c) still
    fails, the issue is LLR scaling on subtracted residuals.

### hb-031 — Fast-path single-pass mode for autonomous-loop latency  [PRIORITY: 0.40]
  mode: ft8
  status: pending
  priority_score: 0.40
  estimated_effort: 0.5 sessions
  expected_delta: 9-10× decode latency reduction at cost of ~1.2% real decode rate
  defensible_prior: yes (hb-001 sweep showed pass 1 = 48ms, pass 2 = 382ms; 98.8% recall at pass 1 alone)
  wild_card: false
  evidence_for:
    - Pass 1 alone recovers 3786 of 3832 multi-pass decodes (98.8%) on hard-200
    - Pass 1 wall-clock: 48ms/WAV. Pass 2+ adds 335ms/WAV.
    - Operator's autonomous loop runs every 15s slot; 50-100ms per decode would let multiple decode windows run concurrently on slow CPUs
    - Already gated by Ft8Config::aggressive_decoding (currently unused — see hb-020)
  evidence_against:
    - Losing 1.2% decodes on busy bands matters for the rare-station hunter use case
    - Adds operating-mode complexity to the coordinator
  notes: |
    Pure plumbing: add a "decode_mode: latency | balanced | deep" field to
    pancetta-config and wire through to the coordinator. Latency mode sets
    max_decode_passes=1. Balanced (default) stays at 3. Deep can go higher
    (5+) for offline batch reprocessing.
    Synergizes with hb-020 (aggressive_decoding audit): if that flag turns
    out to do nothing, this is what it should do.

### hb-029 — Exact-format Display tests for every message subtype  [PRIORITY: 0.45]
  mode: ft8
  status: pending
  priority_score: 0.45
  estimated_effort: 1 session
  expected_delta: diagnostic; surfaces hidden text-format bugs (precedent: hb-023 found ~1900 phantom-novel decodes that were format-mismatched true positives)
  defensible_prior: yes (concrete bug class identified during hb-023)
  wild_card: false
  evidence_for:
    - hb-023 found that the ReportWithR Display impl produced "R -12" (with space) instead of the canonical "R-12". Until fixed, ~1900 decodes per eval run were silently mis-classified as "novel" on the curated tiers — they were correct decodes that the text-match comparator couldn't see.
    - No existing test asserts the EXACT formatted text of any standard / i3=0 / contest message subtype — current tests only check `.contains()` on callsign/grid fragments.
    - The ReplyWithR path (message.rs:195-205) writes `" R"` then `" {grid}"` → `"K1ABC W9XYZ R FN42"`, which is correct per ft8_lib reference, but only happens to be correct — no test guards it.
    - EU-VHF i3=0 type 0/2 and DXpedition i3=0 type 1 paths each have their own Display formatting code that's never asserted against ft8_lib reference output.
  evidence_against:
    - "Exact" format may be over-constrained if WSJT-X output varies (e.g., width/padding differences for boundary cases).
    - Time spent on tests with no current bug evidence is speculative.
  notes: |
    For each StandardMessageType variant (Cq, Reply, ReplyWithR, Report,
    ReportWithR, Rrr, Final73, RR73) and each i3=0 subtype, add a unit
    test that builds a synthetic Ft8Message with known fields and
    asserts `.to_string()` exactly equals the WSJT-X / ft8_lib reference
    output. Cross-check against `vendor/ft8_lib/ft8/message.c` output
    for at least one case per subtype.
    Sub-experiment: add a property-test (proptest) that round-trips
    `encode(text) → decode → format == text` over a generated message
    grammar. Would have caught hb-023 automatically.

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

### hb-024 — Cross-validate novel decodes against JTDX + QSO patterns  [PRIORITY: 0.55]
  mode: ft8
  status: pending
  priority_score: 0.55
  estimated_effort: 1-2 sessions
  expected_delta: diagnostic; informs whether vs_wsjtx_pct understates or overstates true performance
  defensible_prior: partial
  wild_card: false
  evidence_for:
    - Plan 3 main.json shows pancetta finds 3720 "novel" decodes on Hard-1000 — messages jt9 didn't recover (~3.7 per WAV)
    - On Hard-200, 1154 novel decodes against 3354 matched-with-jt9 — novel is 25% of total pancetta decodes
    - If these are real (not FPs), our 37-39% vs_wsjtx_pct is understated; pancetta's true performance is better
    - If they're false positives, we have a precision problem masquerading as a recall win
  evidence_against:
    - Cross-validation requires JTDX integration (deferred from Plan 3) or QSO-pattern matching infrastructure
  notes: |
    Three approaches: (a) install jtdx-cli or use JTDX's GUI in scripted mode to
    decode the same WAVs; treat the (pancetta ∩ JTDX) − jt9 decodes as
    high-confidence novel. (b) Within pancetta's own output, treat a novel decode
    as confirmed if the same callsign appears in adjacent slots (QSO pattern
    continuity). (c) Filter against a public callsign hash database (HamQTH/QRZ).
    Run on Hard-200's novel decodes first — smaller sample, faster turnaround.

### hb-025 — Wild-50 zero-overlap investigation  [PRIORITY: 0.50]
  mode: ft8
  status: pending
  priority_score: 0.50
  estimated_effort: 1 session
  expected_delta: diagnostic; may surface decoder bug or matching-logic bug
  defensible_prior: yes (concrete anomaly in Plan 3 main.json)
  wild_card: false
  evidence_for:
    - Plan 3 main.json: wild-50 tier processed 50 WAVs; jt9 found 96 truth decodes (concentrated in 2 outlier WAVs with 49+43 decodes); pancetta matched 0 of them while finding 3 novel decodes of its own
    - Hard-200 + Hard-1000 don't show this zero-overlap pattern (~37-39% match)
    - Either (a) those 2 outlier WAVs contain unusual content the decoder mishandles, (b) jt9's message formatting on those specific WAVs differs in a way that breaks our exact-trim-match logic, or (c) timing/sync edge case for those recordings
  evidence_against:
    - Random sampling artifact: 2 of 50 WAVs dominate; with more wild samples the effect may average out
    - Low priority (decode_rate isn't a composite term for wild-50)
  notes: |
    Identify the 2 outlier WAVs by hash (per_wav_top_failures in main.json), decode
    each manually with both pancetta and jt9, compare outputs line-by-line. Look
    for: format differences (whitespace, casing, callsign hash representation),
    timing skew (DT offset > 0.5s), unusual modulation. If the issue is matching
    logic (formatting), fix in eval.rs's run_curated_tier match function — that
    would lift the Hard-200/1000 numbers too.

### hb-026 — Wild-card: End-to-end neural decoder  [PRIORITY: wild]
  mode: ft8
  status: pending
  priority_score: 0.0
  estimated_effort: 5+ sessions (heavy)
  expected_delta: unknown — could be +0.20 (transformative) or -0.30 (regression)
  defensible_prior: no
  wild_card: true
  evidence_for:
    - Modern speech recognition (Whisper, Wav2Vec2) demonstrates that end-to-end audio-to-text learned models can outperform pipelined DSP+model systems on noisy real-world signals
    - We have millions of synth samples available (parametric, ground-truth-labeled); training data is not a bottleneck
    - The neural OSD experiment already shipped in pancetta-ft8 — we know the in-repo ML pipeline pattern works
  evidence_against:
    - Abandons decades of FT8-specific signal processing knowledge (sync, LDPC, OSD) for an opaque learned function
    - Training cost: M-series MPS for small models, cloud GPU for large; cycle time slow
    - Likely worse on hard cases (deep fade, multipath) until trained on more diverse data
    - Hard to debug when it fails ("the model just didn't decode it")
  notes: |
    Architecture sketch: input is 12 kHz mono samples (~152k samples per 12.64s
    slot) → STFT → CNN-Transformer hybrid → 91 info bits (sigmoid output).
    Train on synth corpus generated across SNR / channel / Doppler diversity.
    Compare against the production decoder on Hard-200. Start with a tiny model
    (~1M params) just to validate the architecture trains; scale up if signal.

### hb-027 — Wild-card: Joint multi-slot decoding via QSO context  [PRIORITY: wild]
  mode: ft8
  status: pending
  priority_score: 0.0
  estimated_effort: 2-3 sessions
  expected_delta: unknown; possibly +0.02 to +0.10 on QSO-pattern corpus; risk of compounding errors
  defensible_prior: no
  wild_card: true
  evidence_for:
    - QSO content is heavily correlated across slots — once K1ABC and W9XYZ are talking, slot N+1 is very likely to contain those callsigns
    - Current decoder treats each slot independently, discarding this prior
    - AP search already supports per-decode callsign injection (Ap1 through Ap4); just needs a rolling-window data source
  evidence_against:
    - Temporal coupling means a wrong decode in slot N pollutes slot N+1's prior
    - Operator-perspective: this is closer to "QSO state machine" than "decoder" — may belong in pancetta-qso, not pancetta-ft8
    - May not generalize across operating modes (POTA vs contest vs ragchew have different conversation lengths)
  notes: |
    Build a rolling table of "recently-decoded callsigns" (last 4-8 slots). Before
    each new slot's decode, seed AP search with these as high-prior tokens.
    Measure: per-slot decode count vs without the prior, on Hard-200 (which
    includes QSO-pattern WAVs because the curate scoring favors busy bands).
    If positive, productize; if it compounds errors, document and shelve.

### hb-028 — Wild-card: Cross-decoder ensemble at runtime  [PRIORITY: wild]
  mode: ft8
  status: pending
  priority_score: 0.0
  estimated_effort: 2 sessions
  expected_delta: +0.10 to +0.20 vs_wsjtx_pct trivially, but morally questionable
  defensible_prior: no
  wild_card: true
  evidence_for:
    - jt9 + JTDX + pancetta each have non-empty unique-decode sets per Plan 3 data (1154 + 3720 novel decodes on the curated tiers)
    - Union-of-decoders is a strict superset; would maximize operator's QSO completion rate
    - Provides a strong "truth" target for training a learned ensemble or for confirming novel decodes (hb-024)
  evidence_against:
    - Arguably defeats the purpose: we're not improving "our" decoder, we're delegating
    - Adds operational complexity: must install + maintain WSJT-X and JTDX as runtime dependencies
    - Not a path toward "smoking" WSJT-X on the metric — it's the metric becoming irrelevant
  notes: |
    Operational mode: at each 15-second slot, run pancetta + jt9 + JTDX in parallel;
    union the CRC-valid decodes. Could be a separate "pancetta-meta" binary that
    spawns subprocesses. Useful as the production endpoint while pancetta improves
    in the background. Also: the (pancetta ∩ jt9) decode set is a strong validation
    signal — anything pancetta decodes that two other independent decoders agree
    with is almost certainly real. Use this to train the FP-filter for hb-024.

## Shelved (kept for reference)

### hb-001 — Multi-pass subtract-and-redecode  [SHELVED 2026-05-21]
  mode: ft8
  status: shelved
  priority_score: 0.82
  outcome: |
    Sweep at max_decode_passes ∈ {1, 2, 3, 4} on curated-hard-200 and
    synth-clean. Measured pass-2+ contribution: +1.2% real decode rate
    on hard-200 (+47 / 3786 from pass 1 alone); 0% on synth-clean
    (identical decode tables at every setting). Composite delta of
    raising max_passes from 3 → 4 = +0.0001, well into noise floor.
    Status quo (max_passes=3) stays.
  measured_delta: |
    hard-200 sweep table:
      passes=1: recovered 3786, rate 0.4415, time 9.6s
      passes=2: recovered 3829, rate 0.4465, time 76.5s   (+43 vs 1)
      passes=3: recovered 3832, rate 0.4468, time 92.8s   (+3 vs 2)
      passes=4: recovered 3833, rate 0.4469, time 99.7s   (+1 vs 3)
    Pass 2 has an 8× compute multiplier for the 1.1% recall gain.
    Synth-clean shows zero variation across pass counts.
  learning: |
    The "5-10% of WSJT-X" gap isn't in pass count. Pancetta's
    subtract_with_sidelobes leaves residuals that produce only marginal
    new decodes — likely a subtraction-quality issue, not a count issue.
    Two follow-ups: hb-030 (audit subtraction quality on controlled
    two-signal synth) and hb-031 (fast-path max_passes=1 mode for
    latency-sensitive autonomous deployment, since pass 1 alone gets
    98.8% of the multi-pass total at 10% of the wall-clock cost).
  follow_up: hb-030 (subtraction quality audit), hb-031 (fast-path mode).
  journal: research/experiments/2026-05-21-multi-pass-sweep.md
  scorecards: research/scorecards/sweep/ (hard200-passes-{1..4}.json + synth-passes-{1..4}.json)

### hb-002 — Synth plateau investigation (1-of-6 message type)  [SHELVED 2026-05-20]
  mode: ft8
  status: shelved
  priority_score: 0.75
  outcome: |
    Identified the failing message as `K1ABC W9XYZ R-12` — the "Roger +
    signal report" response form. Fails at every SNR from -28 dB to -10 dB
    inclusive (not a sensitivity issue; a structural decoder failure
    specific to R-prefix signal-report responses).
  learning: |
    Synth plateau is a real decoder bug, not a sensitivity limit. Until
    fixed, synth-clean tier composite is capped at 5/6 = 83.3% × full
    weight. See research/experiments/2026-05-20-synth-plateau-investigation.md
    for the full per-message-per-SNR table.
  follow_up: hb-023

## Graduated (merged to main)

### hb-023 — Fix R-signal-report decode failure  [GRADUATED 2026-05-21]
  mode: ft8
  status: graduated
  priority_score: 0.85
  outcome: |
    Identified the root cause as a Display impl bug, not a decoder bug.
    `StandardMessageType::ReportWithR` formatted as `"K1ABC W9XYZ R -12"`
    (with a space between `R` and the signed report) instead of the
    WSJT-X / ft8_lib canonical `"K1ABC W9XYZ R-12"`. The decoder
    structurally decoded R-prefix messages correctly all along —
    only the text representation was wrong, so the synth-eval text
    matcher (`d.message.contains(truth)`) missed every R-report
    decode at every SNR.

    Fix: one-line change to drop the leading space from the
    `write!(f, " {:+03}", report)` in message.rs:225. New unit tests
    `test_report_with_r_display_no_space_before_report` /
    `test_report_with_r_display_positive_report` guard the canonical
    format. Companion cleanup in loopback_qso.rs (removed dual-format
    fallback assertions and outdated "R -12" comments).
  measured_delta: |
    Composite 0.4955 → 0.5234 (+0.0279). Expected was +0.015; the
    bigger surprise was on the curated tiers:
    - synth-clean: plateau lifted from 5/6 to 6/6 at SNR ≥ -20 dB (matched prediction).
    - curated-hard-200: decode_rate 0.3911 → 0.4468 (+0.0557); recovered
      3354 → 3832 (+478); novel 1154 → 676 (-478) — same fix shifted
      478 already-correct decodes from "novel" to "matched".
    - curated-hard-1000: decode_rate 0.3714 → 0.4214 (+0.0500); recovered
      10437 → 11843 (+1406); novel 3720 → 2314 (-1406).
    - No regressions; fixtures still 8/8, wild-50 unchanged.
  learning: |
    Text-match-based eval can mask correctness wins as completeness
    misses. Pancetta's true vs_wsjtx_pct on the curated tiers was
    always ~5 percentage points higher than measured — the bug
    inflated the "novel decode" count by ~1900 across Hard-200 and
    Hard-1000 combined. This retroactively recalibrates the
    "5-10% of WSJT-X decode rate" memory note (which is about the
    autonomous on-air run, not the harness, but the harness baseline
    also underrepresented true matches).
  follow_up: hb-029 (exact-format Display tests for every message subtype).
  scorecard: research/scorecards/history/2026-05-21-fix-r-signal-report.json
  journal: research/experiments/2026-05-21-fix-r-signal-report.md
