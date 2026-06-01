# Hypothesis Bank

# Engineering substance verified 2026-06-02 — see
# docs/engineering/2026-06-02-engineering-substance-audit.md
# Honesty re-labels applied 2026-06-02 (Phase A) — GRADUATED entries
# distinguish behavioral graduations (composite-moving) from
# SHIPPED-INFRA (no decoder change) and SESSION-N-COMPLETE (binding
# A/B pending). Bootstrap-CI policy: see
# research/experiments/2026-06-01-phase-b-bootstrap-ci.md.

last_updated: 2026-06-01T23:45:00Z
current_focus_mode: ft8
wild_card_ratio_target: 0.20
wild_cards_run: 4
exploitation_run: 74
current_ratio: 0.051
# Batch 16 cruft-purge (2026-06-01): bank counters as of this commit.
#   total_entries: 216
#   pending (active, title PRIORITY): 140
#   deferred:                            1   (hb-004)
#   shelved (title SHELVED/DEFINITIVELY): 45
#   graduated/win (title GRADUATED/WIN/CONDITIONAL/PARTIALLY): 30
#   (hb-133 GRADUATED 2026-06-01: saturation-aware composite + refresh-offset
#    registry; instrumentation only, no decoder change.)
#   wild_cards (wild_card: true): 33
#   closure_reminders flagged 2026-06-01: 2   (hb-108, hb-121)
#   Status drift reconciled this batch: 10 entries (hb-016, hb-018,
#     hb-021, hb-037, hb-064, hb-069, hb-072, hb-080, hb-081, hb-087)
#     — hb-064 and hb-069 had unmerged SHELVE/Session-2 commits on
#     iter branches that never landed on main; this batch ported the
#     bank-side updates from those journals into main.
#   Priorities assigned to title-PRIORITY-wild entries: 28
#   (numeric priority_score was already populated by mr-009 aggregator;
#    cruft purge propagated it into the title tag for visibility.)
# mr-009 deep ideation pass (2026-06-01): 115 new candidates spawned
#   (hb-101..hb-215) across 8 parallel sub-passes attacking different
#   assumption axes:
#     architectural (hb-101..hb-114, 14 ideas) — replace hard-decision
#       pipeline stages with distributional output
#     diversity    (hb-115..hb-128, 14 ideas) — second-measurement
#       sources (space, decoder, frequency, time, polarization)
#     metric       (hb-129..hb-143, 15 ideas) — alternative composite
#       axes (TTFD, op-value, QSO-completion, PR-AUC, end-to-end)
#     corpus       (hb-144..hb-157, 14 ideas) — alternative truth sets
#       and stress tiers (consensus, adversarial, jt9-only, etc.)
#     human-loop   (hb-158..hb-172, 15 ideas) — operator HITL feedback
#       channels (decode-confirm, STOP, alarms, post-session review)
#     cross-time   (hb-173..hb-186, 14 ideas) — within-QSO,
#       within-session, cross-session, propagation, sun-cycle state
#     foundation-models (hb-187..hb-201, 15 ideas) — Wav2Vec2,
#       Whisper, diffusion, GNN-BP, deep ensembles, RF-FMs
#     extras       (hb-202..hb-215, 14 ideas) — CAT-driven NB, TX-jitter,
#       SDR-IQ, SIMD-BP, Wiener filter, WSJT-X plugin, log-as-truth
#   See research/experiments/2026-06-01-mr-009-deep-ideation.md for
#   full per-category summaries, cross-category synthesis, and the
#   TOP-10 overall priority ranking. Cross-cutting infra needs:
#   chronological eval tier, shared CrossTimeState, jtdx baseline,
#   saturation-aware composite (hb-133 is the unblocking lever).
# mr-008 ideation pass (2026-05-31): 12 new candidates spawned
#   (hb-089..hb-100) after the 5-shelve session closed five mechanism
#   families (soft cancellation, sync relaxation, OSD-without-Costas,
#   AP-on-residual, score-NMS, neural-OSD-small-corpus). 3 rejected at
#   generation by mr-007 (documented in ideation journal as anti-pattern).
#   Top-5 attackable post-ideation: hb-093, hb-048 a7, hb-089, hb-064 S3,
#   hb-091. Bank shape diversified across 5 open territories (residual
#   quality, precision/throughput, signal class, ML/learned bounded,
#   operational). See research/experiments/2026-05-31-mr-008-ideation.md.
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

### hb-037 — Redesign or remove subtract_with_sidelobes  [SHELVED 2026-05-23 — superseded by hb-031]
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

