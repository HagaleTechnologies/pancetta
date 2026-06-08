# Batch 46 — implementation plan from 16 clean-room specs

**Date**: 2026-06-08
**Process**: Read SPECS only (firewall-respecting); never opened GPL source during plan-writing.
**Inputs**: 16 algorithm specs in `research/specs/` produced by 5 reader agents

## Spec inventory (16 files, ~4700 lines total)

| File | Source | Bank entry | Reader status |
|---|---|---|---|
| spec-wsjtr-sync-bc.md | wsjtr | hb-242 | clean; flagged 2 doc-vs-source discrepancies |
| spec-wsjtr-sync-norm.md | wsjtr | (new candidate) | clean |
| spec-wsjtr-grid-refinement.md | wsjtr | (new candidate) | clean |
| spec-wsjtr-cached-bandpass-downsampler.md | wsjtr | hb-243 | clean; identifies as biggest sensitivity gap |
| spec-wsjtr-f64-tanh-bp.md | wsjtr | (new candidate) | clean |
| spec-wsjtr-zsum-osd-init.md | wsjtr | (new candidate) | **PAIRED-WITH dmin-fix** |
| spec-wsjtr-cross-sequence-a7.md | wsjtr | hb-237 | clean |
| spec-wsjtr-dt-refinement-during-subtract.md | wsjtr | hb-240 | wsjtr measured +1.9% recall |
| spec-wsjtr-per-pass-variation.md | wsjtr | hb-241 | 4 mechanisms identified |
| spec-ft8mon-sub-bin-costas.md | ft8mon | hb-225 | clean |
| spec-ft8mon-soft-decode-pairs.md | ft8mon | hb-223 | one function signature flagged — review |
| spec-ft8mon-known-tone-refinement.md | ft8mon | hb-222 | clean |
| spec-ft8mon-apriori-bit-prior.md | ft8mon | hb-227 | extraction methodology described |
| spec-jtdx-3method-sweep.md | JTDX | hb-228 | needs pancetta-specific syncmin recalibration |
| spec-jtdx-qso-partner-filter.md | JTDX | hb-229 | near drop-in via hb-091 |
| spec-jtdx-relaxed-sync-near-partner.md | JTDX | hb-230 | clean |

## Tier-1 ship candidates (top 4 for Batch 47-49)

### #1 — hb-229 QSO partner band-collapse (Batch 47 SHIP)

**Why first**: Cheapest, lowest-risk, leverages existing infra.

- Plumbing: pancetta has hb-091 `freq_bin_range` already wired
- New work: QSO-state observer in `coordinator/pipeline.rs` to feed
  partner freq into decoder
- Effort: ~250 LOC, 1 session
- Risk: minimal
- Outcome: pure CPU win during in-flight QSO; same recall, less FP
  exposure outside target band

### #2 — hb-242 wsjtr sync_bc partial Costas metric (Batch 47 SHIP)

**Why second**: Targets pancetta's biggest documented coverage hole.

- Mechanism: `max(sync_abc, sync_bc)` — naturally selects partial
  metric when block A is degraded (spec corrected the "fallback gate"
  doc misreading)
- Targets: 1376-truth slot-edge negative-dt 48.3% recall bucket
- Effort: ~60-120 LOC, 1-2 sessions
- Risk: low; partial metric is well-defined; expect modest FP increase
  managed by existing hb-062 filter
- Outcome: +50-150 RR73-class slot-edge truths (per agent estimate)

### #3 — hb-228 JTDX 3-method spectral sweep (Batch 48 SHIP)

**Why third**: Single largest orthogonal recall lift candidate from
research; needs careful tuning.

- Mechanism: 3 magnitude maps (sqrt / power / L1) from same FFT;
  Costas sync on each independently
- **REQUIRES syncmin calibration**: JTDX values (1.225 / 1.5 / 1.1)
  are for linear-magnitude; pancetta uses dB-power. Calibrate per
  metric empirically.
- Effort: ~600 LOC, 3 sessions (one for calibration alone)
- Risk: medium; calibration is empirical; FP explosion if miscalibrated
- Outcome: largest expected lift from sync-stage diversity

### #4 — hb-222 ft8mon post-decode known-tone refinement (Batch 48-49)

