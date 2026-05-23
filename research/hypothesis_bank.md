# Hypothesis Bank

last_updated: 2026-05-23T15:00:00Z
current_focus_mode: ft8
wild_card_ratio_target: 0.20
wild_cards_run: 2
exploitation_run: 9
current_ratio: 0.182

## Active (ranked by score)

### hb-004 — AP-survival gate retune  [PRIORITY: 0.50, deferred 2026-05-22]
  mode: ft8
  status: deferred
  priority_score: 0.50
  estimated_effort: 1 session (gate sweep) + scoping work (eval-AP wiring)
  expected_delta: 0 from gate sweep alone until AP is exercised in eval
  defensible_prior: deferred (Phase-1 finding from 2026-05-22 cycle)
  wild_card: false
  evidence_for:
    - decoder.rs MIN_SYNC_SCORE_FOR_AP=3.0 is set conservatively (the comment in the source says "Sync scores below 4.0 are likely noise" but the gate is at 3.0)
    - Memory notes record AP thresholds were manually tuned; never systematically swept
    - AP levels 1-4 exist (ap.rs); higher levels at low SNR can recover QSO exchanges missed by AP0
  evidence_against:
    - Phase-1 audit during 2026-05-22 cycle: `decode_window` calls `decode_window_with_ap` with `ApContext::default()` → ap_active=false → AP NEVER fires in eval.
    - Sweeping MIN_SYNC_SCORE_FOR_AP without exercising AP would change nothing measurable.
    - Lowering the threshold risks security-relevant false-callsign injection (C-1).
  notes: |
    DEFERRED. The hb-004 hypothesis assumed eval exercises AP — it
    doesn't. To make the gate sweep meaningful, the harness needs to
    inject a my_call into ApContext (or otherwise activate AP) on each
    decode_wav call. That's a separate scoping question with its own
    tradeoffs: injecting a specific my_call biases the eval toward
    that callsign's frequency in the corpus (AP1 helps decodes where
    a station is calling my_call). Two paths to un-defer:

    (a) Inject the most-common callsign in the curated corpus as my_call
        (would bias toward that station's traffic).
    (b) For each WAV, set my_call to the dominant callsign in that
        WAV's jt9 baseline (biases toward the strongest station per
        slot — more representative but more elaborate plumbing).

    Either path is hb-004's prerequisite. Then the gate sweep follows.

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