### hb-048 — AP type 7 (a7) cross-correlation against decoded callsigns  [SHELVED 2026-06-02 — within-WAV mechanism does not surface truths]
  mode: ft8
  status: SHELVED — within-WAV path. Cross-slot path deferred (needs chronological corpus or live trace).
  priority_score: 0.0
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
  status_2026_05_31_session1: |
    Session 1 (design-only) COMPLETE. Spec at
    docs/superpowers/specs/2026-05-31-hb-048-a7-design.md.
    Key decisions:
      * Cross-slot state (prev_slot_calls) lives in Ft8Decoder, not
        coordinator — local feedback loops already live there.
      * Mechanism is structurally different from existing AP1-AP6:
        template cross-correlation REPLACES Costas pre-gate; existing
        AP only re-decodes Costas-admitted candidates. hb-051's
        AP-recovery ceiling does NOT bound a7.
      * Thresholds start at WSJT-X reference (snr7 ≥ 6.0, snr7b ≥ 1.8)
        but will need a wide Session-3 sweep — pancetta's FFT scaling
        may differ from WSJT-X's correlation power normalization.
      * Per-decode template cap of 32 (down from WSJT-X's 206) is the
        first-cut CPU mitigation; source-decode cap of 8 (top-K by SNR).
      * Eval-harness limitation: shuffled-WAV corpus undercounts the
        cross-WAV (production-only) yield. Session 3 will measure
        within-WAV effect only; document the gap.
    Session 2 = template generator + cross-corr primitive + 10 unit
    tests, gated, no production hook. Session 3 = production wiring +
    threshold sweep + A/B eval. Priority stays at 0.45 (active).
    Branch: iter/2026-05-31-hb-048-session1, 1 commit (docs only).
  status_2026_06_01_session2: |
    Session 2 COMPLETE — SESSION-3-PENDING. The "GRADUATES" framing was
    too loose: Session 2 shipped the cross-correlation primitive +
    synthetic-injection verification; the binding hard-200 A/B (Session 3)
    that flips a production default is what would justify GRADUATED.
    Per Phase A honesty pass (2026-06-02), re-labeled as
    SESSION-2-COMPLETE-SESSION-3-PENDING. Both deliverables landed:
      * pancetta-ft8/src/a7.rs (~640 LOC + 15 tests) — A7ExpectedCall,
        A7Template, A7TemplateKind, generate_templates (up to 32 per
        call), cross_correlate (snr7 in LLR domain), best_template_score
        (snr7 + snr7b), dedup_against_previous (the f0=-98 analog).
      * pancetta-research/examples/hb048_a7_synthetic_injection.rs —
        encodes truth `K1ABC W1AW RR73`, generates 22 templates for
        K1ABC, runs noise sweep against the matching template.
    Test count: 15/15 a7 unit tests pass; full pancetta-ft8 --lib
    suite still 219/219 (zero regression).
    Synthetic-injection result (signal_mag=5.0 LLR units, mid-band
    noise_std=3.0, lin SNR +4.4 dB):
      * snr7 = 65.38  (WSJT-X threshold 6.0)
      * snr7b = 1.83  (WSJT-X threshold 1.8)
      * best-template correctly identified in 5/5 trials
      * Match-correct holds across the full sweep (noise_std up to 12,
        lin SNR -7.6 dB).
    Observation for Session 3: snr7b is the tight metric — sits at 1.85
    even with clean LLRs because the bank has structurally-similar
    templates (RRR/RR73/73 share callsign-pair bits). Session 3's
    threshold sweep should map snr7b ∈ {1.5, 1.8, 2.2} carefully.
    Branch: iter/2026-06-01-hb-048-session2, 3 commits.
    Next: Session 3 = wire a7_cross_correlation_pass into
    decode_window_with_ap after V1 joint-pair-retry; threshold sweep on
    hard-200 + synth + fixtures; A/B vs main.
  status_2026_06_02_session3: |
    Session 3 COMPLETE — SHELVED (within-WAV path).
    Production wiring landed: `a7_cross_correlation_pass` invoked in
    `decode_window_with_ap` after V1 joint-pair-retry. 4 new
    `Ft8Config` fields (`a7_enabled` default false; thresholds default
    to WSJT-X 6.0 / 1.8 / 6.25 Hz). 4 new eval CLI flags
    (`--a7-enabled`, `--a7-snr7-threshold`, etc.).
    Sweep on hard-200 (FP-filter on, 6 settings + baseline):
      * snr7=6.0  snr7b=1.8 (WSJT-X ref):   rec Δ=+0   novel Δ=+382
      * snr7=5.0  snr7b=1.5 (most loose):   rec Δ=+1   novel Δ=+727
      * snr7=5.5  snr7b=1.8:                rec Δ=+0   novel Δ=+390
      * snr7=6.0  snr7b=2.2:                rec Δ=+0   novel Δ=+214
      * snr7=6.5  snr7b=2.2:                rec Δ=+0   novel Δ=+212
      * snr7=7.0  snr7b=1.8:                rec Δ=+0   novel Δ=+369
    Bootstrap CI (n=1000, seed=0xb007):
      * smoke: rec CI [+0.0, +0.0] — NOT significant; novel CI
        [+342, +421] — significant FP.
      * most permissive: rec CI [+0.0, +3.0] — NOT significant;
        novel CI [+664, +783] — significant FP.
    SHELVE-definitive (per design spec): hard-200 rec < +5 across the
    full sweep AND no plausible parameter set survives.
    Why the within-WAV path failed where Session 2's synthetic-injection
    micro-test passed: Session 2 verified primitive correctness against
    a known-injected truth; Session 3 measures the production case
    where a7's template bank is rooted at callsigns decoded IN THE
    CURRENT WAV — but the residual at nearby `sync_candidate`s holds
    OTHER stations' transmissions, not the rooted callsign's
    follow-up. WSJT-X's a7 only works cross-slot (slot N's decode
    seeds slot N+1's templates), which the offline eval-harness
    cannot simulate (each WAV is independent).
    `a7_enabled` stays default-false in production. The within-WAV
    path is SHELVED.
    DEFERRED (cross-slot follow-up): the CrossTimeState bridge that
    would seed `prev_slot_calls` from a previous slot's decodes
    remains buildable; testing requires either a chronological-slot
    synthetic corpus or a live production trace. NOT a separate
    hypothesis — shares all Session 2 plumbing. To revisit when:
    (a) chronological-slot corpus exists, (b) production needs to be
    measured live on an FT8 station with a real cross-slot QSO. Until
    then, `a7_enabled = false` is correct.
    Branch: iter/2026-06-02-hb-048-session3, 3 commits.

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

### hb-056 — Cross-cycle non-coherent symbol averaging  [GRADUATED 2026-05-25]
  mode: ft8
  status: GRADUATED — cross_cycle_averaging default true; composite +0.000816 (non-coherent variant)
  priority_score: 0.0
  estimated_effort: 2-3 sessions (delivered in 1 — simpler than the bank entry assumed)
  expected_delta: CONFIRMED non-coherent — hard-200 +14 rec / +8 novel filtered; hard-1000 +82 rec / +48 novel filtered
  defensible_prior: yes — JTDX (`lib/ft8b.f90`, subpasses isubp1={4,7,10})
  wild_card: false
  evidence_for:
    - Full 5-tier (FP filter on): composite 0.556180 → 0.556996 (+0.000816). fixtures 1.0 unchanged; synth-clean @50 -20 unchanged; @90 nudged -18→-20 (single-slot, no-op pass, noise). hard-200 rec 4394→4408, hard-1000 rec 14355→14437; novels +8 / +48 filtered (FP filter absorbs ~87% of raw FP cost).
    - Targeted hard-200 four-way A/B: ctrl-nofilter 4395/1552 vs variant-nofilter 4409/1613 (Δrec +14, Δnovel +61); ctrl-filter 4394/836 vs variant-filter 4408/844 (Δrec +14, Δnovel +8). Recall is filter-invariant; novels addressed.
  evidence_against (resolved):
    - "Needs new corpus" — REFUTED. The 90s multi-slot recordings (batch-11 hb-012 finding) already contain repeats; cross-cycle averaging works entirely within one decode_window call.
    - "200-400 LOC + coordinator state machine" — REVISED. Single-decode-window scope means no coordinator handoff and no new module: ~120 LOC of grouping + linear-power summation + a pass method.
    - "Pancetta has no complex-spectrogram path" — INTRINSIC. Confirmed pancetta can only do NON-coherent integration; the +0.000816 bounds that ceiling. The coherent variant (retain phase) remains hb-074.
  notes: |
    See research/experiments/2026-05-25-cross-cycle-averaging.md +
    docs/superpowers/specs/2026-05-25-cross-cycle-averaging-design.md.
    Spawned hb-074 (coherent / complex-spectrogram rework) as the path
    to JTDX's full headline gain, now defensibly motivated against a
    measured non-coherent baseline.

### hb-074 — Complex-spectrogram coherent cross-cycle averaging  [SHELVED 2026-05-26 — infrastructure kept flag-gated]
  mode: ft8
  status: SHELVED — coherent loses 10 hard-200 recovered vs non-coherent. Phase-estimate noise on marginal candidates raises sum variance; inter-slot phase isn't reliably preserved in real-world audio. Math is correct (unit test 3 dB at N=2); the gain doesn't transfer to the operator corpus.
  priority_score: 0.0
  estimated_effort: 3-5 sessions (large structural change to the spectrogram)
  expected_delta: ~2-3× the non-coherent hb-056 gain (i.e. ~+0.0016-0.0024 composite) if JTDX-class coherent integration carries over; uncertain
  defensible_prior: yes — JTDX's headline sensitivity edge IS the coherent variant; pancetta now has a measured non-coherent baseline (+0.000816) to compare against.
  wild_card: false
  evidence_for:
    - Theoretically: coherent integration improves SNR ~3 dB per doubling vs ~1.5 dB non-coherent, so the gain should be roughly double for N=2 and grow with N.
    - hb-056 graduation proves the cross-cycle MECHANISM works on pancetta's corpus; the limit is the phase-discarded spectrogram, which is a structural property the rework would fix.
  evidence_against:
    - Touches the spectrogram hot path (memory + compute) — careful to not regress wall-clock or break the LLR pipeline.
    - Higher implementation risk than hb-056; the spectrogram is consumed everywhere.
  notes: |
    Approach sketch: extend Spectrogram::power from f64 to Complex<f64>
    (or add a parallel `phase` array), preserve current dB-power view as
    a derived projection, and add a coherent variant of
    sum_tone_magnitudes_linear that sums complex amplitudes (with a
    phase-rotation correction for the inter-slot phase shift). LLR path
    can keep using the power projection; only the cross-cycle pass uses
    the complex view. Eval the same way as hb-056 (4-way hard-200 A/B
    + full 5-tier). Schedule only when there's appetite for a multi-
    session structural rework.

    SHELVE outcome 2026-05-26: implemented end-to-end (Spectrogram::complex,
    par_extract_complex_symbols_from_spectrogram, estimate_candidate_phase_rotor,
    coherent_sum_complex_to_db, --cross-cycle-coherent flag). Unit test
    test_coherent_phase_rotor_and_gain confirms 3 dB N=2 gain on aligned
    synthetics. hard-200 A/B (vs production non-coherent): -10 recovered
    both no-filter and filtered. Diagnosis: noisy phase estimates on the
    marginal candidates we're trying to rescue raise sum variance; inter-
    slot phase not preserved in real-world TX/RX/propagation. See
    research/experiments/2026-05-26-hb-074-coherent-cross-cycle.md.
    Infrastructure kept flag-gated (default off). Spawned hb-075/076/077.

### hb-075 — Phase-magnitude-weighted coherent cross-cycle sum  [GRADUATED 2026-05-26 — biggest single-iter win of the session] (verified pre-CI: may need re-validation)
  mode: ft8
  # Phase A bootstrap-CI retrofit (2026-06-02): +22 hard-200 rec and
  # +78 hard-1000 rec are above the marginal range Phase B's smoke
  # test flagged as potentially within noise. Mechanism (MRC) has
  # canonical primary-source backing. Bootstrap CI per Phase B was
  # not run at graduation time but the delta magnitude makes
  # post-hoc re-validation low priority.
  bootstrap_ci_status: VERIFIED-PRE-CI (large enough delta + MRC has primary-source backing; post-hoc bootstrap CI optional)
  status: GRADUATED — cross_cycle_coherent_mrc default true; composite +0.001283; hard-200 +22 rec / +1 novel; hard-1000 +78 rec / -7 novel. MRC weighting flips the hb-074 sign exactly as the failure-mode diagnosis predicted.
  priority_score: 0.0
  estimated_effort: 1 session (builds on hb-074 infra)
  expected_delta: bounded loss vs non-coherent — and possibly a small win if it discounts noisy-rotor members enough; uncertain
  defensible_prior: yes (paper-standard MRC-style weighting addresses hb-074's exact failure mode)
  wild_card: false
  evidence_for:
    - hb-074 showed that bad phase estimates on marginal members raise variance and drop recall. Weighting each member's contribution by the magnitude of its un-normalised Costas accumulator (`|Σ cs[costas][expected_tone]|` before unit-magnitude division) is the canonical MRC fix: strong rotors count fully, weak rotors count weakly.
    - The hb-074 infrastructure already computes the un-normalised accumulator; only the sum step changes.
  evidence_against:
    - If even the strong-rotor members have unreliable inter-slot phase (hb-074's diagnosis #2), this won't recover the gain — just bounds the loss.
  notes: |
    Replace `acc / mag` in estimate_candidate_phase_rotor with returning
    both the rotor and `mag`; then in coherent_sum_complex_to_db, weight
    each member by its rotor magnitude. Eval as 4-way A/B vs both
    non-coherent and the unweighted hb-074 baseline.

### hb-076 — Per-Costas-block phase recovery  [PRIORITY: 0.20, spawned 2026-05-26 from hb-074]
  mode: ft8
  status: pending — DOWNGRADED post-hb-075. The global rotor + MRC weighting (hb-075) handles the marginal-rotor variance issue without per-block phase modeling. hb-076 still might rescue an additional sliver for genuinely drift-dominated cases, but the headroom is small.
  priority_score: 0.20
  estimated_effort: 1 session (builds on hb-074 infra)
  expected_delta: targets per-slot phase drift not captured by a single global rotor; bounded
  defensible_prior: partial — 3 Costas blocks per slot give independent phase estimates that can model drift
  wild_card: false
  evidence_for:
    - hb-074's global rotor averages 21 Costas samples across the whole slot, missing intra-slot phase drift (LO drift, Doppler accumulation across the 12.64 s message).
    - Three per-block rotors (start/middle/end) let symbols rotate against their nearest block — robust against linear drift.
  evidence_against:
    - 7 samples per block estimates noisier than 21-sample global; the noise gain may swamp the drift correction at low SNR. Same per-candidate failure mode hb-074 hit.
  notes: |
    Three rotors r_start, r_mid, r_end; per-symbol rotor chosen by
    proximity to nearest Costas block. Variant: linearly interpolate
    rotors across symbol positions for smooth drift correction.

### hb-077 — Phase-coherent SDR-IQ eval corpus  [PRIORITY: 0.20, spawned 2026-05-26 from hb-074]
  mode: ft8
  status: pending (hardware/operator dependent) — DOWNGRADED post-hb-075. Real operator audio supports coherent gain after MRC weighting; the SDR-IQ corpus would mainly check whether an upper bound exists beyond MRC.
  priority_score: 0.20
  estimated_effort: 2-3 sessions (capture + manifest + truth) — operator-pending
  expected_delta: diagnostic — tests whether the binding constraint is hb-074's algorithm or pancetta's typical corpus's phase-non-coherence
  defensible_prior: yes — direct SDR-IQ capture (no audio path) guarantees phase coherence end-to-end
  wild_card: false
  notes: |
    Capture a small (10-50 WAV) corpus from a phase-coherent SDR (e.g.,
    Kiwi IQ, hackrf, RTL-SDR with cohrent reference) of a known stable
    transmitter calling repeated CQs. Re-run hb-074 (and hb-075, hb-076)
    against that corpus. If coherent wins there but loses on the operator
    corpus, the algorithm is sound and the operator corpus is the limit;
    if coherent loses everywhere, the approach is closed.

### hb-079 — Coherent iterative-subtract multi-pass  [GRADUATED 2026-05-26 — biggest single-iter composite win in project history] (verified pre-CI: may need re-validation)
  # Canonical name (Phase C, 2026-06-02): **Successive Interference
  # Cancellation (SIC)** per multi-user-detection literature
  # (Verdu 1998, *Multiuser Detection*; Patel-Holtzman 1994 IEEE
  # J. Sel. Areas Commun.). Pancetta's "coherent iterative-subtract
  # multi-pass" is canonically SIC with ML-projection cancellation.
  # See docs/engineering/2026-06-02-engineering-substance-audit.md
  # (claim 15).
  mode: ft8
  # Phase A bootstrap-CI retrofit (2026-06-02): the composite delta
  # (+0.009212) and the hard-200 +158 rec / hard-1000 +401 rec are
  # substantially above any plausible noise floor — well outside the
  # marginal range Phase B's bootstrap CI calls "NOT significant".
  # Listed as "verified pre-CI" because the actual bootstrap CI per
  # Phase B was not run at graduation time, but the magnitude makes
  # post-hoc re-validation low priority. See
  # research/experiments/2026-06-01-phase-b-bootstrap-ci.md.
  bootstrap_ci_status: VERIFIED-PRE-CI (large delta well above marginal range; post-hoc bootstrap CI optional)
  status: GRADUATED — `coherent_multipass` default true. Composite +0.009212 (0.558279 → 0.567491), ~7× the prior biggest single iter. hard-200 +158 rec / +75 novel; hard-1000 +401 rec / +132 novel. The recall ceiling on hard-* was interference, not threshold; the ML projection in the complex spectrogram is the right kernel for coherent subtraction.
  priority_score: 0.0
  estimated_effort: 2-3 sessions (delivered in 1)
  expected_delta: CONFIRMED massively
  defensible_prior: built on hb-075's complex spectrogram + ML projection canonical math
  wild_card: false
  evidence_for:
    - Targeted hard-200 A/B (vs hb-075 prod): no-filter +158 rec / +127 novel; with filter +158 rec / +78 novel. Real:novel ratio after filter ~2:1.
    - Full 5-tier: +401 rec on hard-1000, fixtures + synth preserved exactly, wall-clock +14% (within budget). Composite +0.009212.
    - hb-030 closed the dB-domain kernel; ML projection in complex domain (residual ⊥ rotor, |signal_est|²+|residual|²=|bin|² by orthogonality) is the canonical fix.
  notes: |
    Implementation: reverse_derive_candidate from DecodedMessage (we don't
    keep candidates paired with msgs); subtract_decode_coherent does ML
    projection at each (sym, true_tone) × 2 substeps and refreshes the
    dB power view; coherent_subtract_and_repass orchestrates subtract +
    residual sync_search + decode of new candidates. Sequential decode
    of new candidates after subtract (small count). Lib tests 196→197
    with test_coherent_subtract_ml_projection (orthogonal-decomposition
    invariant). See
    research/experiments/2026-05-26-hb-079-coherent-multipass.md.
    Spawned hb-080 (N>2 passes), hb-081 (MRC-weighted subtract),
    hb-082 (residual-tier sync threshold).

### hb-080 — Iterative-subtract: N>2 passes  [GRADUATED 2026-05-27 — batch 13] (verified pre-CI: may need re-validation)
  # Canonical name (Phase C, 2026-06-02): multi-stage SIC (Successive
  # Interference Cancellation, N passes). See hb-079 cross-ref.
  # Phase A bootstrap-CI retrofit (2026-06-02): +16 hard-200 rec is in
  # the marginal range Phase B's smoke test flagged as potentially
  # within noise; the N=2/3/4/5 sweep monotonicity is the
  # corroborating signal that supports the graduation, but a binding
  # bootstrap CI per Phase B was not run at graduation time.
  bootstrap_ci_status: PENDING (pre-Phase-B graduation; +16 hard-200 rec is in marginal range — sweep monotonicity is corroborating but a binding CI was not computed)
  status_2026_05_27: GRADUATED — `coherent_multipass_iterations` default 1→3. hard-200 sweep N∈{1,2,3,4,5}: N=2 +7 rec, N=3 +9 rec (+16 total vs N=1), N=4/5 saturate. ZERO novel cost across the sweep. Wall-clock 1.78× N=1, within budget. Composite +~0.000935 from hard-200 alone. Tertiary masking is real but saturates at three rounds; deeper signal masking is the joint-decoding territory (hb-086).
  ---- original priority below ----
  [PRIORITY-WAS: 0.45, spawned 2026-05-26 from hb-079]
  mode: ft8
  status: GRADUATED 2026-05-27
  priority_score: 0.0
  estimated_effort: 1 session
  expected_delta: +20-50 hard-200 recovered (additional masked signals revealed in residual-of-residual)
  defensible_prior: yes — hb-079 confirmed pass-2 finds +158; some signals may be tertiary-masked
  wild_card: false
  evidence_for:
    - hb-079 only does pass 1 + pass 2 (subtract once, redecode once). Multi-stage interference is real on the busy bands (a signal masked by both A and B might surface after subtracting A but stay masked by B until B's pass-2 decode subtracts).
    - Implementation: wrap the existing coherent_subtract_and_repass in a loop that runs N times or until no new decodes appear. Maybe `coherent_multipass_iterations: usize` (default 2, sweep {2, 3, 4}).
  evidence_against:
    - Diminishing returns — each pass finds fewer signals than the previous. Pass 3 might add 0-20.
    - Cumulative subtract error compounds; bad pass-1 decodes (rare but real) could damage the residual further.
  notes: |
    The cleanest implementation: change config to `coherent_multipass_iterations: usize` (default 2 = current behavior), loop subtract_and_repass. Sweep {2, 3, 4} on hard-200 to find the elbow.

### hb-081 — MRC-weighted coherent subtract  [SHELVED 2026-05-27 — batch 13]
  # Canonical name (Phase C, 2026-06-02): MRC-weighted "soft SIC"
  # (rotor-confidence-weighted Successive Interference Cancellation).
  # The shelve finding is "the existing full-amplitude SIC is already
  # optimal on this corpus; soft-SIC under-subtracts and leaves
  # residual signal at decoded positions, blocking multipass." See
  # hb-079 SIC cross-ref and docs/engineering/2026-06-02-engineering-substance-audit.md
  # (claim 15: "hb-081 = soft SIC / soft cancellation").
  status_2026_05_27: SHELVED — hard-200 sweep at MRC threshold ∈ {5, 10, 20, 40} all regress -170 to -173 recovered vs full ML subtract (threshold=0). The assumed failure mode (over-subtract from noisy rotors) wasn't real: hb-080 confirms full subtract gives +0 novel cost. *Under-subtracting* instead leaves residual signal energy at decoded positions, which BLOCKS multipass from finding masked candidates. The mechanism's already optimal. Library + CLI flag stay available for re-evaluation if the corpus changes.
  ---- original priority below ----
  [PRIORITY-WAS: 0.40, spawned 2026-05-26 from hb-079]
  mode: ft8
  status: SHELVED 2026-05-27
  priority_score: 0.0
  estimated_effort: 1 session
  expected_delta: small bounded improvement — protects adjacent bins from over-subtraction by weak-rotor decodes
  defensible_prior: yes — direct analogue of hb-075's MRC fix for cross-cycle. The same noisy-rotor problem could over- or under-estimate subtract magnitude.
  wild_card: false
  evidence_for:
    - hb-079 currently subtracts at full ML-projection amplitude regardless of how confident the rotor is. For weak-rotor pass-1 decodes (low-SNR borderline), the projection includes noise variance — subtract amplitude can be wrong by a factor ~|rotor noise| / |signal|.
    - Weight subtraction magnitude by rotor confidence (|acc|): strong-rotor decodes subtract at ~full amplitude, weak-rotor decodes subtract less. Protects adjacent bins.
  notes: |
    Implementation: in subtract_decode_coherent, scale the subtract amount by min(1.0, |acc|/threshold). Adds one parameter (rotor_confidence_threshold).

### hb-082 — Residual-tier sync threshold tuning  [SHELVED 2026-05-27 — batch 13]
  status_2026_05_27: SHELVED — hard-200 sweep at residual threshold ∈ {2.0, 2.5, 3.5} produced ZERO change at every threshold vs production (3.0). The candidates that surface in the residual naturally cluster above 3.0; the threshold isn't binding. Plumbing left in place (residual_min_sync_score: Option<f64>) for future use if the corpus changes.
  ---- original priority below ----
  [PRIORITY-WAS: 0.30, spawned 2026-05-26 from hb-079]

### hb-085 — Cross-cycle on residual  [SHELVED 2026-05-27 — design analysis, batch 13]
  status: SHELVED before implementation — structurally redundant. After hb-079's subtract, the original signal positions are near-zero (averaging with zero dilutes) and residual-revealed candidates aren't at the same `(freq_sub, freq_bin, t0 mod slot)` as any original repeating-station group (no peer to average with). The cross-cycle integration that would help — coherent subtract of the masking signal then cross-cycle on the now-unmasked positions — is what hb-079 already does implicitly. See research/experiments/2026-05-27-hb-085-cross-cycle-on-residual.md.
  priority_score: 0.0

### hb-086 — Joint multi-candidate decoding V1 (force-retry on residual)  [GRADUATED 2026-05-28] (verified pre-CI: may need re-validation)
  mode: ft8
  # Phase A bootstrap-CI retrofit (2026-06-02): +12 hard-200 rec is in
  # the marginal range Phase B's smoke test flagged as potentially
  # within noise (the +3 hard-200 case landed at [-6, +12] CI). The
  # diagnostic-first kill switch (78.3% pair-likely vs 30% threshold)
  # is the corroborating evidence that supports the graduation, but a
  # binding bootstrap CI per Phase B was not run at graduation time.
  # See research/experiments/2026-06-01-phase-b-bootstrap-ci.md.
  bootstrap_ci_status: PENDING (pre-Phase-B graduation; +12 hard-200 rec in marginal range; diagnostic kill switch is corroborating but a binding CI was not computed)
  status_2026_05_28: V1 GRADUATED — `joint_pair_retry` default false→true. Diagnostic confirmed 78.3% pair-likely vs 30% threshold on top-20 worst hard-200 WAVs; PROCEED earned. V1 = force-retry original sync candidates against the (post-multipass) residual spectrogram, bypassing the residual sync_score threshold. hard-200 +12 rec / +1 novel; hard-1000 +17 rec / +9 novel; composite +0.000700 (0.568424 → 0.569123); fixtures + synth preserved; elapsed +2.2%. See research/experiments/2026-05-28-hb-086-joint-pair-retry-v1.md.
  priority_score: 0.0

### hb-086 V2 — Joint LLR with iterative interference cancellation  [SHELVED across THREE corpora (May + refreshed hard-200 + synth-pair-200); synth_pair_retested 2026-06-02]
  # Phase A honesty pass (2026-06-02): "DEFINITIVELY SHELVED" replaced
  # with "SHELVED across two corpora (May + refreshed) — re-test gate:
  # new signal class or new mechanism variant". The hb-146 synth-pair
  # corpus WAS the new-signal-class re-test gate; it was executed
  # 2026-06-02 and produced the same 0% finding → SHELVE confirmed
  # across three corpora.
  mode: ft8
  status_2026_05_30: SHELVED at the diagnostic gate (OLD hard-200 top-20). 0% marginal-SNR neighbors; multi-neighbor count 14.8% strict / 34.8% relaxed.
  status_2026_05_31_recheck: SHELVED across two corpora (May + refreshed). Re-ran the diagnostic on the REFRESHED hard-200 top-20 (100 of the 200 entries are today's K5ARH 20m captures — denser, with 9/20 sample slots reaching jt9's -25 dB SNR floor per survey). Result essentially identical: multi-neighbor 16.7% strict / 33.8% relaxed, **STILL 0% marginal-SNR neighbors** (p10 -5.1 dB, median -1.6 dB, nothing below -15 dB on either window). The pattern is corpus-independent for the *decoded-neighbor* configuration tested here. Re-test gate: new signal class (hb-146 synth-pair-200 ships the configuration V2 is designed for) or a fundamentally new mechanism variant. Phase A honesty pass (2026-06-02) replaced "DEFINITIVELY SHELVED" with this two-corpus phrasing.
  status_2026_06_02_synth_pair_retest: SHELVED on 3rd corpus. Re-ran the diagnostic against the full hb-146 synth-pair-200 (180 WAVs, 88 weak-missed = V1-failed-proxy population). **Multi-neighbor density jumped (14.8% / 16.7% → 45.3% strict and 34.8% / 33.8% → 47.1% relaxed) confirming the corpus DID produce the configuration the V2 design point promised — but marginal-SNR fraction stayed pinned at 0.0% / 0.0%**, and the neighbor-SNR distribution stayed close to hard-200 (median -2.9 dB synth-pair vs -1.5 / -1.6 dB hard-200; p10 -5.3 dB synth-pair vs -5.7 / -5.1 dB hard-200; 0 / 144 strict and 0 / 153 relaxed neighbor samples below -15 dB). The structural finding the 2026-05-31 journal predicted is now empirically confirmed: pancetta's *decoded* neighbors are uniformly high-confidence regardless of corpus, because the population pancetta decodes is upstream-gated by CRC, which selects for sharp LLRs. V2's soft-cancellation mechanism is closed across all three available signal classes. No Session 2 implementation. See `research/experiments/2026-06-02-hb086-v2-synth-pair-retest.md` and diagnostic example `pancetta-research/examples/hb086_v2_synth_pair_retest.rs`.
  **Structural insight (now confirmed across THREE corpora):** pancetta's *decoded* neighbors are uniformly high-confidence, even when the *band* has marginal-SNR truths AND even when the corpus is constructed by-hand to contain marginal-SNR weak truths alongside strong decoded neighbors. When pancetta decodes a neighbor it's because the decode passed CRC, which selects for sharp LLRs → delta-function tone posteriors → soft cancellation collapses to hard subtraction. This is a structural property of pancetta's decoder architecture (hard-decision pipeline through LDPC + CRC), not the corpus. The soft-cancellation family (hb-086 V2, hb-081 per-decode subtract scaling) is structurally closed against pancetta's *decoded* neighborhood across all available signal classes. Hypotheses targeting *missed* truths via different mechanisms (V3 sync-relaxation, hb-064 OSD pruning) are unaffected by this insight. The next re-test gate is no longer "find a corpus" — it is "a fundamentally new mechanism variant" that extracts LLR-equivalent information about a missed weak truth from *the residual at the truth's expected location*, not from a decoded-neighbor's posterior distribution.
  synth_pair_retested: 2026-06-02  # executed the re-test gate; 0% marginal-SNR on synth-pair-200 (same as hard-200). The synth_pair_revisit_candidate flag was retired into this terminal state.
  priority_score: 0.0  # shelved on real-audio (hard-200 / hard-1000) across May + refreshed corpora AND adversarial synth-pair-200; closed against current pancetta pipeline pending a fundamentally new mechanism variant

### hb-086 V3 — Subtract-aware sync threshold relaxation  [SHELVED on real-audio; synth_pair_revisit_candidate 2026-06-01]
  mode: ft8
  status_2026_05_31: SHELVED. Geometric kill-switch (`hb086_v3_subtract_window_potential.rs`) PROCEED'd at 56.8% of V1-uncoverable truths sitting within ±8 bins of a subtracted decode (top-20 hard-200), well above the 20% gate. Implementation landed cleanly (`Ft8Config::joint_residual_sync_relax_db` + `joint_residual_sync_window_bins`; `joint_residual_localized_sync_pass`; `localized_costas_sync_search`). **Sweep at {-0.5, -1.0, -1.5, -2.0} on hard-200 produced 0 additional decoded messages at every threshold.** Mechanism trace (`hb086_v3_trace.rs`) on top-3 worst-WAVs: V3 surfaces 100-131 truly-new (non-collision) candidates per WAV in the targeted window — but they are noise. LDPC "decodes" all of them (BP always converges at production iteration count), CRC catches ~98% as false positives, plausibility rejects the remaining 1-4 per WAV. The residual at sub-3.0 sync_score in the targeted window is *noise*, not weak signal. Plumbing kept at default-off; the hook in `decode_window_with_ap` is one `if relax_db < 0.0` check. See `research/experiments/2026-05-31-hb-086-v3-subtract-aware-sync.md`.

  **Structural insight (the keep)**: geometric proximity diagnostics need a paired *decodability* sub-test before earning PROCEED. The V3 diagnostic measured "where truths exist relative to subtracted bins" (geometric); the V2 diagnostic measured "are neighbors decodable in a soft-relevant way" (decodability proxy via SNR). V2's pre-impl SHELVE was the right call; V3's PROCEED on geometry-only led to a wasted implementation pass. For any future mechanism whose value depends on decoding new candidates surfaced from a perturbation of the production pipeline, the kill-switch needs to extract LLRs at the truth's coordinates from the residual and confirm LDPC+CRC pass — not just check geometric proximity.

  **The hb-086 family is closed on current corpora (real-audio hard-200 / hard-1000 + adversarial synth-pair-200 for V2) + the decoder + implementations tested.** V1 GRADUATED (+12 hard-200), V2 SHELVED across THREE corpora (soft cancellation collapsed to hard subtraction on May + refreshed real-audio AND on adversarial synth-pair-200 — 2026-06-02 retest produced 0% marginal-SNR fraction despite multi-neighbor density jumping from ~15% to ~45%, confirming the closure is structural to pancetta's CRC-gated decode pool rather than corpus-dependent), V3 SHELVED (Costas relaxation surfaced noise on hard-200; hb-146 remains a re-test gate for V3 specifically — V3's mechanism is unaffected by V2's closure). The joint-decoding implementations tested to date show closure on this signal class; the design space documented in `docs/superpowers/specs/2026-05-27-joint-decoding-design.md` is exhausted ON CURRENT CORPORA WITH THE IMPLEMENTATIONS TRIED. The remaining hard-200 wall is for a different family of mechanism: sub-Costas-threshold weak signals (would need AP without sync, callsign-priors-on-residual, or OSD-without-Costas-pre-gate to crack).

  synth_pair_revisit_candidate: true  # hb-146 (2026-06-01) shipped synth-pair-200 with by-construction marginal-SNR weak signals beside strongly-decoded neighbors. V3's hard-200 trace showed it surfaces *noise* in sub-Costas windows because hard-200 has no real weak signal there; on synth-pair-200, the weak truth IS present in the targeted window by construction. Re-eval V3's sweep ({-0.5, -1.0, -1.5, -2.0 dB} × ±{4,8,16} bins) against synth-pair-200 in a future iter. If the relaxed Costas pass at the truth's exact freq_bin extracts LDPC+CRC-passable LLRs in the 0% buckets (ΔSNR ≥ 9 dB AND Δf ≤ 12 Hz), the V3 mechanism is validated as "correct mechanism, wrong corpus" rather than "wrong mechanism." Graduation still requires hard-200 co-improvement; synth-pair-200 is diagnostic-only.

  priority_score: 0.0  # shelved on real-audio; synth-pair re-eval is a separate experiment
  ---- original priority below ----
  [PRIORITY-WAS: 0.30, spawned 2026-05-30 from hb-086 V2 shelve]

### hb-057 — Median-filter DT averaging for sync/AP  [PRIORITY: 0.40, V1 SHELVED 2026-06-01, mechanism PROCEED at population level]
  status_2026_06_01_session2: V1 SHELVED. Implementation landed on `iter/2026-06-02-hb-057-session2` (commit 02cf384): per-callsign median+IQR DT history via `pancetta-ft8::dt_history` (`DtPrior` + `DtPriorLookup` trait + `InMemoryDtHistory` reference impl, 5 unit tests pass) wired into `coherent_subtract_and_repass` step 3 → step 4 boundary as a residual-candidate t0-window filter. Config-gated (`dt_history_enabled` default false). A/B on curated-hard-200, FP filter ON: composite raw 0.279114 → 0.279058 (Δ -0.000056), rec 4942/8853 → 4941/8853 (Δ -1, 95% CI [-3.0, +0.0]), novels 1024 → 1003 (Δ -21, 95% CI [-63.5, +0.0]) — both NOT significant per bootstrap; default-SHELVE per RUNBOOK. Methodology lesson (recorded in `research/experiments/2026-06-02-hb-057-session2.md`): the V1 hook filters residual candidates by the union of prior windows for callsigns ALREADY decoded this WAV — wrong population. The Session-1 diagnostic measured 38.6% recovery for callsigns NOT decoded yet whose prior would narrow their OWN sync; that requires either the AP path (where the candidate callsign is known) or a per-candidate-callsign sync-sweep refactor. V1 plumbing kept in place (default-off; zero production impact). Session 3 candidates: (a) AP-path hook (cheap, reuses plumbing; only fires with operator AP context), (b) per-candidate callsign-keyed residual sync (re-architect, ~2 sessions), (c) reject-not-narrow post-decode filter (out-of-V1-scope per spec). Mechanism NOT falsified — population still 38.6%, statistic still right; just need the right hook.
  status_2026_05_31_session1: SCOPED → PROCEED. Multi-pass is back (hb-079 coherent + hb-080 N=3 + hb-086 V1 joint-pair-retry all GRADUATED), so the prior is no longer latent. Diagnostic `hb057_dt_history_potential.rs` on top-20 hard-200: 38.6% of 647 missed truths are sent by callsigns with stable (<0.1s) or moderate (0.1–0.3s) cross-WAV DT variance — the TARGET population. Kill-switch (10%) cleared by 3.86×. Recoverable-by-prior upper bound at ±0.2s leave-one-out median gate = same 38.6% (the window comfortably covers stable+moderate variance by construction — gate-coverage upper bound, not LDPC-conversion estimate). Design spec at `docs/superpowers/specs/2026-05-31-hb-057-median-dt-design.md`. Storage: coordinator-level (cross-WAV history is the recovery population; per-decoder-thread design has zero access on fresh threads). Hook points: localized_costas_sync_search + AP-path sync_search ONLY (pass 1 untouched — preserves new-station discovery). Window: max(0.2s, IQR × 3) around per-callsign median; 10-sighting ring buffer, 30-min eviction. Session 2 implements + sweeps; Session 3 grad/shelve. Diagnostic-first re-run inside Session 2 confirms population before build.
  ---- original priority entry below ----
  [PRIORITY-WAS: 0.35, spawned 2026-05-25 from mr-002]
  mode: ft8
  status: pending (needs minor plumbing)
  priority_score: 0.40
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
  # Phase A bootstrap-CI retrofit (2026-06-02): small-delta graduation
  # (contest-type rejection in is_plausible); composite movement small
  # enough to be within Phase B's marginal range; no bootstrap CI was
  # run at graduation time. Mechanism is correctness-oriented (false-
  # decode filter), so the binding correctness test is FP-rate on
  # contest WAVs rather than composite delta.
  bootstrap_ci_status: PENDING (pre-Phase-B graduation; small-delta. FP-filter justifies on correctness grounds, not composite — composite CI is secondary)
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

### hb-072 — Directional-CQ modifier whitelist  [GRADUATED 2026-05-26 — batch 12]
  status_2026_05_26: GRADUATED — is_plausible's Cq arm now validates the special_operation modifier against a whitelist (named directionals/programs + ≤3-char numeric + ≤3-char alpha). hard-200 no-filter: -24 novel at +0 recall. Composite unchanged; operational cold-start defense.
  ---- original priority entry below ----
  [PRIORITY-WAS: 0.30, spawned 2026-05-25 from hb-058]
  mode: ft8
  status: GRADUATED 2026-05-26
  priority_score: 0.0
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
  # Phase A bootstrap-CI retrofit (2026-06-02): cleanup-only graduation
  # ("no behavior change" by the entry's own description). Composite
  # delta is exactly zero by construction; bootstrap CI not applicable.
  bootstrap_ci_status: N/A (cleanup-only — no decoder behavior change by construction; in retrospect this is SHIPPED-INFRA-style)
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
  # Phase A bootstrap-CI retrofit (2026-06-02): cleanup-only graduation;
  # composite delta zero by construction.
  bootstrap_ci_status: N/A (cleanup-only — no decoder behavior change by construction)
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
  # Phase A bootstrap-CI retrofit (2026-06-02): composite +0.001049 is
  # at the edge of Phase B's marginal range. The mechanism (Hocevar
  # 2004 layered schedule) is mathematically sound and has external
  # primary-source backing; the binding bootstrap CI per Phase B was
  # not run at graduation time.
  bootstrap_ci_status: PENDING (pre-Phase-B graduation; small composite delta; corroborated by Hocevar 2004 primary source — re-validation is low-priority)
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

### hb-064 — DIA-augmented OSD with iteration-trajectory features  [PRIORITY: 0.42, Session 2 SHELVED 2026-05-31; Session 3 deferred]
  mode: ft8
  status_2026_05_31_session2: SHELVED — Session 2 retrained the existing DIA CNN (20K params) on the Session 1 production-config trajectory JSONL (545 OSD-recovered samples, focal loss γ=2, MPS, 60-epoch cosine schedule). Offline: model beats |LLR|-ordering baseline 5.3× on sample-level top-T recovery (0.291 vs 0.055). A/B on hard-200: composite −0.00022 (4942→4938 rec, 1970→1835 novel), elapsed −7.4% (183.0s→169.5s). Composite regression is the binding SHELVE condition; the −135 novels indicates mild OOD drift (training pool was 33 WAVs incl. only 25 hard-200 head). Defer to Session 3 (bigger corpus and/or different architecture). See commit afd5921 (iter/2026-05-31-hb-064-session2 branch, not merged) + research/experiments/2026-05-31-hb-064-dia-osd-session2.md.
  status: pending Session 3 — plan-sized (priority preserved at 0.42 since Session 2 produced a real but insufficient signal; Session 3 has clear levers).
  priority_score: 0.42
  estimated_effort: 2-3 sessions remaining (bigger training corpus, possibly new architecture)
  expected_delta: paper reports 97% TEP-enumeration reduction at SNR=2dB on CCSDS (128,64); Session 2 hit −7.4% wall-clock elapsed with a composite regression
  defensible_prior: yes (arXiv:2404.14165; pancetta already has a DIA-style neural_osd)
  wild_card: false
  evidence_for:
    - Paper trains a small neural model (~2 dense layers) on per-BP-iteration LLR trajectories (vs just final LLRs) to refine bit reliabilities; sliding-window classifier decides when to early-terminate TEP enumeration.
    - pancetta already has neural_osd.rs with DIA-style model (20K params). This refines the existing module — feature extraction changes from final-LLR to per-iteration-LLR-trajectory.
    - Strong architectural fit per mr-003 audit.
    - Session 2 (2026-05-31) confirmed the model offline beats |LLR| 5.3× on sample-level top-T recovery (0.291 vs 0.055). The paper's mechanism is real on pancetta's BP-failure population.
  evidence_against:
    - Plan-sized: requires training-data regeneration with per-iteration LLR capture; existing pipeline uses final-iter features only. [Resolved Session 1 — capture API in place.]
    - Risk of overfit to synth conditions. [Resolved Session 2 partially — trained on production-config; but 33-WAV pool is small and shows OOD on full hard-200.]
    - Session 2: composite regression −0.00022 (−135 novels) outweighs the −7.4% elapsed gain. Offline metric improvements don't transfer 1:1 to production wins.
  notes: |
    Source: arXiv:2404.14165 + companion arXiv:2307.06575.
    Session 1: BP trajectory capture API + diagnostic — see
      research/experiments/2026-05-31-hb-064-dia-osd-session1.md.
    Session 2: retrain + A/B — see
      research/experiments/2026-05-31-hb-064-dia-osd-session2.md.
    Session 3 levers (NOT YET STARTED):
      a) Scale the training corpus from 33 WAVs (25 hard-200 head) to full
         hard-200 + hard-1000. Expect ~5-10× more recovered positives, less
         OOD drift, possibly enough to flip the composite to +.
      b) Try a wider model: transformer-style bit-attention over the
         trajectory, or hand-add the 7 scalar features as a wide path
         alongside the convolutions.
      c) Train + evaluate against a different OSD operating point (e.g.
         disable parity gate, allow OSD on more BP failures) — Session 2's
         metric was confined to gate-pass samples, missing the bulk of the
         BP-failure population.

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

### hb-067 — mBP offset parameter for OSD pre-conditioning  [SHELVED 2026-05-26 — definitive close]
  mode: ft8
  status: SHELVED definitively. Batch 8 found -32 novels at offset=2.0; batch 12 re-test under post-hb-075 production gave only -11 novels at +1 noise-level recall. The intervening graduations (FP filter, contest-FP rejection, cross-cycle MRC) absorbed most of what mBP offset was filtering. Default stays 0.0; library + CLI remain available for future sweeps.
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

### hb-068 — hb-044 scaled-delta refinement (variant b @ 0.3×)  [GRADUATED 2026-05-30]
  mode: ft8
  # Phase A bootstrap-CI retrofit (2026-06-02): +5 hard-200 rec falls
  # squarely in the marginal range Phase B's smoke test flagged as
  # potentially within noise (the +3 case landed at [-6, +12] CI;
  # +5 is plausibly inside a similar CI). Corroborating signal:
  # synth-clean @90% improved -18 → -20 dB (a +2 dB pure-noise-axis
  # gain that's harder to attribute to rayon scheduling). The
  # binding bootstrap CI per Phase B was not run at graduation time.
  # See research/experiments/2026-06-01-phase-b-bootstrap-ci.md.
  bootstrap_ci_status: PENDING (pre-Phase-B graduation; +5 hard-200 rec in marginal range — synth-clean +2 dB is the corroborating signal that justified GRADUATE)
  status: GRADUATED — `sync_time_interpolation = true` + `sync_time_interp_delta_scale = 0.3`
  priority_score: 0.0
  estimated_effort: n/a
  expected_delta: confirmed +5 hard-200 rec / -7 novel; +17 hard-1000 rec / +2 novel; synth-clean snr@90% -18 → -20 dB (+2 dB); fixtures + wild-50 preserved; composite +0.000292 (0.569123 → 0.569415); elapsed +10.8%
  defensible_prior: yes (hb-044 gain was real; needed gentler application)
  wild_card: false
  outcomes: |
    Iter 2026-05-30 tested three conditional variants on hard-200 (with
    FP filter) and synth-clean:

    Variant (a) score gate — REFUTED
      gate=4.0: rec=4507 / novel=856 (-109 hard-200 — full hb-044 regression)
      gate=5.0: rec=4508 / novel=857 (-108 hard-200)
      Higher gates barely move the needle; bad refinements happen at all scores.

    Variant (b) scaled delta — WINNER
      scale=0.3: rec=4621 / novel=914 / synth snr90=-20  → +5 rec / -7 novel
      scale=0.5: rec=4616 / novel=878 / synth snr90=-20  → 0 rec / -43 novel
      scale=0.7: rec=4577 / novel=876 / synth snr90=-20  → -39 rec / -45 novel
      Monotonic in scale: smaller offset = less perturbation of correctly-
      aligned candidates while still capturing the synth-clean gain.

    Variant (c) reject large deltas — REFUTED
      max_delta=0.3: rec=4616 / novel=921 / synth snr90=-18  → no-op (rejection too aggressive)
      max_delta=0.4: rec=4616 / novel=921 / synth snr90=-18  → no-op
      Real-audio parabolic deltas are mostly > 0.4 (hitting the clamp's
      [-0.5,+0.5] edge — itself a signature of unreliable fits). Rejection
      kills the gain along with the regression.

    Decision: GRADUATE variant (b) at delta_scale=0.3 — keeps the +2 dB
    synth gain AND modestly improves hard-200 (+5 rec). Defaults flipped
    in `Ft8Config`.
  notes: |
    See research/experiments/2026-05-30-hb-068-conditional-refinement.md.
    Variant (d) was tested in batch 8 (2026-05-25) and refuted — confirmed
    failure was not sort displacement but per-candidate perturbation,
    motivating this variant scan.
    Parallel-agent run (research/scorecards/sweep/hb068-b-scale-0.25.json)
    saw scale=0.25 reach rec=4623 / novel=914 on hard-200 — +2 over our
    chosen 0.3 default. Did not validate synth at 0.25; if synth gain
    holds, a follow-up could safely tune the default down. Within
    experimental noise; 0.3 ships as the conservative choice.
    Spawned follow-ups: hb-068 finer-scale sweep ({0.2, 0.25, 0.35});
    hb-069 reconfirm (linear-power interp) may stack with this; hb-086 V2
    should be re-evaluated under scale=0.3.

    **b-0.25 refreshed-corpus retest (2026-05-31, KEEP-b-0.3):**
    Full 5-tier eval at scale=0.25 against refreshed main.json
    (research/scorecards/sweep/hb068-b-0.25-refreshed-5tier.json):
    composite 0.578776 vs main 0.579114 (Δ −0.000339); hard-200
    4936 rec / 1834 novel vs 4942 / 1970 (Δ −6 rec / −136 novel);
    hard-1000 +4 rec / +63 novel; wild-100 +4 rec / −11 novel;
    synth-clean @90 preserved at −20 dB. The corpus refresh
    (100 K5ARH 20m WAVs into hard-200) flipped the optimum back to
    0.3 — the OLD-corpus +2 was a corpus-specific artifact, not a
    fundamental b-scale property. Finer-scale sweep CLOSED: 0.3 is
    the production setting on the refreshed corpus too. Journal:
    research/experiments/2026-05-31-hb-068-b-0.25-refreshed-corpus.md.

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

### hb-069 — hb-044 interpolation in linear power space  [SHELVED 2026-06-01 — composite -0.003049 vs dB; hb-044 refinement closed on current corpus + dB/linear-power variants tested]
  status_2026_06_01: SHELVED — implemented as `Ft8Config::sync_time_interp_linear_power` flag (default false; production unchanged). A/B sweep on refreshed main.json baseline: composite 0.579114 → 0.576065 (-0.003049); hard-200 -54 rec / -34 novel; hard-1000 -94 rec / -33 novel; fixtures + synth preserved. Linear-power interpolation regresses recall and composite. dB-space interpolation stays optimal under the hb-068 b-0.3 production setting. The hb-044 refinement is closed on current corpus + the dB/linear-power variants tested (hb-044 → hb-068 graduated, hb-069 shelved); Phase A honesty pass (2026-06-02) replaced "family fully closed" with this scoped phrasing — a new mechanism variant (e.g. quadratic-fit-with-residual-check) or new corpus could unshelve. Plumbing kept for future use. See research/experiments/2026-05-31-hb-069-linear-power-interp.md and commit d04c596.
  ---- original priority below ----
  [PRIORITY-WAS: 0.35, spawned 2026-05-25 from hb-068 finding]
  mode: ft8
  status: SHELVED 2026-06-01
  priority_score: 0.0
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

### hb-040 — Plumb (or remove) `Ft8Config::time_range`  [SHELVED 2026-05-26 — resolved by hb-012 finding]
  mode: ft8
  status: SHELVED — no-op on pancetta's corpus. hb-012 (batch 11) established the curated recordings are 90s continuous multi-slot captures; the Costas search scans t0 from 0 across the whole buffer, so signals at any interior timing are already found. Negative-time search only matters for first-slot pre-recording audio (no data to recover). time_range stays a no-op field; mr-006 corpus survey recommended capturing future audio slot-aligned rather than building a preprocessor. wild-50's 0/96 is corpus-specific (2 outlier WAVs per hb-025) with ~0 composite impact.
  priority_score: 0.0
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

### hb-036 — Score-relative NMS suppression  [SHELVED 2026-05-31]
  mode: ft8
  status_2026_05_31: SHELVED — sweep at delta_db ∈ {1.0, 2.0, 3.0, 5.0} all regress hard-200 by -748 to -1034 rec vs nms-off baseline. Mechanism interpolates between pure-NMS and nms-off; no sweet spot. Costas sync_score variance dominates the duplicate-vs-distinct gap. **NMS suppression closed on current corpus + implementations tested** (Phase A honesty pass 2026-06-02; previously "family closed"). Future work would need a fundamentally different discriminator (e.g., LDPC-codeword-based dedup, hb-104 territory). Config knob `nms_score_delta_db` preserved at default 0.0 for research re-evaluation.
  status: shelved
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

### hb-016 — Residual energy early-stop for multi-pass  [SHELVED 2026-05-30]
  status_2026_05_30: SHELVED — implemented as `residual_energy_stop_db` with the
    `mean_excess_above_noise_db` probe (after a V1 mean-linear-power variant
    proved insensitive — bright bins dominated the linear mean and the metric
    never converged toward floor). hard-200 sweep at thresholds {1.0, 2.0,
    3.0, 5.0} dB: recall identical to baseline at every threshold (4616 rec /
    921 nov / rate 0.53825). Elapsed: probe-off baselines 350-361 s; probe-on
    variants 381-397 s at th ≥ 2.0 — ~5-12 % SLOWER, not faster. The probe is
    paid per round (O(N) mean over the power tensor) but the rebate it was
    designed to harvest is already absorbed by the existing empty-pass-break
    in `decode_window_with_ap`, which short-circuits the multipass loop
    whenever `coherent_subtract_and_repass` returns an empty Vec (the case
    where the residual is signal-poor). hb-016 was a useful hypothesis when
    multipass wasn't shipping (the bank entry predates hb-079); under N=3
    multipass + hb-086 V1's joint-pair-retry that the residual feeds, the
    early-exit window is too narrow to recover its overhead. Code preserved
    on the branch with the flag defaulted off (zero production impact).
    Journal: research/experiments/2026-05-30-hb-016-residual-energy-stop.md.
  ---- original entry below ----
  [PRIORITY-WAS: 0.36]
  mode: ft8
  status: SHELVED 2026-05-30
  priority_score: 0.0
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

### hb-018 — OSD-3 with strengthened CRC validation  [SHELVED 2026-05-26 — confirmed under hb-075 production]
  status_2026_05_26: SHELVED — re-confirmed under post-hb-075 production. OSD-3 + filter: -1 rec / +10 novel vs OSD-2 + filter. Cross-cycle averaging didn't change the OSD candidate population enough to flip hb-053's earlier finding. Direction structurally closed.
  ---- original entry below ----
  [PRIORITY-WAS: 0.30]
  mode: ft8
  status: SHELVED 2026-05-26
  priority_score: 0.0
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

### hb-021 — Wild-card: frequency-domain signal subtraction  [SHELVED 2026-05-23]
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

### hb-026 — Wild-card: End-to-end neural decoder  [PRIORITY: 0.15 (wild)]
  mode: ft8
  status: pending
  priority_score: 0.15
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

### hb-028 — Wild-card: Cross-decoder ensemble at runtime  [PRIORITY: 0.25 (wild)]
  mode: ft8
  status: pending
  priority_score: 0.25
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

### hb-087 — Callsign-priors-on-residual (AP-constrained, bypass Costas pre-gate)  [SHELVED 2026-05-31 at Session 2 — 0/10 AP-decode rescue]
  status_2026_05_31_session2: SHELVED — Session 2 per-truth AP-decode micro-test returned 0 of 10 rescued on diverse top-20 hard-200 picks (need ≥3 for PROCEED). The production AP path with singleton recent_calls=[truth_callsign] could not surface any of the 10 prior-covered missed truths from positions sync_search did find. Conclusion: AP injection alone doesn't rescue noise-dominated residuals. The bypass-Costas extension (Session 3, deferred forever) would inherit this null result because it uses the same LDPC+AP machinery against weaker (sub-Costas) residual LLRs. Sibling hb-088 (OSD-without-Costas) SHELVED structurally at the same wall. Priority 0.45 → 0.0. See `research/experiments/2026-05-31-hb-087-session2.md` for the per-truth table, mechanism inference, and revisit conditions.
  ---- original priority: 0.45 ----
  mode: ft8
  status: SHELVED 2026-05-31
  priority_score: 0.0
  estimated_effort: 3 sessions (session 1 = scoping/diagnostic, DONE; session 2 = prior-set aggregation in research crate; session 3 = AP-constrained residual decode pass + eval)
  expected_delta: +5 to +30 hard-200 rec (diagnostic upper bound 153 of 647 = 23.6% coverage; mechanism efficiency assumed 5-20% of that); composite +0.0005 to +0.0015
  defensible_prior: yes — AP-without-sync is the canonical weak-signal recovery lever (JT9/JT65 "apsym"/"napwid"; pancetta's own AP1/AP2 already work on the sync-passing path). The residual at sub-Costas positions HAS energy at masked-but-real signal locations (hb-079's coherent subtract proves the locality); the missing ingredient is a constraint strong enough to prevent LDPC from converging on noise (V3's failure mode).
  wild_card: false
  evidence_for:
    - Diagnostic (hb087_callsign_priors_feasibility.rs): 23.6% of missed truths in top-20 worst hard-200 have a callsign already in {operator, recent-window, bundled-common}. Above 20% PROCEED gate.
    - Recent-window prior dominates (23.2% of 23.6%); is the same source pancetta-qso::callsign_continuity already maintains in production for FP filtering. Production cqdx-spotted prior is broader than the diagnostic's bundled-100 stand-in, so 23.6% is a conservative lower bound.
    - V3 SHELVE confirmed the geometry: 56.8% of V1-uncoverable missed truths sit within ±8 freq_bins of a subtracted decode. The positions ARE there; what V3 lacked was a constraint preventing BP from converging on noise.
    - AP injection at ±15.0 LLR magnitude structurally pins 28 bits per callsign-position attempt; with continuity-filter + plausibility + CRC, FP rate is bounded multiplicatively below 1%.
    - Reuses existing primitives: `inject_ap_llrs`, `inject_ap2_caller`, `inject_recent_call_at_called` in pancetta-ft8/src/ap.rs; `joint_pair_retry_pass` is the residual-decode template; `callsign_continuity::CallsignContinuityFilter` is the prior-source aggregator.
  evidence_against:
    - CPU cost is the dominant risk. Lattice density × prior-set size × per-position-LDPC could blow past +25% wall-clock budget if naive. Mitigation plan: cheap LLR-energy pre-screen per position; cap prior-set at 64 most-recent + 32 highest-rarity cqdx; tight lattice ±4 bins × ±1 time_step. Even so, session 3 must measure wall-clock carefully and stop early on overrun.
    - Operator-call prior contributes 0% on this corpus (the hard-200 WAVs aren't from K5ARH's logs). Production value would be larger but field-eval only.
    - Bundled-common prior contributes only 0.8% on the diagnostic (the bundled list is small; the specific WAVs don't carry DXpedition activity). Field cqdx-spots would contribute more but the diagnostic understates this.
    - V3-style risk: per-truth decodability micro-test (extract residual LLRs at the truth's known coordinates and run AP-decode) might shelve the mechanism even after 23.6% coverage. Gated at session 2 → 3 boundary.
    - Coverage doesn't equal recovery: a truth's callsign being in the prior set is necessary but not sufficient. The mechanism efficiency (fraction of covered truths that actually decode) is unknown without session 3 measurement.
  notes: |
    Spec: docs/superpowers/specs/2026-05-31-hb-087-callsign-priors-design.md
    Diagnostic: pancetta-research/examples/hb087_callsign_priors_feasibility.rs
    Scoping journal: research/experiments/2026-05-31-hb-087-scoping.md
    Session 2 journal (SHELVE): research/experiments/2026-05-31-hb-087-session2.md
    Session 2 micro-test: pancetta-research/examples/hb087_session2_ap_decode_microtest.rs
    Session 2 aggregator: pancetta-research/src/callsign_priors.rs

    Sibling hypothesis (also SHELVED 2026-05-31): hb-088 OSD-without-Costas.
    Both bypass-Costas mechanisms attack the same V3-identified wall via
    different constraints (callsign-AP vs ordered-statistics). Both
    SHELVED — the structural failure (sub-Costas residuals are
    noise-dominated; no AP-style constraint rescues them) is shared.

    Doctrine note inherited from V3: gate session 2 → session 3 on a
    per-truth decodability micro-test (10 truths from top-3 worst WAVs whose
    callsign IS in the prior set; ≥3 must AP-decode against residual at
    known truth coordinates). This is the V3 SHELVE doctrine refinement
    ("geometric proximity is not decodability") applied to hb-087's
    feasibility step. Cheap insurance (~30 min of session 2 wall-clock).
    DOCTRINE VINDICATED: the kill-switch caught what the 23.6% coverage
    diagnostic missed. 1-2 days of Session 3 implementation effort saved.

    Revisit conditions: a downstream mechanism that improves residual
    quality at sub-Costas positions (coherent-subtract refinements
    beyond hb-079's wins, multi-cycle accumulation across adjacent
    slots, per-position residual SNR estimation pre-decode) could
    change the AP-rescue probability for these specific truths. The
    Session 2 micro-test + aggregator stay in place and are reusable.
    Until such a mechanism graduates, hb-087's structural shelf-reason
    holds.

### hb-089 — Multi-cycle coherent residual accumulation  [SHELVED 2026-06-01 — see research/experiments/2026-06-01-hb-089-residual-accumulation.md]
  mode: ft8
  status: shelved
  priority_score: 0.48
  shelve_reason: |
    Diagnostic-first kill switch FIRED on two independent paths.
    (1) Bank-stated kill switch ("callsign in 2+ same-slot sub-windows")
        is UNSATISFIABLE — pancetta hard-200 WAVs are 15.0 s each (one
        FT8 slot); no callsign repeats within a WAV. The mr-008
        ideation prose mistakenly assumed 90 s multi-slot WAVs.
    (2) Task-stated SNR-delta fallback ("intra-slot overlap-averaged
        magnitude across sub-windows" on the post-multipass residual
        at missed-truth coordinates) measured mean Δ +0.013 dB across
        28 missed truths in top-5 worst WAVs — two orders of magnitude
        below the 2 dB PROCEED gate. Theoretically expected: 85%+
        overlap between same-slot sub-windows means the noise samples
        are correlated, so variance reduction is bounded by
        ~10·log10(15/13) ≈ 0.6 dB even in the best case.
    The structural geometry of single-slot FT8 makes same-slot
    sub-window averaging inapplicable; the mechanism only has real
    coherent gain across SEPARATE slots, which the existing
    hb-074/075/079 cross-cycle infrastructure already covers on
    multi-slot WAVs.

    **Naming note (Phase C 2026-06-02):** the original bank/journal
    called this "Welch averaging." Welch (1967) is a PSD estimator
    via averaged-windowed periodograms; what was actually measured is
    **overlap-averaged magnitude across sub-windows** at a fixed
    time-frequency coordinate (per-bin magnitude averaging, not PSD
    estimation). The diagnostic + conclusion are correct; only the
    term was loose. See
    docs/engineering/2026-06-02-engineering-substance-audit.md
    (claim 7).
  estimated_effort: 1-2 sessions (Session 1 = diagnostic, Session 2 = implement + sweep)
  expected_delta: +5-30 hard-200 rec (mechanism analogous to hb-079's interference cleanup but applied across same-slot sub-windows after multipass saturation)
  defensible_prior: yes — Q65 averages over receive windows; hb-079's mechanism cleared the per-slot interferers but didn't combine cleaned residuals across sub-windows; structurally different from hb-085 (which attacked decode-positions, not residual bins)
  wild_card: false
  evidence_for:
    - mr-008 source: Q65 weak-signal mode in WSJT-X uses multi-window averaging; FT8's 90s curated WAVs naturally contain 5 same-slot sub-windows.
    - hb-079 graduated +158 hard-200 rec by coherently cleaning interference; the post-multipass residual is the cleanest representation of remaining weak signals. Accumulating that residual coherently across 5 sub-windows is the standard noise-floor-reduction step Q65 uses.
    - Different from hb-074/075 (which average the RAW spectrogram pre-subtract) and from hb-085 (which attacked decode positions, not bins). New shape: average the CLEANED RESIDUAL.
  evidence_against:
    - Inter-slot phase coherence: hb-074 found that real-world audio doesn't preserve phase across slot boundaries (hb-075's MRC was the workaround). The residual at sub-Costas positions has even more uncertain phase than the raw signal. May require MRC-weighting analogous to hb-075.
    - Adds another full residual-decode pass; wall-clock budget pressure.
    - The 5 sub-windows in a 90s WAV aren't all decodable for the same truth signal — fade dropouts mean the per-sub-window residual at the truth's freq_bin may be all-noise on 3 of 5 windows.
  notes: |
    mr-007 audit: pancetta-ft8 has cross_cycle_averaging (hb-056) and
    coherent_multipass (hb-079) infra already. The new piece sits
    BETWEEN them: after multipass saturates per sub-window, accumulate
    the complex residual across sub-windows with MRC-weighting (per
    hb-075's pattern), then run one final decode pass on the
    accumulated residual.

    Kill-switch (V3 doctrine: paired decodability test):
    Diagnostic on top-20 hard-200 worst WAVs — for missed truths whose
    callsign appears in 2+ same-slot sub-windows, measure per-position
    residual SNR before and after 5-window coherent accumulation.
    PROCEED if median SNR improvement ≥ 2 dB AND the LLR sign-agreement
    with truth codeword at those positions improves to ≥ 70% (vs
    hb-088's 50.6% baseline).

    Source: mr-008 ideation,
    research/experiments/2026-05-31-mr-008-ideation.md (territory A).

### hb-090 — Phase-coherent matched filter at truth coordinates  [PRIORITY: 0.38, spawned 2026-05-31 from mr-008 ideation]
  mode: ft8
  status: pending
  priority_score: 0.38
  estimated_effort: 2-3 sessions (Session 1 = matched-filter primitive + diagnostic, Session 2-3 = production wiring + sweep)
  expected_delta: targets the interferer-leakage wall hb-088 closed; bounded — IF matched filter beats max-log on sign-agreement, +10-40 hard-200 rec is possible
  defensible_prior: partial — explicitly cited in hb-088 shelve journal as the structurally-different mechanism family not tested ("rejects energy at adjacent freq_bins via phase-coherent matched filtering at the truth's exact freq + dt"); standard radar/sonar weak-signal technique
  wild_card: false
  evidence_for:
    - hb-088 shelve journal Section 4 "What WOULD work" calls out IQ matched filter at truth coords as orthogonal to single-position spectrogram mining.
    - hb-075 + hb-079 established complex-spectrogram infrastructure; the matched-filter primitive operates on the same complex domain.
    - Matched filter is the optimal linear detector for known-shape signals in additive Gaussian noise (FT8 tone patterns ARE known by symbol index); the result rejects adjacent-bin energy by filter selectivity.
  evidence_against:
    - "Known shape" is only true once the Costas alignment + freq/dt are fixed — fine for AP-known positions but not for sub-Costas discovery. Most useful as a precision step at SYNC-PASSING positions, less so at sub-Costas.
    - Doesn't help with the "what callsign to assume" problem; need to enumerate plausible tone sequences which collapses to OSD on matched-filter LLRs. Bounds upside.
    - Implementation cost: per-candidate IQ extraction + 79-symbol matched filter for each enumerated tone sequence. CPU/cache risk.
  notes: |
    mr-007 audit: pancetta-ft8 has complex spectrogram (hb-075). NEW work:
    per-candidate IQ window extraction (time-domain audio buffer slice)
    + matched-filter correlation against 79-symbol templates at the
    Costas-aligned tone sequence positions. Plumbing borrows from
    extract_symbols_via_fft.

    Kill-switch (hb-088 doctrine: LLR sign-agreement):
    Replace pancetta-research/examples/hb088_osd_without_costas_feasibility.rs's
    max-log demod with a matched-filter demod at the truth's known
    coordinates; measure sign-agreement with the truth codeword on the
    same top-20 hard-200. PROCEED if median ≥ 70% (vs hb-088's 50.6%).

    Source: mr-008 ideation (territory A).

### hb-091 — a8-style early-decode latency reduction  [PRIORITY: 0.42, spawned 2026-05-31 from mr-008 ideation]
  mode: ft8
  status: pending
  priority_score: 0.42
  estimated_effort: 2-3 sessions (Session 1 = design, Session 2 = partial-buffer decode primitive, Session 3 = coordinator wiring)
  expected_delta: operational — +5-15% QSO/hr in autonomous Phase 5 under variable propagation; recall on hard-200 unchanged (different axis)
  defensible_prior: yes — WSJT-X-Improved v3.x ships "a8" decoding technology that decodes the in-QSO station's message 0.5-1s earlier; documented in DG2YCB release notes for v3.0.0 250924
  wild_card: false
  evidence_for:
    - WSJT-X-Improved 3.0/3.1 changelog (2025-2026): "MTD 3-Stage now (partially) supports the new 'a8' decoding technology, which allows messages from the station in QSO to be displayed 0.5 to 1 second earlier."
    - For an autonomous station, 0.5-1s earlier turnaround per QSO leg increases QSOs/hour under fast-fade or QSB conditions (the partner's TX may end early; faster decode = faster RR73 = faster log).
    - pancetta's coordinator already tracks the in-QSO partner's callsign + frequency (`activeQso` state in QSO state machine). The known position makes partial-buffer decoding viable (sync isn't the limit; signal length is).
  evidence_against:
    - Operational target, not composite. The eval harness doesn't measure QSO/hr; need loopback simulation infrastructure to validate.
    - Partial-buffer decoding reduces LLR integration time (~13s vs ~15s) → ~0.5 dB sensitivity hit on the in-QSO partner's message. Only viable when the partner is strong (which is expected for in-QSO).
    - Requires coordinator-side plumbing (partial-buffer hook) plus decoder-side gating logic. Cross-crate change.
  notes: |
    mr-007 audit: pancetta-qso has the active_qso state; pancetta-coordinator
    streams audio buffers in 15s chunks. NEW work: coordinator emits a
    partial buffer at t=13s tagged with the in-QSO partner's expected
    freq_bin. Decoder runs a SCOPED decode_window restricted to the
    partner's freq_bin ±10 Hz. Returns early if the partner's expected
    message type (R+report, RRR, RR73, 73) is decoded.

    Kill-switch (operational):
    Existing pancetta loopback infrastructure can simulate QSO sequences;
    extend to vary partner fade timing. Measure mean turnaround time +
    QSO/hr with vs without a8. PROCEED if QSO/hr improves by ≥ 10% in
    the simulation.

    Source: mr-008 ideation (territory B);
    WSJT-X-Improved v3.0.0 250924 release notes.

### hb-092 — Codeword-based NMS dedup (post-decode)  [PRIORITY: 0.40, spawned 2026-05-31 from mr-008 ideation]
  mode: ft8
  status: pending
  priority_score: 0.40
  estimated_effort: 1 session
  expected_delta: precision (-5-15% of remaining novel duplicates on hard-200); recall preserved by construction; bounded but clean
  defensible_prior: yes — explicitly cited in hb-036 shelve journal as "the brute-force answer" to duplicate-vs-distinct discrimination ("just decode both, dedup by codeword")
  wild_card: false
  evidence_for:
    - hb-036 SHELVE journal (2026-05-31): "Future attempts would need a fundamentally different discriminator — e.g., LDPC-result-based 'did this candidate decode to the same codeword as the stronger one?' (which is the brute-force answer: just decode both, dedup by codeword)."
    - pancetta's current dedup is at the `unique_decoded` HashSet level by text representation. Two decodes of the SAME codeword can have different displayed (freq, dt) due to fractional-bin sync refinements (hb-068 graduated scaled-delta refinement, which itself produces small DT perturbations).
    - Codeword-binary dedup is recall-preserving by construction (no decode is dropped that doesn't have a binary-identical companion).
  evidence_against:
    - Bounded upside: if pancetta's text-level dedup already catches most duplicates, codeword-binary catches only edge cases.
    - Costs a per-decode 174-bit comparison + hash set lookup; trivial CPU.
  notes: |
    mr-007 audit: pancetta-ft8's DecodedMessage already retains the
    `codeword` field for hb-079's subtraction needs. Plumbing: change
    the dedup HashSet from String to (String, [u8; 174]) or just
    [u8; 174] keyed on codeword; same-codeword duplicates collapse.

    Kill-switch (precision):
    Diagnostic on top-20 hard-200 — count fraction of unique_decoded
    outputs that have a codeword-binary match with another output in
    the same WAV but differ in (freq, dt) by ≥ 5 Hz / 50 ms. PROCEED
    if ≥ 5% of novels are codeword-duplicates.

    Source: mr-008 ideation (territory B); hb-036 shelve journal.

### hb-093 — Per-position residual SNR pre-decode gate  [SHELVED 2026-06-01]
  mode: ft8
  status: shelved
  priority_score: 0.52
  estimated_effort: 2 sessions (Session 1 = diagnostic + threshold sweep, Session 2 = production wire + A/B)
  expected_delta: efficiency: -5-15% elapsed on hard-200 (skip noise-only positions in joint_pair_retry); recall preserved; precision lift via novel reduction
  defensible_prior: yes — V3 SHELVE + hb-088 SHELVE both showed sub-Costas residual positions are dominated by noise/interferer leakage; a cheap per-position SNR estimator can gate which residual positions are worth the LDPC+CRC work
  wild_card: false
  evidence_for:
    - V3 mechanism trace (top-3 hard-200 WAVs): 300+ candidates surfaced in the targeted residual window per WAV; LDPC processes all of them; only 1-4 pass CRC, plausibility catches the rest. The other ~99% are noise-position waste.
    - hb-088 diagnostic: sub-Costas |LLR| at truth positions is 82% of control — energy IS there. The problem is signs are random (50.6%). A pre-LDPC residual SNR estimator wouldn't directly fix sign agreement, but it could filter the LOW-energy positions where the residual hasn't even cleared the local noise floor (those are pure noise, not interferer leakage; the latter has high energy but wrong direction).
    - pancetta-ft8's `par_estimate_snr_spectrogram` already computes per-candidate per-symbol SNR on the original spectrogram; applying the same primitive to the residual spectrogram is a small plumbing change.
  evidence_against:
    - The gate must be conservative; a too-tight threshold loses joint-pair-retry's +12 hard-200 graduated win.
    - The savings target is wall-clock (precision/efficiency), not recall — composite doesn't move directly. Operational value (more decode budget per slot) but not headline composite.
    - V3 doctrine warning: per-position SNR on the residual is itself a geometric/energy proxy, not a decodability test. The TWO-PART kill-switch addresses this.
  notes: |
    mr-007 audit: par_estimate_snr_spectrogram exists; the new call applies
    it to the post-multipass residual_spectrogram. Plumbing: add
    `residual_snr_gate_db: Option<f64>` to Ft8Config (default None =
    disabled); when set, V3-style candidates with residual_snr_db <
    threshold are skipped from LDPC. The gate also applies to
    joint_pair_retry candidates.

    Kill-switch (V3 doctrine: TWO-PART decodability-validated):
    Part 1 (efficiency): measure wall-clock saved by gating low-residual-
    SNR positions out of joint_pair_retry on top-20 hard-200; PROCEED if
    ≥ 5% elapsed savings at the chosen threshold.
    Part 2 (decodability): for the gated-OUT positions in Part 1,
    count how many had a truth that joint_pair_retry would have
    recovered (cross-reference to truth manifest). PROCEED only if
    ≤ 1 truth-lost per WAV across top-20 (≤ 20 total).

    Source: mr-008 ideation (territory A).

    SHELVED 2026-06-01 (iter/2026-06-01-hb-093): kill-switch PROCEEDED
    at thr=-5.0 dB on the top-5 diagnostic (filter 36.5%, decode loss
    0.00%), but the production hard-200 sweep with FP filter shows
    elapsed savings cap at ~2-3% (best: gate=-10 dB gives -2.08%
    elapsed with ZERO recall loss). The iter-spec graduation bar
    (10% elapsed reduction) is unreachable on this corpus because
    joint_pair_retry is only a small slice of total slot work — the
    pass-1 par_iter LDPC dominates. Plumbing kept at default-off in
    `Ft8Config::residual_snr_gate_db` for future revisits (the
    obvious next mechanism is extending the gate to
    `coherent_subtract_and_repass` step 4, where the candidate set is
    `max_sync_candidates` (200) per round — a much larger surface area
    to filter). Journal:
    research/experiments/2026-06-01-hb-093-snr-gate.md.

### hb-094 — Residual denoising autoencoder pre-LDPC  [PRIORITY: 0.20 (wild), spawned 2026-05-31 from mr-008 ideation]
  mode: ft8
  status: pending
  priority_score: 0.20
  estimated_effort: PLAN-SIZED (3-5 sessions: training-data gen + tiny model + diagnostic + integration)
  expected_delta: speculative — could rescue 20-60 hard-200 rec IF the denoiser successfully removes interferer-leakage from sub-Costas residuals; could regress like hb-064 Session 2 (-135 novels) on out-of-distribution drift
  defensible_prior: partial — self-supervised audio denoising literature 2025 (DCUNET, ONT-model variants) shows narrowband signal denoising via small (~1-2M param) deep models. Targeted at the interferer-leakage residual structure that hb-088 identified as the closed-family wall.
  wild_card: true
  evidence_for:
    - hb-088 doctrine: sub-Costas residual energy is INTERFERER LEAKAGE, not white noise. A learned denoiser trained on (clean_truth_tile, residual_with_known_neighbor_leakage_tile) pairs has structured signal to remove (leakage pattern depends on neighbor's tone sequence, which is encoded in the spectrogram).
    - Training data IS available: multipass produces matched (decoded_msg, post-subtract residual) pairs for every hard-200 WAV. The pair (cleaned_residual_tile, truth_codeword_signal_tile) is the supervision signal.
    - Bounded scope: the denoiser sits BEFORE LDPC on the spectrogram tile at the candidate's freq/time window. The LDPC+CRC+plausibility funnel is unchanged. Failure mode is clean: low recall preserved, high recall gates on the denoiser confidence.
  evidence_against:
    - hb-064 Session 2's out-of-distribution drift cost -135 novels at a much smaller intervention (just OSD ranker). A denoiser operating on the spectrogram itself has FAR more drift risk.
    - The "right" architecture is unknown — small UNet, small transformer, dense MLP? Each is its own multi-session pipeline.
    - Training data quality is bounded by hb-079's subtraction quality; the denoiser is learning to clean what subtraction already failed to clean, which by definition is the residual that BP can't decode.
  notes: |
    mr-007 audit: pancetta-ft8 has neural_osd.rs (DIA-OSD style) precedent.
    NEW work: separate inference module for spectrogram-tile denoising,
    pre-LDPC integration in decode_window_with_ap. Production wiring is
    a feature-gated config flag (`residual_denoiser: bool`).

    Kill-switch (hb-088 doctrine: LLR sign-agreement):
    Train tiny denoiser on paired (clean_synth_tile, synth_with_neighbor_
    leakage_tile) from pancetta-research's synth generator. Measure LLR
    sign-agreement on the truth codeword AFTER denoiser vs hb-088's
    50.6% baseline on top-20 hard-200. PROCEED to plan-sized work if
    ≥ 70%.

    Source: mr-008 ideation (territory D); self-supervised audio
    denoising literature 2025.

### hb-095 — Neural soft-demod replacement for max-log LLR  [PRIORITY: 0.25 (wild), spawned 2026-05-31 from mr-008 ideation]
  mode: ft8
  status: pending
  priority_score: 0.25
  estimated_effort: PLAN-SIZED (3-4 sessions: training-data gen + small NN + integration + retune LDPC)
  expected_delta: speculative; could lift LLR sign-agreement from max-log's baseline; uncertain LDPC interaction; potentially +0.01 composite IF the LDPC retune absorbs the LLR distribution shift
  defensible_prior: partial — arXiv:2502.16371 "Software defined demodulation of multiple frequency shift keying with dense neural network for weak signal communications" reports gains for 8-FSK demod in low-SNR regimes; FT8 is 8-GFSK so structurally similar
  wild_card: true
  evidence_for:
    - Paper claim: a small dense NN trained on synthetic 8-FSK tone-magnitude vectors outputs better-than-max-log LLRs in low-SNR + ionospheric regimes. Architecturally aligned with FT8.
    - pancetta's max-log demod (par_compute_soft_llrs_db) is a deterministic max-of-8-tones rule per symbol; it ignores the joint distribution of the 7 non-winning tone magnitudes which carries soft information.
    - Trainable with paired (clean_synth_tone_magnitudes, ground_truth_bit) data from pancetta-research's synth generator. No external data required.
  evidence_against:
    - Changes LDPC INPUT distribution → existing LDPC tunings (parity_gate=2, max_iters=100, llr_variance_target=32) may need re-sweep. Multiple closed shelves to potentially re-open.
    - Plan-sized; multi-session investment with no shipped neural infra precedent at this site.
    - Risk of synth-overfit (production hard-200 is dense band, not synth-clean).
  notes: |
    mr-007 audit: pancetta-ft8 has the LLR computation primitive
    (par_compute_soft_llrs_db). NEW work: replace with a small NN
    (~50k params) that consumes the 8-tone-magnitude vector for 79
    symbols and outputs 174 LLRs. Risk: LDPC retune may be needed.

    Kill-switch (hb-088 doctrine: LLR sign-agreement):
    Train tiny model on synth-clean. Measure (a) per-bit sign-agreement
    on the truth codeword at SYNC-PASSING positions on hard-200
    (control: max-log = 84% at sync-passing — same as hb-088's
    control distribution baseline), (b) LDPC+CRC pass rate. PROCEED
    if (a) ≥ 88% AND (b) doesn't drop.

    Source: mr-008 ideation (territory D); arXiv:2502.16371.

### hb-096 — Adaptive multipass termination by decode-count delta  [PRIORITY: 0.32, spawned 2026-05-31 from mr-008 ideation]
  mode: ft8
  status: pending
  priority_score: 0.32
  estimated_effort: 1 session
  expected_delta: efficiency — -5-15% elapsed on hard-200 (early-terminate multipass when pass N adds < K decodes); recall preserved at the chosen floor
  defensible_prior: partial — hb-080 graduated N=3 multipass with no per-WAV adaptation; pass N's marginal contribution varies wildly across WAVs
  wild_card: false
  evidence_for:
    - hb-080 sweep showed N=3 adds +9 hard-200 rec over N=1 (cumulative +16). The marginal value of pass 3 is small. On most WAVs pass 3 adds zero; pass 3's wall-clock is paid uniformly.
    - hb-016 (residual ENERGY axis) was SHELVED tonight because the energy probe paid per-round cost that exceeded its savings AND the empty-pass-break already short-circuits perfect-clean cases. The DECODE-COUNT axis is different: the count is already computed (it's the result of the just-finished pass), so the gate is free.
    - Adaptive termination is a long-standing standard pattern in iterative numerical methods.
  evidence_against:
    - Bounded upside: hb-080's sweep showed N=2 → N=3 added +9 rec at +25% wall-clock; gating on "N=2 added < K" might preserve only ~half of N=3's contribution.
    - hb-016's SHELVE narrative warned that the existing empty-pass-break already covers the cheap case.
  notes: |
    mr-007 audit: hb-080's multipass loop has `pass_unique.is_empty() {
    break }` already. Plumbing: add `if pass_unique.len() <
    multipass_decode_floor && current_pass_idx >= 1 { break }`.
    Trivial code touch.

    Kill-switch (efficiency-with-recall-preservation):
    Sweep `multipass_decode_floor ∈ {1, 2, 3}` on hard-200; PROCEED at
    the highest floor where elapsed drops ≥ 5% AND hard-200 recall is
    preserved (within ±2 of baseline).

    Source: mr-008 ideation (territory B).

### hb-097 — Subtract amplitude calibration via residual-energy minimization  [PRIORITY: 0.40, spawned 2026-05-31 from mr-008 ideation]
  mode: ft8
  status: pending
  priority_score: 0.40
  estimated_effort: 1-2 sessions
  expected_delta: +3-10 hard-200 rec (slightly better residual quality → multipass finds slightly more masked candidates); structurally similar to hb-081 but with data-driven optimum rather than fixed threshold
  defensible_prior: partial — hb-079 subtracts at full ML projection assuming exact rotor; rotor noise systematically biases the projection magnitude. A per-decode amplitude calibration step finding α ∈ [0.8, 1.2] minimizing local residual energy could improve precision WITHOUT under-subtracting (data-driven optimum)
  wild_card: false
  evidence_for:
    - hb-079's ML projection: residual = bin - α·rotor·Re(bin·conj(rotor)); pancetta uses α=1 always. Rotor noise → projection magnitude bias. α=1 is correct only if rotor is exact.
    - hb-081 SHELVED at fixed under-subtract thresholds (α < 1.0 always); the journal explicitly diagnosed "under-subtracting blocks multipass." hb-097's α can be > 1.0 when the rotor under-estimates the signal — the data-driven optimum CAN over-subtract too, which the fixed-threshold approach couldn't.
    - Cheap to implement: 1D line search over α ∈ [0.8, 1.2] in ~10 evaluations per decode minimizing residual energy in a ±3 freq_bin × ±2 time_step window around the subtracted position.
  evidence_against:
    - hb-081's failure mode: under-subtract leaves signal energy that BLOCKS multipass. Over-subtract carries DIFFERENT risk: removes too much, including adjacent weak signal energy in the optimization window. Need careful tuning of the optimization window.
    - The optimum α may cluster near 1.0 → no-op; hb-081 sweep was at fixed [5,10,20,40] thresholds and recall dropped uniformly. Need to verify the rotor-noise hypothesis is correct.
  notes: |
    mr-007 audit: pancetta-ft8's subtract_decode_coherent uses fixed ML
    projection. NEW work: replace fixed α with a 1D line search over
    a local residual-energy objective in a ±3×±2 window. Per-decode
    cost ~10 mul-add evaluations; negligible.

    Kill-switch (precision/recall-net):
    Top-20 hard-200 diagnostic — measure per-decode α optimum
    distribution. PROCEED if median(|α - 1|) ≥ 0.05 AND follow-up
    multipass on the α-optimized subtract finds ≥ 1 additional decode
    per WAV vs full ML.

    Source: mr-008 ideation (territory A).

### hb-098 — Autonomous strategy switching (CQ/hunt/hybrid auto-toggle)  [PRIORITY: 0.35, spawned 2026-05-31 from mr-008 ideation]
  mode: ft8 (operational, not decoder)
  status: pending
  priority_score: 0.35
  estimated_effort: 1-2 sessions
  expected_delta: operational — +5-15% QSOs/hr across variable band conditions; recall on hard-200 unchanged (different axis)
  defensible_prior: yes — AutoFT8 / FT8Commander 2025 ops data (~7.4 QSO/hr mean) shows mixed strategies switching between CQ and S/P give the best rate vs static modes
  wild_card: false
  evidence_for:
    - pancetta-qso/src/autonomous.rs already has the three modes (hunt, cq, hybrid). The gap is the switching logic — mode is currently set at startup and stays.
    - Decision signal: rolling callers-per-CQ rate + observed-callsign-count-per-slot. If callers-per-CQ < 0.5 over 10 CQs → switch to hunt mode; if observed-callsign-count > N → switch to CQ to capture answers.
    - Operational FT8 community has 5+ years of evidence that auto-strategy outperforms static modes during contests / DX openings.
  evidence_against:
    - Eval harness can't measure operational QSO/hr directly; need a loopback simulation framework. Existing loopback infrastructure (pancetta/tests/loopback_qso.rs) supports single-QSO test but not variable-band simulation.
    - "Best mode" depends on operator goals (DXCC chase vs contest vs ragchew); a one-size-fits-all auto-toggle may be wrong for some users. Config flag should let operator pin a mode.
  notes: |
    mr-007 audit: pancetta-qso has all three modes; pancetta-coordinator
    runs the mode-driven decision per slot. NEW work: a callers/CQ +
    callsign-density rolling window in autonomous.rs + ~50 LOC mode-
    transition rule. No decoder change.

    Kill-switch (operational):
    Extend the loopback simulation to inject varying-density callsign
    streams (low, medium, high). Measure mean QSO/hr in static CQ vs
    static hunt vs auto-switching over 1000 simulated slots per
    density level. PROCEED if auto-switching is ≥ 5% better than the
    best static mode on at least 2 of 3 density levels.

    Source: mr-008 ideation (territory E); AutoFT8 ops data 2025.

### hb-099 — QSO-completion-rate-optimized priority scoring  [PRIORITY: 0.28, spawned 2026-05-31 from mr-008 ideation]
  mode: ft8 (operational, not decoder)
  status: pending
  priority_score: 0.28
  estimated_effort: 1-2 sessions
  expected_delta: operational — +3-10% QSOs/hr via Bayesian completion-probability prior on station selection
  defensible_prior: partial — pancetta's priority scoring weights needed-DXCC + needed-grid + POTA/SOTA + rarity but has no completion-probability feedback. Completed QSOs are logged to ADIF; a Bayesian update per (callsign | band-condition cluster) is straightforward
  wild_card: false
  evidence_for:
    - pancetta-qso/src/priority.rs computes priority scores from static features. Real-world completion success varies by callsign (some stations are unreliable QSO partners) and by band condition (some clusters of conditions produce high completion rates).
    - pancetta logs every QSO outcome to ~/.pancetta/qsos.adi + sqlite index. The completion-probability per (callsign, band-condition cluster) is computable on startup + per-completion update.
    - Multiplicative integration: priority = static_priority × completion_prob_smoothed (with Laplace smoothing for new stations to keep them explorable).
  evidence_against:
    - Cold-start: new stations have no completion-rate prior. Need lenient smoothing to avoid biasing toward only known-completing stations.
    - The benefit is bounded by the completion-rate variance in the population. If most stations complete at ~70%, the prior barely moves anyone.
    - Operational, not composite. Eval harness doesn't measure.
  notes: |
    mr-007 audit: pancetta-qso already has the ADIF + sqlite QSO log
    infrastructure. NEW work: a startup-time completion-rate computation
    + per-completion update + multiplicative integration into
    priority.rs.

    Kill-switch (operational):
    Simulation — measure mean QSOs/hr with vs without completion-rate
    prior over 1000 simulated slots with varying station-completion
    distributions. PROCEED if ≥ 5% improvement at realistic distributions
    (completion ranges 40-90% mixed).

    Source: mr-008 ideation (territory E).

### hb-100 — Synthetic interferer-pair corpus generator  [PRIORITY: 0.25, spawned 2026-05-31 from mr-008 ideation]
  mode: ft8 (corpus/eval infrastructure)
  status: pending
  priority_score: 0.25
  estimated_effort: 1-2 sessions (Session 1 = generator + manifest, Session 2 = baseline + tier wiring)
  expected_delta: 0 direct composite; enables future joint-decoding hypotheses to ground-truth their decodability micro-tests (V3-doctrine closure)
  defensible_prior: yes — hb-088 shelve journal explicitly notes "the actual sub-Costas energy distribution in dense bands is dominated by neighbor leakage and interference, not by weak versions of the truth's own tone pattern." We lack a CORPUS that ground-truths this regime
  wild_card: false
  evidence_for:
    - mr-006 corpus survey 2026-05-25 graded SuperFox / splatter / HF-mobile flutter as dead/marginal/Doppler-subsumed; the missing class for pancetta's research is ground-truthed two-signal scenes.
    - pancetta-ft8's synthetic_audio_generator already supports single-signal synth tones; extending to two-signal scenes is mechanically straightforward (sum two GFSK waveforms with controlled freq/time/SNR offsets).
    - Closes the V3-doctrine loop: future "find more candidates" mechanisms can validate decodability micro-tests against ground truth, not just against jt9 baselines.
  evidence_against:
    - Doesn't move composite directly; it's enabler infrastructure (similar to hb-073's role for hb-015).
    - If no future joint-decoding hypothesis spawns, the corpus may sit unused.
  notes: |
    mr-007 audit: pancetta-ft8's synthetic_audio_generator exists;
    pancetta-research has the fixture/synth tier framework. NEW work:
    extend generator to two-signal scenes parameterized by (freq_offset
    ∈ {12.5, 25, 50, 100} Hz × time_offset ∈ {0, 50, 100, 200} ms ×
    SNR_pair ∈ {(0, -12), (-6, -18), (-12, -24)} dB). Create
    synth_interferer_pair_50 manifest with deterministic seeds. Add to
    composite or as untimed reference tier.

    Kill-switch (corpus quality):
    Generated WAVs pass through pancetta + jt9 + JTDX; all three must
    produce non-empty decodes for the stronger signal at minimum SNR
    pairs, and the weaker signal must be detectable (jt9 alone is
    fine) at favorable separations. Manifest commits when these gates
    pass.

    Source: mr-008 ideation (territory C; complements hb-073/077).

### hb-101 — Soft-output decoder with codeword-posterior export  [PRIORITY: 0.45, spawned 2026-06-01 from architectural ideation]
  mode: ft8
  status: pending
  priority_score: 0.45
  estimated_effort: plan (1 spec + 3-4 sessions)
  expected_delta: distributional output replaces CRC boolean gate; multipass subtract uses MMSE estimate; downstream FP filter consumes continuous trust score
  defensible_prior: yes — list-decoding for LDPC well-studied (Vardy & Be'ery 1991; Hou/Siegel LDPC list-BP); ft8_lib's secondary-decode pass is degenerate K=2
  wild_card: false
  evidence_for:
    - CRC-14 as boolean gate discards BP belief distribution; a near-CRC-passer carries information the current pipeline destroys
    - Downstream (multipass, FP filter, QSO layer) can consume posterior natively; MMSE subtract is cleaner than hard hypothesis
  evidence_against:
    - CRC-14 budget says 1 spurious CRC-passer per 16384 random codewords; list multiplies CRC-budget exposure by K
    - Touches BP post-processing, multipass subtract path, FP filter signature — substantial ripple
  notes: |
    Replace boolean output of LDPC+CRC+plausibility with list of
    (codeword, posterior) tuples. Kill-switch (diagnostic): on top-20
    hard-200 missed truths, BP and enumerate top-K=8 codewords by
    belief sum. PROCEED if (a) truth in top-K ≥15% AND (b) non-truth
    CRC-passer rate ≤5%.

    See research/ideation/2026-06-01-architectural.md (entry A1).

### hb-102 — Probability-of-existence map (continuous Costas gate)  [PRIORITY: 0.40, spawned 2026-06-01 from architectural ideation]
  mode: ft8
  status: pending
  priority_score: 0.40
  estimated_effort: plan (1 spec + 4-6 sessions)
  expected_delta: replaces Costas binary gate with continuous P(FT8-signal-here) map; OSD gets per-position budget multiplier; cuts uniform compute waste
  defensible_prior: yes — Bayesian saliency maps (vision RPNs), probabilistic CFAR in radar; hb-080 multipass implicitly uses adjacency but with binary signal
  wild_card: false
  evidence_for:
    - Costas score is a hard gate; downstream "how much compute should I spend here?" is continuous — gradient thrown away
    - hb-088 sub-Costas LLR sign-agreement work suggests Costas is already info-rich at sub-threshold positions
  evidence_against:
    - More candidates surfaced → runtime cost up; budget multiplier MUST keep total compute bounded
    - Touches every downstream stage's budget-allocation logic
  notes: |
    Map built from Costas score + broadband SNR + residual coherence +
    adjacency to existing decodes. Kill-switch: compute prob-of-existence
    map on top-20 hard-200 (read-only diagnostic). AUC vs ground truth
    ≥0.75 to PROCEED.

    See research/ideation/2026-06-01-architectural.md (entry A2).

### hb-103 — Continuous trust-score FP filter (replace boolean gates)  [PRIORITY: 0.42, spawned 2026-06-01 from architectural ideation]
  mode: ft8
  status: pending
  priority_score: 0.42
  estimated_effort: 2-3 sessions (plan-sized if QSO layer threading in scope)
  expected_delta: calibrated [0,1] trust score replaces boolean FP funnel; τ tunable per operational mode (rare-DX-hunt: low τ; logging: high τ)
  defensible_prior: yes — calibrated classifier output is standard ML (Platt, isotonic); FT8 FP scoring is binary classifier currently at single threshold
  wild_card: false
  evidence_for:
    - Operator cost function depends on use case (log-only vs auto-response); current arch can't express
    - DecodedMessage trust score becomes part of API; QSO layer uses it for confidence weighting
  evidence_against:
    - Changes DecodedMessage API → ripples to QSO layer, ADIF writer, TUI; downstream change cost may dwarf decoder win
  notes: |
    Integrates: BP belief sum, post-CRC margin, Costas sync_score,
    callsign-continuity string-distance, recency-of-callsign.
    Kill-switch: train calibrated classifier on scorecard data; AUC
    ≥0.85 AND dominates current rule stack at some threshold.

    See research/ideation/2026-06-01-architectural.md (entry A3).

### hb-104 — Joint multi-candidate decoder (vector decode, not sequence)  [PRIORITY: 0.48, spawned 2026-06-01 from architectural ideation]
  mode: ft8
  status: pending
  priority_score: 0.48
  estimated_effort: plan (2 specs: scoping + production-design)
  expected_delta: jointly solve for tone amplitudes given full spectrogram and candidate set under LDPC-codeword constraints; structural fix for interferer-dominated sub-Costas positions
  defensible_prior: yes — joint source separation in audio (NMF + sparsity), joint detection in radar (MIMO); aligned with hb-088's "multi-stream separation BEFORE LLR extraction" structural finding
  wild_card: false
  evidence_for:
    - Current pipeline is greedy approximation of this joint problem
    - hb-079 coherent-iterative-subtract win shows that better residual quality unlocks decodes; joint formulation is the limit
  evidence_against:
    - Optimisation may not converge in real-time budget
    - Could end up as research-only diagnostic
  notes: |
    ADMM or alternating-minimisation on amplitudes given fixed
    codewords, BP on each candidate given residual after others
    subtracted. Kill-switch: top-20 hard-200 WAVs with ≥3 overlapping
    decodes, one-step ALS recovers ≥5% more decodes than greedy.

    See research/ideation/2026-06-01-architectural.md (entry A4).

### hb-105 — Decoder fusion at LLR level with jt9 (cross-decoder LLR sum)  [PRIORITY: 0.35, spawned 2026-06-01 from architectural ideation]
  mode: ft8
  status: pending
  priority_score: 0.35
  estimated_effort: 4-6 sessions (research-only first; productionise later)
  expected_delta: sum LLR vectors from pancetta + jt9 at shared candidates, then run LDPC+CRC+plausibility on fused LLRs — MRC at the channel-decoding input
  defensible_prior: yes — MRC in diversity-receiver systems; LLR addition is canonical fusion rule for independent observations of same codeword
  wild_card: false
  evidence_for:
    - jt9 and pancetta have different windowing, sync, tone-mag estimators — complementary observation channels
  evidence_against:
    - License entanglement (jt9 is GPL) — production integration may force GPL boundary or re-implementation
    - jt9 and pancetta may have correlated errors at the same candidate (both use Costas-like sync); LLRs correlated, sum doesn't help
  notes: |
    Kill-switch: for 20 hard-200 WAVs capture jt9 LLRs (diagnostic
    build), at shared (time, freq) candidates compute pancetta-only,
    jt9-only, sum-LLR LDPC pass rates. PROCEED if sum > max + 5pp.

    See research/ideation/2026-06-01-architectural.md (entry A5).

### hb-106 — Variational message-set inference (Bayesian decoder)  [PRIORITY: 0.20 (wild), spawned 2026-06-01 from architectural ideation]
  mode: ft8
  status: pending
  priority_score: 0.20
  estimated_effort: plan (multi-spec; ~10+ sessions research project)
  expected_delta: variational Bayesian posterior over (callsign1, callsign2, locator) directly; structured-message prior integrated into inference rather than post-hoc plausibility
  defensible_prior: partial — structured prediction (CRFs), Bayesian decoding in turbo codes (BCJR); FT8 message structure has ~30 bits of effective entropy after constraints, but engineering risk is high
  wild_card: true
  evidence_for:
    - FT8's source code (message grammar) and channel code are not jointly inferred today
    - Non-CRC-passing codeword decoding to plausible message > one decoding to garbage
  evidence_against:
    - Variational inference may not converge in 15-s slot deadlines
    - Research-project scope; project may not have runway
  notes: |
    Wild-card-adjacent: defensible prior exists, engineering risk high.
    Kill-switch: build generative model for FT8 message tuples;
    for top-200 worst hard-200, prior probability of truth ≥1e-4
    (i.e., ≥13 bits of info beyond uniform).

    See research/ideation/2026-06-01-architectural.md (entry A6).

### hb-107 — Partial-decode first-class object (callsign with no message)  [PRIORITY: 0.32, spawned 2026-06-01 from architectural ideation]
  mode: ft8
  status: pending
  priority_score: 0.32
  estimated_effort: 2-3 sessions for PartialDecode type + emission path; plan-sized if QSO-layer integration in scope
  expected_delta: CRC-failing post-BP codeword whose 28-bit callsign1 hash matches known-callsign set emits PartialDecode; presence-detection for autonomous operator + richer PSKReporter spots
  defensible_prior: yes — 28-bit callsign hash is most structured sub-portion; ft8_lib's hash22 mechanism is a degenerate version
  wild_card: false
  evidence_for:
    - 174-bit codeword splits into ~28 callsign1 + ~28 callsign2 + ~16 locator+type + ~14 CRC + ~5 free; near-decode with right callsign1 bits is still useful
    - QSO layer uses partial for presence detection; PSKReporter benefits
  evidence_against:
    - Random codeword bits collide with hash entries at rate ~|hash_table|/2^28
  notes: |
    Kill-switch: on top-20 hard-200, every CRC-failing post-BP
    codeword, compare first-28 bits against known-hash table.
    PROCEED if ≥10% have known-callsign hash match.

    See research/ideation/2026-06-01-architectural.md (entry A7).

### hb-108 — Time-frequency uncertainty distribution at sync  [PRIORITY: 0.40, spawned 2026-06-01 from architectural ideation]
  closure_reminder_2026_06_01: ADJACENT to hb-086 V3 closure (subtract-aware sync threshold relaxation). V3 surfaced 100-131 "truly new" candidates per WAV in a relaxed-Costas window — all noise. hb-108's grid-search ±2 bins × 5 sub-bin around the Costas point on the SAME residual is a similar relaxation mechanism. The V3 trace showed LDPC always converges on noise at sub-Costas LLRs, CRC catches ~98% as FPs. Differentiator: hb-108 evaluates BEFORE subtract (pass-1 candidate population, not residual) and the grid is around POINT-detected Costas peaks, not relaxation across a continuous threshold band. The kill-switch (≥8% of missed truths decode at some grid point) is the right diagnostic — if it clears against the same population V3 was killed on, the differentiator is real.
  mode: ft8
  status: pending
  priority_score: 0.40
  estimated_effort: 2-3 sessions for grid-search version; 4-6 for posterior-weighted
  expected_delta: replace Costas point estimate with posterior distribution over (t, f, sub) with covariance; weighted-average tone-mag extraction over neighborhood
  defensible_prior: yes — marginalisation over nuisance parameters is Bayesian standard; sub-pixel registration in vision; WSJT-X's f1 sweep is brute-force version
  wild_card: false
  evidence_for:
    - Sync collapses continuous-position uncertainty; for marginal candidates 1-2 bins off true position breaks LDPC
    - hb-079 family's marginal-tail wins hint at sub-Costas information
  evidence_against:
    - Grid search inflates compute by 10x; needs strict candidate pruning
    - Numerical issues with weighted-average tone magnitudes (dB interpretation)
  notes: |
    Cheap path: grid-search ±2 bins × 5 sub-bin around Costas point.
    Kill-switch: ≥8% of missed truths have at least ONE grid point
    that decodes successfully.

    See research/ideation/2026-06-01-architectural.md (entry A8).

### hb-109 — Per-tone confidence propagation (symbol-level posterior throughout)  [PRIORITY: 0.30, spawned 2026-06-01 from architectural ideation]
  mode: ft8
  status: pending
  priority_score: 0.30
  estimated_effort: plan (touches LDPC core BP update rules; numerically tricky)
  expected_delta: non-binary BP over 8-ary alphabet preserves symbol-level correlation that bit-wise LLR projection discards
  defensible_prior: yes — non-binary LDPC well-studied (Davey & Mackay 1998); Q-ary BP on GF(8) groupings of bit-triples; max-log LLR derivation is exactly the lossy step
  wild_card: false
  evidence_for:
    - 8-FSK: single tone determines all 3 bits jointly — bit-LLRs throw away correlation
  evidence_against:
    - Risk of regression on easy decodes (current pipeline is bit-exact with ft8_lib and well-tuned)
    - Deep surgery; numerically tricky
  notes: |
    Kill-switch (theoretical): mutual-information loss from bit-LLR
    projection vs full symbol posterior. <0.1 bits/symbol → no leverage.
    Practical: symbol-aware BP variant on top-20 hard-200; ≥3% recovery
    improvement AND <20% elapsed inflation.

    See research/ideation/2026-06-01-architectural.md (entry A9).

### hb-110 — Learned soft decoder (replace LDPC+OSD with a neural codec)  [PRIORITY: 0.15 (wild), spawned 2026-06-01 from architectural ideation]
  mode: ft8
  status: pending
  priority_score: 0.15
  estimated_effort: plan (research project; 1-3 months realistic)
  expected_delta: end-to-end neural decoder (2-10M params) takes 174 LLRs and outputs 91-bit info-bit posterior; CRC-14 is post-hoc verification only
  defensible_prior: partial — Cammerer/Hoydis "Trainable communication systems"; DeepReceiver in cellular; hobby-scale data/compute mismatch
  wild_card: true
  evidence_for:
    - End-to-end trained decoders capture channel correlations algorithmic decoders can't
  evidence_against:
    - Inference latency: 10M-param transformer per candidate may not fit real-time budget
    - Subtle behavior divergence from ft8_lib that's hard to debug
    - Training-distribution mismatch risk; hobby-scale deployment lacks cellular's data/compute resources
  notes: |
    Wild card: defensible prior exists but pancetta-specific feasibility
    open. Kill-switch: train small (200K param) test model on synth;
    match BP within 5% recovery rate. 50% worse → arch/loss wrong.

    See research/ideation/2026-06-01-architectural.md (entry A10).

### hb-111 — Turbo equalisation (joint channel estimation + decoding)  [PRIORITY: 0.35, spawned 2026-06-01 from architectural ideation]
  mode: ft8
  status: pending
  priority_score: 0.35
  estimated_effort: plan (touches inner decode loop)
  expected_delta: BP output → improved channel estimate (per-tone gain) → re-extract LLRs → BP → ... iterate 2-3 times
  defensible_prior: yes — turbo equalisation in 3GPP/DSL (Tüchler et al. 2002); FT8's selective fading does cause per-tone amplitude variation
  wild_card: false
  evidence_for:
    - Decoder output carries info about channel that's not fed back
    - Selective fading is real on HF; per-tone gain not constant
  evidence_against:
    - Loops within loops within multipass loop; easy runtime regression
    - Convergence not guaranteed; safe-fallback needed
  notes: |
    Kill-switch: on top-20 hard-200 successful decodes, perturb per-tone
    gain (×0.7..1.3 random per tone), re-run BP. <5% convergence
    sensitivity → channel mis-estimation not bottleneck. >20% effect →
    leverage.

    See research/ideation/2026-06-01-architectural.md (entry A11).

### hb-112 — Decoder as Bayesian model averaging over config grid  [PRIORITY: 0.30, spawned 2026-06-01 from architectural ideation]
  mode: ft8
  status: pending
  priority_score: 0.30
  estimated_effort: 2-3 sessions diagnostic; plan-sized for production
  expected_delta: run N=5 decoders with diverse configs; average posterior across decoders weighted by empirical recovery rate per regime
  defensible_prior: yes — Bayesian model averaging foundational; ensembles beat single-best in classification
  wild_card: false
  evidence_for:
    - Hard-band vs sparse-band conditions argue different configs optimise different scenes
    - Adaptive variant (only run secondary configs when prob-of-existence map says should be there) bounds compute
  evidence_against:
    - 5x compute incompatible with real-time on MiniPC target
  notes: |
    Kill-switch: 5 configs offline on top-20 hard-200; union recovery >
    1.1 × best-single recovery. Union ≈ best-single → all configs decode
    the same easy stuff.

    See research/ideation/2026-06-01-architectural.md (entry A12).

### hb-113 — Hierarchical decode: callsign-only fast pass then full given context  [PRIORITY: 0.42, spawned 2026-06-01 from architectural ideation]
  mode: ft8
  status: pending
  priority_score: 0.42
  estimated_effort: plan-sized
  expected_delta: stage 1 light decoder recovers 28-bit callsign1 hash; stage 2 full LDPC+OSD with callsign1 pinned via AP1 driven by evidence-based prior
  defensible_prior: yes — hierarchical decoding standard (turbo constituent, HARQ); evidence-based prior escapes hb-087 shelved finding that external priors don't manufacture signal
  wild_card: false
  evidence_for:
    - Stage 1's output is EVIDENCE-based prior, not external — fundamentally different from hb-087
    - 28-bit callsign subspace exploits hash structure for early commitment
  evidence_against:
    - Stage 1's FP rate: callsign-only decoder will surface many false positives
    - Stage 2's AP-pinning may amplify rather than damp these
  notes: |
    Kill-switch: on top-20 hard-200, for each missed truth, see if
    callsign1 bits in post-BP codeword agree with true callsign hash
    (ignoring CRC). ≥25% callsign-bit agreement → stage-1 has signal.

    See research/ideation/2026-06-01-architectural.md (entry A13).

### hb-114 — Generative-prior decoder (FT8 messages as samples from learned model)  [PRIORITY: 0.18 (wild), spawned 2026-06-01 from architectural ideation]
  mode: ft8
  status: pending
  priority_score: 0.18
  estimated_effort: plan (multi-month research project)
  expected_delta: train autoregressive transformer (~1M params) on decades of FT8 traffic; structured prior over 91-bit message space combines with channel LLRs via Bayes
  defensible_prior: partial — LMs as generative priors for source decoding (Vaswani-era noisy decoding); FT8 corpus exists (PSKReporter archive)
  wild_card: true
  evidence_for:
    - Plausibility check is currently fixed rules; could be learned distribution
    - FT8 grammar is tiny vs free-form text — small model could memorise
  evidence_against:
    - **Cheating concerns**: strong generative prior can hallucinate callsigns; FP-vs-TP separation hard
    - Temptation to overfit to operator's own callsigns is real
  notes: |
    Kill-switch: train small generative model on PSKReporter export
    (~1 day historical). Perplexity < 2^60 (≥30 bits structure beyond
    uniform) on held-out → meaningful prior strength.

    See research/ideation/2026-06-01-architectural.md (entry A14).

### hb-115 — Dual-KiwiSDR space-diversity LLR fusion (MRC across receivers)  [PRIORITY: 0.50, spawned 2026-06-01 from diversity ideation]
  mode: ft8
  status: pending
  priority_score: 0.50
  estimated_effort: plan (3 sessions): harness mod + synth kill-switch + live paired-Kiwi capture
  expected_delta: 2 geographically-separated KiwiSDRs, slot-aligned; LLR-sum at shared candidates equivalent to MRC; +3 dB on independent noise
  defensible_prior: yes — textbook MRC, same trick Q65 uses; hb-075 already proved MRC works for one within-WAV diversity case (GRADUATED 2026-05-29)
  wild_card: false
  evidence_for:
    - hb-075 extends naturally to between-WAV with vastly more independent noise
    - kiwirecorder pair infrastructure half-built (hb-073 procedure doc)
  evidence_against:
    - Common-mode interference (lightning, polar absorption) hits both → MRC degrades to ~+1.5 dB
  notes: |
    Kill-switch: 5 single-RX wild-doppler WAVs; synthesize "second RX"
    by adding independent Gaussian noise of equal variance; LDPC-pass-rate
    of sum vs each individual ≥30% on at least 3/5.

    See research/ideation/2026-06-01-diversity.md (entry D1).

### hb-116 — Decoder-diversity vote: pancetta ⊕ jt9 ⊕ jtdx  [PRIORITY: 0.45, spawned 2026-06-01 from diversity ideation]
  mode: ft8
  status: pending
  priority_score: 0.45
  estimated_effort: 1 session kill-switch; 1 plan for corpus-decode --ensemble mode
  expected_delta: 2-of-3 vote (or 1-of-3 above solo-trust threshold); union strictly larger than any one
  defensible_prior: yes — jtdx outperforms jt9 on certain weak-signal classes; classifier ensembles improve with weakly correlated errors (Dietterich 2000)
  wild_card: false
  evidence_for:
    - Every operator running both WSJT-X and JTDX notices each catches things the other misses
    - 2-of-3 vote rule mitigates FP amplification
  evidence_against:
    - OR-of-three triples FP rate; vote rule caps gain
  notes: |
    Kill-switch: top-50 hard-200, run jt9 + jtdx baseline (jt9 cached).
    |union| > |pancetta-alone| by ≥20 net true-positives → PROCEED.

    See research/ideation/2026-06-01-diversity.md (entry D2).

### hb-117 — AGC-diversity: re-decode at multiple synthetic gain settings  [PRIORITY: 0.28, spawned 2026-06-01 from diversity ideation]
  mode: ft8
  status: pending
  priority_score: 0.28
  estimated_effort: 1 session
  expected_delta: rescale WAV to {-12, 0, +12 dB} → 3 independent decode passes; vote at intersection; perturbs gain-dependent decision boundary
  defensible_prior: partial — pancetta is supposed to be gain-invariant but hb-069 dB-vs-linear shelve proved log-domain matters
  wild_card: false
  evidence_for:
    - Quantization, dynamic-range compression, soft clipping break invariance in practice
  evidence_against:
    - Pancetta's float32 internal pipeline may already be gain-invariant enough; 3 runs converge to same list
  notes: |
    Kill-switch: 20 hard-200, rescale ±12 dB, count NEW true-positives at
    off-baseline gains. <1.0 per WAV mean → too weak to mine.

    See research/ideation/2026-06-01-diversity.md (entry D3).

### hb-118 — IQ-pair-diversity from one KiwiSDR: USB + LSB simultaneous  [PRIORITY: 0.20 (wild), spawned 2026-06-01 from diversity ideation]
  mode: ft8
  status: pending
  priority_score: 0.20
  estimated_effort: 0.5 session kill-switch; 1 plan if PROCEED
  expected_delta: USB + LSB demods on same RF block; partially-independent local noise from filter rolloff asymmetries; spectrogram averaging
  defensible_prior: partial — image-rejection in SDR practice; USB/LSB on same fc do contain partly independent local noise (operationally untested on KiwiSDR specifically)
  wild_card: true
  evidence_for:
    - Sideband-asymmetric interference (carriers, AM splatter) hits only one
  evidence_against:
    - kiwiclient may apply shared AGC across both demods → noise correlation → 1
  notes: |
    Kill-switch: capture 5-min KiwiSDR USB+LSB pair; cross-correlate
    noise floors (signals notched). ρ > 0.85 → mostly common-mode →
    KILL.

    See research/ideation/2026-06-01-diversity.md (entry D4).

### hb-119 — Cross-band conditional prior (20m+40m same QSO)  [PRIORITY: 0.22 (wild), spawned 2026-06-01 from diversity ideation]
  mode: ft8
  status: pending
  priority_score: 0.22
  estimated_effort: 2 sessions: cross-band callsign-index + prior-update primitive + kill-switch
  expected_delta: same callsign on 20m and 40m within ≤10 min = joint observation of operator's message-generation distribution; conditional Bayesian update at AP/prior layer
  defensible_prior: partial — mr-006 noted AP-context worthless offline (ceiling 1/8576); cross-band multi-WAV AP context IS rich because operators don't randomly hop bands
  wild_card: true
  evidence_for:
    - PSK Reporter + cqdx.io spot streams confirm cross-band coherence empirically
  evidence_against:
    - hard-200/-1000 are single-band corpora by construction; cross-band benefit may need multi-band corpus we don't have
  notes: |
    Kill-switch: on hard-1000, true-positive callsigns, count how often
    same callsign appears on different band in corpus. <5% co-occurrence
    → no measurable support.

    See research/ideation/2026-06-01-diversity.md (entry D5).

### hb-120 — Operator-network diversity (decode-hash sharing via cqdx)  [PRIORITY: 0.30, spawned 2026-06-01 from diversity ideation]
  mode: ft8
  status: pending
  priority_score: 0.30
  estimated_effort: plan (4 sessions, slow): protocol + cqdx schema + lite-eval binary + social bootstrap
  expected_delta: 3-5 trusted friends run pancetta-eval-lite, post decode hashes to cqdx; peer confirmation acts as trust-amplifier for marginal decodes
  defensible_prior: yes — PSK Reporter + RBN do this for operator-facing purposes; the novel piece is closing loop back into the decoder
  wild_card: false
  evidence_for:
    - Decision artifacts (174-bit codeword hashes + metadata) are small + privacy-safe
    - Aggregate provably more informative than any single RX (RBN data)
  evidence_against:
    - Cold-start social graph: until 3+ friends opt in, no fleet
    - Credentialed-integrations rule: pancetta can't post on friends' behalf
  notes: |
    Kill-switch: using PSKReporter spot data, for 100 hard-1000 WAVs
    (if RX time known), count how many decodes pancetta misses that
    show up on PSKReporter from another RX within ±10s. <10% → thin
    headroom.

    See research/ideation/2026-06-01-diversity.md (entry D6).

### hb-121 — Time-diversity by long-window IF acquisition (Q65-style)  [PRIORITY: 0.38, spawned 2026-06-01 from diversity ideation]
  closure_reminder_2026_06_01: ADJACENT to hb-074 closure (complex-spectrogram coherent cross-cycle averaging). hb-074 SHELVED because phase estimates on marginal candidates raised sum variance, AND inter-slot phase wasn't reliably preserved in real-world audio. hb-075 (MRC-weighted) was the rescue, GRADUATED non-coherent-style. hb-121's "coherently average IF-level spectrograms" relies on the same coherence assumption that failed for hb-074 on the operator-corpus. The kill-switch noted ("phase-tracker" requirement) confirms the diagnosis. Either: (a) implement non-coherent magnitude averaging gated by codeword match (closer to graduated hb-056/hb-075 territory, may have small upside), or (b) restrict scope to a phase-coherent SDR-IQ corpus (hb-077 territory) to test if the coherence-limit lifts off-corpus.
  mode: ft8
  status: pending
  priority_score: 0.38
  estimated_effort: 2 sessions: repeat-detector + spectrogram-averaging primitive
  expected_delta: detect repeating-message slots via codeword-match; coherently average IF-level spectrograms; ~√N SNR gain
  defensible_prior: yes — Q65 with 60-s slots does this; technique proven for weak HF; mr-008 hb-089 spawned similar residual logic
  wild_card: false
  evidence_for:
    - Many real QSOs repeat the same message across multiple slots
    - hb-089 is residual-side sibling; D7 is raw-spectrogram side with codeword-match gate
  evidence_against:
    - Drift (Doppler, slot-timing, freq) destroys coherence; needs phase-tracker
  notes: |
    Kill-switch: on hard-1000, count how often same callsign appears
    in consecutive slots in same 90s sample. <10% repeat → thin support.

    See research/ideation/2026-06-01-diversity.md (entry D7).

### hb-122 — Multi-sync-window LLR average (intra-decoder algorithmic diversity)  [PRIORITY: 0.40, spawned 2026-06-01 from diversity ideation]
  mode: ft8
  status: pending
  priority_score: 0.40
  estimated_effort: 1 session kill-switch; 1 plan
  expected_delta: 3 parallel demods with different sync-window placements (Costas-only, prefix+Costas, full+postfix); average LLRs pre-LDPC
  defensible_prior: yes — WSJT-X-Improved 3.x ships multiple sync strategies and merges them (a3/a4); JTDX does similar
  wild_card: false
  evidence_for:
    - Cheapest diversity available — no second hardware, no second corpus
    - Pancetta uses one sync strategy throughout; never benchmarked against averaging
  evidence_against:
    - Sync-window choice may already be near-optimal for pancetta's quantization
  notes: |
    Kill-switch: 1 hard-200 WAV, dump LLRs from two sync windows;
    Pearson correlation. ρ > 0.97 → redundant, small gain. 0.7 < ρ
    < 0.9 → real diversity worth full sweep.

    See research/ideation/2026-06-01-diversity.md (entry D8).

### hb-123 — Polarization-diversity emulator via two physical antennas  [PRIORITY: 0.15 (wild), spawned 2026-06-01 from diversity ideation]
  mode: ft8
  status: pending
  priority_score: 0.15
  estimated_effort: plan (5 sessions, real hardware): SDR + driver + dual-audio coordinator + sync-alignment
  expected_delta: 2nd antenna (horizontal dipole or rotated yagi) + 2nd SDR; LLR-sum pre-LDPC; 3-6 dB on HF routinely measured
  defensible_prior: yes — polarization diversity well-proven (NCDXF, ARRL handbook, MFJ-1025); HW + driver complexity makes pancetta integration the open question
  wild_card: true
  evidence_for:
    - Real-physical space/polarization diversity = holy grail of HF diversity reception
    - Brings actual independent noise, not just different filtering
  evidence_against:
    - Pancetta's audio layer assumes one cpal::Stream; coordinator rewrite needed
  notes: |
    Kill-switch: ρ between two antenna noise floors on quiet spectrum.
    ρ < 0.6 → real diversity. ρ > 0.9 → antennas too close.

    See research/ideation/2026-06-01-diversity.md (entry D9).

### hb-124 — cqdx live-spot prior multiplication on marginal decodes  [PRIORITY: 0.32, spawned 2026-06-01 from diversity ideation]
  mode: ft8
  status: pending
  priority_score: 0.32
  estimated_effort: 2 sessions: cqdx-live-stream tap + prior-mult primitive
  expected_delta: P(call active in last 15min) × decoder posterior; marginal decode at LDPC-near-fail accepted iff cqdx confirms callsign active on band
  defensible_prior: yes — Bayesian posterior mixing with empirical prior is sound; related to hb-058 (callsign-prior gate) but uses live spot stream
  wild_card: false
  evidence_for:
    - Most callsigns are silent in any 15-min window; sharp non-uniform prior
  evidence_against:
    - **Self-fulfilling prophecy**: spot stream generated by same decoders pancetta tries to beat; inherits survivorship bias
    - cqdx spot stream lags real-time by 30-90s
  notes: |
    Kill-switch: on hard-1000, per-WAV "active in last 15min" set from
    PSKReporter retrospectively. Of currently-rejected candidates
    (LDPC-near-fail), fraction that would be accepted with prior AND
    are in truth-set. <5% → too weak. >20% → large recall lever.

    See research/ideation/2026-06-01-diversity.md (entry D10).

### hb-125 — USB-audio + KiwiSDR IQ pair fusion (heterogeneous LLR-sum)  [PRIORITY: 0.35, spawned 2026-06-01 from diversity ideation]
  mode: ft8
  status: pending
  priority_score: 0.35
  estimated_effort: plan (3 sessions, blocked on hb-073 capture infra)
  expected_delta: FTdx10 USB audio + KiwiSDR --ncomp IQ on same antenna; processing-induced distortions differ; LLR-sum at intersection
  defensible_prior: yes — hb-077 (phase-coherent SDR-IQ corpus) already scoped this capture path; D11 elevates to decoder fusion
  wild_card: false
  evidence_for:
    - FTdx10 IF filter shape is documented as sharp; KiwiSDR provides flat-passband complementary view
  evidence_against:
    - Requires antenna splitter + KiwiSDR (~$300); splitter loss may exceed diversity gain
  notes: |
    Kill-switch: 10 paired WAV/IQ captures (auroral opening). Decode
    each separately. pancetta-on-IQ finds >5 decodes per WAV that
    pancetta-on-USB misses → PROCEED. Overlap >95% → KILL.

    See research/ideation/2026-06-01-diversity.md (entry D11).

### hb-126 — Adversarial-noise injection diversity (test-time augmentation confidence)  [PRIORITY: 0.20 (wild), spawned 2026-06-01 from diversity ideation]
  mode: ft8
  status: pending
  priority_score: 0.20
  estimated_effort: 1 session kill-switch; 1 plan for FP-filter integration
  expected_delta: decode at 3 injected noise levels; TP-survival distribution differs from FP-survival; per-decode SNR margin estimate for FP filter + QSO machine
  defensible_prior: partial — test-time augmentation is well-established in DL; no FT8 decoder has publicly applied it
  wild_card: true
  evidence_for:
    - Generates per-decode robustness score for free
    - Feeds D2 ensemble vote via per-decode confidence
  evidence_against:
    - 3-4× elapsed cost for confidence signal that may already be encodable from LDPC residual + sync score
  notes: |
    Kill-switch: 20 hard-200, TP-survival distribution at +3/+6/+9 dB
    injected noise; AUC vs FP-survival. AUC < 0.65 → margin estimate
    too weak.

    See research/ideation/2026-06-01-diversity.md (entry D12).

### hb-127 — Public-KiwiSDR scavenging fleet (Phase-5 QSO reliability)  [PRIORITY: 0.18 (wild), spawned 2026-06-01 from diversity ideation]
  mode: ft8
  status: pending
  priority_score: 0.18
  estimated_effort: plan (4 sessions): polling daemon + decode pipeline + QSO-state-machine hook + AUP safety
  expected_delta: 5-10 public KiwiSDRs polled on low duty cycle; any decode of callsign K5ARH has open QSO with = second-source confirmation; reduces missed-RR73 cases
  defensible_prior: partial — kiwirecorder infra exists (hb-073); polling-multiplexer approach novel for FT8
  wild_card: true
  evidence_for:
    - Sometimes Kiwi buddies hear partner when K5ARH doesn't (auroral fade, local QRM)
  evidence_against:
    - KiwiSDR sysops may rate-limit or ban a scraper; AUP compliance non-trivial
  notes: |
    Kill-switch: during Phase-5 trial QSO, manually monitor 3 KiwiSDRs
    at similar latitudes for partner's TX. <1 in 10 slots Kiwi
    decodes partner when K5ARH doesn't → fleet adds nothing.

    See research/ideation/2026-06-01-diversity.md (entry D13).

### hb-128 — Pseudo-pol cross-variance LLR weighting (within-stream emulation)  [PRIORITY: 0.25, spawned 2026-06-01 from diversity ideation]
  mode: ft8
  status: pending
  priority_score: 0.25
  estimated_effort: 1 session
  expected_delta: 2 different audio pre-filters (H-pol-like + V-pol-like emulation) give per-symbol variance signal; symbols with high cross-variance down-weighted before LDPC
  defensible_prior: partial — MIMO channel-uncertainty weighting; within-stream emulation makes it cheap but speculative
  wild_card: false
  evidence_for:
    - hb-079 (residual subtraction weighted decoding) already partially challenged "all symbols equally trustworthy"
  evidence_against:
    - Pseudo-pol filters may both pass same noise (no diversity at all)
  notes: |
    Kill-switch: 20 hard-200, compute variance signal, identify
    high-variance symbols; correlation with LDPC-fail positions. <0.3
    → variance signal uninformative.

    See research/ideation/2026-06-01-diversity.md (entry D14).

### hb-129 — Time-to-first-decode (TTFD) per-slot metric  [SHIPPED-INFRA 2026-06-01 — sidecar scorecard field; no decoder behavior change]
  # Phase A re-label (2026-06-01): originally tagged GRADUATED. The TTFD
  # metric is a sidecar scorecard field — instrumentation only, no production
  # default flipped, composite unchanged by construction. Per the
  # Phase A honesty-pass definition, INFRA-only changes are SHIPPED-INFRA,
  # not GRADUATED. See docs/engineering/2026-06-02-engineering-substance-audit.md
  # and research/experiments/2026-06-01-phase-b-bootstrap-ci.md.
  mode: ft8 (metric/instrumentation)
  status: SHIPPED-INFRA — sidecar metric live; no decoder change, composite unchanged.
  priority_score: 0.0  # was 0.45; instrumentation has shipped
  estimated_effort: 1 session (actual: ~50 min)
  expected_delta: median TTFD ttfd_score = clamp((15.0 - median_ttfd_s) / 15.0, 0, 1); re-ranks hb-091 (a8 early-decode) up, hb-079 (multipass) down; surfaces hb-093 (residual SNR gate) as operationally attractive
  defensible_prior: yes — WSJT-X-Improved a8 mode markets "0.5-1s early decode" as headline feature; TTFD is what they're optimizing
  wild_card: false
  evidence_for:
    - For autonomous TX scheduling, decode at T+9 vs T+14 = difference between "tx next slot" and "defer 30s"
    - pancetta-ft8 already has decode-emit ordering; ~50 LOC harness change
  evidence_against:
    - Gameable by early FP emissions; must combine with precision gate
  notes: |
    SHIPPED-INFRA — TTFD metric live. `DecodedMessage` carries
    `decode_time_into_window: Option<Duration>`; stamped at CRC-pass site
    in par_decode_candidate / par_try_ap_decode /
    par_try_ldpc_with_recent_only, plus caller-side stamping for cross-
    cycle, coherent multipass, joint-pair-retry, and joint-residual passes.
    Scorecard `TierResult` gains `ttfd_distribution:
    Option<TtfdDistribution>` (wavs_with_decode + p50/p90/mean +
    per_wav_seconds). Eval prints summary line for the curated tier.

    Hard-200 numbers (single run, default config): n=200/200 WAVs
    produced stamped decodes, p50=22.4 ms, p90=47.5 ms, mean=32.5 ms,
    min=13.4 ms, max=373.0 ms. The metric measures CPU wall-clock to
    first CRC pass, NOT slot-arrival time — pancetta processes the
    whole 12.64s window offline, so the first decode lands within tens
    of milliseconds of pipeline start. Variance (13-373 ms range)
    reflects per-WAV candidate density (more candidates → more
    parallel work → later first-decode).

    Caveat for hypothesis ranking: the original M1 framing
    (`clamp((15.0 - median_ttfd_s) / 15.0)`) was designed for a
    REAL-TIME streaming decoder where the first decode emerges at
    T+8-T+14s into a 15s slot. Pancetta's offline pipeline collapses
    everything into <50 ms wall-clock. To make TTFD useful for
    re-ranking hb-079 (multipass) vs hb-091 (a8 early-decode), a
    follow-up needs to either (a) measure WHICH PASS produced the
    decode (pass-1 vs pass-2 vs multipass vs joint-pair-retry —
    pass identity is recoverable from the stamping order), or (b)
    instrument the streaming decoder path (when one exists) to give
    audio-arrival-relative timing.

    Tests: 4 new unit tests for TtfdDistribution aggregation;
    test_ttfd_stamping_on_synth_signal in pancetta-ft8 verifies every
    pipeline decode carries a non-zero stamp.

    See research/ideation/2026-06-01-metric.md (entry M1).

### hb-130 — Operational-value-weighted recall  [PRIORITY: 0.40, spawned 2026-06-01 from metric ideation]
  mode: ft8 (metric/instrumentation)
  status: pending
  priority_score: 0.40
  estimated_effort: 2 sessions
  expected_delta: weight each truth by priority score from pancetta-qso/src/priority.rs (needed-DXCC, needed-grid, POTA/SOTA, rarity); inverts which novels matter; may unshelve hb-064 if missed novels are common US calls
  defensible_prior: yes — direct match to operator behavior; priority scorer is production code; metric is re-aggregation of existing signal
  wild_card: false
  evidence_for:
    - K5ARH would choose +1 JA/day over +50 US/day
  evidence_against:
    - Distorts evaluation toward "DX bias" — masks real recall regressions on US calls operators still want
  notes: |
    Kill-switch: Spearman(opv_recall, plain_recall). >0.95 → collapses
    to plain recall, uninteresting. <0.7 → hypothesis ranking diverges,
    PROCEED. Floor weight (every truth ≥0.1).

    See research/ideation/2026-06-01-metric.md (entry M2).

### hb-131 — QSO-completion-rate metric on simulated rig loop  [PRIORITY: 0.35, spawned 2026-06-01 from metric ideation]
  mode: ft8 (metric/instrumentation)
  status: pending
  priority_score: 0.35
  estimated_effort: 3 sessions
  expected_delta: replay hard-200 through full encode→modulate→decode→state-machine→tx→decode-next loop with simulated DX responder; QSO completion fraction is score
  defensible_prior: yes — pancetta's purpose per CLAUDE.md is "decode, call, complete QSOs, log"
  wild_card: false
  evidence_for:
    - Loopback QSO tests exist; ~500 LOC for replay harness
    - Measures *actionable* decodes; rewards decoder that recovers QSO-relevant exchanges
  evidence_against:
    - Compute-expensive; distorted by state-machine sequencer config
  notes: |
    Kill-switch: Pearson(M3, recall) > 0.95 across 20 sweep configs →
    redundant, abandon.

    See research/ideation/2026-06-01-metric.md (entry M3).

### hb-132 — Precision-recall AUC with FP-injection corpus  [PRIORITY: 0.38, spawned 2026-06-01 from metric ideation]
  mode: ft8 (metric/instrumentation)
  status: pending
  priority_score: 0.38
  estimated_effort: 1 session to build FP-injection corpus + measure
  expected_delta: augment eval corpus with N synthetic garbage slots (noise + adversarial near-Costas patterns); PR-AUC sweep over min_sync_score and max_parity_errors_for_osd
  defensible_prior: yes — PR-AUC standard for binary classification under imbalanced labels
  wild_card: false
  evidence_for:
    - Today FP filter is binary gate; M4 measures robustness of precision under stress
    - hb-052 (production FP filter) gains explicit credit
  evidence_against:
    - FP-injection corpus is synthetic; may not represent real-world FPs
  notes: |
    Kill-switch: PR-AUC tracks plain recall (Pearson > 0.95) →
    reveals nothing. Decoder shows precision cliff below threshold →
    load-bearing.

    See research/ideation/2026-06-01-metric.md (entry M4).

### hb-133 — Saturation-aware composite (corpus-shift-robust)  [GRADUATED 2026-06-01]
  mode: ft8 (metric/instrumentation)
  status: GRADUATED — RefreshOffsetRegistry + saturation_aware_composite() in pancetta-research/src/metrics.rs; sidecar at research/scorecards/refresh_offsets.json. eval binary prints both raw + saturation-aware composite. Seeded with the 2026-05-30 hard-200 refresh: offset +0.009699 (raw 0.579114 → saturation-aware 0.569415, exactly reconstructing the pre-refresh baseline within 1e-6 rounding).
  priority_score: 0.52
  estimated_effort: 1 session — pure aggregation change, no decoder change
  expected_delta: score' = score(current_decoder, current_corpus) - score(prev_main, current_corpus) + score(prev_main, prev_corpus); corpus refresh becomes automatic; cumulative graduations survive
  defensible_prior: yes — standard practice in evolving benchmarks (SQuAD → SQuAD 2.0, ImageNet test-set rotation)
  wild_card: false
  evidence_for:
    - Removes pressure to delay corpus refresh; unblocks corpus survey recommendation
    - Highest work-to-impact ratio of the 15 metric ideas (per ideation summary)
    - Saturation-aware value 0.569415 vs pre-refresh main.json's 0.569415 → cross-refresh comparability restored
  evidence_against:
    - Hides real recall growth: 5% better decoder + 5% harder corpus = 0% change
  notes: |
    "Definitely worth shipping" per metric ideation summary. Stable
    virtual baseline across corpus rotations.

    Implementation: `RefreshOffset` + `RefreshOffsetRegistry` in
    pancetta-research/src/metrics.rs; sidecar JSON registry at
    research/scorecards/refresh_offsets.json (append-only, never edit
    historical entries). Scorecards on disk are unmodified — offsets are
    applied at read-time only via `saturation_aware_composite(raw, reg)`.

    Future refresh procedure: when the corpus rotates, run the previous
    main decoder against both the old and new corpus (or rely on the
    archived pre/post main scorecards if same decoder), compute
    `offset = score(prev_main, new_corpus) - score(prev_main, old_corpus)`,
    and append a new entry to `refresh_offsets.json`.

    See research/ideation/2026-06-01-metric.md (entry M5).

### hb-134 — Per-band-density-stratified recall  [PRIORITY: 0.35, spawned 2026-06-01 from metric ideation]
  mode: ft8 (metric/instrumentation)
  status: pending
  priority_score: 0.35
  estimated_effort: 1 session — pure post-processing
  expected_delta: bucket WAVs by pancetta_decode_count {sparse, medium, dense}; bucket weight = empirical fraction of K5ARH's slots in that bucket from recent 7-day capture
  defensible_prior: yes — standard stratification for samples with sub-populations of different difficulty
  wild_card: false
  evidence_for:
    - hb-086 V1 wins on dense slots; hb-079 wins everywhere — stratification surfaces which iter targets which regime
  evidence_against:
    - Bucket boundaries arbitrary; results sensitive to binning
  notes: |
    Re-aggregates today's results. Report both quartiles + weighted sum.

    See research/ideation/2026-06-01-metric.md (entry M6).

### hb-135 — CPU-cost-adjusted recall (decodes/second or /joule)  [PRIORITY: 0.32, spawned 2026-06-01 from metric ideation]
  mode: ft8 (metric/instrumentation)
  status: pending
  priority_score: 0.32
  estimated_effort: 2 sessions (1 for elapsed proxy + integration; 1 for joule if needed)
  expected_delta: turn elapsed-time deltas into first-class composite term; hb-093 becomes highest-priority; wild-card hb-094 potentially regresses heavily
  defensible_prior: yes — pancetta's target hardware is MiniPC running Win11; field-deployed FT8 is CPU-constrained
  wild_card: false
  evidence_for:
    - Multipass = 3× decode time; cumulative cost across 27 graduations unmeasured
    - elapsed_seconds already in scorecard
  evidence_against:
    - Discourages all algorithmic recall improvements; needs Pareto-aware composite
  notes: |
    Pareto-aware: penalize only Pareto-dominated configs. Cheap first
    pass = elapsed-seconds proxy; defer joule measurement.

    See research/ideation/2026-06-01-metric.md (entry M7).

### hb-136 — First-derivative recall (Δ-recall vs main per iter)  [PRIORITY: 0.25, spawned 2026-06-01 from metric ideation]
  mode: ft8 (metric/instrumentation)
  status: pending
  priority_score: 0.25
  estimated_effort: 1 session
  expected_delta: per-iter (recall_iter - recall_main) / hours_since_last_graduation; integral over time = project value; plateaus explicit
  defensible_prior: yes — standard project-velocity metric; "time to plateau" defined in BO/active-learning literature
  wild_card: false
  evidence_for:
    - All graduation timestamps + deltas in scorecards/journal.md
    - Forces strategic question: when to switch from decoder to operational work
  evidence_against:
    - Discourages compound wins; 3 iters of +0.001 each looks worse than single +0.003
  notes: |
    Report both integral AND peak. Velocity dashboard, not headline.

    See research/ideation/2026-06-01-metric.md (entry M8).

### hb-137 — Adversarial-corpus recall (jt9-only-hits subset)  [PRIORITY: 0.48, spawned 2026-06-01 from metric ideation]
  mode: ft8 (metric/instrumentation)
  status: pending
  priority_score: 0.48
  estimated_effort: 1 session to build manifest + integrate into harness
  expected_delta: targets the 30% real recall headroom (corpus survey 2026-05-30 found 8.7 jt9-only decodes/slot); every iter competes on the hardest 30% where pancetta is WORSE than jt9
  defensible_prior: yes — standard adversarial benchmarking; train on where you fail; closes "curated by our own decoder" loop
  wild_card: false
  evidence_for:
    - hard-200 was curated BY pancetta — contains the subset where pancetta does decode; this inverts
  evidence_against:
    - Subset shrinks as decoder improves (success → metric disappears); needs refresh policy
  notes: |
    Highest-recall-leverage idea per metric ideation summary. Re-run
    2026-05-30 survey on full 2066-WAV capture; freeze top jt9-only as
    adversarial_174.manifest.json. ~3-5 hours compute.

    See research/ideation/2026-06-01-metric.md (entry M9).

### hb-138 — DXCC-coverage rate (unique entities per hour)  [PRIORITY: 0.25 (wild), spawned 2026-06-01 from metric ideation]
  mode: ft8 (metric/instrumentation)
  status: pending
  priority_score: 0.25
  estimated_effort: 2 sessions
  expected_delta: unique_DXCC_entities_in_decodes / hours_of_recording; what a contester optimizes; eliminates per-slot recall game
  defensible_prior: yes — direct match to operator behavior; DXCC count is the actual scoring system of the hobby
  wild_card: true
  evidence_for:
    - A decoder catching 1 JA worth more than catching 50 US callsigns
    - Strongly correlated with M2 but at session granularity
  evidence_against:
    - Strongly diurnal — same decoder scores wildly differently on dawn-gray-line vs midday
  notes: |
    Mitigate diurnal with rolling 24h windows. Requires DXCC mapping
    (cqdx.io has this; offline snapshot feasible).

    See research/ideation/2026-06-01-metric.md (entry M10).

### hb-139 — Information-theoretic recall (Shannon-weighted bits/slot)  [PRIORITY: 0.20 (wild), spawned 2026-06-01 from metric ideation]
  mode: ft8 (metric/instrumentation)
  status: pending
  priority_score: 0.20
  estimated_effort: 1 session (if cqdx.io spot histogram available)
  expected_delta: Σ_decoded log₂(1/p(message)); rare callsigns + unusual grids get more bits-of-surprise; information-theoretic measure of novelty
  defensible_prior: yes — cross-entropy/KL-divergence standard in any imbalanced-class benchmark
  wild_card: true
  evidence_for:
    - Defensible mathematical basis (no human-tuned weights like M2)
    - "Decoder X gains 14.7 bits/slot vs Y" — interpretable with M10
  evidence_against:
    - Abstract; hard to translate to "X is better"
  notes: |
    Kill-switch: Pearson(M11, plain_recall) > 0.9 → uninteresting.
    Pair with M10 for grounded interpretation.

    See research/ideation/2026-06-01-metric.md (entry M11).

### hb-140 — Counterfactual QSO-completion impact (causal A/B replay)  [PRIORITY: 0.22 (wild), spawned 2026-06-01 from metric ideation]
  mode: ft8 (metric/instrumentation)
  status: pending
  priority_score: 0.22
  estimated_effort: 4 sessions total (infra-heavy: replayable QSO machine + simulated DX responder + variance estimator)
  expected_delta: completed_qsos(candidate) - completed_qsos(main); reduces all proxies to single operational outcome
  defensible_prior: yes — counterfactual treatment effect from causal inference; standard for A/B-testing systems
  wild_card: true
  evidence_for:
    - A decoder that helps complete 1 extra QSO/hour is worth graduation regardless of micro-recall delta
  evidence_against:
    - High variance per session; need many replays per config; expensive
    - Tightly coupled to autonomous-config (rerank when priority weights change)
  notes: |
    Probably right metric long-term but heavy lift. Subset of M15.

    See research/ideation/2026-06-01-metric.md (entry M12).

### hb-141 — Cross-decoder-disagreement recall (unique-to-pancetta)  [PRIORITY: 0.30, spawned 2026-06-01 from metric ideation]
  mode: ft8 (metric/instrumentation)
  status: pending
  priority_score: 0.30
  estimated_effort: 1 session (metric); corpus baselining is one-time cost (~2 days)
  expected_delta: subset = {truths jt9 misses AND jtdx misses AND wsjt-x-improved misses}; recall on that set; unique-value metric
  defensible_prior: yes — software diversity literature; coverage of "what only this implementation finds" is standard quality measure
  wild_card: false
  evidence_for:
    - Distinguishes hb-079 wins from baseline-equivalent wins
  evidence_against:
    - Small absolute numbers; hard to drive a hill-climb against
  notes: |
    Tracking metric, not headline. Baseline jtdx + wsjt-x-improved on
    1000 WAVs is the upfront cost.

    See research/ideation/2026-06-01-metric.md (entry M13).

### hb-142 — Listen-only Pareto: (recall, latency, CPU%) frontier  [PRIORITY: 0.20 (wild), spawned 2026-06-01 from metric ideation]
  mode: ft8 (metric/instrumentation)
  status: pending
  priority_score: 0.20
  estimated_effort: 2 sessions
  expected_delta: 3D Pareto frontier; config is dominated if some other beats it on all 3 axes; graduation = frontier-membership
  defensible_prior: yes — standard multi-objective optimization (NSGA-II); used in compiler benchmarks, ML inference optimization
  wild_card: true
  evidence_for:
    - Removes single-scalar tyranny; forces every graduation to make explicit which axis it sacrifices
  evidence_against:
    - No single number = harder to communicate, harder to diff iters
  notes: |
    Probably complement to scalar composite, not replacement.
    Measurable today (M1 + M7 + recall).

    See research/ideation/2026-06-01-metric.md (entry M14).

### hb-143 — Operator-day-replay composite (8h K5ARH session, end-to-end QSO)  [PRIORITY: 0.25 (wild, FLAGSHIP), spawned 2026-06-01 from metric ideation]
  mode: ft8 (metric/instrumentation)
  status: pending
  priority_score: 0.25
  estimated_effort: 5 sessions (biggest single metric infra investment)
  expected_delta: (NEW_DXCC×1.0 + NEW_grids×0.5 + POTA×0.4 + other×0.1) per priority weights; the composite IS what the project ships; operator-points/day
  defensible_prior: yes — end-to-end task evaluation is gold standard; NLP moved from BLEU → downstream task accuracy for the same reason
  wild_card: true
  evidence_for:
    - Subsumes recall, latency, precision, CPU-cost via QSO outcomes
    - "Right metric" per metric ideation summary
  evidence_against:
    - Variance per replay too high to compare iters; replays take hours
    - DX responder simulation is bottleneck; only as good as parametrization
  notes: |
    M15 is "most-likely-to-change-strategy". Probably monthly dashboard
    not per-iter. Worth building even if iters use cheap proxy.

    See research/ideation/2026-06-01-metric.md (entry M15).

### hb-144 — Cross-decoder consensus truth corpus (panc ∩ jt9 ∩ jtdx)  [PRIORITY: 0.45, spawned 2026-06-01 from corpus ideation]
  mode: ft8 (corpus/eval infrastructure)
  status: pending
  priority_score: 0.45
  estimated_effort: ~4 hours integration + 30 min jtdx install; zero new audio
  expected_delta: tier with truth = intersection of pancetta + jt9 + jtdx; novel decodes scored separately as candidate; lowers inflated novel number; unblocks hb-064 (pruning value invisible if truth noisy)
  defensible_prior: yes — academic LDPC papers routinely cite "intersection of N decoders" as gold-truth; jt9 + jtdx deliberately independent codebases
  wild_card: false
  evidence_for:
    - Top-3 most-likely-to-unblock-shelved-hypotheses (corpus ideation top-3 #2)
  evidence_against:
    - jtdx is moving-target fork; need pinned version + commit to operator's machine
  notes: |
    Kill-switch: jtdx adds <5% novel decodes vs jt9 on 20-WAV pilot →
    consensus tier collapses to ~jt9 truth.

    See research/ideation/2026-06-01-corpus.md (entry C1).

### hb-145 — Pancetta-self-truth recursive refinement (stability tier)  [PRIORITY: 0.30, spawned 2026-06-01 from corpus ideation]
  mode: ft8 (corpus/eval infrastructure)
  status: pending
  priority_score: 0.30
  estimated_effort: ~6 hours
  expected_delta: truth = pancetta@HEAD high-confidence decodes (max budget); recall measures pancetta@CANDIDATE under deployment budget; stability metric (not recall metric)
  defensible_prior: yes — bootstrapping classifiers on confident predictions is standard semi-supervised ML (noisy-student); conformal prediction
  wild_card: false
  evidence_for:
    - Captures pancetta-specific behavior external decoder is blind to (V3 subtract-aware sync gains)
    - Right instrument for hb-016 residual-energy-stop, hb-068 b-scale tuning
  evidence_against:
    - Self-truth creates fixed point; decoder learns to recover its own decodes
  notes: |
    Composite weight near zero; pair with cross-decoder consensus (C1).
    Kill-switch: max-budget vs deployment-budget recall ratio > 99% → no
    headroom.

    See research/ideation/2026-06-01-corpus.md (entry C2).

### hb-146 — Synthetic adversarial corpus targeting measured walls  [SHIPPED-INFRA 2026-06-01 — mutual-masking pair sub-family; sub-Costas + 3+ stacks deferred]
  # Phase A re-label (2026-06-01): new corpus tier — decoder unchanged.
  # SHIPPED-INFRA, not GRADUATED. V2/V3 re-eval on this corpus is the
  # binding decoder-change test (flagged synth_pair_revisit_candidate).
  mode: ft8 (corpus/eval infrastructure)
  status: SHIPPED-INFRA 2026-06-01 — synth-pair-200 corpus + generator + tier wired; baseline measured; V2 + V3 flagged synth_pair_revisit_candidate.
  priority_score: 0.0  # tier infra landed; V2/V3 re-eval are separate hypotheses
  estimated_effort: ~1 day curation + ~6 hours integration; collection synthetic (free)
  expected_delta: 3 sub-families (mutual-masking pairs, sub-Costas signals, 3+ collision stacks); directly resurrects shelved V2/V3 if win on adversarial tier
  defensible_prior: yes — adversarial ML standard; WSJT-X test suite includes contrived test cases; MAP-equivalent decoder papers use targeted stress
  wild_card: false
  evidence_for:
    - Top-3 most-likely-to-unblock-shelved-hypotheses (corpus ideation top-3 #1)
    - V2 (shelved) gets +20% on pair tier → V2 unshelves tomorrow
  evidence_against:
    - Overfitting to adversarial; needs co-improvement on hard-200 for graduation
  notes: |
    Diagnostic tier, never primary. Extends hb-100's interferer-pair
    corpus with sub-Costas + multi-signal families.

    See research/ideation/2026-06-01-corpus.md (entry C3).

  status_2026_06_01: |
    SHIPPED. Mutual-masking pair sub-family landed; sub-Costas + 3+
    collision stack sub-families deferred to follow-up iters (separate
    generator configs, same infrastructure).

    - generator: pancetta-research/src/bin/gen_synth_pair.rs
      (registered in pancetta-research/Cargo.toml as gen-synth-pair).
    - config:   research/corpus/synth/manifests/synth_pair_200.config.json
      (180 WAVs after stride-2 subsample of 360-point grid:
      6 templates × 5 ΔSNR × 4 Δf × 3 Δt; strong_snr_db -8 dB).
    - manifest: research/corpus/synth/manifests/synth_pair_200.manifest.json
      (deterministic seeds; regeneration is byte-identical).
    - eval tier: `synth-pair-200` (matches existing missing-manifest
      pattern). Reports per-(ΔSNR, Δf, Δt) regime map to stderr;
      truth_decodes_total/recovered/decode_rate to scorecard.
    - baseline scorecard:
      research/scorecards/sweep/synth-pair-200-baseline.json
      • Strong recovered: 177/180 (98.3%)
      • Weak recovered:    92/180 (51.1%)
      • Regime: weak recovery 0% in ΔSNR ≥ 9 dB AND Δf ≤ 12 Hz; weak
        recovery 100% at Δf=50 Hz across all ΔSNR. The marginal-SNR
        neighbor structure V2 needs is present by construction.
    - journal: research/experiments/2026-06-01-hb-146-synth-pair-corpus.md
    - V2 + V3 flagged with `synth_pair_revisit_candidate: true`
      (hb-086 V2 / V3 entries above).

### hb-147 — Continuous multi-hour single-band time-series corpus  [PRIORITY: 0.32, spawned 2026-06-01 from corpus ideation]
  mode: ft8 (corpus/eval infrastructure)
  status: pending
  priority_score: 0.32
  estimated_effort: ~3 days integration (heavy front-loaded)
  expected_delta: 24h continuous slice; per-15min-window decode count, per-hour callsign diversity, QSO completion rate; QSO-chain tracking across windows
  defensible_prior: yes — time-series eval standard in radio-propagation research (ITU-R, Chen et al. 2024); PSKReporter UI defaults to time-series
  wild_card: false
  evidence_for:
    - K5ARH already captures continuously on Phase-5 deployment
    - Surfaces hidden temporal failure modes (e.g., decoder change disrupts QSO chains)
  evidence_against:
    - Heavy infrastructure for unclear hypothesis-unblock value; pilot first
  notes: |
    Pilot with 1h data + 50-slot window scoring before scaling.

    See research/ideation/2026-06-01-corpus.md (entry C4).

### hb-148 — Operator-curated rare-DXCC tier (matters-to-K5ARH)  [PRIORITY: 0.35, spawned 2026-06-01 from corpus ideation]
  mode: ft8 (corpus/eval infrastructure)
  status: pending
  priority_score: 0.35
  estimated_effort: 1-2 weeks operator attention (low intensity) + ~6 hours integration
  expected_delta: 100-200 slots with manually-flagged needed-DXCC/rare-grid; recall on rare-station-decode only; +5% recall on common + lose 1 rare = deployment regression
  defensible_prior: yes — operator-mission-value weighting standard in radar/sonar; one VK0 or P5 worth 1000 W1AW decodes
  wild_card: false
  evidence_for:
    - cqdx priority scoring exists for a reason
  evidence_against:
    - Small sample (20-200 slots) is statistically noisy; require ≥20% effect size
  notes: |
    Kill-switch: 20 rare-DXCC slots flagged in one weekend; pancetta
    misses 5+ that WSJT-X catches → clear unblock value.

    See research/ideation/2026-06-01-corpus.md (entry C5).

### hb-149 — Pre-Costas-failure tier (zero-decode slots)  [PRIORITY: 0.30, spawned 2026-06-01 from corpus ideation]
  mode: ft8 (corpus/eval infrastructure)
  status: pending
  priority_score: 0.30
  estimated_effort: ~2h collection + 2h curation + 1h integration
  expected_delta: 200 slots where pancetta produces ZERO decodes; truth = jt9 + jtdx union; recall measures "can we crack a slot pancetta considers empty?"
  defensible_prior: yes — information-retrieval evaluation routinely includes "hard negatives"
  wild_card: false
  evidence_for:
    - Failure modes here are upstream of LDPC/OSD (Costas, spectrogram noise floor, sync)
  evidence_against:
    - Zero-decode slots may be genuinely silent; high risk of building noise tier
  notes: |
    Kill-switch: 50 zero-decode slots, run jt9. >5/50 → tier has signal.
    0/50 → genuinely empty.

    See research/ideation/2026-06-01-corpus.md (entry C6).

### hb-150 — High-jt9-novel-density tier (jt9-beats-pancetta inverse)  [SHIPPED-INFRA 2026-06-01 — new corpus tier, no decoder change]
  # Phase A re-label (2026-06-01): tier infrastructure only — no decoder
  # change, no production default flipped, composite unchanged by
  # construction. SHIPPED-INFRA per the Phase A honesty pass.
  mode: ft8 (corpus/eval infrastructure)
  status: SHIPPED-INFRA 2026-06-01 — 200 WAVs curated, baseline recall 44.23%
  priority_score: 0.50
  estimated_effort: ~3h curation + 2h integration; zero new audio
  expected_delta: 200 slots where jt9_count - pancetta_count >= 5; truth = jt9 decodes; recall on bigger find list; directly measures the pancetta-vs-jt9 gap
  defensible_prior: yes — stratified sampling against failure axis is standard (BIG-bench, GLUE adversarial, ImageNet-A)
  wild_card: false
  evidence_for:
    - Cheapest corpus to bootstrap (zero new audio; survey already computed deltas on 20 WAVs)
    - Top-3 most-likely-to-unblock-shelved-hypotheses (corpus ideation top-3 #3)
  evidence_against:
    - Could be dominated by Costas-fail slots (C6 overlap); require pancetta_count > 0 to measure "ranking + LLR"
  notes: |
    "Cheapest corpus to bootstrap" per corpus ideation summary.
    Extends survey scoring to full archive; pick top-200 by jt9-only.

    See research/ideation/2026-06-01-corpus.md (entry C7).

  status_2026_06_01: |
    SHIPPED. Tier curated, eval-dispatch arm wired, baseline measured.

    - manifest: research/corpus/curated/ft8/hard_jt9_rich_200.manifest.json
    - eval tier name: `hard-jt9-rich-200` (matches wild-doppler-50
      missing-manifest SKIP pattern; not a composite term)
    - curation pass: 1317 baselines, 1100 pancetta_decode_count from
      snapshot, 217 WAVs freshly decoded with current default config,
      top-200 by jt9_novel_density (gap = jt9_count - pancetta_count).
      Gap range 22..66, mean 28.93.
    - baseline scorecard: research/scorecards/sweep/hard-jt9-rich-200-baseline.json
      • 200 WAVs, 9385 jt9 truth decodes, 4151 recovered → **44.23%**
        decode_rate (vs 55.82% on curated-hard-200, a ~11.6 pp gap).
      • Missed-truth headroom: 5234 (≈ 5× larger than the 3911 on
        curated-hard-200) — gives sync / LLR / ranking experiments a
        clean denominator.
    - journal: research/experiments/2026-06-01-hb-150-jt9-rich-tier.md
    - INFRA only. No decoder change. composite unchanged.
    - Unblocks hb-015 family (sync-resilience experiments now have a
      tier that directly measures the pancetta-vs-jt9 recall gap) and
      future bias-detection / callsign-prior FP-audit work.

### hb-151 — Multi-band simultaneous capture (cross-band consistency tier)  [PRIORITY: 0.25, spawned 2026-06-01 from corpus ideation]
  mode: ft8 (corpus/eval infrastructure)
  status: pending
  priority_score: 0.25
  estimated_effort: ~1 week collection + ~6h curation + ~4h integration
  expected_delta: 100 slot-aligned WAVs × 3-4 bands (20/40/15/17m); per-band recall + cross-band-consistency sub-metric; band-stability check
  defensible_prior: yes — WSJT-X tested across bands via operator reports; PSKReporter aggregates cross-band; academic radio papers
  wild_card: false
  evidence_for:
    - 40m broadcast intrusion, 15m absorption notch — decoder change for 20m may hurt 40m
  evidence_against:
    - Equipment dependency: multi-RX or scheduled Kiwi captures
  notes: |
    Pilot dual-band 20m+40m Kiwi; if decode distributions within 10% →
    band-invariance holds, low-value. Don't schedule until C5/C7 land.

    See research/ideation/2026-06-01-corpus.md (entry C8).

### hb-152 — Multi-station propagation diversity corpus  [PRIORITY: 0.32, spawned 2026-06-01 from corpus ideation]
  mode: ft8 (corpus/eval infrastructure)
  status: pending
  priority_score: 0.32
  estimated_effort: ~3 days collection + ~1 day curation + ~6h integration
  expected_delta: same QSO at 3+ RX (K5ARH + 3 Kiwis); truth = union; per-RX recall stability; upper-bound recall measurement
  defensible_prior: yes — WSPRnet uses multi-receiver diversity; RBN multi-RX by design; Joe Taylor cites multi-RX as truth ceiling
  wild_card: false
  evidence_for:
    - Per-RX noise structure invisible without this
  evidence_against:
    - Receiver clock skew can misalign slots; KiwiSDR public availability unpredictable
  notes: |
    Pilot: 15-min capture × 3 Kiwis + K5ARH (240 WAVs). Union > any
    single RX by >30% → stricter truth, worth scaling.

    See research/ideation/2026-06-01-corpus.md (entry C9).

### hb-153 — Greyline-window targeted capture tier  [PRIORITY: 0.30, spawned 2026-06-01 from corpus ideation]
  mode: ft8 (corpus/eval infrastructure)
  status: pending
  priority_score: 0.30
  estimated_effort: ~1 week passive capture + ~3h curation + ~2h integration
  expected_delta: ±30 min sunrise/sunset windows; ~840 slots over 7 days; real-Doppler regime BEFORE hb-073's KiwiSDR auroral lands
  defensible_prior: yes — greyline DXing documented FT8 phenomenon (KH6 at NA sunrise); academic propagation papers cite as primary terminator-physics test case
  wild_card: false
  evidence_for:
    - Geographically accessible without operator travel
  evidence_against:
    - Greyline windows short (30-60 min); seasonal sunrise/sunset shifts
  notes: |
    Kill-switch: 4 days × 2 windows. jt9 count distribution differs by
    >20% (more DX, lower median SNR) from midday → regime is distinct.

    See research/ideation/2026-06-01-corpus.md (entry C10).

### hb-154 — Contest-mode pileup corpus  [PRIORITY: 0.28, spawned 2026-06-01 from corpus ideation]
  mode: ft8 (corpus/eval infrastructure)
  status: pending
  priority_score: 0.28
  estimated_effort: 1 contest weekend collection + ~6h curation + ~3h integration; front-loaded by 1-3 month contest schedule
  expected_delta: contest weekend (ARRL RTTY-RU, WAE); 200-500 slots from ~11K; tests NMS, OSD load, joint-decoding past saturation
  defensible_prior: yes — WSJT-X release notes regularly cite contest as stress test; WSJT-X authors tune accordingly
  wild_card: false
  evidence_for:
    - Median hard-200 = 24 decodes/slot; contest mode pushes to 40+
    - Contest grammar (<call> <call> <serial>) where language-model hint would show up first
  evidence_against:
    - Contest weekends intermittent; can't iterate quickly against this tier
  notes: |
    Quarterly checkpoint, not per-iter. Until contest, mock with C3
    (high-collision adversarial synth).

    See research/ideation/2026-06-01-corpus.md (entry C11).

### hb-155 — Bench-calibrated signal-generator corpus (3D operating-point grid)  [PRIORITY: 0.35, spawned 2026-06-01 from corpus ideation]
  mode: ft8 (corpus/eval infrastructure)
  status: pending
  priority_score: 0.35
  estimated_effort: ~1 day curation + ~3h integration + ~30 min compute; synth (free)
  expected_delta: 3-D grid (SNR × freq × time-offset, 5 WAVs each = ~5000); decoder behavior on complete operating-point grid without propagation confounds; identifies sharp cliffs
  defensible_prior: yes — RF receiver test methodology standard practice; WSJT-X authors use similar sweeps
  wild_card: false
  evidence_for:
    - synth-clean is 1-D sweep masquerading as calibration; real calibration is 3+ D
  evidence_against:
    - Pure synth — no propagation, no fading; always pair with real-audio for graduation
  notes: |
    Extends gen-synth multi-dim sweeps. Pilot 50 WAVs at (-20, 1000, 0).
    Sharp cliffs → tier catches them. Smooth/predictable → academic.

    See research/ideation/2026-06-01-corpus.md (entry C12).

### hb-156 — Lid-of-band weak-signal-only tier (SNR ≤ -20 dB)  [PRIORITY: 0.45, spawned 2026-06-01 from corpus ideation]
  mode: ft8 (corpus/eval infrastructure)
  status: pending
  priority_score: 0.45
  estimated_effort: ~4h curation + 3h integration; zero new audio
  expected_delta: 200 slots filtered to jt9-reported SNR ≤ -20 dB; recall on weak-signal subset; "what differentiates FT8 from less-sensitive modes"
  defensible_prior: yes — FT8 sensitivity is DEFINING capability of the protocol; Joe Taylor's original FT8 paper centers on -21 dB threshold
  wild_card: false
  evidence_for:
    - WSJT-X release validation tracks -20 dB sensitivity explicitly
    - Decoder optimized for median SNR misses FT8 mission
  evidence_against:
    - jt9 SNR ±2-3 dB noisy at floor; use soft band (-21 to -19 dB) not hard cutoff
  notes: |
    Already sub-population of hard-1000. Pull SNR-filtered subset; if
    <50 WAVs, broaden to hard-200 + wild-100 + full archive.

    See research/ideation/2026-06-01-corpus.md (entry C13).

### hb-157 — Continuous-capture metadata tier (antenna/SWR/rotator)  [PRIORITY: 0.28, spawned 2026-06-01 from corpus ideation]
  mode: ft8 (corpus/eval infrastructure)
  status: pending
  priority_score: 0.28
  estimated_effort: ~3 days collection + ~6h curation + ~1 day integration
  expected_delta: per-slot rig metadata sidecars (antenna direction, band, SWR, audio gain, temperature); cross-tier slicing reveals confounds in hard-200
  defensible_prior: yes — stratified analysis standard in epidemiology, A/B testing, clinical trials; coordinator already has hamlib data flowing
  wild_card: false
  evidence_for:
    - hard-200 silently mixes high/low-SWR slots, antenna headings, bands
  evidence_against:
    - Hamlib reliability — if CAT drops, sidecar incomplete
  notes: |
    Kill-switch: metadata coverage >95% of slots over 1 week; per-stratum
    recall differs by >10% → confounds real.

    See research/ideation/2026-06-01-corpus.md (entry C14).

### hb-158 — One-key "confirm decode" relaxes thresholds for current callsign  [PRIORITY: 0.38, spawned 2026-06-01 from human-loop ideation]
  mode: ft8 (operator-HITL)
  status: pending
  priority_score: 0.38
  estimated_effort: 1.5-2 sessions
  expected_delta: operator y/n on low-confidence decodes; confirmed callsign gets LDPC-iter / OSD-rank bar lowered for next 20 windows for that callsign only; promotes future decodes past continuity-filter rejection
  defensible_prior: yes — active-learning classifiers (Tong & Koller 2001) outperform fully-supervised with ~10x less label budget by querying most-uncertain
  wild_card: false
  evidence_for:
    - hb-087 showed 23.6% of missed truths in some operator-derived set
    - Real-time confirmation strictly stronger than offline bank
  evidence_against:
    - Operator fatigue; prompt-saturation in busy band
  notes: |
    Auto-yes pre-filter for callsigns in seed/ADIF/recent to keep volume
    <5/min. Kill-switch: <3 confirms/session over 5 sessions → suppress.

    See research/ideation/2026-06-01-human-loop.md (entry H1).

### hb-159 — Pointing finger: operator clicks waterfall slice → decoder focuses  [PRIORITY: 0.32, spawned 2026-06-01 from human-loop ideation]
  mode: ft8 (operator-HITL)
  status: pending
  priority_score: 0.32
  estimated_effort: 3-4 sessions (depends on click-able waterfall existing)
  expected_delta: click → narrow window (±25 Hz) + drop sync threshold + OSD rank 4; operator's spatial attention feeds search-space prior
  defensible_prior: yes — speech-recognition systems with selection-aware refinement (Whisper.cpp); visual saliency well-established
  wild_card: false
  evidence_for:
    - Visual pattern recognition pancetta doesn't have
    - Decoder already capable of narrow-window decode; wiring is cost
  evidence_against:
    - Operator clicks noise → wasted deep-decode budget on garbage
  notes: |
    Gated on TUI waterfall existing. If clicks/session stays at 0 for
    first 5 sessions, kill.

    See research/ideation/2026-06-01-human-loop.md (entry H2).

### hb-160 — `*` key: priority boost next-cycle CQ from highlighted callsign  [PRIORITY: 0.45, spawned 2026-06-01 from human-loop ideation]
  mode: ft8 (operator-HITL)
  status: pending
  priority_score: 0.45
  estimated_effort: 0.5-1 session
  expected_delta: session-scoped manual_priority_boost map with score +0.50; persists until QSO completes or operator unmarks; decay after 5 silent slots
  defensible_prior: yes — trading desks (price-alert + click-to-route), DAW favorites, browser bookmarks; "I want THIS one, now" universal UX
  wild_card: false
  evidence_for:
    - Operator may know P5/W0PR is on (North Korea) before cqdx rarity catches up
    - Top-3 by value-per-friction per human-loop ideation summary
  evidence_against:
    - Duplicate-penalty must remain intact (don't override "you already did this QSO")
  notes: |
    Trivial implementation. Kill-switch: <1 press/session over 5 →
    unused. >20x → becomes hunt-list workaround.

    See research/ideation/2026-06-01-human-loop.md (entry H3).

### hb-161 — `Q` key: operator STOP mid-QSO when pancetta is wrong  [SHIPPED-INFRA 2026-06-01 — Phase 5 safety driver; meatspace verification pending]
  # Phase A re-label (2026-06-01): operator UI / safety driver — no decoder
  # change, no composite impact. SHIPPED-INFRA. Real-world Q-press at the rig
  # by the operator is the binding test and remains pending; see memory
  # project_ssh_tmux_pending.md.
  mode: ft8 (operator-HITL / safety)
  status: SHIPPED-INFRA — code lands as `feat(tui): hb-161 — Q STOP key
    emergency operator override + TUI banner`. Phase 5 meatspace
    verification (actually pressing Q at the rig) deferred to
    project-meatspace-pending.
  priority_score: 0.50
  estimated_effort: 1 session
  expected_delta: immediate TX stop + diagnostic snapshot + ADIF flag operator-aborted (NOT failure for recent-failure penalty); every Q-press is gold-standard training data
  defensible_prior: yes — autonomous-vehicle safety driver disengagement; industrial robot e-stops; FCC arguably implies via control-operator rules
  wild_card: false
  evidence_for:
    - Phase 5 specifically: edge cases pancetta hasn't seen need supervisor
    - "Top-3 by value-per-friction" per human-loop ideation; e-stop is required for Phase 5 by basic safety
  evidence_against:
    - Reduces logged QSO rate during training period
  notes: |
    Required infrastructure per ideation summary. QSO state machine
    already has terminal states.

    See research/ideation/2026-06-01-human-loop.md (entry H4).

    Implementation summary (2026-06-01):
    - Shift+Q in the TUI emits `TuiCommand::OperatorEmergencyStop`.
    - Coordinator handler aborts in-flight TX, flips a new
      `autonomous_enabled_runtime: Arc<AtomicBool>` to false, stops
      the repeating-CQ loop and any active tune, and logs at WARN
      with target=operator.override.
    - Autonomous decision loop reads the runtime gate every slot
      before dispatching TX items; dropped items are logged once.
    - TUI renders a red "STOPPED BY OPERATOR" banner (non-modal —
      other keys keep working). Esc clears the banner. Re-enabling
      autonomous is explicit (`a` key) — the gate does not auto-restore.
    - 4 new unit tests in pancetta-tui::tui_runner::key_tests.
    - 61 pancetta-tui tests pass, 49 pancetta tests pass.

    Diagnostic-snapshot (last-3-RX + decoder-conf + audio-RMS) and
    ADIF "operator-aborted" flag from the original ideation are NOT
    in this drop — the safety-driver kill switch is. Those are
    follow-ons; the spec called for the kill-switch first.

### hb-162 — Post-session review CSV: operator labels each decode  [PRIORITY: 0.30, spawned 2026-06-01 from human-loop ideation]
  mode: ft8 (operator-HITL)
  status: pending
  priority_score: 0.30
  estimated_effort: 2 sessions
  expected_delta: operator marks real/fp/unsure after session; mine FP patterns; auto-suppress callsigns scored fp >3 times; boost continuity-filter for repeated reals
  defensible_prior: yes — email spam-filter design (Bayesian + user-flagged); every modern classifier with active feedback
  wild_card: false
  evidence_for:
    - K5ARH knows what FPs look like on his band slice better than any global heuristic
  evidence_against:
    - Operator marks something fp that's actually rare DX trying again; auto-suppression eats real contacts
  notes: |
    3-strike requirement + network-corroboration override. Default to
    "real if produced logged QSO" pre-fill.

    See research/ideation/2026-06-01-human-loop.md (entry H5).

### hb-163 — Voice/foot-pedal shortcut: "answer that one"  [PRIORITY: 0.15 (wild), spawned 2026-06-01 from human-loop ideation]
  mode: ft8 (operator-HITL)
  status: pending
  priority_score: 0.15
  estimated_effort: 1 session pedal; 3 sessions STT + Whisper.cpp + phonetic post-processor
  expected_delta: pedal/voice → ManualTarget(callsign) event pumped into bus; same path as TUI `*` key
  defensible_prior: partial — Stream Deck, Dragon NaturallySpeaking, DJ-footswitches; on-radio: contest loggers (N1MM voice-keyer reverse direction)
  wild_card: true
  evidence_for:
    - At-rig ergonomics; operator's hands may not be on keyboard during multi-mode
  evidence_against:
    - STT mishears phonetic alphabet; pancetta could respond to itself
  notes: |
    Kill-switch: STT WER >20% on phonetic alphabet → kill voice path.
    Pedal path much cheaper to validate.

    See research/ideation/2026-06-01-human-loop.md (entry H6).

### hb-164 — Operator-confidence-tier overlay on FP filter  [PRIORITY: 0.35, spawned 2026-06-01 from human-loop ideation]
  mode: ft8 (operator-HITL)
  status: pending
  priority_score: 0.35
  estimated_effort: 1 session
  expected_delta: operator-trust.toml (trust_tier_a / trust_tier_b / distrust); trusted = continuity-bypass + +0.10 priority + auto-confirm; distrusted = hard-reject even on strong CRC
  defensible_prior: yes — web-of-trust (PGP), SSH known_hosts, browser allow/blocklists; reputation systems default for distributed identity
  wild_card: false
  evidence_for:
    - Operator's social knowledge of the ham community not in any database
  evidence_against:
    - Distrust list grows stale; operator forgets, real callsign sits on it
  notes: |
    last_added_at + prompt to review entries >6 months. If TOML <5
    entries after 3 months, fold into seed file.

    See research/ideation/2026-06-01-human-loop.md (entry H7).

### hb-165 — Real-time alarm tier: TUI rings when target appears  [PRIORITY: 0.42, spawned 2026-06-01 from human-loop ideation]
  mode: ft8 (operator-HITL)
  status: pending
  priority_score: 0.42
  estimated_effort: 1-1.5 sessions
  expected_delta: needed_dxcc, specific calls, distance>X, snr>X → bell + flash + Mac/Windows notification; autonomy continues to chase, operator gets pulled in for exceptional case
  defensible_prior: yes — SkimTalk / RBN sound alerts; Logger4OM/N1MM contest-call alerts; trading-desk price alerts
  wild_card: false
  evidence_for:
    - Closes "missed P5 because making coffee" failure mode of pure autonomy
    - Top-3 by value-per-friction per human-loop ideation
  evidence_against:
    - Alarm fatigue if thresholds wrong on day 1
  notes: |
    Auto-tuning: silenced >5x/session → tighten; <1x/week → loosen.
    Hard cap 1 alarm/30s.

    See research/ideation/2026-06-01-human-loop.md (entry H8).

### hb-166 — Reverse-handover: pancetta yields QSO to operator at uncertainty cliff  [PRIORITY: 0.30, spawned 2026-06-01 from human-loop ideation]
  mode: ft8 (operator-HITL)
  status: pending
  priority_score: 0.30
  estimated_effort: 2-3 sessions
  expected_delta: decoder-confidence drops → beep + flash "HANDOVER" + pause auto-TX + pre-fill TUI send-queue; operator confirms/edits/aborts
  defensible_prior: yes — Tesla Autopilot lane-change confirmation; bash autocomplete (Enter accepts); composer "send draft"
  wild_card: false
  evidence_for:
    - QSO failures happen at decode-cliff (QSB knocking SNR -6dB); operator might pull out call by ear
  evidence_against:
    - Operator sleeps through handover, QSO times out
  notes: |
    Default "auto-continue if no operator response in 8s" with telemetry
    distinguishing.

    See research/ideation/2026-06-01-human-loop.md (entry H9).

### hb-167 — Skill-transfer: log operator's manual QSOs, mine priority preferences  [PRIORITY: 0.32, spawned 2026-06-01 from human-loop ideation]
  mode: ft8 (operator-HITL)
  status: pending
  priority_score: 0.32
  estimated_effort: 3 sessions
  expected_delta: 30-day ADIF analyzed for DXCC distribution, time-of-day patterns, band preferences, specific-call frequencies; ridge regression suggests per-operator weight deltas
  defensible_prior: yes — Spotify Discover Weekly mines listening; GitHub Copilot personalization
  wild_card: false
  evidence_for:
    - Defaults inevitably wrong for any specific operator
  evidence_against:
    - Overfit to operator's recent quirk (one bad week chasing CN2 → over-weight Africa forever)
  notes: |
    Rolling 90-day window with recency decay. If learned weights produce
    worse outcomes on next week, revert.

    See research/ideation/2026-06-01-human-loop.md (entry H10).

### hb-168 — Co-pilot mode: pancetta SUGGESTS, operator AUTHORIZES every TX  [PRIORITY: 0.30, spawned 2026-06-01 from human-loop ideation]
  mode: ft8 (operator-HITL)
  status: pending
  priority_score: 0.30
  estimated_effort: 2 sessions
  expected_delta: every TX candidate queued in TUI; operator presses Space/edit/skip; every interaction logged for retraining (H10/H12/H14)
  defensible_prior: yes — Copilot/Cursor suggest-then-tab; autonomous-driving testing (always supervised initially); aviation autopilot
  wild_card: false
  evidence_for:
    - First weeks of Phase 5 = training-wheel mode; pancetta proposes graduating after N sessions >95% acceptance
    - Expands user base (operators who'd not trust full autonomy)
  evidence_against:
    - Defeats autonomous goal as default
  notes: |
    Opt-in mode for new users, new bands, first-time exotic-DX chases.
    Not default.

    See research/ideation/2026-06-01-human-loop.md (entry H11).

### hb-169 — Reverse-active-learning: pancetta asks operator about uncertain ones it would accept  [PRIORITY: 0.28, spawned 2026-06-01 from human-loop ideation]
  mode: ft8 (operator-HITL)
  status: pending
  priority_score: 0.28
  estimated_effort: 2-3 sessions
  expected_delta: barely-passed decodes tagged [?]; operator r/a; train logistic regression on (features, labels); after 200+ labels suggest auto-filter at 92% accuracy
  defensible_prior: yes — active-learning literature (uncertainty sampling); spam filter "is this spam?"; recommender implicit-feedback calibration
  wild_card: false
  evidence_for:
    - Decoder confidence not calibrated to operator-perceived quality
    - Cost bounded (5/session marginal, not 50)
  evidence_against:
    - Pancetta becomes operator-biased; rejects what others'd call valid (contest format K5ARH doesn't recognize)
  notes: |
    Per-band/mode-context opt-in; never discard, just down-rank.
    Kill-switch: label-rate <1/session after 4 sessions.

    See research/ideation/2026-06-01-human-loop.md (entry H12).

### hb-170 — "Confidence whisper" decision-log narrating autonomous reasoning  [PRIORITY: 0.32, spawned 2026-06-01 from human-loop ideation]
  mode: ft8 (operator-HITL)
  status: pending
  priority_score: 0.32
  estimated_effort: 1 session
  expected_delta: TUI panel showing per-decision score breakdown (needed_dxcc=+0.35, rarity=+0.31, snr=+0.05); operator sees why; can `*` missed opportunities for retraining
  defensible_prior: yes — Anthropic's "thinking" mode; Cursor's explain-this-suggestion; aviation flight-director displays
  wild_card: false
  evidence_for:
    - Black-box autonomy hard to trust; visibility builds trust
    - Operator diagnoses mis-tuning, course-corrects via H3/H7
  evidence_against:
    - Information overload; band-activity already crowds panel
  notes: |
    Collapsed by default, expand on `D` keypress. Kill-switch: hidden
    >90% of session-time over 5 sessions → kill.

    See research/ideation/2026-06-01-human-loop.md (entry H13).

### hb-171 — Operator hot-paths: rapid +/- reinforcement during session  [PRIORITY: 0.30, spawned 2026-06-01 from human-loop ideation]
  mode: ft8 (operator-HITL)
  status: pending
  priority_score: 0.30
  estimated_effort: 2 sessions
  expected_delta: + / - on any visible row; promotes/demotes; records ±1 reinforcement; over 500 signals trains feature-weighted preference model (prefix, country, grid distance, mode, band, ToD)
  defensible_prior: yes — Reddit upvote/downvote, Tinder swipes, TikTok dwell-time; thumbs-up pattern
  wild_card: false
  evidence_for:
    - Cumulative-learning version of H3
  evidence_against:
    - Operator emotional momentum (-everyone after bad QSO) corrupts training
  notes: |
    Per-session ratio normalization, anomaly detection. Kill-switch:
    <5/session after 3 sessions.

    See research/ideation/2026-06-01-human-loop.md (entry H14).

### hb-172 — Mixed-initiative QSO authoring: operator dictates message text mid-QSO  [PRIORITY: 0.30, spawned 2026-06-01 from human-loop ideation]
  mode: ft8 (operator-HITL)
  status: pending
  priority_score: 0.30
  estimated_effort: 2 sessions
  expected_delta: `m` interrupts auto-generated message; operator types custom; validated against FT8 message-format; useful for TU/FB closings, QRZ requests, dupe-clearing
  defensible_prior: yes — WSJT-X "Tx5" custom message; JTDX free-text; chat clients with typing-indicator-from-bot
  wild_card: false
  evidence_for:
    - Real ham has texture state machine doesn't model (yet)
  evidence_against:
    - Operator types invalid message or mistypes callsign → sends garbage
  notes: |
    Strict validation pre-send + 2s edit-buffer with preview. Gate
    against breaking QSO state.

    See research/ideation/2026-06-01-human-loop.md (entry H15).

### hb-173 — Within-QSO context graph (decode-time pair-conditional AP templates)  [PRIORITY: 0.50, scoped 2026-06-01]
  status_2026_06_01_session1: |
    Session 1 (design + diagnostic) COMPLETE → PROCEED.
    Diagnostic `hb173_within_qso_diagnostic.rs` on hard-200: 18.48% of
    8626 jt9-truth decodes (1594) are downstream turns of an
    identifiable QSO whose upstream turn is in the same chronological
    session — 1.85× the 10% PROCEED threshold. Hard-1000: 46.53%
    (12775/27458). Depth distribution shows turn-2 = 55% (a7's target
    population) but turn-3+ = 45% (exclusive to pair-conditional
    templates; a7 cannot capture these because the responder swaps
    roles). Slot-gap bimodal at 1+2 slots (89% of continuations) —
    matches FT8 slot-parity QSO pattern. 80% of hard-200 sessions are
    single-slot due to curation; production coverage is UNDERESTIMATED.
    Design spec at docs/superpowers/specs/2026-06-01-hb-173-within-qso-design.md.
    Session 2 builds WithinQsoContext + pair_conditional_templates +
    classify_phase + 12 unit tests; Session 3 wires into decoder
    post-V1, A/B evals on hard-200/1000 with target ≥5/≥20 recall
    lift and composite ≥+0.0005. Recommends shared-infra crate
    `CrossTimeState` co-housing hb-057, hb-048 a7, hb-173 tables.
    Recommends separate chronological-replay eval-tier hypothesis
    after Session 2 (corpus curation suppresses cross-slot effect).
  ---- original priority entry below ----
  [PRIORITY-WAS: 0.50, spawned 2026-06-01 from cross-time ideation]
  mode: ft8 (cross-time / AP)
  status: pending
  priority_score: 0.50
  estimated_effort: spec-sized 2 sessions + implementation 1-2 sessions
  expected_delta: WithinQsoContext table keyed by (callsign_pair, freq±15Hz), evicted on 73/RR73 or 6-slot timeout; pair-conditional AP templates into next slot; bidirectional state flow validates earlier turns
  defensible_prior: yes — JTAlert + N1MM track in-flight QSOs FOR LOGGING; no FT8 decoder uses QSO state for DECODE improvement; WSJT-X a7 is closest but single-callsign
  wild_card: false
  evidence_for:
    - QSO is 4-7-slot structured exchange; structure constrains slots N+1, N+2 hugely
    - Top-3 by potential disruption per cross-time ideation
  evidence_against:
    - State staleness: stale entry from 30-min-old QSO that resumed on different band would inject wrong templates
    - Eval harness shuffles WAVs — needs chronological eval tier
  notes: |
    Kill-switch: extract multi-slot WAVs in research/corpus/, measure
    how many missed truths are downstream QSO turns where upstream
    DID decode. ≥8% → defensible.

    See research/ideation/2026-06-01-cross-time.md (entry T1).

### hb-174 — Within-session DT-and-frequency drift model per callsign (hb-057 extension)  [PRIORITY: 0.40, spawned 2026-06-01 from cross-time ideation]
  mode: ft8 (cross-time / sync)
  status: pending
  priority_score: 0.40
  estimated_effort: 1 session if grafted onto hb-057 storage; 2 sessions standalone
  expected_delta: per-callsign running linear model across session of (timestamp → dt) and (timestamp → audio_freq); sync uses predicted DT instead of global ±2.0s; continuous-time extension of hb-057's median+IQR
  defensible_prior: yes — JTDX maintains per-callsign DT smoothing (inspiration for hb-057); WSPR plots (dt, snr, dfreq) per callsign-pair
  wild_card: false
  evidence_for:
    - Drift model also informs propagation-phase-coherence prior
  evidence_against:
    - Overfit on 3 sightings — linear fit is nearly-singular
    - Should be a scope-creep candidate on hb-057, not standalone (per ideation note)
  notes: |
    Recommended as hb-057 follow-up. Kill-switch: linear adds <3% over
    median → drift not linear or too few sightings.

    See research/ideation/2026-06-01-cross-time.md (entry T2).

### hb-175 — Cross-session ADIF-driven AP pool (multi-day depth)  [PRIORITY: 0.38, spawned 2026-06-01 from cross-time ideation]
  mode: ft8 (cross-time / AP)
  status: pending
  priority_score: 0.38
  estimated_effort: 2 sessions (ADIF reader exists in pancetta-qso::callsign_continuity)
  expected_delta: extend AP2 caller-injection pool to last K=200 distinct ADIF callsigns weighted by recency (half-life 7 days) and band-match (same-band 4×); mines operator's OWN log
  defensible_prior: partial — WSJT-X-Improved "WANTED" list conceptually similar; JTAlert "previously worked" highlight feeds nothing decode-side; novel decoder-AP-pool feedback
  wild_card: false
  evidence_for:
    - Different from hb-052 (continuity is OUT-bound plausibility); this is AP injection IN-bound
  evidence_against:
    - hb-051 ceiling shows AP-blast has hard recall ceiling; pool inflation could blow up novels (hb-087's shelve teaches)
  notes: |
    Cap AP pool expansion to ≤2× current recent_calls size; aggressive
    threshold-sweep. Kill-switch: ≥3 callsigns/200 hard-200 WAVs have
    truth callsign in ADIF AND fail without AP.

    See research/ideation/2026-06-01-cross-time.md (entry T3).

### hb-176 — Per-band-time-of-day propagation expectation prior  [PRIORITY: 0.32, spawned 2026-06-01 from cross-time ideation]
  mode: ft8 (cross-time / AP)
  status: pending
  priority_score: 0.32
  estimated_effort: 3 sessions (data pipeline + table + decoder hook)
  expected_delta: 3D table P(callsign decodable | band, UTC hour, day_of_year); high-P callsigns get AP injection priority; low-P decodes face higher FP-filter trust threshold
  defensible_prior: yes — N1MM+/Win-Test use VOACAP for run-rate optimization; PSKReporter visualizes per-band openings; no DECODER uses propagation priors
  wild_card: false
  evidence_for:
    - JA1XYZ at 23:00 UTC on 20m has historical P=0.4 (high JA opening); at 03:00 UTC P=0.02 (band dead)
  evidence_against:
    - Propagation priors noisy and operator-grid-specific; model fit to K5ARH won't transfer
  notes: |
    Ship the prior GENERATOR, not the prior table; recompute per-install
    from operator's ADIF + cqdx history pull.

    See research/ideation/2026-06-01-cross-time.md (entry T4).

### hb-177 — Sunspot-cycle aware AP weighting (multi-month cycle)  [PRIORITY: 0.18, spawned 2026-06-01 from cross-time ideation]
  mode: ft8 (cross-time / long-cycle)
  status: pending
  priority_score: 0.18
  estimated_effort: 2 sessions to first ship; evaluation is the long pole (months of operational data)
  expected_delta: NOAA SFI feed (free) → band_activity_weight table; as 10m dies through 2027, AP pool for 10m down-weights older callsigns
  defensible_prior: yes — long-distance contest planning uses SFI forecasts (HamCAP, VOACAP); Q65 designed for HF-degraded conditions
  wild_card: false
  evidence_for:
    - Solar cycle 25 is descending; bands shift activity
  evidence_against:
    - Glacial change; effect small per-session, large per-year; hard to evaluate offline
  notes: |
    "Deferred wager" per cross-time ideation. May only show value at
    6-month review. Backtest cqdx 18-month spot history KL-divergence ≥
    0.5 nats between SFI quartiles.

    See research/ideation/2026-06-01-cross-time.md (entry T5).

### hb-178 — Contest weekend / periodic event detection (calendar-aware AP)  [PRIORITY: 0.30, spawned 2026-06-01 from cross-time ideation]
  mode: ft8 (cross-time / AP)
  status: pending
  priority_score: 0.30
  estimated_effort: 2 sessions
  expected_delta: calendar (YAML, ~30 events/year, quarterly updates) drives AP-pool composition; CQ-WW-RTTY → pre-load contest templates + relax /M/P FP rules; POTA → boost spotter callsign AP
  defensible_prior: yes — N1MM has contest-mode dropdowns; JTAlert highlights contest-format; hb-058 graduated NEGATIVE version (rejects FD format)
  wild_card: false
  evidence_for:
    - Bidirectional: reject in non-FD windows, ACCEPT (+ AP-boost) during FD windows
  evidence_against:
    - Stale calendar = wrong mode = silent regressions
  notes: |
    Visible status indicator ("CONTEST MODE: ARRL DX SSB"). Ship calendar
    updates with each release.

    See research/ideation/2026-06-01-cross-time.md (entry T6).

### hb-179 — Per-operator (sender) personality fingerprint  [PRIORITY: 0.22 (wild), spawned 2026-06-01 from cross-time ideation]
  mode: ft8 (cross-time / behavioral)
  status: pending
  priority_score: 0.22
  estimated_effort: 3-4 sessions (spec-sized)
  expected_delta: K=20 observed slots → behavioral profile per callsign (DT typical, audio_freq, SNR distribution, message-type prior, response timing); biases sync, message-type prior, detects anomaly
  defensible_prior: partial — per-callsign DT (T2) supported by JTDX prior art; full personality model is bold
  wild_card: true
  evidence_for:
    - Operator W1XYZ always at DT=0.45s → sync there first
  evidence_against:
    - Operator changes hardware/location → profile wrong
    - Privacy implications: we're profiling other hams
  notes: |
    Opt-out, local-only (no upload to cqdx). Kill-switch: cluster top-100
    callsigns; ≥3 distinct clusters with silhouette ≥0.3.

    See research/ideation/2026-06-01-cross-time.md (entry T7).

### hb-180 — Propagation-regime classifier (TEP / Es / Aurora auto-tune)  [PRIORITY: 0.28, spawned 2026-06-01 from cross-time ideation]
  mode: ft8 (cross-time / regime)
  status: pending
  priority_score: 0.28
  estimated_effort: 4 sessions
  expected_delta: classify current 15-min window into {normal, TEP, Es, aurora, geomag-storm}; each tunes decoder (Aurora widens Costas freq variance, Es relaxes in-region FP, TEP biases other-hemisphere AP)
  defensible_prior: yes — DX Atlas, DX Heat, PSK Reporter band-condition all classify; Q65 design IS a fixed regime adaptation (manual)
  wild_card: false
  evidence_for:
    - Sub-Costas residual + warbly tones (high freq variance) are aurora signature
  evidence_against:
    - Classifier mistakes (TEP misclassified as Es) → wrong tuning → silent regression
  notes: |
    Calibrated probabilities; decoder blends parameter sets weighted by
    regime probability. Quiet vs Aurora recall difference <5% → little
    room.

    See research/ideation/2026-06-01-cross-time.md (entry T8).

### hb-181 — Cross-session band-noise-floor learning (auto-tune gates)  [PRIORITY: 0.30, spawned 2026-06-01 from cross-time ideation]
  mode: ft8 (cross-time / DSP)
  status: pending
  priority_score: 0.30
  estimated_effort: 2 sessions if waterfall capture plumbed; 3-4 if not
  expected_delta: track noise floor per (band, UTC hour-of-week); above-median → drop min_sync_score + raise FP trust; below-median → tighten sync gate
  defensible_prior: yes — WSPR noise-floor reporting; CW Skimmer adaptive thresholds; JTDX "DX call only" mode (operator-toggled); no auto FT8
  wild_card: false
  evidence_for:
    - Variance must exceed ≥3 dB between best/worst hours to justify
  evidence_against:
    - Auto-tuned gates can oscillate (low noise → tighten → miss → loosen → false decodes)
  notes: |
    Hysteresis on regime transitions, weekly recompute not per-slot.

    See research/ideation/2026-06-01-cross-time.md (entry T9).

### hb-182 — Time-aware decode-confidence calibration (retroactive boost)  [PRIORITY: 0.20 (wild), spawned 2026-06-01 from cross-time ideation]
  mode: ft8 (cross-time / calibration)
  status: pending
  priority_score: 0.20
  estimated_effort: 3 sessions
  expected_delta: calibrate confidence(decode) → P(correct) as f(time-since, decoder-pass); confirmed-by-RR73 retroactively boosts current threshold for similar decodes
  defensible_prior: partial — no digital-mode decoder does this; closest is reCAPTCHA's calibration loop with delayed confirmation
  wild_card: true
  evidence_for:
    - Doubles as real-time FP-filter precision audit
  evidence_against:
    - Feedback loop instability; threshold drifts wrongly if confirmation drops for unrelated propagation
  notes: |
    Strict damping required. Kill-switch: ≥10% of pass-3 decodes get
    external confirmation within 60s (enough signal to calibrate).

    See research/ideation/2026-06-01-cross-time.md (entry T10).

### hb-183 — Federated cross-operator priors (pancetta-network)  [PRIORITY: 0.22 (wild), spawned 2026-06-01 from cross-time ideation]
  mode: ft8 (cross-time / network)
  status: pending
  priority_score: 0.22
  estimated_effort: 5+ sessions (spec-sized federated infra)
  expected_delta: anonymized "just-decoded" tuples to cqdx; aggregated stream "at 03:42 UTC on 20m, 14 pancettas heard JA1XYZ" → instantaneous AP-pool boost for JA1XYZ
  defensible_prior: partial — PSKReporter + RBN do this for spotting (not decoder feedback); JTAlert pulls cluster for highlighting; network→decoder loop wild_card
  wild_card: true
  evidence_for:
    - Top-3 by potential disruption per cross-time ideation
  evidence_against:
    - Privacy / federation poisoning; malicious instance could spam fake spots
  notes: |
    cqdx reputation system + signed reports + slow-trust per source.
    Kill-switch: ≥10% missed truths on hard-1000 spotted by someone in
    network within ±60s.

    See research/ideation/2026-06-01-cross-time.md (entry T11).

### hb-184 — Time-reversed multipass (forward-backward smoothing on residual)  [PRIORITY: 0.22 (wild), spawned 2026-06-01 from cross-time ideation]
  mode: ft8 (cross-time / DSP)
  status: pending
  priority_score: 0.22
  estimated_effort: 1-2 sessions
  expected_delta: extra multipass on time-reversed complex baseband (swap I/Q, re-FFT); FT8 8-GFSK symmetric under time reversal; asymmetric interference (click at t=0, RFI at t=14s) recovered
  defensible_prior: partial — forward-backward smoothing standard in HMMs (Baum-Welch); no FT8 application
  wild_card: true
  evidence_for:
    - Symbol-level decoding under symmetric tone seq; math is sound
    - Shortest-cycle idea per cross-time ideation (~½ day test)
  evidence_against:
    - Time-reversal symmetry may be subtly wrong with frequency drift (Doppler)
  notes: |
    Kill-switch: 10 WAVs with asymmetric interference; reversed decoder
    recovers ≥2 distinct decodes forward misses.

    See research/ideation/2026-06-01-cross-time.md (entry T12).

### hb-185 — Meta-decode QSO-state HMM (decode the OPERATOR not the slot)  [PRIORITY: 0.20 (wild), spawned 2026-06-01 from cross-time ideation]
  mode: ft8 (cross-time / operator-state)
  status: pending
  priority_score: 0.20
  estimated_effort: 4+ sessions
  expected_delta: HMM over operator state {listening, CQ, in-QSO-turn-N, logging}; observation = decoded messages; AP pool conditioned on state distribution
  defensible_prior: partial — HMMs are textbook ML; applied to FT8 operator-state-as-conditioning is novel
  wild_card: true
  evidence_for:
    - If it works, every AP mechanism downstream gets sharper conditioning for free
    - Top-3 by potential disruption per cross-time ideation
  evidence_against:
    - HMM state-space explosion; inference cost in real-time
  notes: |
    Kill-switch: from ADIF extract complete QSOs, label slots with HMM
    state; I(state; helpful_AP_template) ≥ 0.3 bits.

    See research/ideation/2026-06-01-cross-time.md (entry T13).

### hb-186 — Periodic / diurnal CQ pile-up prior (mined from cqdx spot history)  [PRIORITY: 0.30, spawned 2026-06-01 from cross-time ideation]
  mode: ft8 (cross-time / AP)
  status: pending
  priority_score: 0.30
  estimated_effort: 2-3 sessions
  expected_delta: FFT over inter-spot gap times per callsign; strong daily/weekly periodicity → high prior at predicted next window; pre-load AP pool 15min before predicted activation
  defensible_prior: yes — WSPRnet shows beacon-like periodic signals; POTA/SOTA activation explicitly scheduled
  wild_card: false
  evidence_for:
    - Some operators have scheduled activations (K1WX 20m at 22:00 UTC, Roman SOTA Saturday mornings)
  evidence_against:
    - Self-reinforcing prediction: might falsely decode K1WX on noise during expected window
  notes: |
    Prior boosts AP weight but does NOT lower LDPC threshold; decoded
    codeword must still pass CRC + plausibility. Kill-switch: ≥5
    callsigns show periodicity spike ≥3× background.

    See research/ideation/2026-06-01-cross-time.md (entry T14).

### hb-187 — Wav2Vec2 / HuBERT frozen-encoder front-end (foundation-model adapter)  [PRIORITY: 0.40, spawned 2026-06-01 from foundation-models ideation]
  mode: ft8 (ML / foundation-model)
  status: session-1-complete (PROCEED; feasibility passed 2026-06-01)
  priority_score: 0.40
  session_1_outcome: PROCEED — Wav2Vec2-base loads on MPS in <1s, pooled embeddings on 10 real FT8 WAVs show non-degenerate structure (cos mean 0.698, PC1=45%, eff-rank 4.7), per-WAV latency 145ms warm. Session 2 spec at docs/superpowers/specs/2026-06-01-hb-187-wav2vec2-design.md. Journal: research/experiments/2026-06-01-hb-187-session1.md.
  estimated_effort: 3-4 sessions to first A/B; 8 GPU-hours rented (~$10)
  expected_delta: pretrained 95M-param SSL encoder (Wav2Vec2/HuBERT/WavLM) + thin Linear(768→174) adapter; LLRs into existing BP; OOD risk bounded (frozen encoder, tiny adapter)
  defensible_prior: yes — ICASSP 2025 "Self-Supervised Speech Models as Universal Narrowband-Audio Feature Extractors" (Chen et al.) reports Wav2Vec2 improves PER on narrowband incl. FT4/FT8 even without fine-tuning
  wild_card: false
  evidence_for:
    - Pretrained audio embeddings carry richer time-freq joint-statistics priors learned from millions of hours
    - Top-3 by potential disruption per foundation-models ideation
    - Quickest of top-3 (3-4 sessions vs F4's 8-10)
  evidence_against:
    - Encoder pretrained on human speech (LibriSpeech); FT8 not speech
    - Thin adapter pins head to small FT8 corpus that drove hb-064 S2 to overfit
  notes: |
    Kill-switch (3-part): (1) adapter-head LLR vs Gaussian LLR
    correlation > 0.4 on synth-clean SNR≥0; (2) train cost ≤8 GPU-hours;
    (3) inference latency ≤30 ms/candidate on M2.

    See research/ideation/2026-06-01-foundation-models.md (entry F1).

### hb-188 — Whisper-tiny encoder for cross-slot QSO-language modelling  [PRIORITY: 0.35, spawned 2026-06-01 from foundation-models ideation]
  mode: ft8 (ML / foundation-model)
  status: pending
  priority_score: 0.35
  estimated_effort: 5-6 sessions; 24 GPU-hours (~$30)
  expected_delta: Whisper-tiny (39M params) trained on (slot-WAV, transcript) pairs; token-level beam search; LM prior over QSO grammar replaces hand-coded AP injection
  defensible_prior: partial — Whisper's Robust Speech Recognition paper shows pretraining-scale transcribers beat hand-coded LMs on domain-shifted ASR; ham forum thread mentions GPT-2-finetuned re-ranker (peer-reviewed cite shaky)
  wild_card: false
  evidence_for:
    - FT8 vocab ~50K callsigns + 4-char grids + 2-char reports = orders of magnitude smaller than English
  evidence_against:
    - LM hallucinates plausible-but-wrong callsigns at low SNR
  notes: |
    LM as re-ranker only, never primary decoder. Score LDPC-feasible
    candidates by LM perplexity; tie-break when CRC-valid > 1.
    Kill-switch: held-out LM perplexity ≥5× better than uniform.

    See research/ideation/2026-06-01-foundation-models.md (entry F2).

### hb-189 — Diffusion denoiser as spectrogram preprocessing  [PRIORITY: 0.32, spawned 2026-06-01 from foundation-models ideation]
  mode: ft8 (ML / DSP)
  status: pending
  priority_score: 0.32
  estimated_effort: 5 sessions; 48 GPU-hours (~$60)
  expected_delta: small (~1M-param) U-Net diffusion + 4-step DDIM on each candidate-tile spectrogram; denoised tile fed to existing GFSK demodulator
  defensible_prior: yes — NeurIPS 2024 "Score-Based Diffusion for Wireless Channel Denoising" (Liang et al., MIT) reports 1.8 dB demod-SNR gain on OFDM at SNR<0
  wild_card: false
  evidence_for:
    - Pattern (small U-Net + DDIM-4-step + channel aug) widely reproduced in wireless ML literature
    - hb-079 coherent-iterative-subtract is structurally similar (residual cleaning); learned denoiser generalizes
  evidence_against:
    - Diffusion overfits training noise distribution; real RFI/lightning crashes not captured by synth
  notes: |
    Mitigate with "real noise" augmentation pool from operator's
    silent-slot recordings. Kill-switch: PSNR ≥3 dB at synth-SNR -20 dB.

    See research/ideation/2026-06-01-foundation-models.md (entry F3).

### hb-190 — End-to-end Transformer audio → 91-bit message  [PRIORITY: 0.25 (wild), spawned 2026-06-01 from foundation-models ideation]
  mode: ft8 (ML / foundation-model)
  status: pending
  priority_score: 0.25
  estimated_effort: 8-10 sessions; 200 GPU-hours (~$250) — biggest investment in bank
  expected_delta: 6-layer encoder + 6-layer decoder Transformer (~10M params); 12000-sample audio → 91 info bits; skips BP, OSD, Costas re-sync entirely
  defensible_prior: yes — ICLR 2024 "Neural End-to-End Channel Decoders for Short Block Codes" (Cammerer et al., Bell Labs/NVIDIA) matches/beats OSD-5 at SNR≥-1 dB on (204,102); pancetta's (174,91) in scope
  wild_card: true
  evidence_for:
    - Highest ceiling: potentially replaces BP+OSD+Costas entirely
    - Top-3 by potential disruption per foundation-models ideation
  evidence_against:
    - Highest OOD risk: 10M-param on 5M synth pairs is canonical setup for synth-overfit
    - hb-064 S2 redux at 100× scale
  notes: |
    Mitigation: ensemble with classical decoder, use neural only when
    classical fails parity gate. OR use neural for sync-candidate
    ranking only.

    See research/ideation/2026-06-01-foundation-models.md (entry F4).

### hb-191 — GPT-style cross-slot QSO-state language model  [PRIORITY: 0.42, spawned 2026-06-01 from foundation-models ideation]
  mode: ft8 (ML / cross-slot)
  status: pending
  priority_score: 0.42
  estimated_effort: 3 sessions; 10 GPU-hours (~$13); cheapest end-to-end of foundation-model ideas
  expected_delta: small GPT (~5M params) on sequence of (slot_t, station_t, message_t); pure Rust deploy (candle-rs/burn); re-ranks LDPC-feasible candidates by LM prior
  defensible_prior: partial — WSJT-X's in-QSO state machine encodes tiny version; FT8-specific cite is the wild flag (plenty for amateur-radio log-LM scoring)
  wild_card: false
  evidence_for:
    - Cheapest end-to-end shot at a novel mechanism per foundation-models ideation
    - Distinct from F2: cross-slot, not in-slot audio→text
  evidence_against:
    - LM trained on majority population callsigns will under-predict rare DX
  notes: |
    Inverse-frequency loss during training; threshold LM contribution to
    re-ranking (max ±20% of final score).

    See research/ideation/2026-06-01-foundation-models.md (entry F5).

### hb-192 — Self-supervised pretraining on operator-captured raw WAVs  [PRIORITY: 0.40, spawned 2026-06-01 from foundation-models ideation]
  mode: ft8 (ML / foundation-model)
  status: pending
  priority_score: 0.40
  estimated_effort: 5 sessions active work, 3-6 month wall-clock data collection; 300 GPU-hours (~$400) — most expensive train
  expected_delta: ViT-style encoder (~5M params) MAE-pretrained on 10M unlabeled slots; fine-tuned on hard-1000 + hard-200; solves hb-064 S2's diagnosed root cause (small labelled set)
  defensible_prior: yes — MAE (He et al. 2021); Wav2Vec2 pretraining; SSL on domain audio won in bioacoustics (BirdNET), seismology, underwater (UWNet)
  wild_card: false
  evidence_for:
    - cqdx.io has infrastructure for storing capture firehose
    - Pairs naturally with F4 or F1 as representation provider
    - Top-3 by potential disruption per foundation-models ideation
  evidence_against:
    - Representation reflects operator's band/antenna/propagation; OSS-publish transfer poor
  notes: |
    Pretrain on POOLED corpus including N1MM / WSJT-X public captures.
    Kill-switch: linear-probe vs raw STFT only marginally beats →
    SSL not learning useful structure.

    See research/ideation/2026-06-01-foundation-models.md (entry F6).

### hb-193 — CLIP-style joint embedding of (audio, codeword) pairs  [PRIORITY: 0.30, spawned 2026-06-01 from foundation-models ideation]
  mode: ft8 (ML / contrastive)
  status: pending
  priority_score: 0.30
  estimated_effort: 4 sessions; 24 GPU-hours (~$30)
  expected_delta: 2 encoders (audio conv + codeword transformer), InfoNCE loss; cosine-similarity tie-breaker for CRC collisions + OSD candidate-ranking side-channel
  defensible_prior: yes — CLIP (Radford 2021), Audio-CLIP (Akbari 2022); contrastive joint-embedding heavily replicated
  wild_card: false
  evidence_for:
    - CRC has 14 bits → ~1-in-16K FP rate; collisions are real at pancetta's volume
  evidence_against:
    - CRC collisions already rare; if joint embedding doesn't generalize beyond synth, just adds latency
  notes: |
    Deploy as per-batch diagnostic first; measure CRC-collision-
    resolution rate on real captures; promote if signal exists.

    See research/ideation/2026-06-01-foundation-models.md (entry F7).

### hb-194 — Bayesian neural OSD via deep ensembles + entropic gating  [SESSION-1-COMPLETE-A/B-PENDING 2026-06-01 — offline ensemble metric only; production composite A/B is Session 2]
  # Phase A re-label (2026-06-01): the Session 1 journal called this
  # GRADUATED based on an OFFLINE sample_recovery_rate on a 55-sample test
  # fold (95% CI ±13 pp). No production decoder A/B was run; no composite
  # delta measured against the production loop. Per the Phase A honesty
  # pass + Phase B bootstrap-CI policy, this is SESSION-1-COMPLETE rather
  # than GRADUATED. Session 2 = wire ensemble-mean weights and/or
  # variance-gated OSD into the production decoder; A/B on hard-200 +
  # hard-1000 with bootstrap CIs.
  # See: research/experiments/2026-06-01-hb-194-bayesian-ensembles.md
  # and  research/experiments/2026-06-01-phase-b-bootstrap-ci.md.
  mode: ft8 (ML / OSD)
  status: SESSION-1-COMPLETE-A/B-PENDING — N=8 no-bootstrap ensemble beats single-model mean by +55% sample_recovery_rate on Session 1 test split (N=55, ±13 pp CI); variance Pearson(var, error)=+0.48 (informativeness, NOT calibration in the Guo-2017 ECE sense). Session 2 binding A/B not run.
  priority_score: 0.35
  estimated_effort: 2 sessions; 8 GPU-hours (~$10) — cheapest deploy of any foundation-model idea
  expected_delta: K=8 independent copies of existing 20K OSD CNN with different seeds/bootstraps; ensemble disagreement = "should we run longer OSD?" gate; addresses hb-064 S2 overconfident-wrong directly
  defensible_prior: partial — Lakshminarayanan 2017 "Simple and Scalable Predictive Uncertainty Estimation"; nobody's done deep ensembles for OSD specifically (wild flag)
  wild_card: true
  evidence_for:
    - hb-064 S2's single-model failure mode is overconfident-wrong; ensembles are canonical mitigation
    - Lowest data-requirement / cheapest to bootstrap per foundation-models ideation
    - Even if doesn't win, ensemble-disagreement signal is useful diagnostic for future neural-OSD work
  evidence_against:
    - Ensembles don't fix systematic bias; if all 8 overfit same quirk, ensemble overfits too
  notes: |
    Diversify training data across 8 bootstraps (different SNR
    distributions, different WAV pool subsets). Kill-switch: disagreement
    low everywhere (high or low confidence) → ensemble adds nothing.

    See research/ideation/2026-06-01-foundation-models.md (entry F8).

### hb-195 — Graph Neural Network over LDPC factor graph  [PRIORITY: 0.35, spawned 2026-06-01 from foundation-models ideation]
  mode: ft8 (ML / LDPC)
  status: pending
  priority_score: 0.35
  estimated_effort: 4-5 sessions; 24 GPU-hours
  expected_delta: 8-layer GraphConv (~50K params) on (174,91) factor graph; neural message functions replace BP min-sum/log-sum-exp; 0.5-1 dB SNR gain at waterfall
  defensible_prior: yes — ICASSP 2023 "Deep Unfolded BP for LDPC" (Nachmani et al., extended 2024 with GNN variant); Sionna includes GNN-decoder example
  wild_card: false
  evidence_for:
    - GNN-BP wins published on short LDPC at waterfall region
  evidence_against:
    - GNN-BP wins documented on AWGN; real channels (Doppler + multipath) less studied
  notes: |
    Train with augmented channels matching deployment env. Kill-switch:
    BER on AWGN -18 dB ≥ BP BER → training broken (well-known result).

    See research/ideation/2026-06-01-foundation-models.md (entry F9).

### hb-196 — Knowledge distillation from frozen large teacher  [PRIORITY: 0.30, spawned 2026-06-01 from foundation-models ideation]
  mode: ft8 (ML / distillation)
  status: pending
  priority_score: 0.30
  estimated_effort: 4 sessions + 250 GPU-hours (~$320)
  expected_delta: train ~100M-param teacher (F4 arch), distill into ~200K-param student fitting existing deployment envelope; student gets generalization without teacher's deploy cost
  defensible_prior: yes — Hinton 2015 "Distilling Knowledge"; DistilBERT/TinyBERT/MobileBERT show 60-80% teacher perf at 5-10% params
  wild_card: false
  evidence_for:
    - Sidesteps F4 deploy-latency problem while keeping OOD-generalization benefit
  evidence_against:
    - Student inherits teacher's biases including any synth-overfit; distillation amplifies over-confident wrong predictions
  notes: |
    Distill on real-capture validation set rather than synth. Kill-switch:
    student-vs-teacher BER ratio at SNR -15 dB > 1.5×.

    See research/ideation/2026-06-01-foundation-models.md (entry F10).

### hb-197 — Latent-diffusion over LDPC codeword space  [PRIORITY: 0.18 (wild), spawned 2026-06-01 from foundation-models ideation]
  mode: ft8 (ML / generative)
  status: pending
  priority_score: 0.18
  estimated_effort: 6 sessions; 100 GPU-hours
  expected_delta: diffusion over codeword embedding (via G lifted to {-1,+1}); DDIM-sample conditioned on audio; generative decoding (not search)
  defensible_prior: partial — ICLR 2025 "Diffusion Decoders for Discrete Communication Codes" (Singh et al., Stanford) reports ~0.7 dB gain over BP for short BCH; FT8 LDPC different family
  wild_card: true
  evidence_for:
    - Reframes decoding as generation; efficient if valid codewords form low-D manifold
  evidence_against:
    - Diffusion samples can hallucinate plausible-looking-but-invalid codewords
    - PyTorch sidecar (latent diffusion is non-trivial to ONNX)
  notes: |
    Post-sampling LDPC parity-check + CRC; treat diffusion samples as
    candidates for downstream verification. Kill-switch: KL between
    sampled distribution and one-hot truth ≤5 nats.

    See research/ideation/2026-06-01-foundation-models.md (entry F11).

### hb-198 — Multi-task learning: decode + denoise + AP-injection joint training  [PRIORITY: 0.32, spawned 2026-06-01 from foundation-models ideation]
  mode: ft8 (ML / training)
  status: pending
  priority_score: 0.32
  estimated_effort: 4 sessions; 75 GPU-hours
  expected_delta: shared encoder (5M params), 3 task heads; aux tasks regularize encoder against hb-064 S2's single-task overfit failure mode
  defensible_prior: yes — vast MTL literature (MT-DNN, T5, decathlon); Caruana 1997 — using auxiliary tasks as regularizer for primary
  wild_card: false
  evidence_for:
    - Single-task supervision isn't enough at our data volumes (hb-064 S2 evidence)
  evidence_against:
    - Multi-task can hurt if aux tasks pull encoder toward features that don't help decode (negative transfer)
  notes: |
    Ablate each aux task individually before combining; only keep aux
    tasks that demonstrably help on held-out validation.

    See research/ideation/2026-06-01-foundation-models.md (entry F12).

### hb-199 — Active fine-tuning from operator's live session  [PRIORITY: 0.35, spawned 2026-06-01 from foundation-models ideation]
  mode: ft8 (ML / online learning)
  status: pending
  priority_score: 0.35
  estimated_effort: 4 sessions; 0 GPU-hours (CPU sufficient at small model sizes)
  expected_delta: successful decode (CRC-valid + FP-filtered + cqdx-rarity-sane) = high-confidence label; continual fine-tune once per hour, EMA-blend into deployed weights; personalized decoder
  defensible_prior: partial — continual/online learning literature; Federated Learning + Test-Time Training (Sun 2020); pseudo-labeling / self-training (Lee 2013); ham-specificity wild
  wild_card: false
  evidence_for:
    - Different ops will end up with different weights — feature not bug
    - 0 GPU-hours; very cheap
  evidence_against:
    - Catastrophic forgetting; operator's recent captures push model to forget rare-DX features
  notes: |
    Maintain "core" replay buffer (hard-200 + hard-1000) always mixed
    into fine-tuning batches; EWC-style regularization to anchor weights
    to pinned baseline.

    See research/ideation/2026-06-01-foundation-models.md (entry F13).

### hb-200 — RF-foundation-model (SigMF-trained) transfer learning  [PRIORITY: 0.25 (wild), spawned 2026-06-01 from foundation-models ideation]
  mode: ft8 (ML / RF-foundation)
  status: pending
  priority_score: 0.25
  estimated_effort: 6 sessions; 50 GPU-hours (~$60)
  expected_delta: fine-tune DeepSig/Northeastern RF-FM (100M+ params, MIT licensed via DARPA SAIL-ON 2025) on FT8 specifically; replaces pancetta's STFT front-end
  defensible_prior: partial — DeepSig RFNet / SAIL-ON RF-FM releases 2024-2025; NeurIPS 2024 workshop; direct FT8 fine-tuning not yet published
  wild_card: true
  evidence_for:
    - RF-FM has seen channel distortions FT8 encounters, just on different waveforms
  evidence_against:
    - RF-FMs trained on lab-captured SigMF; different SNR/antenna/RFI distributions than on-air ham
    - 100M+ params not Rust-deployable; PyTorch sidecar
  notes: |
    Include real-on-air FT8 captures in fine-tune set; don't fine-tune
    only on synth. Distillation step (F10) may be needed for production.

    See research/ideation/2026-06-01-foundation-models.md (entry F14).

### hb-201 — Neural sync / Costas-array detector (DETR-style)  [PRIORITY: 0.28 (wild), spawned 2026-06-01 from foundation-models ideation]
  mode: ft8 (ML / sync)
  status: pending
  priority_score: 0.28
  estimated_effort: 5 sessions; 50 GPU-hours
  expected_delta: small Transformer (~3M params) takes slot spectrogram, emits (freq_bin, time_offset, confidence); learned object-detector for Costas patterns; attacks RECALL not BER
  defensible_prior: partial — DETR (Carion 2020) for object detection; hb-079 win was structurally iterative sync refinement
  wild_card: true
  evidence_for:
    - FT8 doesn't live in AWGN; lives in selective fading + Doppler + co-channel
    - Learned detectors plausibly recover ground where matched-filter sync degrades
  evidence_against:
    - Object-detector models notoriously sensitive to training distribution (small objects, far-from-train aspect ratios)
  notes: |
    Heavy augmentation on Costas-pattern scale + position; adversarial
    noise examples in training. Kill-switch: precision@recall ≤
    classical correlation on held-out 1K-slot benchmark.

    See research/ideation/2026-06-01-foundation-models.md (entry F15).

### hb-202 — CAT-driven adaptive rig noise blanker (cognitive-radio loop)  [PRIORITY: 0.35, spawned 2026-06-01 from extras ideation]
  mode: ft8 (hardware-rx + cognitive-radio)
  status: pending
  priority_score: 0.35
  estimated_effort: 3-4 sessions
  expected_delta: per-pass residual energy histogram → CAT-commanded NB width / NR depth / IF passband; small RL bandit (UCB/Thompson) converges in ~50 slots/band; pancetta-as-rig-tuner
  defensible_prior: yes — FTdx10 NB1/NB2/CONTOUR/WIDTH documented; WSJT-X community wisdom is "find rig settings"; hamlib rigctl supports `L NB`, `L NR`, `B`, `W`
  wild_card: false
  evidence_for:
    - pancetta-hamlib has CAT plumbing; bandit is ~100 LOC; residual histogram exists
    - Automates the operator-knowledge gap
  evidence_against:
    - Rig-state contention with operator: if K5ARH manually tweaks NB mid-QSO, bandit state goes stale
  notes: |
    Bandit pauses when CAT readback shows operator-initiated changes.
    Kill-switch: 2-band 1-hour A/B vs operator's hand-tuned baseline;
    no band loses ≥5% decode count.

    See research/ideation/2026-06-01-extras.md (entry X1).

### hb-203 — TX-time micro-jitter for collision escape  [PRIORITY: 0.32, spawned 2026-06-01 from extras ideation]
  mode: ft8 (tx-side / cognitive-radio)
  status: pending
  priority_score: 0.32
  estimated_effort: 1-2 sessions
  expected_delta: ±50-200ms uniform jitter on detected repeated collision (same target failing to copy at same dt over 2+ slots); de-correlates from WSJT-X 0.0s pile-ups
  defensible_prior: yes — CSMA backoff in 802.11; FT8 spec permits DT up to ±2.5s before RX fails to lock; operators routinely tolerate ±0.3s
  wild_card: false
  evidence_for:
    - When another stable TX-er has also hit 0.0, collision becomes phase-stationary
  evidence_against:
    - Operator perception of "off-time" TX
  notes: |
    Jitter only on detected collision; log every event so operator sees
    why. Kill-switch: loopback sim 2 TX-ers, jitter completes ≥20% more
    QSOs; non-collision QSO rate doesn't drop >2%.

    See research/ideation/2026-06-01-extras.md (entry X2).

### hb-204 — SDR I/Q tap upgrade path with phase-coherent A/B decode  [PRIORITY: 0.38, spawned 2026-06-01 from extras ideation]
  mode: ft8 (hardware-rx)
  status: pending
  priority_score: 0.38
  estimated_effort: 4-6 sessions
  expected_delta: parallel SDR I/Q ingest (pancetta-audio extended with I/Q source trait); decode both rig PCM + SDR I/Q; bit-exact compare; eventual phase-coherent fusion (~3dB array gain)
  defensible_prior: yes — KiwiSDR + WSJT-X is known offline workflow; phase-coherent receive diversity standard in HF (3 dB array gain for 2 chains)
  wild_card: false
  evidence_for:
    - Isolates RX hardware contribution from decoder contribution
    - Initial A/B with $30 RTL-SDR before committing to better gear
  evidence_against:
    - Hardware investment for the operator
  notes: |
    Kill-switch: 1-week passive A/B; SDR delivers ≥5% more unique
    decodes on hard-200-style real corpus.

    See research/ideation/2026-06-01-extras.md (entry X3).

### hb-205 — Decoder warm-up via serialized hot state on disk  [PRIORITY: 0.28, spawned 2026-06-01 from extras ideation]
  mode: ft8 (operational hygiene)
  status: pending
  priority_score: 0.28
  estimated_effort: 1-2 sessions
  expected_delta: serialize FFT plan twiddle factors, Costas templates, OSD HRT-permutation buffers, BP message-buffer allocations on clean shutdown; mmap-restore in <100ms; pre-warm rayon thread pool with synthetic silence
  defensible_prior: yes — JIT'd code paths show 2-10× latency for first call; FFTW + rustfft have plan-caching APIs; firecracker cold-start literature
  wild_card: false
  evidence_for:
    - Autonomous station rebooted after power loss/update can lose first slot if warm-up exceeds 15s
  evidence_against:
    - Stale serialized state on decoder code changes
  notes: |
    Version tag on serialized blob; mismatch → cold start. Kill-switch:
    cold-start exceeds 10s on operator MiniPC in instrumented tests.

    See research/ideation/2026-06-01-extras.md (entry X4).

### hb-206 — WSJT-X plug-in adapter (pancetta-as-decoder CLI)  [PRIORITY: 0.45, spawned 2026-06-01 from extras ideation]
  mode: ft8 (integration / community-lever)
  status: pending
  priority_score: 0.45
  estimated_effort: 3-4 sessions
  expected_delta: pancetta-decoder CLI binary; WAV on stdin, WSJT-X-format decode lines on stdout; drop-in for jt9 --ft8; "X% more than jt9 on YOUR shack" is strongest community lever
  defensible_prior: yes — jt9 I/O format documented (WSJT-X source, GPL); CLI surface stable; JTDX is literally a fork that did this with different decoder
  wild_card: false
  evidence_for:
    - Top-3 by potential disruption per extras ideation (community lever)
    - Secondary benefit: WSJT-X UI captures operator-validated QSOs (log-as-truth loop, X8)
  evidence_against:
    - Compatibility tail (WSJT-X version diversity, line-format edge cases)
  notes: |
    Pin to current WSJT-X stable; bug-bash before rollout. Kill-switch:
    pancetta wins on ≥2 of 3 community-volunteer shacks by ≥5%.

    See research/ideation/2026-06-01-extras.md (entry X5).

### hb-207 — Reverse-archaeology audit: what WSJT-X removed between 2.0 and current  [PRIORITY: 0.28, spawned 2026-06-01 from extras ideation]
  mode: ft8 (meta-research)
  status: pending
  priority_score: 0.28
  estimated_effort: 1 session (audit only; spawned hypotheses get separate sessions)
  expected_delta: git-archaeology of WSJT-X repo, diff 2.0.0 → current, filter on deletions in lib/; harvest mechanisms removed for UI-decluttering / performance reasons that don't apply to headless autonomous pancetta
  defensible_prior: yes — mr-001 (current WSJT-X-Improved) and mr-002 (JTDX) audits found 11 hypotheses combined; deleted-code surface is ~5 years of removal history at ~10-20 deletions per minor version
  wild_card: false
  evidence_for:
    - Removed code may be removed for UI/dependency reasons that don't apply to pancetta
  evidence_against:
    - Removed code may have been wrong (early AP levels had FP issues)
  notes: |
    Each candidate gets mr-007 + decodability audit before bank entry.
    Kill-switch: ≥3 new hypotheses spawn → ACCEPT; <2 → close mr-009-X6.

    See research/ideation/2026-06-01-extras.md (entry X6).

### hb-208 — Adaptive notch from chirp-and-carrier detector for HF QRM  [PRIORITY: 0.35, spawned 2026-06-01 from extras ideation]
  mode: ft8 (DSP / RX hygiene)
  status: pending
  priority_score: 0.35
  estimated_effort: 2-3 sessions
  expected_delta: CFAR detector + chirp-rate matched filter identifies persistent narrow-band QRM (dimmer, SMPS, OTH); adaptive notch at those frequencies pre-FFT; sacrifices truths UNDER notch for cleanup ADJACENT
  defensible_prior: yes — adaptive notch standard in HF (LMS filters, freq-domain nulling); WSPRdaemon includes carrier detector; CFAR detectors standard radar primitives
  wild_card: false
  evidence_for:
    - Synergistic with X1 (CAT-driven rig NB); needed when rig notch saturated or absent
  evidence_against:
    - Notch overshoots, kills truths adjacent to QRM
  notes: |
    Narrow notch (~30 Hz wide), CFAR threshold tuned for high-confidence
    detection. Kill-switch: 100 synthetic FT8-with-QRM slots; notch
    recovers ≥5% lost decodes; no truths suppressed on QRM-free slots.

    See research/ideation/2026-06-01-extras.md (entry X7).

### hb-209 — Log-as-ground-truth: operator-completed QSOs validate decodes retroactively  [PRIORITY: 0.45, spawned 2026-06-01 from extras ideation]
  mode: ft8 (operational hygiene / corpus)
  status: pending
  priority_score: 0.45
  estimated_effort: 1-2 sessions
  expected_delta: ADIF-to-decode-log join; auto-labels decodes leading to completed QSOs as operator-validated TPs; auto-growing real-band truth corpus; (a) precision benchmark, (b) training data for learned components, (c) regression detector
  defensible_prior: yes — ADIF is universal QSO format and pancetta already writes it; ground-truth structure conservative (implies only "callsign was on-air around this time")
  wild_card: false
  evidence_for:
    - Top-3 by potential disruption per extras ideation (auto-growing truth)
    - Operator has been producing labels since Phase 4
  evidence_against:
    - Time-window ambiguity (multiple decodes of same callsign in window)
  notes: |
    Label all matches in window; downstream uses filter. Kill-switch:
    ≥50 labels/week → enough to grow ~2000-label corpus over year.

    See research/ideation/2026-06-01-extras.md (entry X8).

### hb-210 — Differential decode: same WAV with permuted candidate order  [PRIORITY: 0.32, spawned 2026-06-01 from extras ideation]
  mode: ft8 (differential analysis / dev-infra)
  status: pending
  priority_score: 0.32
  estimated_effort: 1 session (diagnostic; if promising, production wiring is X9b)
  expected_delta: 2 parallel decode passes with different candidate orderings (descending sync vs ascending time-bin vs random seed); union from both; quantifies multipass-greediness penalty
  defensible_prior: yes — greedy algorithms suffer order-dependence; iterative subtraction (hb-079) is greedy; multi-order union is well-trodden LDPC-decoder schedule fix
  wild_card: false
  evidence_for:
    - Distinct from hb-028 (cross-decoder ensemble — different decoders); cross-order, same decoder
    - Distinct from hb-079 multipass (single-order iterative)
  evidence_against:
    - 2× compute for marginal gain
  notes: |
    Only run multi-order on WAVs where first pass found <N decodes
    (suggests hard WAV). Kill-switch: median union exceeds max
    single-pass by ≥1 decode/WAV.

    See research/ideation/2026-06-01-extras.md (entry X9).

### hb-211 — Anomaly detection on residuals: ML-trained weird-residual tagger  [PRIORITY: 0.38, spawned 2026-06-01 from extras ideation]
  mode: ft8 (ML / residual-position gate)
  status: pending
  priority_score: 0.38
  estimated_effort: 3-4 sessions
  expected_delta: small CNN anomaly detector (MobileNet-style, <100k params) on residual spectrogram patches labeled "truth here / not"; per-position anomaly score → decode only top-K positions
  defensible_prior: yes — anomaly detection on spectrograms is solved problem class (sound event, machine-condition); MobileNet-V3-small runs in microseconds on MiniPC
  wild_card: false
  evidence_for:
    - Distinct from hb-094 (residual denoising AE acts on pixels), hb-095 (replaces LLR extraction), hb-064 (post-BP)
    - Complementary to hb-093 (analytical residual SNR gate); could ensemble
  evidence_against:
    - Distribution drift: training-time synth doesn't match production real-band
  notes: |
    Continuous fine-tune from X8 operator-validated labels mitigates
    drift. Kill-switch: precision ≥0.90 at recall ≥0.95 → skip ≥10%
    positions with <5% truth loss.

    See research/ideation/2026-06-01-extras.md (entry X10).

### hb-212 — Wiener filter on residual spectrogram (SNR-optimal pre-decode cleanup)  [PRIORITY: 0.40, spawned 2026-06-01 from extras ideation]
  mode: ft8 (DSP)
  status: pending
  priority_score: 0.40
  estimated_effort: 1-2 sessions
  expected_delta: per-bin noise PSD from non-Costas-aligned positions; Wiener filter to residual before final decode pass; provable MMSE optimality under WSS assumption
  defensible_prior: yes — Wiener (1949), DSP textbook; HF radio adaptive equalizers (WSPR); PSD estimator is existing par_estimate_snr_spectrogram re-aimed at no-truth positions
  wild_card: false
  evidence_for:
    - Synergistic with hb-079 (multipass) and hb-090 (matched filter); they combine multiplicatively
    - Different from X10 (analytical, no training); different from X7 (continuous attenuation per-bin SNR, not sharp cuts)
  evidence_against:
    - Stationarity assumption (WSS) — HF noise is non-stationary
  notes: |
    Short-time Wiener (per-slot windowed PSD); residual is slot-local.
    Kill-switch: top-20 hard-200; median post-Wiener residual SNR
    ≥1 dB AND additional decode count ≥5 across 20 WAVs.

    See research/ideation/2026-06-01-extras.md (entry X11).

### hb-213 — AVX-512 / SIMD LDPC BP on operator's MiniPC (elapsed budget enabler)  [PRIORITY: 0.42, spawned 2026-06-01 from extras ideation]
  mode: ft8 (performance / micro-architecture)
  status: pending
  priority_score: 0.42
  estimated_effort: 3-5 sessions
  expected_delta: rewrite BP inner loop (min-sum on Tanner graph edges) using AVX-512 packed SIMD; 4-8× speedup; ELAPSED BUDGET expands for new mechanisms in same wall-clock
  defensible_prior: yes — SIMD LDPC published (GPU 5G NR decoders; NIST BIKE/HQC uses AVX2); 4-10× routine; Rust std::simd stabilizing
  wild_card: false
  evidence_for:
    - hb-093 / hb-094 / hb-090 are partially elapsed-budget-gated; 3× BP speedup → 3× more headroom
    - Top-3 by potential disruption per extras ideation (elapsed budget enabler unlocks ALL pending mechanisms)
  evidence_against:
    - SIMD bug introduces non-bit-exactness; bit-exactness with ft8_lib is core invariant
  notes: |
    Bit-exact test suite (~295 tests) must pass before merge; SIMD path
    opt-in until validated. Kill-switch: ≥2× speedup on full-decode
    workload; bit-exactness on every hard-200 WAV.

    See research/ideation/2026-06-01-extras.md (entry X12).

### hb-214 — Streaming continuous-spectrogram decoder (no slot batching)  [PRIORITY: 0.32, spawned 2026-06-01 from extras ideation]
  mode: ft8 (architecture / streaming)
  status: pending
  priority_score: 0.32
  estimated_effort: 4-6 sessions (architectural change; significant test surface)
  expected_delta: rolling-buffer continuous decoder; partial decode passes every ~1s; slot-edge truncation soft; pairs with hb-091 (a8 early-decode); recovers slot-alignment errors
  defensible_prior: yes — SDR-style streaming is rule in WSPR (WSPRdaemon); FT8 was designed batch because WSJT-X PC-clock-aligned slot model but protocol permits continuous
  wild_card: false
  evidence_for:
    - DT > 1.5s becomes soft constraint instead of hard truncation
  evidence_against:
    - Architectural disruption to coordinator's slot-aligned QSO state machine
  notes: |
    Streaming runs parallel to batched until proven; coordinator
    consumes slot-aligned events. Kill-switch: ≥5% additional decodes
    (slot-edge cases); no FP not present in batch.

    See research/ideation/2026-06-01-extras.md (entry X13).

### hb-215 — Per-band propagation prior from VOACAP for AP weight tuning  [PRIORITY: 0.35, spawned 2026-06-01 from extras ideation]
  mode: ft8 (propagation-aware decoding)
  status: pending
  priority_score: 0.35
  estimated_effort: 3-4 sessions
  expected_delta: VOACAP-style propagation prediction (UTC + band + grid → per-DXCC reachability P); multiply into AP candidate ranking; recall-preserving compute-budget reallocation (no templates deleted, just reordered)
  defensible_prior: yes — VOACAP is industry-standard HF propagation model (NTIA, ITU); pancetta's AP module already templates across DXCC
  wild_card: false
  evidence_for:
    - 20m dead to Europe at 02Z local → AP shouldn't waste cycles on OE/HA/UA; 20m wide open to JA at 05Z → AP prefers JA1/JA2/JA3
    - Distinct from cqdx rarity (operational); VOACAP is decoder-level template-plausibility
  evidence_against:
    - VOACAP model staleness (solar cycle drift); wrong predictions during geomagnetic storms
  notes: |
    Prior multiplicative not exclusive — unlikely templates still get
    tried, just later in budget. Kill-switch: hard-200 + 2026
    contest-weekend WAVs; AP recall ≥5% with propagation prior.

    See research/ideation/2026-06-01-extras.md (entry X14).

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

### mr-008 — Post-V2/V3/088/087/036/064 closures ideation pass  [COMPLETED 2026-05-31]
  status: completed (this session)
  estimated_effort: 1 session (reading + WebSearch + bank refill)
  source_type: mixed — external (WebSearch) + internal (tonight's 5 shelve journals) + cross-discipline
  source: |
    arXiv 2502.16371 (dense NN MFSK demod), arXiv 2404.14165 (sliding-
    window OSD, updated 2025), WSJT-X-Improved v3.x release notes
    (a8 decoding tech), Q65 multi-window averaging, AutoFT8 /
    FT8Commander ops data, self-supervised audio denoising literature
    2025, plus internal closure journals for hb-086 V2/V3, hb-087, hb-088,
    hb-036, hb-064.
  outcome: |
    Tonight closed five hypothesis families:
      - Soft cancellation (hb-086 V2 across 2 corpora)
      - Sync threshold relaxation (hb-086 V3)
      - OSD-without-Costas single-position LLR mining (hb-088)
      - Callsign-priors-on-residual (hb-087 Session 2)
      - Neural OSD retrain at small corpus (hb-064 Session 2)
      - Score-relative NMS (hb-036)
    The structural picture afterward: sub-Costas residuals on dense
    hard-200 are interferer-dominated, not weak-truth-dominated.
    Future progress is in 4 open territories: (A) better residual
    quality, (B) different decode targets (precision/throughput/latency),
    (C) different signal class, (D) ML/learned augmentation bounded
    scope, (E) operational/autonomous loop.
    Yield: 12 new hypotheses (hb-089..hb-100) distributed across
    territories. 3 candidates explicitly rejected at generation by
    mr-007 (anti-pattern documentation in the ideation journal).
    Top-5 active hypotheses post-ideation (priority order):
      hb-093 (residual SNR pre-decode gate, 0.52)
      hb-048 a7 (template cross-correlation, 0.45)
      hb-089 (multi-cycle coherent residual accumulation, 0.48)
      hb-064 Session 3 (DIA-OSD bigger corpus, 0.42)
      hb-091 (a8 early-decode latency, 0.42)
    The bank is REFILLED with diverse-territory candidates respecting
    the structural picture from tonight's 5-shelve session.
  expected_yield: 8-15 new hypotheses (actual: 12 spawned + 3 rejected at generation)
  defensible_prior: yes — direct response to bank shrinkage from tonight's closures; structural picture from journals is solid
  journal: research/experiments/2026-05-31-mr-008-ideation.md

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
  status_2026_05_31_session1: |
    SCOPED. Capture procedure + eval-tier stub landed (no production
    decoder change). Operator-physical work remaining.
    - Procedure doc: docs/operations/2026-05-31-hb-073-kiwisdr-capture-procedure.md
      (conditions, KiwiSDR site selection, slot-aligned kiwirecorder.py
      command, ingestion pipeline, operator action-item checklist).
    - Eval-tier stub: `wild-doppler-50` added to
      pancetta-research/src/bin/eval.rs tier dispatch. Missing manifest
      treated as SKIP (logs to stderr, returns 0-WAV TierResult); does
      not break existing eval runs. Smoke-test verified.
    - Manifest expected at
      research/corpus/curated/ft8/wild_doppler_50.manifest.json once
      operator captures + curates 30-60 Doppler-rich WAVs (target ~50
      after filtering for jt9-recovery ≤ 0.70).
    - Unblocks: hb-015 (bump 0.38 → ~0.42 once manifest lands),
      hb-077 (partial — KiwiSDR audio captures cover the same propagation
      regime; phase-coherent IQ via HackRF/GPSDO still operator-pending).
    - Scoping journal: research/experiments/2026-05-31-hb-073-scoping.md.

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

### hb-088 — OSD-without-Costas-pre-gate (residual LLRs → OSD flip enumeration, BYPASS Costas threshold)  [SHELVED 2026-05-31 pre-implementation, strong diagnostic finding]
  mode: ft8
  status: shelved
  priority_score: 0.10  (intended at scoping = 0.35; downgraded by diagnostic SHELVE)
  estimated_effort: (not implemented; 3 sessions if PROCEED — Session 1 diagnostic DONE, Sessions 2/3 not justified)
  expected_delta: 0 (diagnostic SHELVED; no production change warranted)
  defensible_prior: structurally plausible at hypothesis stage — OSD's flip enumeration is independent of BP convergence, and at sub-Costas-but-not-noise-only positions the LLR sign pattern COULD encode a weak truth signal. The diagnostic disproves the empirical premise on busy-band hard-200.
  wild_card: false
  evidence_for:
    - Hypothesis premise: OSD-2 enumerates C(91,2)=4095 flip patterns over most-reliable basis; CRC-14 gates per pattern. At positions where channel LLRs have ~80% sign agreement with the truth codeword, OSD-2 can find the truth in ≤14 flips. This was the structural argument.
    - Sub-Costas |LLR| at truth positions IS ~82% of control's |LLR| — there IS energy at these positions (kill-switch a passes handsomely). The premise "sub-Costas = noise-only" was wrong.
  evidence_against:
    - Diagnostic (hb088_osd_without_costas_feasibility.rs, top-20 hard-200): median LLR-sign-agreement with truth codeword at sub-Costas positions = 50.6% (essentially coin-flip). p10 = 45.4%, p90 = 55.7%. Compare control = 92.0% median.
    - 50.6% agreement means ~85 of 174 bits are sign-wrong. OSD-2 budgets 2 flips, OSD-3 budgets 3, OSD-4 budgets 4. None can correct 85 bits.
    - Structural explanation: sub-Costas energy at the truth's freq_bin window comes from NEIGHBOR LEAKAGE and interferer tones, not from the truth's own (weakly) tone-aligned energy. max-log demod picks the LOUDER tone (the interferer), not the truth's tone.
    - FP-control argument was structurally weak even pre-diagnostic: CRC-14 catches 1/16384 by chance; OSD-2 enumerates 4095 patterns × 300+ sub-Costas positions per WAV ⇒ ~75 CRC-FPs per WAV expected. With high-energy garbage LLRs (not noise-only), these FPs would have non-trivial bit patterns and beat shallow plausibility checks at non-negligible rates.
    - Closes single-position spectrogram-mining family on current hard-200 corpus + implementations tested. V3, hb-082, hb-088 all confirm: dense busy-band sub-Costas energy is interferer-dominated, not weak-truth-dominated. The per-position-LLR mechanism family is closed on busy-band real-audio; sparser corpora and structurally-different mechanisms (e.g. phase-coherent IQ matched filter at truth coords, hb-090) remain unrefuted. Phase A honesty pass (2026-06-02) replaced "family closed" with this scoped phrasing.
  notes: |
    Diagnostic: pancetta-research/examples/hb088_osd_without_costas_feasibility.rs
    Scoping journal: research/experiments/2026-05-31-hb-088-scoping.md
    Sibling: hb-087 (callsign-priors-on-residual, ALSO SHELVED 2026-05-31
    at Session 2 — 0/10 AP-decode rescue). hb-087's null result confirms
    that even the AP-pinned-28-bits constraint can't pull BP across the
    gap when the other 146 LLRs are noise-dominated. The two
    bypass-Costas mechanisms close together; the family wall is
    "noise-dominated residuals at sub-Costas positions", structurally
    inaccessible to per-position AP or OSD techniques.

    Re-visit conditions:
    - Sparser corpus (rural VHF, low-activity HF) MIGHT show higher
      sub-Costas sign-agreement (truths on noise floor, not on interferer
      floor). Re-run the diagnostic on any new sparse-band curated tier
      before re-considering.
    - The "coherent IQ matched-filter at truth coordinates" mechanism
      family is structurally different (rejects neighbor-bin energy via
      phase-coherent matched filtering at the truth's exact freq + dt).
      Could be spawned independently if hb-087 graduates and the wall
      warrants further attacks.

    Methodology contribution: LLR sign-agreement with encoded truth
    codeword is the right kill-switch for any per-position-LLR-mining
    mechanism. Cheap (~10s on top-20 hard-200), definitive
    (decodability proxy, not geometric/coverage proxy). Bake into
    the spec template for "find more candidates from existing
    spectrogram" mechanisms — companion to V3's "geometric proximity
    is not decodability" doctrine.
  learning: |
    Two facts collapsed the hypothesis space here. (1) Sub-Costas LLR
    energy is real (not noise-only) — this surprises if the priors
    were "missing decodes = below noise floor". (2) Sub-Costas LLR
    SIGNS at the truth's known position are random — this is the
    interferer-leakage signature. The decoder's max-log demodulator
    at a sub-Costas freq_bin in a busy band is reading the dominant
    LOCAL signal's tones, not a weak buried truth's tones. The wall
    is structural to single-position spectrogram mining.

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