**Why fourth**: Multipass amplifier; cleaner subtraction → cleaner residual
→ more mp=2 decodes. Pairs with hb-240.

- Mechanism: after successful LDPC+CRC, re-optimize (Hz, dt) via
  12-point 2D grid scored by phase-coherence; use refined coords
  for subtraction
- Effort: ~200-300 LOC, 1-2 sessions
- Only useful when `max_decode_passes > 1` (Fast tier baseline)
- Outcome: amplifies mp=2's existing yield

## Tier-2 ship candidates (Batch 49-50)

### hb-237 cross-sequence A7 (PLAN-SIZED)

- 5-6 sessions, ~550 LOC. Requires CrossSequenceCallCache in pancetta-qso.
- Big strategic win: leverages pancetta's autonomous-station QSO state
  that wsjtr explicitly avoids.
- **FP-amplification risk** flagged by spec: if seed callsigns are FPs,
  hb-237 injects fake templates into next slot. Mitigation: dmin/dmin2
  gate (1.3× margin) + hb-062 trust set check on seed callsigns.

### hb-223 ft8mon soft_decode_pairs

- Independent two-symbol coherent pair LLR producer; runs in parallel
  with single-symbol soft demod; whichever produces valid LDPC codeword
  wins.
- 150-250 LOC, 1-2 sessions.
- Requires complex (phase-preserving) per-symbol FFT bins.
- Expected: ~1-3 dB on slow-fading channels.
- **Action needed**: agent flagged one function signature as possibly
  verbatim — review spec before implementation. If verbatim, rephrase.

### hb-225 ft8mon 4×4 sub-bin Costas grid

- Cached global FFT + bin-rotation trick (not 16 fresh FFTs).
- Targets band-middle 1000-2000 Hz scalloping recall hole.
- Spec recommends Fast-tier-only via config flag.
- 250-400 LOC, 2-3 sessions.

### hb-243 wsjtr cached-bandpass downsampler

- Single biggest sensitivity gap vs WSJT-X per spec.
- **Structural replacement** for pancetta's spectrogram-throughout
  pipeline — high risk, high reward.
- 750-950 LOC, 3-4 sessions.
- Defer until top-2 ship cleanly; this is the heaviest item in queue.

### hb-241 wsjtr per-pass parameter variation

- Four independent mechanisms identified by spec:
  1. amplitude/power metric split (pass 1 amp, pass 2+ power) — **safest**
  2. depth-escalation 1→2→3→3
  3. sync-relaxation 1.00/0.85/0.75/0.65 with 0.5· clamp
  4. subtraction-order cycling
  5. + cheap skip-pass-on-zero-decodes guard
- Spec recommends shipping metric-split first; others opt-in.
- 200-300 LOC for metric-split, 1-2 sessions.
- Tier-gated off on Slow.

## Tier-3 (lower-priority / paired)

### hb-238/zsum OSD init — PAIRED with dmin fix

Spec explicitly flags: "without distance-fix, zsum-init ships a known
FP regression." Implementation strategy must combine:

1. First: implement OSD distance tracking (`dmin`-based selection
   across attempts) — replaces pancetta's current "first CRC-pass wins"
2. Then: add zsum snapshots at BP iter 1 and 2; OSD attempts on each

BUT — pancetta's hb-238 audit in Batch 45 concluded the dmin fix
is a research-level redesign with uncertain net impact. Need to
decide whether to invest in this multi-part change.

Estimated: 100 LOC (dmin redesign) + 280 LOC (zsum snapshots) = 380 LOC,
3 sessions, MEDIUM risk.

Defer to Batch 50+ pending top-4 ship results.

### hb-240 wsjtr DT refinement during subtract

- 21-point grid at 5-sample spacing + parabolic interpolation; skip-on-large-deviation
- Wsjtr measured **+1.9% recall** (+80 / 4314)
- Recommends between-pass only (not within-pass)
- 200 LOC, 1-2 sessions
- Pairs with hb-222 (both subtraction amplifiers)

### hb-227 ft8mon apriori bit prior

- **Specific values cannot be lifted** (ft8mon's are GPL-licensed corpus
  output). Pancetta must derive from own corpus via:
  1. Validated-decode corpus → bit-frequency counts → Laplace smoothing
  2. ~150 LOC extraction tool + ~30-50 LOC fusion in LDPC init