### hb-038 — Re-sweep max_sync_candidates at nms-off baseline  [PRIORITY: 0.50, spawned 2026-05-23]
  mode: ft8
  status: pending
  priority_score: 0.50
  estimated_effort: 0.5 sessions
  expected_delta: +0.001 to +0.005 composite at cap=300; significant FP-bloat risk
  defensible_prior: yes (hb-007 sweep showed cap=300 yields +39 recovered at nms-off, vs hb-003's +21 at the cap=200 elbow under nms-on)
  wild_card: false
  evidence_for:
    - hb-007 combined sweep (2026-05-23): at threshold=1.5 or 2.0, cap=300 yields rec=4376 (+39 vs current cap=200 production), novel=1228 (+191), composite +0.0022. Cap=500/800 add no more recovered but inflate novel.
    - The hb-003 sweep that set cap=200 ran at nms-on; the elbow may have shifted with nms-off.
    - +38% wall-clock cost at cap=300 (288s vs 209s on hard-200) — fits the 3s/WAV budget comfortably.
  evidence_against:
    - +191 novel for +39 recovered is a 5:1 ratio of unconfirmed:confirmed. Many novels are likely LDPC+CRC false positives on noise candidates surfacing with the larger cap.
    - hb-024 (cross-validation) becomes more urgent before promoting; we don't currently know if novels are real.
    - Marginal composite gain (+0.0022) — same scale as the diminishing-returns-cycle hb-006 win we already accepted.
  notes: |
    Run full 5-tier eval at max_sync_candidates=300, all other settings at production
    defaults (threshold doesn't matter at this cap per hb-007). If guard tiers
    (fixtures + synth) stay clean and composite goes up by >+0.001, promote.
    If novel-decode count grows in ways that look like FP-bloat (esp. on synth where
    we know there should be no novels), shelve.

### hb-037 — Redesign or remove subtract_with_sidelobes  [PRIORITY: 0.50, spawned 2026-05-22]
  mode: ft8
  status: pending
  priority_score: 0.50
  estimated_effort: 2-3 sessions for kernel redesign; 0.5 for removal
  expected_delta: speed (current multi-pass is no-op for nearby weak signals); +0 sensitivity from current state but unlocks future multi-pass work
  defensible_prior: yes (hb-030 probe proved the current kernel masks recoverable signals within ~25 Hz of strong)
  wild_card: false
  evidence_for:
    - hb-030 (2026-05-22) probe: 9 of 16 two-signal cases showed "subtraction masks recoverable weak signal". 0 of 16 showed "subtraction surfaces missed weak signal." The current kernel is net-negative for nearby weak signals.
    - hb-001 (2026-05-21) macro sweep: pass 2+ contribution is +1.2% on hard-200. Now we know WHY: subtract_with_sidelobes leaves artifacts at the strong signal's TF cell that contaminate the neighborhood.
    - The hb-019 NMS-off win has a unified explanation: pass 1 sees more signals when NMS doesn't suppress them, and pass 2+ would never have surfaced those signals via subtraction (per this probe).
  evidence_against:
    - Removing multi-pass loses the ability to decode "stacks" of QSOs that overlap in time but not in frequency (where subtraction is actually clean).
    - Redesigning the kernel is non-trivial (longer window for sidelobe reduction trades against frequency resolution).
  notes: |
    Three paths:
    (a) Replace the time-domain reconstruction-and-subtract with a
        frequency-domain "zero out the strong signal's spectrogram bins"
        approach (synergizes with hb-021 wild card).
    (b) Improve the time-domain kernel: longer / better-shaped subtraction
        window for sidelobe reduction. Trade-off: longer window = wider
        masked region around the strong signal.
    (c) Remove multi-pass entirely. Set max_passes=1 as production
        default (synergizes with hb-031). Reclaim the wall-clock budget
        for other pass-1 work (more candidates, OSD-3 with stronger FP
        filter, more LDPC iters).
    Prefer (c) for fastest implementation; (a) if a deeper structural
    improvement is wanted. (b) is incremental work on a known-broken
    approach — least appealing.


### hb-036 — Score-relative NMS suppression  [PRIORITY: 0.40]
  mode: ft8
  status: pending
  priority_score: 0.40
  estimated_effort: 1-2 sessions
  expected_delta: keep hb-019's win at lower wall-clock cost; partial recovery of the +58% NMS-off CPU penalty
  defensible_prior: yes (hb-008 sweep showed pure TF-distance NMS can't recover the cost; a smarter suppression criterion might)
  wild_card: false
  evidence_for:
    - hb-008 sweep (2026-05-22) confirmed pure TF-distance NMS at any non-trivial radius loses 239+ decodes vs nms-off on hard-200.
    - The current algorithm conflates "duplicate-of-strong-signal" (same TF cell, near-identical sync_score) with "distinct-weaker-signal" (same TF cell, very different sync_score). The former is what NMS should suppress; the latter is what it should keep.
    - A score-relative suppression rule (suppress only if within TF radius AND sync_score ≤ stronger_neighbor.sync_score - N dB) would discriminate.
  evidence_against:
    - "Score within N dB" needs the right N. Too tight = no suppression (back to nms-off cost); too loose = same problem as TF-distance NMS.
    - Sync score isn't strictly proportional to SNR — it's a Costas correlation, which has its own noise distribution.
  notes: |
    Implement nms_candidates() with a new condition: keep candidate j if
    (dt > nms_time_radius || df > nms_freq_radius || j.sync_score >
    i.sync_score - score_delta_db). Sweep score_delta_db ∈ {1.0, 2.0,
    3.0, 5.0} with reasonable TF radii (t=2, f=1). Goal: composite ≥
    nms-off's 0.5529 with wall-clock 30-50% better than nms-off.

### hb-034 — Audit OSD-3's +313 unconfirmed novel decodes  [PRIORITY: 0.40]
  mode: ft8
  status: pending
  priority_score: 0.40
  estimated_effort: 1 session
  expected_delta: diagnostic; determines whether OSD-3 with stronger FP filter is worth pursuing (hb-018)
  defensible_prior: yes (concrete data from hb-005 sweep)
  wild_card: false
  evidence_for:
    - hb-005 sweep (2026-05-22): at OSD-2→OSD-3 with iters=25, recovered count is identical (4051) but novel count jumps from 868 to 1181 — +313 unconfirmed decodes that aren't in jt9's truth set.
    - At iters=50: same pattern, +337 novel for +7 recovered going OSD-2 → OSD-3.
    - If any meaningful fraction (>10%?) of those 313 novel decodes are real (i.e., jt9 missed them too), then OSD-3 with a stronger FP filter (hb-018) would be net-positive for true sensitivity.
  evidence_against:
    - Most likely all 313 are CRC-14 collisions (the parity gate ≤4 isn't tight enough at OSD-3's 125K trials per candidate).
    - Even at 10% real, the per-decode cost of OSD-3 (~20% wall-clock at OSD-2 over none) may not be worth the marginal gain.
  notes: |
    Three approaches to validate:
    (a) Cross-decode the same hard-200 WAVs with JTDX (if installable);
        treat (pancetta-OSD-3 ∩ JTDX) − jt9 decodes as high-confidence
        novel. This would also meaningfully scope hb-024.
    (b) QSO-pattern continuity: if a "novel" decode's callsign appears
        in adjacent slots' jt9 decodes, the novel is likely real.
    (c) Plausibility filter on the decoded message: valid callsign
        format, valid grid, etc. Quick FP cut.
    Run on the OSD-3 + iters=50 scorecard since that has the most data.
    If <5% are real, document and shelve OSD-3 permanently. If >20%
    are real, fold into hb-018 and push for the stronger FP filter.

### hb-035 — Sweep for max BP convergence rate (reduce OSD fallback)  [PRIORITY: 0.45]
  mode: ft8
  status: pending
  priority_score: 0.45
  estimated_effort: 1 session
  expected_delta: speed (wall-clock) + small sensitivity; informs which knobs reduce OSD-fallback rate
  defensible_prior: yes (hb-005 + hb-006 both produced 3-5% speedups by reducing OSD fallback)
  wild_card: false
  evidence_for:
    - hb-005 (LDPC iters 25 → 50) made the 5-tier eval 3% faster — more BP convergence = fewer expensive OSD calls.
    - hb-006 (LLR variance 24 → 32) made it 5% faster — same mechanism.
    - Both speedups were a side-effect; neither was the headline metric. A deliberate target on "BP convergence rate" could unlock more.
    - The BudgetTracker (decoder.rs:373) limits per-WAV decode time; if BP converges more often, more candidates fit in the budget = more decodes.
  evidence_against:
    - Diagnostic-first; no guaranteed code change drops out
    - Could find that the current setting is already at the BP/OSD tradeoff frontier
  notes: |
    Instrument the decoder to emit per-candidate convergence outcomes:
    (a) BP converged in N iters; (b) BP did not converge, OSD attempted,
    OSD succeeded; (c) OSD attempted, OSD failed; (d) parity gate
    blocked OSD. Run on hard-200 with current production settings, then
    sweep:
    - LLR_TARGET_VARIANCE ∈ {28, 32, 36, 40, 48} (extend the hb-006 sweep)
    - LDPC iter cap ∈ {50, 75, 100} (extend the hb-005 sweep)
    - Combined sweeps for crossing effects.
    Goal: find a setting that pushes BP convergence rate from current
    (estimate ~80%?) to ~90%+ while keeping decode rate ≥ current.

### hb-033 — Why does sync_cap=300 only beat sync_cap=200 by 21 decodes?  [PRIORITY: 0.45]
  mode: ft8
  status: pending
  priority_score: 0.45
  estimated_effort: 1 session
  expected_delta: diagnostic; informs whether NMS or LDPC is the bottleneck past rank-200
  defensible_prior: yes (hb-003 sweep showed sharp elbow at sync_cap=200)
  wild_card: false
  evidence_for:
    - hb-003 sweep: sync_cap=200 gives +219 decodes vs baseline; sync_cap=300 gives only +240 (+21 more). NMS or LDPC is gating decodes past candidate rank ~200.
    - Two distinguishable causes: (a) NMS merges candidates 201-300 with stronger neighbors before LDPC sees them; (b) LDPC + OSD fails on candidates that survive NMS but rank low (low sync score → low LLR confidence → harder convergence).
    - The wall-clock cost of sync_cap=300 vs 200 was +20% per WAV; for 0.5% more decodes that's a poor tradeoff — but the underlying cause matters.
  evidence_against:
    - Pure diagnostic; no guaranteed code change drops out
    - The 0.5% additional headroom may not be worth pursuing even if the cause is identified
  notes: |
    Instrument the decoder to emit per-candidate-rank outcomes on a
    busy-band WAV (one of hard-200's worst — e.g., wav_hash
    bb445ede300...). For each Costas candidate from rank 1 to 300:
    - Did NMS suppress it? (record post-NMS survival)
    - If it survived NMS, did LDPC converge?
    - If LDPC failed, did OSD fall back?
    Plot decode-success rate vs candidate rank. If the rate drops
    cliff-like at rank ~200, NMS is the culprit (sync scores converge);
    if it drops gradually, LDPC convergence is.

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

### hb-032 — Remove or repurpose dead `aggressive_decoding` field  [PRIORITY: 0.40]
  mode: ft8
  status: pending
  priority_score: 0.40
  estimated_effort: 0.5 sessions
  expected_delta: cleanup; removes a documentation footgun
  defensible_prior: yes (hb-020 audit confirmed the field is dead)
  wild_card: false
  evidence_for:
    - hb-020 audit (2026-05-21) confirmed: Ft8Config::aggressive_decoding has 3 references in pancetta-ft8/src/ — field decl, doc comment, default value. Zero reads in the decode pipeline.
    - Surrounding cargo-cult: integration_tests.rs sets the flag with no behavioral assertion; benches/decoder_benchmark.rs has an "aggressive" benchmark that is bit-identical to "default" (the companion settings it bundles are already defaults); README.md + SPECTRAL_ANALYSIS_ENHANCEMENTS.md + examples/enhanced_spectral_analysis.rs document the flag as a real feature.
    - Pre-OSS-publish (per memory project_oss_publish_prep.md), so a minor breaking-API change is acceptable.
  evidence_against:
    - Public API change (`pub aggressive_decoding`). Any external consumer that sets it would need to remove the line.
    - Option (b) repurposing has scope overlap with hb-031 — better to do them together than separately.
  notes: |
    Three cleanup options:
    (a) Delete the field + all referencing code (cleanest; minor
        breaking-API change but acceptable pre-OSS-publish).
    (b) Repurpose to drive a "fast | balanced | deep" preset (this
        is the same plumbing hb-031 needs — combining the two would
        be efficient).
    (c) Deprecate with `#[deprecated]` + document as a no-op.
    Recommended: do (b) when hb-031 lands; do (a) if hb-031 doesn't
    land before OSS publish. The README, example, and benchmark all
    need updates in any branch.

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

### hb-007 — MIN_SYNC_SCORE threshold sweep  [SHELVED 2026-05-23]
  mode: ft8
  status: shelved
  priority_score: 0.56
  outcome: |
    Sweep at MIN_SYNC_SCORE ∈ {1.5, 2.0, 2.5, 3.0, 3.5, 4.0} on
    hard-200 produced BIT-IDENTICAL decode counts (4337/1037/0.2529)
    at every value. The threshold knob is fully dead at the current
    production max_sync_candidates=200 — the truncate-to-200 is the
    binding gate, not the threshold check (200+ candidates per WAV
    exceed score=4.0 already).

    Combined sweep at (threshold ∈ {1.5, 2.0}) × (cap ∈ {300, 500, 800})
    confirmed the threshold is dead at any cap. Caps above 200 do
    surface +39 recovered (one elbow at cap=300) plus +191-470 novel
    decodes, but the threshold value within {1.5, 2.0} doesn't matter.
  measured_delta: 0 (no production change — threshold value irrelevant)
  learning: |
    Threshold-first hypotheses are misleading when an upstream
    truncate exists. The conceptual model "lower the threshold to
    surface marginal candidates" only works if the threshold is the
    binding constraint; here the cap is. Future filter-tuning
    hypotheses should first verify the filter is actually limiting.

    Cap = 300 yields a small (+39 rec, +0.0022 composite) gain on
    hard-200 over the current cap = 200, but this is hb-003 territory
    re-evaluated at the new nms-off baseline (hb-003 graduated at
    cap=200 under nms-on). Spawned as hb-038.

    The novel-count saturation pattern (cap=200 → 800: 1037 → 1507
    novel for +38 net recovered) is corroborating evidence for hb-024
    becoming more urgent — many of these novels are likely LDPC+CRC
    FPs on noise candidates that pass the parity gate.
  follow_up: hb-038 (re-sweep max_sync_candidates at nms-off baseline).
  scorecards: research/scorecards/sweep/hard200-msync-*.json + research/scorecards/sweep/hard200-msync*-cap*.json
  journal: research/experiments/2026-05-23-min-sync-score-sweep.md

### hb-030 — subtract_with_sidelobes residual quality audit  [SHELVED 2026-05-22 with strong diagnostic finding]
  mode: ft8
  status: shelved
  priority_score: 0.60
  outcome: |
    Two-signal synth probe over (weak_snr ∈ {-15, -18, -20, -22} dB) ×
    (freq_offset ∈ {12.5, 25, 50, 100} Hz). Result: 9 of 16 cases
    showed "subtraction MASKS recoverable weak signal" (weak alone
    decodes; pass 2 after subtracting strong fails). ZERO of 16 cases
    showed "subtraction surfaces missed weak signal." Multi-pass is
    currently dead infrastructure — confirmed by direct mechanism, not
    just macro counts.
  measured_delta: 0 (diagnostic only, no production change)
  learning: |
    The subtract_with_sidelobes kernel leaves artifacts at the strong
    signal's TF cell that contaminate the spectrogram within ~25 Hz on
    either side. Any weak signal in that band becomes undecodeable in
    the residual, even though it would decode if the strong weren't
    present. Beyond ~50 Hz separation, the two signals don't interfere
    in the spectrogram and both decode in pass 1 without subtraction.

    This unifies several prior findings:
    - hb-001 (multi-pass) showed only +1.2% pass-2+ contribution. Now
      we know why: the kernel is broken.
    - hb-019 (nms-off) gave +1973 decodes by letting pass 1 see
      candidates NMS was suppressing. Those candidates would never
      have been recovered by pass 2 (per this probe).
    - hb-008 (NMS radius sweep) confirmed pure TF-distance NMS can't
      recover the cost. The decoder needs to see all candidates in
      pass 1.

    The right pancetta-ft8 architecture is "pass 1 finds everything;
    subtract+redecode is overhead." Multi-pass is a dead lever until
    the subtraction kernel is rewritten OR removed.
  follow_up: |
    hb-037 (kernel redesign or removal); hb-031 (bumped priority
    0.40 → 0.55 — fast-path single-pass is now well motivated).
  scorecards: (n/a — probe only)
  journal: research/experiments/2026-05-22-subtract-quality-audit.md

### hb-008 — NMS radius parameter sweep  [SHELVED 2026-05-22]
  mode: ft8
  status: shelved
  priority_score: 0.65
  outcome: |
    Sweep of (nms_time_radius, nms_freq_radius) ∈ {(0,0), (1,0),
    (2,1), (2,2), (4,1), (4,2), (8,2)=historical} on hard-200 with
    NMS re-enabled. Conclusion: pure TF-distance NMS at ANY non-trivial
    radius loses 239+ decodes vs nms-off. The hypothesis that tighter
    radii could recover most of hb-019's gain at lower wall-clock cost
    was wrong.
  measured_delta: 0 — production unchanged; nms_enabled=false stays
  learning: |
    Three findings:
    1. NMS based purely on TF-distance is fundamentally too coarse for
       FT8 signal density. Real distinct stations frequently share TF
       cells (time-sharing a freq, or close enough that Costas peaks
       overlap). TF-distance can't distinguish "duplicate of strong"
       from "distinct weaker."
    2. t=0 f=0 ≈ nms-off in decode count but is 27% SLOWER (211s vs
       166s) — O(n²) NMS loop overhead even when its body becomes a
       no-op. Skipping the function entirely (nms_enabled=false) is the
       right way to disable.
    3. The sensitivity-vs-wall-clock tradeoff is decided: nms-off costs
       +58% wall-clock for +1973 decodes (hb-019). The radius sweep
       can save ~15-20% wall-clock at the cost of 240+ decodes — a
       bad trade.

    Infrastructure (`nms_time_radius`, `nms_freq_radius` Ft8Config
    fields + CLI flags) lands as reusable for hb-036 (score-relative
    NMS redesign).
  follow_up: hb-036 (score-relative NMS suppression — discriminate duplicate-of-strong from distinct-weaker via sync_score comparison).
  scorecards: research/scorecards/sweep/hard200-nms-* (8 sweep settings)
  journal: research/experiments/2026-05-22-nms-radius-sweep.md

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

### hb-020 — Wild-card: aggressive_decoding flag audit  [SHELVED 2026-05-21]
  mode: ft8
  status: shelved
  priority_score: 0.0
  wild_card: true
  outcome: |
    Audit confirmed `Ft8Config::aggressive_decoding` is dead code:
    field decl + doc comment + default in decoder.rs:100-127, ZERO
    reads anywhere in the decode pipeline. Setting it to `true` has
    no effect.
  measured_delta: 0 (no code change; audit only)
  learning: |
    The flag is a documentation/code-coherence footgun. README.md
    (lines 135-140 and 256), examples/enhanced_spectral_analysis.rs,
    SPECTRAL_ANALYSIS_ENHANCEMENTS.md, and the integration test all
    treat it as a real feature. The "aggressive" benchmark in
    decoder_benchmark.rs is bit-identical to the "default" benchmark
    (companion settings it bundles are already defaults). Anyone
    running `cargo bench` and comparing the two would get matching
    numbers with no flag indicating something's off.

    "Aggressive" is also the natural name for hb-031's fast-vs-deep
    toggle — so the cleanup spawns hb-032 with a recommended "repurpose
    it for hb-031" path rather than just deletion.
  follow_up: hb-032 (cleanup: remove, repurpose for hb-031, or deprecate).
  journal: research/experiments/2026-05-21-aggressive-decoding-audit.md

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

### hb-031 — Fast-path single-pass mode  [GRADUATED 2026-05-23, speed win]
  mode: ft8
  status: graduated
  priority_score: 0.55
  outcome: |
    Lowered production default `Ft8Config::max_decode_passes` from 3
    to 1. Direct confirmation via 5-tier eval that multi-pass infra
    is contributing essentially nothing at the current nms-off
    baseline. The composite delta is -0.0007 (within noise); the
    wall-clock delta is -49% (full 5-tier eval drops 1237s → 631s).
  measured_delta: |
    Full 5-tier at max_passes=1 vs main (max_passes=3):
      fixtures + synth-clean + wild-50: identical
      hard-200:  rec 4337 → 4325 (-12, -0.28%), novel -17
      hard-1000: rec 14153 → 14126 (-27, -0.19%), novel -81
      composite: 0.5529 → 0.5522 (-0.0007)
      5-tier elapsed: 1237s → 631s (-49%, halved)
  learning: |
    1. Multi-pass was overhead, not capability. The combined
       evidence (hb-001 +1.2% under nms-on, hb-030 probe mechanism,
       this -0.2-0.3% at nms-off baseline) is unambiguous.
    2. The composite metric undervalues wall-clock improvements. The
       formula has no wall-clock term; a -0.0007 composite hides a
       2× decode-time speedup. Treat composite as necessary-but-not-
       sufficient for production decisions.
    3. Diagnostic-driven decisions (hb-030 probe) led to a higher-
       confidence ship than a sweep would have produced.
    4. The +35% relative jump in hard-1000 decode rate from
       experiment-run start (0.371 → 0.504) was achieved while ALSO
       cutting per-WAV decode time by ~35% (from ~430 ms to ~280 ms
       post-hb-031). Sensitivity and speed wins were not in tension.
  follow_up: |
    hb-037 (subtract kernel redesign — if salvageable, re-raise
    max_decode_passes default). No new spawns.
  scorecard: research/scorecards/history/2026-05-23-fast-path-single-pass.json
  journal: research/experiments/2026-05-23-fast-path-single-pass.md

### hb-019 — Wild-card: disable NMS  [GRADUATED 2026-05-22, biggest win since hb-023]
  mode: ft8
  status: graduated
  priority_score: 0.0 (wild card)
  wild_card: true
  outcome: |
    A/B test of NMS enabled vs disabled on hard-200, then full 5-tier
    confirmation. The bank entry predicted "very likely a regression";
    reality: +1973 recovered decodes (+13.7% relative on hard-1000,
    +6.6% on hard-200). Production `Ft8Config::nms_enabled` flipped
    `true → false`.
  measured_delta: |
    Full 5-tier at nms_enabled=false vs main:
      hard-200:  rec 4070 → 4337 (+267, +6.6%), novel +162, rate +0.0311
      hard-1000: rec 12447 → 14153 (+1706, +13.7%), novel +876, rate +0.0607
      fixtures + synth-clean + wild-50: unchanged (zero FPs in guard tiers)
      composite: 0.5373 → 0.5529 (+0.0156)
      5-tier elapsed: 783s → 1237s (+58%, still well within 3s/WAV budget)
  learning: |
    Three insights:
    1. The bank entry's prior was wrong. NMS radii (time=8, freq=2) were
       too coarse for FT8's signal density — merging real signals 25 Hz
       apart on busy bands. Conventional wisdom about NMS being a pure
       efficiency optimization was wrong for this domain.
    2. Wild-card audits can produce the biggest wins. The diminishing
       returns trend across parameter-sweep cycles (hb-005/006 marginal)
       was a sign to step outside the sweep frame, not optimize harder.
    3. Fixtures + synth-clean as FP guard worked exactly as designed —
       zero change on those tiers while the busy curated tiers gained
       +1973 decodes is the cleanest signal the harness has produced.
  follow_up: hb-008 (NMS radius sweep, priority bumped 0.52 → 0.65 — likely recovers most of the win at ~50% of the wall-clock cost).
  scorecard: research/scorecards/history/2026-05-22-nms-disable-audit.json
  journal: research/experiments/2026-05-22-nms-disable-audit.md

### hb-006 — LLR normalization target tuning  [GRADUATED 2026-05-22, marginal]
  mode: ft8
  status: graduated
  priority_score: 0.58
  outcome: |
    Sweep at LLR_TARGET_VARIANCE ∈ {16, 20, 24, 28, 32, 36}. Production
    raised 24.0 → 32.0 (the peak of a monotonic 16→32→flat shape).
    Marginal but real WIN: +5 recovered on hard-200, +11 on hard-1000,
    composite 0.5370 → 0.5373 (+0.0003), no regressions. synth-clean
    unchanged at every variance value — the predicted sensitivity gain
    did NOT materialize on AWGN.
  measured_delta: |
    Full 5-tier at var=32 vs main:
      hard-200: rec +5, novel +5, rate +0.0006
      hard-1000: rec +11, novel +17, rate +0.0004
      composite: +0.0003
      5-tier elapsed: -44s (-5% — BP converging more efficiently, fewer
      OSD fallbacks fire)
  learning: |
    Three observations:
    1. The hypothesis was right about existence of an optimum but
       wrong about magnitude. Predicted +0.01-0.04 on synth SNR@50%;
       got 0 on synth + 0.0003 on composite.
    2. Diminishing returns are real: hb-023 (+0.0279) → hb-003 (+0.0128)
       → hb-005 (+0.0008) → hb-006 (+0.0003). Each ~3-5× smaller than
       the prior. Worth considering higher-impact hypothesis classes
       next: hb-030 (subtraction quality), hb-024 (cross-validation),
       or hb-015 (Doppler).
    3. Two consecutive cycles produced 3-5% wall-clock speedups as
       side effects of changing the BP/OSD interaction. Spawned hb-035
       to target this metric deliberately — if a knob can push BP
       convergence rate higher, the OSD-fallback frequency drops and
       both speed and (within-budget) decode count rise.
  follow_up: hb-035 (sweep for max BP convergence rate).
  scorecard: research/scorecards/history/2026-05-22-llr-variance-sweep.json
  journal: research/experiments/2026-05-22-llr-variance-sweep.md

### hb-005 — OSD beta + iteration sweep  [GRADUATED 2026-05-22]
  mode: ft8
  status: graduated
  priority_score: 0.63
  outcome: |
    2×4 sweep of (osd_depth ∈ {none, 1, 2, 3}) × (ldpc_iters ∈ {25, 50})
    on hard-200. Production change: raised LDPC_MAX_ITERATIONS from 25
    to 50. OSD depth stays at Some(2) — OSD-3 explodes novel decodes
    (+313 vs OSD-2 at iters=25) for zero additional recovered, almost
    certainly mostly CRC-14 false-positives that the current parity
    gate ≤4 doesn't catch.
  measured_delta: |
    Full 5-tier eval at OSD-2 + iters=50 vs main:
      hard-200:   rate 0.4724 → 0.4740 (+0.0016); rec +14, novel +2
      hard-1000:  rate 0.4402 → 0.4425 (+0.0023); rec +64, novel -54
      fixtures + synth + wild-50: unchanged
      composite:  0.5362 → 0.5370 (+0.0008)
      wall-clock: 848s → 828s (-3% — more BP convergence = fewer
                  expensive OSD fallbacks)
  learning: |
    Three insights from this cycle:
    1. OSD's contribution to confirmed decodes is tiny — 6 decodes
       across the OSD ∈ {none, 1, 2, 3} range at iters=25 on hard-200.
       OSD is not where the WSJT-X gap lives.
    2. LDPC iterations is a quality knob: hard-1000 gained +64 recovered
       AND lost 54 novel — more BP convergence converts fuzzy "novel"
       decodes into confirmed truth-matches.
    3. hb-004 (AP gate retune) needs prerequisite work: eval's
       decode_window calls decode_window_with_ap with an empty
       ApContext, so AP never fires in eval. Updated hb-004 status to
       "deferred" with the scope question documented.
  follow_up: hb-034 (audit OSD-3's +313 novel decodes — cross-validate or shelve OSD-3).
  scorecard: research/scorecards/history/2026-05-22-osd-sweep.json
  journal: research/experiments/2026-05-22-osd-sweep.md

### hb-003 — Sync candidate count sweep  [GRADUATED 2026-05-22]
  mode: ft8
  status: graduated
  priority_score: 0.70
  outcome: |
    Sweep at max_sync_candidates ∈ {50, 100, 150, 200, 250, 300}
    found a clear elbow at 200. Production default raised from 100 to
    200 in pancetta-ft8/src/decoder.rs::MAX_SYNC_CANDIDATES.

    Hard-200: 0.4468 → 0.4724 (+0.0255, +5.7%) decode rate; +219
    recovered, +192 novel.
    Hard-1000: 0.4214 → 0.4402 (+0.0188, +4.5%) decode rate; +529
    recovered, +465 novel.
    Synth-clean + fixtures unchanged; no FPs introduced.
    Composite 0.5234 → 0.5362 (+0.0128).
  measured_delta: |
    +748 real decodes across the two curated tiers; +657 novel.
    Wall-clock cost: ~+52% per WAV on hard tiers (avg 672 ms/WAV at
    sync_cap=200, well within the 3000 ms decoder budget).
  learning: |
    The "5-10% of WSJT-X" gap was partly a sync-cap issue: the
    101st-300th-ranked Costas candidates contained ~6% of the real
    decodes pancetta was missing. hb-003 was right where hb-001 was
    wrong — same sweep shape, different parameter, decisive result.

    Diminishing returns past 200: sync=300 adds only 21 decodes over
    sync=200. NMS or LDPC convergence is the bottleneck at that rank
    range — see hb-033 follow-up.

    Sub-experiment (b) was redundant: max_candidates=100 was never
    binding. The sync cap was the only meaningful gate.

    enhanced_spectral_analysis.rs example needed the
    `..Default::default()` update due to exhaustive struct literal
    syntax — flagged as a pattern that should be cleaned up across
    other example/test sites (rolled into hb-032).
  follow_up: hb-033 (audit why sync_cap=300 only adds 21 over 200).
  scorecard: research/scorecards/history/2026-05-22-sync-candidate-sweep.json
  journal: research/experiments/2026-05-21-sync-candidate-sweep.md

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
