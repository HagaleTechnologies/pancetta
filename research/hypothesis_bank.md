# Hypothesis Bank

last_updated: 2026-05-25T21:50:00Z
current_focus_mode: ft8
wild_card_ratio_target: 0.20
wild_cards_run: 4
exploitation_run: 60
current_ratio: 0.063
# Batch 9 (2026-05-25): SHIPPED FP filter + composite WIN (+0.000641).
#   First main.json composite movement since hb-038 (April 2026):
#     0.554489 → 0.555131.
#   hb-052 GRADUATED — production filter wired into coordinator/ft8.rs
#     (operator ADIF + cqdx-live + 500-deep rolling window, cold-start
#     lenient until reference ≥ 100).
#   hb-053 GRADUATED (gate=6 + iters=100) — both bumped in
#     pancetta-ft8 defaults; predicated on filter catching the precision
#     regression these wider knobs would otherwise cause.
#   hb-062 GRADUATED — coordinator hot-path wire done; ApplicationCoordinator
#     owns Option<Arc<CallsignContinuityFilter>>, ft8 decoder thread
#     applies post-decode / pre-broadcast.
#   Mid-batch root-cause: eval-side FpFilter incorrectly dropped a 2017
#     basicft8 fixture. Fix: fixtures tier bypasses filter (separates
#     decoder regression from filter precision testing; cold-start lenient
#     mode makes production behavior diverge from strict eval anyway).
# Batch 8 (2026-05-25): composite push (Option A).
#   hb-062 parts 1+2+3 DONE (library + ADIF + cqdx integration; 13 tests)
#     coordinator hot-path wire DEFERRED to batch 9 — DONE in batch 9.
#   hb-067 mBP offset: -48 novels at zero recall (small win, mechanism mismatch);
#     NOT graduated (decision pending)
#   hb-068 SHELVED — hb-044 regression is from interpolation itself, not sort
#   Spawned hb-069 (linear-power interpolation for hb-044 rescue)
# Batch 7 (2026-05-25): mr-001 follow-ups + mr-003 LDPC audit.
#   hb-044 SHELVED for prod (conditional WIN on synth, -116 on hard-200)
#   hb-046 SHELVED via architecture mismatch (3rd from mr-001)
#   hb-034 confirmed SHELVED (filter doesn't rescue)
#   mr-003 harvested 5 candidates (hb-063..hb-067)
#   hb-068 spawned for hb-044 conditional variants
# Batch 6 (2026-05-25): FP filter library + revisits.
#   hb-052 graduated as library (infra); production blocked on hb-062
#   2 hb-053 revisits show wider gate + iters=100 win with filter
#   Spawned hb-062 (cqdx.io integration — unblocks production)
# Batch 5 (2026-05-25): plumbing + bank refill.
#   mr-002 JTDX audit: 5 new clean-attach + plan-sized hypotheses
#     (hb-054..hb-058). JTDX code frozen 2022, rich harvest, no
#     future-version risk.
#   mr-004 source-drift audit: 2 more dead Ft8Config fields surfaced
#     (hb-060 enable_multithreading, hb-061 frequency_range)
#   jt9 slot-cut helper: works on slot-aligned; spawned hb-059 for
#     alignment-detection follow-up
#   Doppler tier now wired into composite (5% weight)
#   hb-013 SHELVED (already fixed)
# Bank state: 8 new active hypotheses queued; bank is REFILLED.
# Note: mr-001 (WSJT-X-Improved audit) added hb-043..hb-048 — six new
# pending hypotheses sourced from external research. Bank no longer
# "exhausted" — the meta-research cycle works.

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

### hb-010 — Spectrogram window function sweep  [SHELVED 2026-05-25 — batch 11]
  mode: ft8
  status: SHELVED — Hann (ft8_lib sin² parity) optimal; Blackman/Kaiser8 break a fixture, Kaiser5 ties. Code reverted.
  priority_score: 0.0
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

### hb-011 — LDPC iteration count sweep (25 → 50)  [SHELVED 2026-05-25 — batch 11, stale]
  mode: ft8
  status: SHELVED — already covered. LDPC_MAX_ITERATIONS is 100 (not 25); hb-005 graduated 50, hb-035 swept 50/75/100, batch 9 shipped 100.
  priority_score: 0.0
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

### hb-037 — Redesign or remove subtract_with_sidelobes  [PRIORITY: SHELVED — superseded by hb-031]
  mode: ft8
  status: SHELVED (2026-05-23 — path (c) shipped via hb-031; paths (a)/(b) refuted by profile)
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: n/a
  defensible_prior: yes (original hb-030 finding stands, but its remedy was already shipped)
  wild_card: false
  evidence_against:
    - Path (c) "set max_passes=1 as production default" already SHIPPED via hb-031 (2026-05-22). Production decoder now runs single-pass; no subtract is invoked.
    - Path (a) "frequency-domain spectrogram subtraction" rejected by 2026-05-23 multipass-profile: spectrogram is 0.4% of pass time; reusing it saves <1.3% wall-clock. See [[hb-021]] for the profile rejection.
    - Path (b) "improve the time-domain kernel" is incremental work on a feature (multi-pass) that production no longer uses. The 28 extra decodes pass 2 yields on hard-200 (0.5% recall lift) doesn't justify a kernel redesign even if the redesign were free.
  notes: |
    SHELVED. The only remaining scenario where this matters: if a future
    cycle re-enables multi-pass at materially higher recall yield. Open
    a NEW hb-NNN at that point rather than reviving this one — the
    framing has moved on. See research/experiments/2026-05-23-multipass-profile.md.


### hb-044 — Sub-sample DT refinement  [CONDITIONAL WIN 2026-05-25; SHELVED for prod]
  mode: ft8
  status: SHELVED for production; spawned hb-068 for conditional/scaled variants
  priority_score: 0.0
  estimated_effort: implementation complete (parabolic + linear-interp)
  expected_delta: synth-clean SNR@90% −2dB; hard-200 −116 recovered
  defensible_prior: validated on synth, refuted on hard-200
  wild_card: false
  outcomes: |
    Implementation (batch 7 iters 1-2):
    - CostasCandidate gains time_refinement: f64 (parabolic fit of sync peak)
    - parabolic_peak_refinement helper function + 3 unit tests
    - lookup_time_interp helper applies fractional shift via linear interpolation
      in both extract_symbols_from_spectrogram and par_extract_symbols_from_spectrogram
    - Ft8Config::sync_time_interpolation flag (default false)
    - CLI --sync-time-interpolation
    Sweep result (curated-hard-200 + synth-clean):
      synth-clean SNR@90%: -18.0 → -20.0 dB (1-step improvement, +1 decode at -20dB cell)
      curated-hard-200: 4365 → 4249 rec (-116, -2.7%); 952 → 925 novel (-27)
      Composite weight on hard-200 (0.5) dominates synth (0.3) → SHELVE for prod.
  notes: |
    Real signal in clean conditions. Real regression on noisy multi-slot WAVs
    — likely candidate displacement under top-300 cap due to score inflation.
    See research/experiments/2026-05-25-batch-7-mr001-followups.md iters 1-2.
    Future variants to explore (hb-068): score-gated refinement, scaled delta,
    rejection of large deltas, "only refine if would-be-dropped" rule.

### hb-045 — Localized baseline / noise-floor estimation  [SHELVED 2026-05-24]
  mode: ft8
  status: SHELVED — technique doesn't apply to pancetta's architecture
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: REFUTED — pancetta already does per-candidate-local SNR estimation
  defensible_prior: turned out wrong on architecture-fit grounds
  wild_card: false
  evidence_against:
    - Audit (2026-05-24): pancetta's `par_estimate_snr_spectrogram` (decoder.rs:3093) and `par_estimate_snr_fft` (decoder.rs:3117) ALREADY compute SNR per-candidate per-symbol (best-tone vs worst-tone within each symbol's 8 tones). No global noise floor is used anywhere in the decode pipeline.
    - The WSJT-X technique solves a problem that doesn't exist in pancetta (global noise floor + strong signal dragging the floor up).
    - `Ft8Config::min_snr_db` is dead code (declared, defaulted, never read). Spawned hb-049 to remove it (mirror of hb-032 cleanup pattern).
    - `estimate_noise_floor` function exists but only in a test, not in the decode pipeline.
  notes: |
    SHELVED. See research/experiments/2026-05-24-localized-baseline-audit.md.
    The mr-001 source review didn't catch the architecture mismatch.
    Spawned mr-007 to add architecture-fit check before promoting
    harvested hypotheses to active.

### hb-046 — Two-stage STD-then-MTD pass scheduling  [SHELVED 2026-05-25 — architecture mismatch]
  mode: ft8
  status: SHELVED — WSJT-X benefit is latency, not sensitivity; pancetta is offline
  priority_score: 0.0
  estimated_effort: implementation complete (two variants tested)
  expected_delta: REFUTED — both v1 (subset) and v2 (NMS-on different population) give Δrec=0
  defensible_prior: turned out wrong on architecture-fit grounds
  wild_card: false
  outcomes: |
    Implementation (batch 7 iters 3-4):
    - Ft8Decoder.with_two_stage(on); two_stage_first_config field
    - v1: cheap=sync_cap=100/no-osd/iters=25 + std → Δrec=0 (cheap ⊂ std)
    - v2: cheap=nms-on/cap=200 + std → Δrec=0 (text-dedup absorbs the
      different candidate populations into same message strings)
  notes: |
    The WSJT-X-Improved "two-stage" benefit is LATENCY (process partial slot
    data before all 50 symbols received) — pancetta is OFFLINE eval, full
    slot always available. The cheap-then-thorough pattern doesn't add
    sensitivity when both passes converge on the same decoded messages.
    Third architecture-mismatch shelve from mr-001 (after hb-045, hb-047).
    Note: JTDX "subpass" (mr-002) is conceptually DIFFERENT — that's
    iteration over different START SAMPLES with cross-cycle averaging
    (hb-056). Don't conflate the two.
    See research/experiments/2026-05-25-batch-7-mr001-followups.md iters 3-4.

### hb-047 — Auto-tightened passband detection  [SHELVED 2026-05-24 via mr-007 audit]
  mode: ft8
  status: SHELVED — architecture audit shows minimal attach-point
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: REFUTED — <1% wallclock + indeterminate precision gain on wild-50
  defensible_prior: turned out wrong on architecture-fit grounds
  wild_card: false
  evidence_against:
    - mr-007 audit (2026-05-24): pancetta's sync_search is only 1.3% of pass time (per hb-021 profile); narrowing the search range maximum saves ~0.5% wallclock.
    - Precision wall is "candidates that surface ARE noise", not "candidates from outside the passband leak in". The cap (300) is what limits the population; passband narrowing would only matter if it displaced real candidates from the top 300, which it wouldn't given the flat sync_score distribution past rank 300 (hb-033).
    - Natural target (wild-50) has only 4 total novel decodes — no measurable signal for the technique.
  notes: |
    SHELVED via mr-007's first application. See
    research/experiments/2026-05-24-passband-architecture-audit.md.
    First test of architecture-fit audit at iter-pick time saved
    ~100 LOC + an eval. mr-002 (JTDX audit) is more attractive next
    external-source target — JTDX is architecturally closer to
    pancetta than WSJT-X-Improved.