- Expected: small modest +TPs on common message types
- 2 sessions

### hb-230 JTDX relaxed sync ±3 Hz near partner

- Pairs with hb-229 QSO partner band-collapse
- Stacks with hb-217 RR73 fix at sync layer
- 160 LOC, 1 session

### wsjtr 40th-percentile sync normalization (new candidate)

- Adaptive noise-floor estimation; spec describes flat 1.2 threshold
  (NOT the doc summary's scaled values — agent flagged discrepancy)
- 40-80 LOC, 1-2 sessions
- Could replace pancetta's existing dB threshold behind a toggle

### wsjtr 5×5 grid refinement via Goertzel (new candidate)

- Sub-bin precision both axes
- ~4200 Goertzel evaluations per refined candidate
- Spec recommends gating by hardware tier (Fast: ~1000 candidates,
  Moderate: ~100, Slow: 0)
- 270 LOC, 2-3 sessions

### wsjtr f64 tanh-domain BP (new candidate)

- Numerical precision improvement; std-lib tanh/atanh in
  log-magnitude-decomposition form
- Replaces pancetta's f32 fast_tanh/fast_atanh
- 260 LOC, 2-3 sessions
- Expected: +0.5-2% recall

### JTDX `lqsothread` bonus mode (not in spec but flagged by agent)

- Inserts 2 virtual candidates at `nfqso ± 5s` DT
- Force-emit family member; complements hb-217 RR73 fix
- Worth a separate small spec + ~50 LOC implementation

## Recommended Batch 47 ship target (next session)

**hb-229 + hb-242 paired ship** (both Tier-1 #1 and #2):

1. **hb-229 QSO partner band-collapse** — ~250 LOC; uses hb-091 freq_bin_range; adds QSO state observer in coordinator/pipeline.rs
2. **hb-242 sync_bc partial Costas** — ~60-120 LOC; adds in decoder.rs sync path

Both leverage existing pancetta infrastructure (hb-091 plumbing,
existing 3-block sum sync). Both have low risk. Both can be shipped
together in 1-2 sessions of an Implementer thread.

**Implementer thread rules** (Batch 47+):
- Dispatch fresh Agent for code-writing
- Implementer has access to: pancetta source + `research/specs/*.md`
- Implementer does NOT have access to wsjtr / ft8mon / JTDX source
- Implementer follows hypothesis bank conventions: commit cites
  hb-NNN; comments say "inspired by spec ref" not "ported from wsjtr"

## Dependencies / blockers identified

1. **hb-238/zsum pairing**: distance-fix MUST land before zsum (or ship
   together as one unit) — otherwise FP regression
2. **hb-228 syncmin calibration**: empirical per-metric; could need
   a synth-clean corpus probe to land before main implementation
3. **hb-243 cached-bandpass**: structural replacement of pancetta's
   spectrogram pipeline; high blast radius; defer until top-2 ship
4. **hb-237 FP-amplification**: gate seed callsigns through hb-062
   trust set before using them as templates
5. **hb-227 apriori**: must NOT lift ft8mon's specific values; must
   derive pancetta-corpus table first

## Counters

- Specs produced: 16
- New bank candidates surfaced: 5 (wsjtr sync-norm, wsjtr grid-refinement,
  wsjtr f64 tanh BP, wsjtr zsum OSD init, JTDX lqsothread virtual
  candidates)
- Existing bank entries with specs ready: 11 (hb-222/223/225/227/228/
  229/230/237/240/241/242 + hb-243 spec written)
- Implementation queue ready: top-2 paired for Batch 47

## Substance-check notes

- **Reader agents demonstrated clean-room discipline**: prose-only,
  cited paths-as-traceability, flagged doc-vs-source discrepancies,
  one agent explicitly noted possibly-verbatim function signature for
  review.
- **Some specs paired**: zsum-init paired with dmin-fix; hb-222 paired
  with hb-240 (both subtraction quality). Implementation plans must
  respect these dependencies.
- **Sane scope sizing**: spec line counts (206-429 LOC) and effort
  estimates (1-4 sessions) suggest the implementation queue is
  realistic for the next 6-10 batches.