### hb-048 — AP type 7 (a7) cross-correlation against decoded callsigns  [PRIORITY: 0.45, spawned 2026-05-24 from mr-001]
  mode: ft8
  status: pending
  priority_score: 0.45
  estimated_effort: PLAN-SIZED (~3 sessions, design doc first)
  expected_delta: step-change recall potential — but high FP risk
  defensible_prior: yes (Joe Taylor 2021 commit + active uptake in WSJT-X-Improved)
  wild_card: false
  evidence_for:
    - WSJT-X mainline commit f13e31820470291fdd49627287a2dc08f3fa674c (Joe Taylor, 2021) introduces lib/ft8_a7.f90: after decoding callsign C, build ~206 plausible follow-up message templates and cross-correlate against next slot's residual.
    - Synergizes naturally with pancetta-qso's QSO state machine and the existing `recently_responded_to` callsign tracking.
    - The mr-001 audit flagged this as "*the* high-leverage idea pancetta's bank doesn't have."
  evidence_against:
    - Brings AP-style FP pressure that pancetta currently doesn't have. WSJT-X went through multiple iterations of "better suppression of low-confidence false decodes generated by AP decoding."
    - Not a CLI sweep — needs new module (~200-400 LOC) for template generation + correlation, plus state in coordinator for cross-slot callsign memory.
    - Bit-exact decode count will change.
  notes: |
    Source: https://www.repo.radio/w4kek/WSJT-X/commit/f13e31820470291fdd49627287a2dc08f3fa674c
    The mr-001 report calls this "Plan-sized scoping ticket, not a single
    hb-NNN." First step: design doc outlining template structure, snr7
    threshold (WSJT-X uses snr7 >= 6.0, snr7b >= 1.8), per-callsign cooldown
    integration with pancetta-qso's recently_responded_to.

### hb-050 — Rolling callsign-window tracker  [SHELVED 2026-05-24 — closed by hb-051 ceiling]
  mode: ft8
  status: SHELVED — infrastructure built but adds zero recall (4.7x wallclock penalty)
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: REFUTED — hb-051 ceiling = 1 decode; rolling-window can't beat that
  defensible_prior: turned out wrong
  wild_card: false
  evidence_against:
    - hb-051 diagnostic (2026-05-24): perfect-information AP hints recover ONE decode out of 8576 truth on hard-200. Rolling-window can only do worse.
    - hb-050 sweep on hard-200 with --ap-rolling-window 50: Δrec=0, Δnovel=0, 4.7x wallclock penalty (1187s vs ~250s baseline).
    - The eval-iteration order isn't even chronological (per-corpus shuffle), so the rolling window is essentially "random recent callsigns" — but even if it were perfect, hb-051 caps the upside at 1 decode.
  notes: |
    SHELVED. Infrastructure (--ap-rolling-window flag, Mutex<VecDeque>
    in Ft8Decoder wrapper) is in place and can be reused if a future
    use case emerges. Possibly still useful for OPERATIONAL on-air
    decoding where the rolling window pulls from real chronological
    slots and overlaps with the operator's QSO state — different
    eval context. Re-evaluate if/when operator-side rolling-window
    lands. See research/experiments/2026-05-24-batch-4-unblock.md.

### hb-051 — AP-recovery ceiling diagnostic  [WIN (diagnostic) 2026-05-24]
  mode: ft8
  status: COMPLETED — ceiling = 1/8576 decodes; closes AP line on hard-200
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: 0; decisive diagnostic
  defensible_prior: yes
  wild_card: false
  outcome: |
    Per pancetta-research/examples/ap_recovery_ceiling.rs on hard-200
    (200 WAVs, ~15 min wall): with truth callsigns injected as
    ApContext.recent_calls (perfect information), pancetta recovers
    EXACTLY ONE additional decode beyond AP-off baseline (4666 → 4667).
    WAVs with recovery: 1 / 200.
    Verdict per the bank-entry interpretation table (<0.5% ceiling):
    hb-050 / hb-027 lines closed.
  notes: |
    Result drives the SHELVE of hb-050 and (by extension) hb-027.
    See research/experiments/2026-05-24-batch-4-unblock.md iter 1.

### hb-054 — Costas 2-of-3 sync rescore (sync8 segment fallback)  [SHELVED 2026-05-25 — batch 10]
  mode: ft8
  status: SHELVED — max(syncf, syncs) adds FPs with zero recall gain on pancetta's busy-band corpus; code reverted
  priority_score: 0.0
  estimated_effort: 1 session
  expected_delta: REFUTED on hard-200 — -1 rec / +35 novel (both no-filter)
  defensible_prior: was yes (JTDX 2.2.159 ships this); corpus-mismatch for pancetta
  wild_card: false
  evidence_against:
    - Implemented faithfully (max(syncf, syncs), syncs = trailing blocks). hard-200 no-filter A/B: recovered 4377 → 4376 (-1, float noise), novel 1787 → 1822 (+35, +2.0% FPs). Zero real-decode recovery.
    - max(syncf, syncs) only ever RAISES a candidate score → strictly relaxes the min_sync_score gate → surfaces noise candidates whose trailing two blocks align by chance; a fraction clear LDPC+CRC as CRC-14 collision FPs. Precision wall again (batches 2-8).
    - On pancetta's full-slot busy-band captures the leading Costas block is rarely the limiter, so JTDX's "corrupted-leading-block rescue" finds nothing to rescue. JTDX's edge is weak single-station / slot-misaligned captures.
  notes: |
    See research/experiments/2026-05-25-costas-two-of-three.md.
    Spawned hb-070 (gated trailing-block rescue — only relax when the
    leading block is *detectably* depressed; low priority, better
    tested against a slot-misaligned corpus).

### hb-055 — Adaptive OSD depth based on signal context (ndeep 3→4→5)  [SHELVED 2026-05-25 — batch 10, mr-007]
  mode: ft8
  status: SHELVED via mr-007 architecture-fit — no headroom in pancetta; no code/eval
  priority_score: 0.0
  estimated_effort: 1 session
  expected_delta: REFUTED before implementation — three independent blockers
  defensible_prior: was yes (JTDX); closed by pancetta-internal OSD-ladder findings
  wild_card: false
  evidence_against:
    - pancetta's OSD implements ONLY depths 0-3 (osd.rs::decode). JTDX's ndeep {3,4,5} ladder maps onto pancetta's {0,1,2,3}, already fully swept. No depth 4/5 to climb to.
    - The deeper end is refuted: hb-034 (OSD-3 loses 1 real, +284 novels), hb-041 (OSD-0 breaks the basicft8 fixture), hb-053 revisit (OSD-3+filter STILL loses 1 real). OSD-2 is the pinned elbow from both directions.
    - The adaptive TRIGGER needs QSO/MyCall AP hints; eval doesn't populate them and hb-051 measured the AP-recovery ceiling at 1/8576 on hard-200 even with perfect callsign info. ~0 headroom offline and on-air.
    - hb-014: OSD contributes ~0 recall vs jt9 anyway; its sole value is the one basicft8 fixture. "More OSD effort" can't add recall.
  notes: |
    See research/experiments/2026-05-25-adaptive-osd-depth.md. mr-007 at
    pick time closed what harvest-time mr-007 missed (lacked pancetta's
    hb-034/041/051/053). OSD-depth surface now closed every direction.
    Orthogonal mr-003 OSD work still open: hb-065 (GE removal, speed),
    hb-064 (DIA trajectory features — retrain OSD on layered-BP trajs).

### hb-056 — Cross-cycle coherent symbol averaging (csold buffer)  [PRIORITY: 0.60 plan-sized, spawned 2026-05-25 from mr-002]
  mode: ft8
  status: pending (plan-sized — design doc before iters)
  priority_score: 0.60
  estimated_effort: 2-3 sessions (plumbing) + Plan-spec
  expected_delta: significant — JTDX's headline sensitivity advantage on repeating CQs
  defensible_prior: yes — JTDX subpasses isubp1={4,7,10} use `s2(i) = |cs|² + |csold|²` averaging
  wild_card: false
  evidence_for:
    - JTDX maintains `complex csold(0:7,79)` populated from evencq/oddcq structs (last cycle's symbol field per CQ candidate by freq+DT).
    - Subpass branches isubp1=4,7,10 compute `s2(i) = |cs|² + |csold|²` — coherent (amplitude) integration across the 15s slot boundary for stations that repeat their CQ.
    - This is the headline JTDX sensitivity edge that operators reference when comparing to WSJT-X.
  evidence_against:
    - Pancetta has NO cross-slot symbol-buffer cache; decoder is currently stateless across slots.
    - Plumbing: per-candidate symbol stash in coordinator cycle handoff (~50 LOC), freq+DT proximity matching (~100 LOC), LLR computation extension to accept averaged-symbol mode (~100 LOC). Total ~200-400 LOC.
    - Needs a new corpus: contiguous slots from same weak repeating station (current corpus is single-slot WAVs).
    - Risk medium for architecture; once plumbed, low decode-quality risk.
  notes: |
    Plan-spec before implementation. Touchpoints:
    - new module pancetta-ft8/src/csold.rs (cross-cycle buffer)
    - pancetta-ft8/src/decoder.rs (LLR computation extension)
    - pancetta/src/coordinator/pipeline.rs (cycle handoff)
    - new corpus generator: contiguous-slot synth (extend gen-synth)
    Eval ceiling unknown without the new corpus.

### hb-057 — Median-filter DT averaging for sync/AP  [PRIORITY: 0.35, spawned 2026-05-25 from mr-002]
  mode: ft8
  status: pending (needs minor plumbing)
  priority_score: 0.35
  estimated_effort: 1-2 sessions
  expected_delta: latent — multi-pass is disabled (hb-031), so the per-station DT history value is mostly future
  defensible_prior: partial — JTDX uses this; pancetta currently doesn't track DT per callsign
  wild_card: false
  evidence_for:
    - JTDX commit "use median filter in average DT calculation" (Feb 2022). Median-of-N is robust to fade-dropout outliers.
    - Could feed back to sync as a prior — narrow the time-window search per known correspondent.
  evidence_against:
    - Pancetta tracks DT per-decoded-message but not per-correspondent. Plumbing: track median DT per recently-heard callsign in a small history table.
    - Value mostly latent — pancetta's multi-pass is disabled per hb-031, so the prior doesn't get to inform a second pass.
  notes: |
    Defer until multi-pass returns OR until two-stage scheduling
    (hb-046) lands and could use this as the second-stage prior.

### hb-058 — `/R` and ARRL Field-Day false-decode filters  [GRADUATED (focused) 2026-05-25 — batch 10]
  mode: ft8
  status: GRADUATED — contest-type rejection shipped in is_plausible; /R + directional-CQ parts spawned as follow-ups
  priority_score: 0.0
  estimated_effort: 1 session
  expected_delta: CONFIRMED — hard-200 -429 contest-type FP novels at +0 recall (no-filter)
  defensible_prior: yes (JTDX Feb-2022 commits) + instrumented audit
  wild_card: false
  evidence_for:
    - fp_format_audit.rs on hard-200: RTTYRoundup 335, FieldDay 83, Contest 15 novels — all ZERO recovered. Rejecting them in is_plausible (like the existing Telemetry rejection) removes -429 novels no-filter at +0 recall.
    - Production value is cold-start defense: the continuity filter (hb-062) runs LENIENT with an empty operator log, so message-type rejection is the primary contest-FP guard then. Also catches contest-format FPs on genuinely-spotted callsigns the continuity filter would pass. Eval-redundant (jt9-baseline filter) ≠ production-redundant.
  evidence_against:
    - Composite doesn't measure novels, so no headline-metric movement (like hb-014, hb-052).
    - DXpedition NOT rejected (88 FPs kept) — real DXpeditions are the highest-value hunt target.
  notes: |
    See research/experiments/2026-05-25-contest-type-fp-filter.md +
    pancetta-research/examples/fp_format_audit.rs. /R and directional-CQ
    were recall-risky in the audit (315 /R = 0-real but rovers exist;
    directional CQ 77-real/55-FP) → spawned hb-071 + hb-072.

### hb-071 — Single `/R` suffix FP handling  [PRIORITY: 0.30, spawned 2026-05-25 from hb-058]
  mode: ft8
  status: pending
  priority_score: 0.30
  estimated_effort: 1 session
  expected_delta: precision (315 hard-200 novels carry a /R suffix, 0 real here) but real-world recall risk
  defensible_prior: partial — JTDX filters /R FPs, but rovers (K1ABC/R) are legitimate rare traffic
  wild_card: false
  evidence_for:
    - fp_format_audit.rs: 315 single-/R novels on hard-200, ZERO recovered.
  evidence_against:
    - Real rovers exist on-air and are valid hunt targets; blanket-rejecting /R costs recall in the field even though hard-200 shows 0.
  notes: |
    Don't blanket-reject. Options: reject /R only where structurally
    invalid for the message context, or only under the continuity
    filter's cold-start lenient mode. Low priority.

### hb-072 — Directional-CQ modifier whitelist  [PRIORITY: 0.30, spawned 2026-05-25 from hb-058]
  mode: ft8
  status: pending
  priority_score: 0.30
  estimated_effort: 1 session
  expected_delta: precision (~55 hard-200 FP) without dropping the 77 real directional CQs
  defensible_prior: yes (JTDX "filter out directional CQ false decodes")
  wild_card: false
  evidence_for:
    - fp_format_audit.rs: "CQ <modifier>" is 77 recovered / 55 novel on hard-200 — a clean whitelist could split them.
  evidence_against:
    - Blanket filtering costs 77 real decodes. Needs a validated modifier set (DX, continents, CQ zones, POTA/SOTA, numeric); risks dropping valid-but-unlisted modifiers.
  notes: |
    Reject only CQ whose modifier is outside the whitelist. Pair with
    fp_format_audit.rs to confirm the split before shipping. Low priority.

### hb-059 — Slot-alignment detection for jt9 slot-cut on unaligned WAVs  [PRIORITY: 0.35, spawned 2026-05-25 from batch-5 iter 2]
  mode: ft8
  status: pending
  priority_score: 0.35
  estimated_effort: 1 session
  expected_delta: unblocks jt9 baseline generation on hard-200/1000 (currently 0 decodes via slot-cut)
  defensible_prior: yes (problem proven in batch-5 iter 2 smoke test)
  wild_card: false
  evidence_for:
    - The jt9 slot-cut helper produces correctly-sized chunks but they're misaligned with FT8 slot boundaries when the source WAV doesn't start on a slot.
    - Two approaches:
      (a) sweep starting offsets 0-14s (~14× jt9 invocations per 15s chunk)
      (b) detect alignment from spectral energy (Costas-correlation peak in time)
    - (b) is cheaper but more complex. (a) is brute force but reliable.
  evidence_against:
    - Niche: only matters if we want jt9 baselines on hard-200/1000 WAVs that don't already have pre-computed baselines in research/baselines/ft8/.
    - We already have those baselines so the urgency is low.
  notes: |
    Defer until hb-028 (cross-decoder ensemble) actually needs runtime
    jt9 calls on hard corpora. Currently the FP filter MVP uses
    pre-baselined truth instead.

### hb-060 — Remove dead `Ft8Config::enable_multithreading` field  [GRADUATED 2026-05-25 — batch 10]
  mode: ft8
  status: GRADUATED — field + all sites removed (cleanup, no behavior change)
  priority_score: 0.0
  estimated_effort: 0.5 sessions
  expected_delta: cleanup
  defensible_prior: yes (mr-004 audit)
  wild_card: false
  evidence_for:
    - Removed field + Default + test assert (decoder.rs), integration_tests.rs line, README table/example. Also removed the misleading `single_thread` benchmark (set the dead flag → bit-identical to `default`). Tests 193 lib + 11 integration pass; all targets build.
  notes: |
    See research/experiments/2026-05-25-dead-config-fields.md (combined
    with hb-061). Fourth dead Ft8Config field retired (after hb-032,
    hb-049, hb-061).

### hb-061 — Remove dead `Ft8Config::frequency_range` field  [GRADUATED 2026-05-25 — batch 10]
  mode: ft8
  status: GRADUATED — field + all sites removed (cleanup, no behavior change)
  priority_score: 0.0
  estimated_effort: 0.5 sessions
  expected_delta: cleanup
  defensible_prior: yes (mr-004 audit)
  wild_card: false
  evidence_for:
    - Removed field + Default (decoder.rs), enhanced_spectral_analysis.rs override, README + SPECTRAL_ANALYSIS_ENHANCEMENTS.md references. Actual bounds stay hardcoded (MIN_FREQ_BIN..max_freq_bin in costas_sync_search). Builds clean.
  notes: |
    Shipped together with hb-060. See
    research/experiments/2026-05-25-dead-config-fields.md.

### hb-052 — Production FP filter (callsign continuity)  [GRADUATED 2026-05-25]
  mode: ft8
  status: GRADUATED — production wire complete; composite +0.000641
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: realized: -129 novels on hard-200, -364 on hard-1000, -2 on wild-50; recall preserved/positive
  defensible_prior: yes
  wild_card: false
  outcome: |
    Shipped 2026-05-25 (batch 9). Production wire summary:
    - pancetta-qso::callsign_continuity::CallsignContinuityFilter built
      at coordinator startup with sources: operator ADIF
      (~/.pancetta/qsos.adi), cqdx-live spots (from CqdxBridge),
      500-deep rolling window. Cold-start lenient until reference
      ≥ 100 callsigns.
    - ApplicationCoordinator owns Option<Arc<...>>; pancetta/src/
      coordinator/ft8.rs applies it between decode-merge and
      broadcast loop.
    - Eval-side FpFilter library (pancetta-research) is its own
      strict-membership implementation used to validate the filter
      against jt9 baselines on the hard corpora.
    - 23 unit tests across the three crates pass.

    Composite result (batch 9 final 5-tier eval):
      0.554489 → 0.555131 (+0.000641)
      hard-200:  rec +11,  novel -129
      hard-1000: rec +48,  novel -364
      fixtures + synth-clean preserved
  notes: |
    See research/experiments/2026-05-25-batch-9-ship-filter.md.
    Mid-batch fix: fixtures tier was incorrectly applying the eval-side
    filter (strict-membership) and falsely dropping basicft8/170923_082015.wav
    whose callsigns are absent from the jt9 baseline corpus. Production
    wouldn't see this regression — cold-start lenient mode handles fresh
    stations. Fix in pancetta-research/src/bin/eval.rs::run_fixtures_tier
    drops the filter on that tier (decoder regression test, not filter
    test). [[hb-053]] graduations rode along with this shipping.

### hb-053 — Revisit shelved hypotheses with FP filter  [PARTIALLY GRADUATED 2026-05-25]
  mode: ft8
  status: gate=6 + iters=100 GRADUATED; hb-034, hb-018 revisits remain
  priority_score: 0.30
  estimated_effort: 1-2 sessions per remaining revisit (hb-034, hb-018)
  expected_delta: realized: +11 hard-200 rec, +48 hard-1000 rec via the two graduated knobs
  defensible_prior: yes (hb-052 MVP + batch 6 revisits + batch 9 production validation)
  wild_card: false
  outcomes_so_far: |
    Graduated (batch 9, 2026-05-25): max_parity_errors_for_osd 2→6,
    LDPC_MAX_ITERATIONS 50→100. Shipped together with [[hb-052]] /
    [[hb-062]] production filter.

    Batch 6 iter 4 (2026-05-25) — hb-014 with filter applied:
      gate=2+filter:   4364 rec / 811 novel (-1, -141)
      gate=4+filter:   4364 rec / 814 novel (-1, -138)
      gate=6+filter:   4365 rec / 820 novel ( 0, -132)
      => gate=6+filter MATCHES production recall AND reduces novels
         by 132 vs production (no filter). Analytical WIN.

    Batch 6 iter 5 (2026-05-25) — hb-035 with filter applied:
      iters=50  + filter: 4364 rec / 811 novel (-1, -141)
      iters=100 + filter: 4376 rec / 818 novel (+11, -134)
      => iters=100+filter gives +11 real decodes AND -134 novels
         vs production. Stronger analytical WIN than the gate
         revisit (adds recall, not just removes novels).

    Production scorecard delta (batch 9 final, gate=6 + iters=100 +
    filter combined):
      hard-200:  4365 → 4376 rec, 952 → 823 novel
      hard-1000: 14219 → 14267 rec, 3172 → 2808 novel
  notes: |
    Two knobs shipped. Remaining revisits: hb-034 (OSD-3) and hb-018
    (OSD-3 + CRC filter) under same framing — both now eligible since
    the filter is live in production. See
    research/experiments/2026-05-25-batch-9-ship-filter.md.

### hb-063 — Layered / WR-LBP belief propagation scheduling  [GRADUATED 2026-05-25 — batch 10]
  mode: ft8
  status: GRADUATED — layered_bp default true; composite +0.001049 (biggest single-iter move since hb-038)
  priority_score: 0.0
  estimated_effort: 1-2 sessions
  expected_delta: CONFIRMED — hard-200 +18 rec, hard-1000 +88 rec, -16% decode wall-clock, zero guard-tier regression
  defensible_prior: yes (Hocevar 2004; arXiv:2410.13131)
  wild_card: false
  evidence_for:
    - Full 5-tier (with FP filter): composite 0.555131 → 0.556180. fixtures 1.0, synth-clean -20/-18 unchanged. hard-200 rec 4376→4394 / novel 823→836; hard-1000 rec 14267→14355 / novel 2808→2849.
    - Controlled hard-200 A/B (no filter): flooding@100 4377 rec / 270s; layered@100 4395 rec / 226s (-16%); layered@50 4385 rec / 223s. layered@50 beats flooding@100 at HALF the iters — confirms ~2x convergence.
    - Layered decodes existing candidates BETTER (more reach syndrome=0 within budget) rather than admitting more candidates — first lever in many batches to add real recall, not just trade FPs. The +194 raw novels collapse to +13 under the production filter; recall is filter-invariant.
  notes: |
    Implemented as a layered branch in belief_propagation_with_trajectory
    (running posterior + immediate folding of each check's messages).
    Covers SumProduct + MinSum. Default flipped false→true. Unit test
    test_ldpc_layered_bp_converges. Lib tests 192→193.
    See research/experiments/2026-05-25-layered-bp.md. mr-003 #1 delivered.
    Follow-ups: hb-067 (mBP offset), hb-065 (adaptive GE — profile first),
    hb-064 (DIA trajectory features — could retrain OSD on layered trajs).

### hb-064 — DIA-augmented OSD with iteration-trajectory features  [PRIORITY: 0.42, spawned 2026-05-25 from mr-003]
  mode: ft8
  status: pending (plan-sized — needs training pipeline). NOW the correct OSD-speed lever: hb-065 profile (batch 11) shows TEP enumeration is 99.6% of OSD time, and this prunes TEP (paper: -97% TEP at SNR=2dB). Doubly motivated — layered BP (batch 10) changed the per-iter LLR trajectories the DIA model consumes, so pair with a retrain on layered trajectories.
  priority_score: 0.42
  estimated_effort: 2-3 sessions (training data + model + integration)
  expected_delta: significant TEP-enumeration speedup (paper reports 97% reduction at SNR=2dB on CCSDS (128,64))
  defensible_prior: yes (arXiv:2404.14165; pancetta already has a DIA-style neural_osd)
  wild_card: false
  evidence_for:
    - Paper trains a small neural model (~2 dense layers) on per-BP-iteration LLR trajectories (vs just final LLRs) to refine bit reliabilities; sliding-window classifier decides when to early-terminate TEP enumeration.
    - pancetta already has neural_osd.rs with DIA-style model (20K params). This refines the existing module — feature extraction changes from final-LLR to per-iteration-LLR-trajectory.
    - Strong architectural fit per mr-003 audit.
  evidence_against:
    - Plan-sized: requires training-data regeneration with per-iteration LLR capture; existing pipeline uses final-iter features only.
    - Risk of overfit to synth conditions.
  notes: |
    Source: arXiv:2404.14165 + companion arXiv:2307.06575.
    First step: verify pancetta's current DIA feature extraction
    (probably final-iter only). Then plan trajectory-capture data
    pipeline.

### hb-065 — Adaptive Gaussian-Elimination removal in OSD  [SHELVED 2026-05-25 — batch 11]
  mode: ft8
  status: SHELVED — profiled: GE is 0.4% of OSD time (529ms vs 147s TEP on hard-200). Adaptive-GE removal is a no-op; TEP enumeration dominates → hb-064.
  priority_score: 0.0
  estimated_effort: 1 session (profile + 1 session impl)
  expected_delta: OSD CPU cost reduction at unchanged FER (magnitude depends on GE fraction of OSD time)
  defensible_prior: partial (arXiv:2206.10957; gain depends on whether GE actually dominates pancetta's OSD-2)
  wild_card: false
  evidence_for:
    - OSD complexity dominated by per-call Gaussian elimination on the most-reliable basis (MRB).
    - Two early-decision conditions allow skipping GE entirely on many OSD calls.
    - Pancetta runs OSD-2 on every BP failure under parity gate=2; CPU savings would be meaningful if GE dominates.
  evidence_against:
    - At OSD-2, TEP enumeration is C(91,2)≈4k patterns per call — might dominate over GE, making the technique a no-op.
    - Profile required first.
  notes: |
    Source: arXiv:2206.10957 (Yue, Wang et al., 2022 IEEE TCom 2023).
    First step: instrument OSD path to time GE vs TEP enumeration on
    hard-200. If GE is <20% of OSD time, shelve.

### hb-066 — BP-RNN diversity ensemble for OSD pre-processing  [PRIORITY: 0.30, spawned 2026-05-25 from mr-003]
  mode: ft8
  status: pending (plan-sized; deferred)
  priority_score: 0.30
  estimated_effort: 3+ sessions
  expected_delta: speculative
  defensible_prior: weak (paper arXiv:2206.12150 targets short-block; pancetta-specific gain unknown)
  wild_card: false
  notes: |
    Multiple specialized BP-RNN decoders, each targeting distinct
    absorbing-set/trapping-set patterns. Plan-sized; defer until
    hb-063 + hb-065 + hb-067 are exhausted.

### hb-067 — mBP offset parameter for OSD pre-conditioning  [SOFT WIN 2026-05-25 — not graduated]
  mode: ft8
  status: tested — small precision finding, mechanism mismatch
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: -3 to -48 novels at zero recall cost (sweep result)
  defensible_prior: turned out partially right (paper's mechanism doesn't match observed behavior)
  wild_card: false
  outcomes: |
    Batch 8 iter 4 (2026-05-25) implemented + swept on hard-200:
      bp_offset=0.5: 4365 rec / 949 novel (Δ -3)
      bp_offset=1.0: 4365 rec / 932 novel (Δ -20)
      bp_offset=2.0: 4365 rec / 920 novel (Δ -32)
      bp_offset=4.0: 4365 rec / 904 novel (Δ -48)
    Recall preserved at all values. Novel monotonically decreases.
    BUT: mechanism is not the paper's "more flip patterns" — the
    offset interacts with the parity gate (which uses offset-adjusted
    LLRs), effectively tightening the gate. Not the intended lever.
  notes: |
    Library + CLI in place. Default 0.0 (no behavior change).
    Decision: don't graduate yet — mechanism mismatch suggests this
    interacts with hb-014. Spawn hb-067-followup if the parity-gate
    interaction needs investigating. Could combine with FP filter
    for additive precision.

### hb-068 — hb-044 variant (no sort-score inflation)  [SHELVED 2026-05-25]
  mode: ft8
  status: SHELVED — variant d doesn't fix hb-044 regression
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: REFUTED — hb-068 produces -117 recovered (same as hb-044's -116)
  defensible_prior: turned out wrong (displacement was not the cause)
  wild_card: false
  outcomes: |
    Batch 8 iter 5 (2026-05-25): variant (d) implemented — keep
    integer-bin sync_score for sort, use fractional offset for
    symbol extraction. Result essentially identical to hb-044
    original: 4248 rec (vs 4249) / 914 novel (vs 925). synth-clean
    SNR@90% gain still -18 → -20 dB.
    The hard-200 regression is NOT from sort-displacement. It's
    from the spectrogram interpolation itself perturbing already-
    correctly-aligned candidates.
  notes: |
    SHELVED. Reverted implementation to original hb-044 (refined
    score for sort) since neither variant graduates and both have
    flag default false. Spawn hb-069 for interpolation-in-linear-
    power-space as a different angle on the problem.

### hb-068 — hb-044 conditional/scaled refinement variants  [PRIORITY: 0.45, spawned 2026-05-25 from hb-044 mixed result]
  mode: ft8
  status: pending
  priority_score: 0.45
  estimated_effort: 1-2 sessions
  expected_delta: keep hb-044's synth-clean +2dB SNR@90% gain while removing the hard-200 -116 regression
  defensible_prior: yes (hb-044 batch 7 shows the gain exists; need a way to limit downside)
  wild_card: false
  evidence_for:
    - hb-044 implementation works correctly (parabolic + linear-interp). Synth-clean SNR@90% gained 2 dB. But hard-200 lost 116 decodes.
    - Likely cause: refinement-inflated scores displace better candidates in the top-300 cap on busy-band WAVs.
    - Variants worth testing:
      (a) score-gated refinement (only when sync_score > threshold)
      (b) scaled delta (0.5× instead of 1.0×)
      (c) reject large deltas (|delta| > 0.3 → use integer)
      (d) refinement that NEVER inflates score (use refined position, original score for sorting)
  notes: |
    Variant (d) is the most principled — eliminates the displacement
    mechanism. Try first.

### hb-062 — cqdx.io production FP-filter source  [GRADUATED 2026-05-25]
  mode: ft8 (production wiring)
  status: GRADUATED — full production pipeline (cqdx + ADIF + rolling) wired into coordinator
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: unblocked [[hb-052]] production deployment and [[hb-053]] graduations; both shipped batch 9
  defensible_prior: yes
  wild_card: false
  outcome: |
    Shipped 2026-05-25 (batch 9). Three parts (batches 8+9):
    - pancetta-cqdx: CqdxCache::spotted_callsigns() returns
      HashSet<String> from spot_groups + rarity_scores (batch 8)
    - pancetta-qso/src/callsign_continuity.rs: CallsignContinuityFilter
      with strict + lenient cold-start modes, ADIF + iter + cqdx
      sources, thread-safe via RwLock. 13 unit tests (batch 8)
    - pancetta/src/coordinator/{mod.rs, ft8.rs}: ApplicationCoordinator
      owns Option<Arc<...>>, built at startup from operator
      ~/.pancetta/qsos.adi + initial cqdx-spotted snapshot + 500-deep
      rolling cap + 100-callsign cold-start threshold. FT8 decoder
      thread applies post-decode / pre-broadcast (batch 9).

    Composite result: 0.554489 → 0.555131 (+0.000641). See
    research/experiments/2026-05-25-batch-9-ship-filter.md.
  notes: |
    Periodic cqdx-spot refresh into update_cqdx_spotted is NOT yet
    wired (filter only sees the snapshot taken at coordinator
    startup). Acceptable for the initial ship — operator can restart
    the station to refresh. Spawning followup if/when this becomes
    operationally noticeable.

### hb-069 — hb-044 interpolation in linear power space  [PRIORITY: 0.35, spawned 2026-05-25 from hb-068 finding]
  mode: ft8
  status: pending
  priority_score: 0.35
  estimated_effort: 1 session
  expected_delta: rescue hb-044's synth-clean SNR@90% gain at lower hard-200 cost
  defensible_prior: partial — hb-044 spectrogram interpolation is in dB space; linear-power interpolation may preserve symbol energies better
  wild_card: false
  evidence_for:
    - hb-068 (batch 8 iter 5) confirmed the hard-200 regression is from interpolation perturbing already-correctly-aligned candidates, NOT from sort-displacement.
    - Linear interpolation in dB space is non-linear in actual power; small fractional shifts can disproportionately affect log values near the noise floor.
    - Converting to linear (10^(dB/10)) before interpolation, then back to dB, may preserve symbol energy more accurately.
  evidence_against:
    - 2x more pow/log operations per interpolated lookup → meaningful CPU cost.
    - May still regress hard-200 if the issue is more fundamental than just interp space.
  notes: |
    Implementation: change lookup_time_interp in pancetta-ft8/src/decoder.rs
    to convert dB→linear, interpolate, convert back. Re-sweep hb-044
    on hard-200 + synth-clean.

### hb-070 — Gated trailing-block Costas rescue  [PRIORITY: 0.30, spawned 2026-05-25 from hb-054]
  mode: ft8
  status: pending
  priority_score: 0.30
  estimated_effort: 1 session
  expected_delta: small — bounded by # of hard-corpus signals with a genuinely corrupted leading Costas block (hb-054 says: very few)
  defensible_prior: partial — fixes hb-054's over-relaxation, but the corpus may simply not need it
  wild_card: false
  evidence_for:
    - hb-054 (batch 10) shelved the ungated max(syncf, syncs): it relaxes the sync gate for ALL candidates, adding +35 novel FPs on hard-200 with zero recall gain.
    - A gated form would apply syncs ONLY when the leading block is detectably depressed (e.g. syncf < syncs - δ AND block[0] score << block[1..]), so a clean full-slot signal never sees the relaxed gate — eliminating the FP source while keeping the rescue for genuinely leading-corrupted signals.
  evidence_against:
    - hb-054 found essentially no real signals on hard-200/-1000 that need the rescue. Upside likely tiny on the curated tiers.
    - Best motivated against a slot-misaligned / weak-single-station corpus (cf. hb-025: wild-50 captures at dt ∈ [-2.5,-1.4]) which pancetta doesn't yet have as a scored tier.
  notes: |
    Defer until a slot-misaligned or weak-single-station corpus exists
    to score against. On the current curated tiers it will at best be a
    no-op. See research/experiments/2026-05-25-costas-two-of-three.md.

### hb-049 — Remove dead `Ft8Config::min_snr_db` field  [WIN 2026-05-24]
  mode: ft8
  status: GRADUATED — field + const + all referencing sites removed
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: cleanup (no behavior change)
  defensible_prior: yes (hb-045 audit + removal verified)
  wild_card: false
  evidence_for:
    - Removed Ft8Config::min_snr_db + MIN_DECODE_SNR const + all 7 referencing sites (decoder.rs decl/Default/const/test_default, integration_tests.rs ×2, decoder_benchmark.rs ×2, README.md ×2, SPECTRAL_ANALYSIS_ENHANCEMENTS.md, examples/enhanced_spectral_analysis.rs).
    - `grep -rn min_snr_db pancetta-ft8/` returns nothing post-change.
    - Tests: 189 lib + 7 integration pass; examples build clean.
  notes: |
    Mirror of hb-032 (aggressive_decoding removal). See
    research/experiments/2026-05-24-min-snr-db-removal.md.
    Recommend running mr-004 (quarterly source-drift audit) to
    catch remaining dead config flags in one pass.

### hb-043 — AP my_call-less injection  [WIN 2026-05-24 infrastructure]
  mode: ft8
  status: GRADUATED (infrastructure) — my_call-less AP path plumbed end-to-end
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: 0 composite from this iter; unblocks hb-027
  defensible_prior: yes
  wild_card: false
  evidence_for:
    - 2026-05-24 implementation: added `inject_recent_call_at_called` to ap.rs (companion to existing `inject_ap2_caller`); new `RecentInjectPos` enum + `par_try_ldpc_with_recent_only` helper in decoder.rs (~80 LOC); new code path in par_try_ap_decode that runs when my_call.is_none() && !recent_calls.is_empty(); extended ap_active check.
    - Sanity sweep on hard-200 with 5 + 20 popular callsigns: rec=4365/novel=952 unchanged (the popular callsigns are already AP0-recoverable). Wallclock scales linearly with N (5 calls → 190s, 20 calls → 437s), confirming the AP path activates correctly without bugs.
    - hb-027 (joint multi-slot via QSO context) is now unblocked. Next step: rolling-window callsign tracker.
  notes: |
    See research/experiments/2026-05-24-ap-mycall-less-injection.md.
    Note: AP only runs when AP0 FAILS — by construction, AP can only
    add decodes that AP0 missed. Sanity test with popular callsigns
    produces 0 delta because AP0 already handles those. Real-value
    measurement requires hb-050 (rolling-window source) + hb-051
    (AP-recovery ceiling diagnostic).

### hb-042 — Score-based cap  [SHELVED 2026-05-24]
  mode: ft8
  status: SHELVED — score-based cap is just count-based cap in disguise
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: REFUTED — same Pareto frontier as hb-033
  defensible_prior: turned out wrong (score distribution is smooth, no elbow)
  wild_card: false
  evidence_against:
    - 2026-05-24 sweep at cap=500 min_sync ∈ {4.0, 4.5, 5.0} on hard-200:
      cap=500 min=4.0 ≡ cap=500 min=4.5 BIT-IDENTICAL (no candidates in top-500 have score ∈ [4.0,4.5]).
      cap=500 min=5.0 trims 11 novels at zero recall change — smooth fade, no elbow.
      All three configs are equivalent to hb-033's cap=500 finding (4372 rec / 1076 novel), confirming the score floor doesn't bite where the count cap binds.
    - hb-007's "min_sync_score is dead" finding HOLDS at the new production state (cap=300, NMS off, gate=2). The score-distribution shape didn't change meaningfully.
  notes: |
    SHELVED. Score and count caps are dual parameterizations of the
    same pruning rule; neither addresses the underlying precision
    wall. See research/experiments/2026-05-24-score-based-cap.md.

### hb-041 — Disable OSD fallback entirely (parity gate = 0)  [SHELVED 2026-05-24]
  mode: ft8
  status: SHELVED — gate=0 loses 1 fixture-tested real decode (basicft8/170923_082015.wav)
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: REFUTED — fixture corpus catches the 1 decode that OSD provides
  defensible_prior: partial (turned out wrong on the right tier)
  wild_card: false
  evidence_against:
    - 2026-05-24 full 5-tier sweep at gate=0 vs current production gate=2:
      hard-200/1000 recovered unchanged (4365 / 14219 = preserved).
      synth-clean SNR@50% / @90% preserved (-20 / -18 dB).
      Novels drop -10% (-92) on hard-200 and -11% (-336) on hard-1000.
      BUT fixtures pass_rate drops 1.0 → 0.875 (7/8): basicft8/170923_082015.wav
      decodes 1 message at gate=2 and 0 messages at gate=0. OSD provides the
      ONLY decode path for that one ground-truth real-world signal.
    - Composite -0.0188, entirely from the fixture-pass-rate drop (0.15 weight × -0.125 = -0.01875).
    - The iter-2 "OSD contributes ~0 recall" finding was measured against jt9-derived truth and is incomplete; the fixture corpus catches the marginal OSD recovery that jt9 also misses.
  notes: |
    SHELVED. Gate=2 confirmed at the elbow.
    See research/experiments/2026-05-24-osd-disable-audit.md.
    Future "tighten OSD further" hypotheses are structurally closed —
    any further OSD reduction must first justify losing the fixture-
    tested basicft8 decode.

### hb-040 — Plumb (or remove) `Ft8Config::time_range`  [PRIORITY: 0.35]
  mode: ft8
  status: pending
  priority_score: 0.35
  estimated_effort: 0.5-1 session
  expected_delta: niche; recovers misaligned recordings (e.g., the 92/96 wild-50 truth decodes at dt < -1.4)
  defensible_prior: yes (hb-025 audit confirmed time_range is dead code)
  wild_card: false
  evidence_for:
    - hb-025 audit (2026-05-23): `Ft8Config::time_range` exists at decoder.rs:126 with default 2.0 but is unused anywhere in the decode pipeline. Spectrogram time_padding is hardcoded to 0.
    - Setting time_range=3.0 had zero effect on wild-50 (still 0/96 recovered).
    - The 92/96 wild-50 truth decodes have dt ∈ [-2.5, -1.4]; current decoder can't search those offsets because the audio buffer's t=0 is the slot's t=0.
  evidence_against:
    - Niche benefit: wild-50 outliers are misaligned recordings (corpus quirk, not on-air operational state).
    - Plumbing requires audio-buffer padding + adjusting Costas search start position — non-trivial code touch.
    - Alternative: remove the dead field (simpler, drops a misleading API knob).
  notes: |
    Two paths:
    (a) **Plumb:** thread time_range through to Spectrogram::time_padding.
        Pad audio buffer with leading silence corresponding to time_range
        seconds. Costas search starts at t=-time_range instead of t=0.
        Operational benefit: handles recordings that don't start exactly
        on slot boundary (real on-air capture has jitter).
    (b) **Remove:** delete the field + default. Smaller, cleaner. Same
        pattern as hb-020 (`aggressive_decoding` is also dead and
        spawned hb-032 for removal).
    Prefer (a) if operational recording-alignment is a concern; (b)
    otherwise.

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

### hb-034 — OSD-3 follow-up under gate=2  [SHELVED 2026-05-24]
  mode: ft8
  status: SHELVED — OSD-3 LOSES 1 real decode and adds +284 novels
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: REFUTED — negative recall delta, +30% novels
  defensible_prior: yes (sweep data)
  wild_card: false
  evidence_against:
    - 2026-05-24 sweep on hard-200 at current production state (gate=2, cap=300, NMS off): OSD-3 yields 4364 recovered (-1 vs OSD-2's 4365) and 1236 novel (+284 vs OSD-2's 952). Wallclock unchanged.
    - The +200-ish novels added by OSD-3 are a fixed property of the width-3 trial expansion (~125K trials/candidate, statistically meaningful CRC-14 collision rate), invariant under parity gate width.
    - Combined with the iter-2 finding that OSD's recall contribution is ~0 on hard-200 vs jt9 truth, OSD-3's "+novels" are nearly all FPs.
  notes: |
    SHELVED. See research/experiments/2026-05-24-osd3-followup.md.
    hb-018 (FP filter for OSD-3) becomes moot — nothing to filter
    toward without recall benefit. hb-041 (disable OSD fallback)
    becomes more compelling.

### hb-035 — Sweep for max BP convergence rate  [SHELVED 2026-05-24]
  mode: ft8
  status: SHELVED — LLR axis dead (hb-006 elbow holds); iters axis marginal
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: REFUTED — LLR widening regresses; iters extension trades 1.75:1 novel/real (poor)
  defensible_prior: turned out partial
  wild_card: false
  evidence_against:
    - 2026-05-24 sweep on hard-200 (current prod gate=2 cap=300):
      LLR ∈ {32, 40, 48} at iters=50: LLR=40 loses -12 rec; LLR=48 loses -14 rec. The hb-006 elbow at variance=32 is intact under the new production state.
      iters ∈ {50, 75, 100} at LLR=32: iters=100 gains +12 rec at +21 novel (1:1.75 real/novel ratio, vs top-300 5:1). Marginal recall, poor precision.
      Composite delta in either direction is ±0.0008 — at the limit of what parameter sweeps can move.
  notes: |
    SHELVED. See research/experiments/2026-05-24-bp-convergence-sweep.md.
    Future parameter tuning hits the precision wall — same as the OSD
    line. Path forward is structural (NMS-aware subtract, joint multi-
    slot via QSO context, cross-decoder ensemble), not parametric.

### hb-033 — sync_cap saturation audit  [SHELVED 2026-05-24]
  mode: ft8
  status: SHELVED — cap=300 is at the elbow; higher caps not worth it
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: REFUTED — +7 decodes at +160% wallclock + degraded precision
  defensible_prior: yes (saturation sweep data)
  wild_card: false
  evidence_against:
    - 2026-05-24 sweep on hard-200 at gate=2: cap=300 → 4365 rec / 952 nov / 255s; cap=400 → 4371 (+6) / 1026 (+74) / 409s; cap=500 → 4372 (+7) / 1076 (+124) / 662s. Marginal real/FP ratio collapses from 1:0.2 (in the top 300) to 1:12 (in 300-400) to 1:18 (in 400-500).
    - Wallclock blows past the 3000ms/WAV operational budget on individual WAVs at cap=500.
    - Recall gain is trivially small (+0.08% absolute) and is dwarfed by the FP increase.
  notes: |
    SHELVED — cap=300 stays as production default. The cap=200→300
    win that hb-038 found doesn't extrapolate to 300→400→500.
    See research/experiments/2026-05-24-sync-cap-saturation.md.
    Successor: hb-042 (try a score-based cap instead of a count-based one).

### hb-012 — Negative time offset extension (early-arriving DX signals)  [SHELVED 2026-05-25 — batch 11]
  mode: ft8
  status: SHELVED — premise invalid: corpus is 90s continuous multi-slot recordings; full-buffer Costas scan from t0=0 already covers all interior timing. Operational-only (unmeasurable in harness).
  priority_score: 0.0
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

### hb-013 — MIN_FREQ_BIN floor reduction  [SHELVED 2026-05-25 — already fixed]
  mode: ft8
  status: SHELVED — confirmed MIN_FREQ_BIN=0 in decoder.rs:82
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: 0 (the fix had landed at some prior point)
  defensible_prior: confirmed
  wild_card: false
  notes: |
    Batch 5 iter 5 confirmed MIN_FREQ_BIN=0 (line 82 of decoder.rs).
    The gap referenced in the decoder_sensitivity memory had been
    closed at some prior point. No further action needed.

### hb-014 — Parity gate sweep  [GRADUATED 2026-05-23]
  mode: ft8
  status: GRADUATED — production default `max_parity_errors_for_osd: 4 → 2`
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: ~0 composite (recall flat); -21% novel decodes; -26% wallclock
  defensible_prior: yes (sweep data)
  wild_card: false
  evidence_for:
    - 2026-05-23 sweep {0..6} on curated-hard-200: recovered count was IDENTICAL (4365) from gate=0 through gate=4. Gate=5/6 gained ONE real decode (4365→4366) at +211/+531 additional novels (likely FPs).
    - Verified on curated-hard-1000: gate=2 vs main (gate=4) lost 3 real decodes (out of 28104 = 0.011%, well within noise) and dropped novels 4019 → 3172 (-21%).
    - Wallclock cut from 331s → 246s on hard-200 (-26%).
    - Note: OSD's recall contribution on hard-200 is essentially zero vs jt9 truth (gate=0 = gate=6 on recovered). OSD's role is now narrowed to the highest-confidence near-misses (≤2 parity errors after BP).
  notes: |
    See research/experiments/2026-05-23-parity-gate-sweep.md.
    Successor: hb-041 (consider gate=0 to fully disable OSD fallback).

### hb-015 — Doppler-resilient sync search (phase-coherent integration)  [PRIORITY: 0.38 — blocked on hb-073 real-Doppler corpus]
  mode: ft8
  status: pending — best validated against a REAL Doppler tier (hb-073); the crude synth-doppler model may understate the gain (lacks true spread). mr-006 (batch 11): bump to ~0.42 once hb-073 lands.
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

### hb-032 — Remove or repurpose dead `aggressive_decoding` field  [WIN 2026-05-24]
  mode: ft8
  status: GRADUATED — field deleted; bench/test names renamed to match actual knobs
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: cleanup (no behavior change)
  defensible_prior: yes (hb-020 audit)
  wild_card: false
  evidence_for:
    - Removed Ft8Config::aggressive_decoding field + Default impl + all 5 referencing sites (decoder.rs, integration_tests.rs, decoder_benchmark.rs, README.md, SPECTRAL_ANALYSIS_ENHANCEMENTS.md). The "aggressive" benchmark renamed to "high_sensitivity" to describe what the bundled knobs actually do.
    - Tests: 189 lib + 35 integration pass post-deletion. Build clean.
  notes: |
    Picked option (a) over option (b) (repurpose as preset enum).
    Repurposing a dead bool would muddy a "remove dead code" commit
    with new API design. A fresh fast|balanced|deep preset (if ever
    wanted) is a clean separate hb-NNN.
    See research/experiments/2026-05-24-aggressive-decoding-removal.md.

### hb-021 — Wild-card: frequency-domain signal subtraction  [PRIORITY: SHELVED]
  mode: ft8
  status: SHELVED (2026-05-23 — profile rejected motivation)
  priority_score: 0.0
  estimated_effort: 3 sessions
  expected_delta: REFUTED — upper bound ~1.3% wall-clock
  defensible_prior: no (rejected by 2026-05-23 profile)
  wild_card: true
  evidence_against:
    - 2026-05-23 multipass-profile (hard-200, max_passes=2): spectrogram is 0.4% of pass time, sync_search is 0.9%, combined pre-decode is 1.3%. Even if freq-domain subtract made pass-1 pre-decode entirely free, total speedup is ~1.3%.
    - The actual multi-pass bottleneck is the time-domain `subtract_with_sidelobes` on pass 0 (547 ms/WAV, 43% of pass-0 wallclock). That cost is paid in time domain regardless of whether subsequent passes reuse the spectrogram.
    - Pass 2 yields 28 new decodes out of 5575 (0.5% recall lift). Cost/benefit doesn't justify the rewrite even if it were faster.
    - hb-031 already harvested the multi-pass overhead by setting max_passes=1 in production.
  notes: |
    SHELVED. See research/experiments/2026-05-23-multipass-profile.md for
    full per-pass timing breakdown. Revisit only if a future change
    significantly raises pass-2's recall yield (e.g., NMS-aware subtract
    or a much better candidate generator on pass 2).

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

### hb-027 — Wild-card: Joint multi-slot decoding via QSO context  [SHELVED 2026-05-24 via hb-051]
  mode: ft8
  status: SHELVED — same architectural premise as hb-050; closed by hb-051 ceiling
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: REFUTED — hb-051 caps perfect-information AP recovery at 1 decode on hard-200
  defensible_prior: turned out wrong
  wild_card: true
  evidence_against:
    - hb-051 diagnostic (2026-05-24): perfect-information AP injection (truth callsigns as hints) recovers 1 decode out of 8576 on hard-200. Any approximation of "callsigns from prior slots" can only do worse.
    - The architectural premise — that injecting recently-observed callsigns as AP priors will unlock significant recall — is closed for the hard-200 corpus shape.
    - Pancetta's AP code path only fires when AP0 fails. On hard-200, AP0 already handles candidate callsigns that have any reasonable signal; the failures are weak-signal candidates where the LDPC-with-bias still can't converge.
  notes: |
    SHELVED. May still apply in OPERATIONAL on-air context (different
    corpus shape; potentially different failure modes). Re-evaluate
    only with operator-side data once that infrastructure exists.
    See research/experiments/2026-05-24-batch-4-unblock.md iter 1 + 3.

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

## Meta-research (idea generators)

These entries are not single hypotheses — they are SOURCES + METHODS
for generating new hypotheses. When the regular bank thins out (the
"parameter sweeps exhausted" plateau), run a meta-research cycle:
pick an `mr-NNN` entry, execute its discovery method, harvest the
findings into 3-5 new `hb-NNN` entries with proper
evidence_for/against/notes.

**Internet research is on the table.** WebSearch + WebFetch are
available and underused. Particularly high-value for FT8 decoder
work — most algorithmic improvements over the last 5 years are
documented in WSJT-X / JTDX commit history, academic papers on
LDPC/OSD, and ham radio discussion forums. Don't restrict the
search to in-repo sources.

### mr-001 — Audit WSJT-X commits in last 12 months  [COMPLETED 2026-05-24]
  status: completed (executed via Explore-style agent)
  estimated_effort: 1 session (Explore agent + harvesting)
  source_type: external git history
  source: https://sourceforge.net/p/wsjt/wsjtx/ci/main/tree/ + git log
  outcome: |
    Key finding: WSJT-X main is mostly DORMANT on FT8 decoder algorithms
    since ~2021 (Joe Taylor's a7 commit was the last substantive change).
    Active development moved to WSJT-X-Improved fork (DG2YCB):
      - v3.0.0 Dec 2025: 2-stage / 3-stage MTD pass scheduling
      - v3.1.0 May 2026: sub-sample DT refinement, optimized baseline,
        auto-tightened passband
    Yield: 6 new hypotheses (hb-043, hb-044, hb-045, hb-046, hb-047, hb-048)
    sourced from this audit. The meta-research approach WORKS — external
    audit pulled in 6 fresh hypotheses where the in-repo bank was thinning.
    Recommendation: pivot to JTDX (mr-002) when WSJT-X-Improved findings
    are exhausted.
  expected_yield: 2-5 new hypotheses (actual: 6 incl one spawn from the AP wiring work)
  defensible_prior: yes — confirmed by audit results

### mr-002 — JTDX delta vs WSJT-X  [COMPLETED 2026-05-25]
  status: completed (background Explore agent, ~5 min)
  estimated_effort: 1 session
  source_type: external repo + changelog
  source: https://sourceforge.net/projects/jtdx/ — code browser, CHANGELOG, forum threads
  outcome: |
    Key finding: JTDX public source is FROZEN at 2.2.159 (April 2022).
    v2.2.160 beta-only; dev team halted public publishing for
    geopolitical reasons. Rich harvest with no future-version risk.
    Agent applied mr-007 architecture-fit audit at harvest time —
    flagged 1 plan-sized item + 1 deferred-plumbing item before they
    consumed iter slots.
    Yield: 5 new hypotheses (hb-054, hb-055, hb-056, hb-057, hb-058)
    — 3 clean-attach + 1 plan-sized + 1 deferred-plumbing.
    Recommendation: pivot to mr-003 (academic LDPC literature) as
    next external source. JTDX-Improved is a UI fork only.
  expected_yield: 2-4 hypotheses (actual: 5)
  defensible_prior: yes — confirmed by audit results

### mr-003 — LDPC/OSD academic literature 2020-2026  [COMPLETED 2026-05-25]
  status: completed (background Explore agent, ~2 min)
  estimated_effort: 1-2 sessions
  source_type: academic papers via WebSearch
  source: arXiv (cs.IT) — short-block LDPC, layered BP, OSD, neural-augmented
  outcome: |
    Field is ACTIVE for short-block LDPC + BP + OSD (driven by 5G NR
    control channels, CCSDS deep-space, quantum codes). Agent applied
    mr-007 architecture-fit audit at harvest time.
    Yield: 5 new hypotheses ranked by attachment fit:
      hb-063 Layered/WR-LBP BP scheduling (arXiv:2410.13131) — top pick
      hb-064 DIA-augmented OSD w/ iteration trajectories (arXiv:2404.14165) — plan-sized
      hb-065 Adaptive GE removal in OSD (arXiv:2206.10957) — needs profile first
      hb-066 BP-RNN diversity ensemble (arXiv:2206.12150) — plan-sized, deferred
      hb-067 mBP offset parameter (arXiv:2306.00443) — cheap one-iter sweep
    Flagged as "not pancetta-relevant": quantum LDPC, long-code 5G NR.
    Recommendation: hb-063 first (lowest risk, biggest budget headroom).
    If hb-063/065/067 cycle is exhausted, pivot to signal-processing
    literature on coherent sync (pancetta's sensitivity gap at very
    low SNR is increasingly sync-side).
  expected_yield: 3-7 hypotheses (actual: 5)
  defensible_prior: yes — confirmed by audit results

### mr-004 — Source-code drift audit (quarterly)
  status: pending
  estimated_effort: 0.5 session
  source_type: internal code review
  source: pancetta-ft8/src + pancetta-research/src
  method: |
    Periodic re-grep for: dead config flags (like hb-020 found
    aggressive_decoding), TODO/FIXME comments, surface-vs-actual
    drift (config knobs that don't flow into the decode pipeline),
    obsolete code paths left over from earlier experiments. Run
    quarterly or after every 5 iters.
  expected_yield: 1-3 cleanup hypotheses + occasional structural finds
  defensible_prior: yes — hb-020 (aggressive_decoding) and hb-025 (time_range dead) both came from this technique

### mr-005 — Cross-cutting pattern review of shelved hypotheses
  status: pending
  estimated_effort: 1 session
  source_type: internal journal corpus
  source: research/experiments/*.md (now 20+ files)
  method: |
    Read all shelved journals as a single corpus. Look for
    cross-cutting patterns we missed iter-by-iter. Specifically:
    (a) hypotheses that were shelved as "no win" but might unlock
    each other if combined, (b) common excuses ("the bank entry's
    motivation was wrong") that hint at meta-process bugs, (c)
    diagnostic findings that quietly identified structural gaps
    we never followed up on (hb-039's "97% novels are FPs" is one).
  expected_yield: 1-3 reopen-worthy hypotheses + meta-process insights
  defensible_prior: yes — we already have the "precision wall" insight from cross-iter pattern (hb-014 + hb-034 + hb-035 + hb-041 all hit the same wall)

### mr-007 — Architecture-fit audit for harvested hypotheses  [spawned 2026-05-24 from hb-045]
  status: pending
  estimated_effort: 0.5 session per harvest batch (added to mr-001/002/003 procedure)
  source_type: internal — adds a check step to external-source harvests
  source: hb-045 audit (architecture mismatch caught at iter time, not harvest time)
  method: |
    BEFORE promoting a candidate hb-NNN from a meta-research harvest
    (mr-001, mr-002, mr-003) to active, run an architecture-fit audit:
    1. What pancetta module/function does the technique correspond to?
    2. Does pancetta's existing code have the failure mode the technique
       fixes, or does pancetta's design avoid that failure mode by
       construction?
    3. Is the necessary plumbing (e.g., a config flag, a baseline
       computation, an SNR threshold) ALREADY USED, partly used (dead
       config flag like aggressive_decoding/min_snr_db), or absent?
    4. If absent/dead, the hypothesis is either (a) "first install the
       plumbing, then test the technique" or (b) shelve as
       architecture-mismatch.
  rationale: |
    hb-045 wasted an iter SHELVING a hypothesis whose architecture
    mismatch was knowable at harvest time. mr-007 catches this class
    before it consumes an iter slot.
  expected_yield: prevents 1-3 wasted iters per harvest cycle

### mr-006 — Real-world FT8 corpus expansion survey  [COMPLETED 2026-05-25 — batch 11]
  status: COMPLETED — background-agent survey; "don't expand corpus for composite now"
  estimated_effort: 1-2 sessions
  source_type: external recordings + forum discussion
  source: pskreporter.info, WSJT-X user group, DXpedition recordings on YouTube/QRZ
  outcome: |
    Skeptical survey (research/experiments/2026-05-25-corpus-expansion-survey.md).
    Headline: DON'T expand the corpus for composite this batch — the
    existing tiers aren't the bottleneck (precision wall + no multi-pass/
    QSO-context are). Findings:
    - 3 of 5 stress classes DEAD on architecture-fit: (a) DXpedition
      pileups now use SuperFox (a non-FT8 waveform pancetta CANNOT decode;
      Hound uplinks are just ordinary FT8 already covered by hard-*);
      (d) splatter has no decoder lever (FP filter already handles it);
      (e) HF-mobile flutter is subsumed by Doppler.
    - 2 task-named sources DISQUALIFIED: PSKReporter stores spots/metadata
      only (NO audio archive); the arXiv 2512.23160 "Weak Signal" dataset
      is spectrograms/vectors, NOT WAV.
    - ONE worthwhile class: (c) real polar/auroral/TEP Doppler — obtainable
      as native 12 kHz mono WAV via KiwiSDR + kiwirecorder.py (slot-aligned
      via :12/:27/:42/:57 cron). Spawned hb-073.
    - Slot-alignment: don't build a preprocessor — capture slot-aligned at
      source; for already-misaligned audio the dormant time_padding plumb
      (hb-040/hb-012) is the in-decoder fix, but its corpus payoff is ~0
      (see hb-012 batch-11 shelve: continuous multi-slot recordings already
      cover interior timing). The cheap always-do: capture future operator
      recordings slot-aligned so the next "wild" data is usable.
  defensible_prior: confirmed — busy-NA hard-* is one class, but most other
    classes are non-actionable for pancetta's offline/pass-1/no-AP architecture

### hb-073 — Real-Doppler eval tier (KiwiSDR auroral/TEP capture)  [PRIORITY: 0.40, spawned 2026-05-25 from mr-006]
  mode: ft8
  status: pending — enabler (acquire corpus), not a direct composite win
  priority_score: 0.40
  estimated_effort: 1-2 sessions (capture + manifest + baseline)
  expected_delta: no direct composite move; replaces the crude synth-doppler model (5% weight) with REAL data and gives hb-015 a real denominator
  defensible_prior: yes — synth-doppler is documented-crude (multiplicative cosine, not true Doppler); JTDX's known Doppler edge proves the gap is real and decoder-addressable
  wild_card: false
  evidence_for:
    - mr-006: real auroral/TEP/EME paths produce genuine Doppler spread + drift the crude synth model lacks. KiwiSDR native output is 12 kHz mono WAV; kiwirecorder.py cron lands on slot boundaries.
    - Unblocks hb-015 (frequency-ramp Costas sync) — the structural sync lever that survives the precision wall.
  evidence_against:
    - Doppler is only 5% of the composite; an enabler, not a near-term win.
    - Requires being on-air during the right opening (auroral/TEP) — acquisition latency.
  notes: |
    Capture 20-50 slot-aligned 12 kHz WAVs from a high-latitude public
    KiwiSDR on 6 m/10 m during an auroral or TEP opening; truth via jt9
    baseline. Schedule only when there's appetite for the multi-session
    hb-015 structural-sync line. See
    research/experiments/2026-05-25-corpus-expansion-survey.md.

## Shelved (kept for reference)

### hb-039 — Resolve the 856 isolated novels (hb-024 follow-up)  [SHELVED 2026-05-23]
  mode: ft8
  status: shelved
  priority_score: 0.45
  outcome: |
    Self-consistency check on hard_1000 isolated callsigns: 97.1%
    (2278/2346) are singletons — appear in exactly ONE pancetta WAV.
    Only 2.9% (68) appear in 2+ distinct pancetta WAVs.
  measured_delta: 0 (diagnostic only)
  learning: |
    Singleton-callsign-in-novels is a strong FP signature: random
    LDPC+CRC convergences in different noise WAVs almost never
    produce the same fake callsign. Combined with hb-024:

    Refined precision estimate on hard_1000:
      - Recovered (jt9-matched):     4326
      - Continued novels (real):     1572
      - Multi-isolated novels:        ~26 (likely real rare DX)
      - Singleton-isolated novels:  ~830 (likely FPs)

    Pancetta precision: ~5924 real / (5924 + 830 FP) ≈ 87.7% —
    matches hb-024's worst-case bound.

    hb-014 (parity gate sweep) is now better-motivated: tightening
    the gate from ≤4 to ≤3 might drop the FP rate to ~5% at cost
    of some real decodes.
  follow_up: hb-014 (already in active bank) — tighten parity gate to drop FPs.
  scorecards: (n/a — probe output only)
  journal: research/experiments/2026-05-23-resolve-isolated-novels.md

### hb-025 — Wild-50 zero-overlap investigation  [SHELVED 2026-05-23]
  mode: ft8
  status: shelved
  priority_score: 0.50
  outcome: |
    The 2 outlier WAVs (92e31566..., 28f0ce9e...) accounting for 92
    of the 96 wild-50 truth decodes are slot-misaligned recordings:
    ALL their jt9 truth decodes are at dt ∈ [-2.5, -1.4]. Pancetta's
    audio buffer starts at slot t=0; signals before t=0 are outside
    the search window.

    Secondary finding (more important): `Ft8Config::time_range` is
    DEAD code. The field exists with default 2.0 but isn't threaded
    through to the spectrogram's time_padding (hardcoded to 0).
    Setting time_range=3.0 had zero effect.
  measured_delta: 0 (production unchanged)
  learning: |
    wild-50's 0/96 score is a sampling artifact (2 of 50 WAVs draw
    misaligned recordings), not a decoder limitation on the operational
    on-air corpus. Hard-200/1000 don't show this pattern because
    their curation explicitly filters for pancetta-decodable content.

    The dead `time_range` field is the bigger maintainability finding
    — same surface-vs-actual gap as hb-020's `aggressive_decoding`.
    Spawned hb-040 to either plumb it through (so misaligned
    recordings can be handled) or remove it (cleanup).
  follow_up: hb-040 (plumb or remove time_range field).
  scorecards: research/scorecards/sweep/wild50-tr-3.0.json
  journal: research/experiments/2026-05-23-wild-50-zero-overlap.md

### hb-009 — Block-score ranking vs sync-only ranking  [SHELVED 2026-05-23]
  mode: ft8
  status: shelved
  priority_score: 0.50
  outcome: |
    Hard-200 A/B with `block_score_rerank ∈ {true, false}`:
    BIT-IDENTICAL decode counts (rec=4365, novel=1210). Ranking
    affects only completion order under rayon's parallel iteration,
    which doesn't affect WHICH decodes succeed because all 300
    candidates get tried (no biting decode cap at production scale).
  measured_delta: 0 (production unchanged)
  learning: |
    Ranking knobs are pointless when the consumer is parallel +
    unfiltered. The hb-009 hypothesis pre-dated the parallel decode
    path; under serial decoding with a hard cap, ranking would
    matter. Under rayon with no biting cap, it doesn't. Future
    "ordering matters" hypotheses should first verify a binding
    consumer cap.

    Block_score computation isn't wasted — it runs but the re-rank
    is a no-op. Removing the sort would save a tiny amount of CPU
    per WAV (timing A/B was within 5% noise).
  follow_up: revisit only if hb-037 restores multi-pass with a biting decode budget.
  scorecards: research/scorecards/sweep/hard200-blockscore-{on,off}.json
  journal: research/experiments/2026-05-23-block-score-ranking.md

### hb-024 — Cross-validate novel decodes  [SHELVED 2026-05-23, strong diagnostic finding]
  mode: ft8
  status: shelved
  priority_score: 0.55
  outcome: |
    Callsign-continuity probe on hard_1000 (2433 total novels):
      continued (callsign seen elsewhere): 1572 (64.6%)
      isolated (callsign never seen):       856 (35.2%)
      malformed (no callsign extractable):    5 (0.2%)

    Continuity histogram: the 50+ bucket alone has 863 novels (35.5%)
    — callsigns seen in 50+ other WAVs' jt9 truth. Overwhelming
    evidence those are real, active stations. Conservative tally of
    "almost certainly real" novels (≥4 other appearances): 1493/2433
    = 61.4%. Likely-real (≥1 other appearance): 1572/2433 = 64.6%.
  measured_delta: |
    No production code change. Diagnostic-only.

    Recalibrates the metric interpretation: pancetta's "novel"
    decodes are mostly REAL — jt9 missed them. Adding 65% of novels
    as real lifts the operationally-useful decode count on hard_1000
    from 14126 (current main.json) to ~15700, i.e. ~1500 extra real
    decodes per 1000 hard WAVs that pancetta finds where jt9 doesn't.
  learning: |
    1. Pancetta beats jt9 by a meaningful margin on busy bands —
       not just "catching up to 50%" but genuinely finding things
       jt9 misses. ~1500 unique-pancetta-decodes per 1000 WAVs.
    2. The composite `decode_rate` metric is conservative (jt9-only)
       and undervalues pancetta's unique-find performance. Future
       hypothesis evaluation should treat novels as ~65% real.
    3. The 50+ continuity bucket is rock-solid evidence; the 0
       (isolated) bucket is the only remaining ambiguity. hb-039
       spawned to resolve it.
    4. FP-filter work (hb-014, hb-034) is still motivated but less
       urgent — worst-case precision is still ~87% even if all
       isolated novels are FPs.
  follow_up: hb-039 (resolve the 856 isolated novels via self-consistency, external lookup, or SNR/DT plausibility).
  scorecards: (n/a — probe output only)
  journal: research/experiments/2026-05-23-cross-validate-novels.md

### hb-022 — Wild-card: per-candidate SNR-adaptive LDPC iters  [SHELVED 2026-05-23]
  mode: ft8
  status: shelved
  priority_score: 0.0 (wild card)
  wild_card: true
  outcome: |
    Two A/B tests on hard-200 with adaptive iter scheduling enabled:
    - Symmetric {high=25, mid=50, low=100} by sync_score thresholds
      {>8, 4-8, <4}: -19 recovered, +13 novel, -0.0012 composite,
      +12% wall-clock. The high-SNR cut hurts.
    - Asymmetric {high=50, mid=50, low=100}: BIT-IDENTICAL decode
      counts to baseline, +15% wall-clock. The low-SNR boost finds
      zero additional truth-matched decodes.
    Both directions of adaptive scheduling are net-negative or zero.
  measured_delta: 0 (production unchanged — flag default off)
  learning: |
    1. sync_score is not a reliable BP-convergence predictor at the
       tested thresholds. score > 8 includes many candidates that
       still need 50+ iters; cutting to 25 loses decodes.
    2. BP that doesn't converge by iter 50 doesn't converge by 100
       either. The extra iters just spin without producing new
       truth-matched decodes — likely Tanner-graph cycles or
       converged-on-wrong-codeword.
    3. The hb-005 sweep already captured the LDPC-iters elbow at 50
       (going 25 → 50 added +14 recovered). Going 50 → 100 adds 0,
       so the 25→50→100 curve is sharply diminishing.
    4. OSD-2 (with parity gate ≤ 4) is the real heavy-lifting fallback
       for hard codewords. Pushing BP iters higher doesn't help
       because OSD is already catching what it can. Future "go deeper"
       work should target OSD (hb-014 parity gate, hb-034 OSD-3
       validation), not BP iters.
  follow_up: |
    None. Result is decisive. The infrastructure
    (`adaptive_ldpc_iters` config flag + per-thread 3-decoder
    dispatch + CLI flag) lands as reusable but flag-gated to off.
  scorecards: research/scorecards/sweep/hard200-adaptive-{off,on,asym}.json
  journal: research/experiments/2026-05-23-adaptive-ldpc-iters.md

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

### hb-014 — Parity gate sweep / OSD precision-recall  [GRADUATED 2026-05-23]
  mode: ft8
  status: graduated
  priority_score: 0.41
  outcome: |
    Swept Ft8Config::max_parity_errors_for_osd ∈ {0..6} on hard-200,
    verified gate=2 on hard-1000. Recall flat from gate=0 through
    gate=4 (4365 / 4366 on hard-200); novel-decode count grows
    monotonically with gate width. Production graduated from gate=4
    to gate=2: zero recall cost, -21% novels, -26% wallclock.
  measured_delta: |
    hard-200:  recovered 4365 → 4365 (=); novel 1210 → 952 (-21%)
    hard-1000: recovered 14222 → 14219 (-3, noise); novel 4019 → 3172 (-21%)
    wallclock (hard-200 single run): 331 s → 246 s (-26%)
    composite: unchanged at 0.5545 (composite ignores novels by design)
  learning: |
    1. OSD's recall contribution on jt9-derived truth is essentially
       zero — gate=0 and gate=6 yield the same recovered count. OSD
       only generates "novel" decodes (jt9 missed them) and per hb-039
       most isolated novels are likely FPs.
    2. The composite metric doesn't penalize FPs, so precision wins
       are invisible in composite. They still matter for on-air
       operation (fewer fake QSO attempts) and CPU usage.
    3. The "right" parity gate isn't a recall/precision tradeoff at
       all on hard-200 — it's a pure precision-and-speed knob with
       no downside under the current jt9-based composite.
  follow_up: hb-041 (consider gate=0 to fully disable OSD fallback).
  journal: research/experiments/2026-05-23-parity-gate-sweep.md

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

### hb-029 — Exact-format Display tests  [GRADUATED 2026-05-23, regression net]
  mode: ft8
  status: graduated
  priority_score: 0.45
  outcome: |
    Added 9 new `assert_eq!`-based unit tests in pancetta-ft8/src/message.rs
    asserting the exact `to_string()` output for every StandardMessageType
    variant (Cq, CQ-DX, Reply, ReplyWithR, Report ±, Rrr, Final73, RR73).
    Plus the 2 ReportWithR tests from hb-023, that's 11 total covering
    all 8 variants. Lib test count: 180 → 189; all pass.
  measured_delta: 0 (test-only; no production behavior change)
  learning: |
    `.contains()`-based tests catch "is something there" bugs; only
    `assert_eq!` catches "is it formatted correctly" bugs (the
    hb-023 class). ReplyWithR's "R EM48" (with space) vs ReportWithR's
    "R-12" (no space) is exactly the kind of two-conventions-in-one-
    enum confusion this guards against.
  follow_up: i3=0 / DXpedition / contest message format tests if those paths become operationally important.
  scorecard: (n/a — test-only)
  journal: research/experiments/2026-05-23-exact-format-display-tests.md

### hb-038 — Re-sweep max_sync_candidates at nms-off  [GRADUATED 2026-05-23]
  mode: ft8
  status: graduated
  priority_score: 0.50
  outcome: |
    5-tier eval at max_sync_candidates=300 (vs prior 200): composite
    +0.0023, hard-200 +40 rec / +190 novel, hard-1000 +96 rec /
    +482 novel, guards unchanged. Wall-clock per-WAV roughly doubles
    (~490 ms → ~940 ms) but stays well within the 3000 ms budget.
    The hb-003 elbow shifted upward after hb-019 turned NMS off,
    which let pass 1 see more candidates.
  measured_delta: |
    composite: 0.5522 → 0.5545 (+0.0023)
    hard-200:  rec 4325 → 4365 (+40, +0.9%); novel 1020 → 1210 (+190)
    hard-1000: rec 14126 → 14222 (+96, +0.7%); novel 3537 → 4019 (+482)
    5-tier elapsed: 631 → 1211 s (+92%, partial undo of hb-031 speed
    win but still well within per-WAV budget; cumulative wall-clock
    since pre-run baseline is ~25% better, not regressed).
  learning: |
    1. Parameter elbows established under one production state may
       not hold under another. After every WIN that flips a
       structural knob (like hb-019 nms-off), the adjacent parameter
       sweeps are worth re-running.
    2. Per hb-024 (~65% of novels are real), the +672 novels here
       represent ~+437 real decodes, giving total ~+573 operational
       decodes per ~1200 hard WAVs.
    3. The right shape long-term might be a runtime mode (`latency`
       vs `balanced` vs `deep`) — see hb-031 disposition.
  follow_up: none new.
  scorecard: research/scorecards/history/2026-05-23-cap-300-resweep.json
  journal: research/experiments/2026-05-23-cap-300-resweep.md

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
