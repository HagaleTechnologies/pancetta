---
slug: batch-7-mr001-followups
mode: ft8
state: mixed (1 partial-win, 2 shelves, 1 confirmation, mr-003 bank refill)
created: 2026-05-25T00:00:00Z
last_updated: 2026-05-25T00:00:00Z
branch: iter/2026-05-25-batch-7-mr001-followups
disposition: |
  hb-044 (sub-sample DT): CONDITIONAL WIN — synth-clean SNR@90% improves
    -18 → -20 dB; but hard-200 regresses -116 recovered. Doesn't graduate
    as-is. Spawned hb-068 for conditional/scaled variants.
  hb-046 (two-stage scheduling): ARCHITECTURE MISMATCH per mr-007 audit.
    The WSJT-X benefit is latency, not sensitivity; pancetta is offline.
    Two implementation variants both gave zero delta. SHELVE.
  hb-034 with filter: CONFIRMS original SHELVE. Filter doesn't rescue
    OSD-3 (still -1 real decode vs OSD-2).
  mr-003 academic LDPC audit: RICH HARVEST — 5 candidates with
    mr-007 audit applied at harvest time. 3 clean-attach + 1
    plan-sized + 1 deferred.
---

## Context

Batch 7 per the multi-batch plan: mr-001 follow-ups (hb-044, hb-046)
+ remaining hb-053 revisit (hb-034). mr-003 (academic LDPC literature
audit) launched as background Explore agent on iter 1 to address
the growing wild-card debt.

## Iter 1: hb-044 part 1 — parabolic sync refinement (compute side)

### Implementation

`pancetta-ft8/src/decoder.rs`:
- `CostasCandidate` now derives `Copy, Debug` and gains
  `time_refinement: f64` (fractional time-bin offset in [-0.5, +0.5]).
- New `Ft8Config::sync_time_interpolation: bool` flag (default false).
- New free function `parabolic_peak_refinement(y_left, y_center, y_right)
  → (refined_score, fractional_offset)`. Returns (y_center, 0.0) if
  not concave-down.
- In `costas_sync_search`, when flag is on AND candidate fits in
  bounds (t0 > 0 && t0+1 ≤ max), compute scores at t0±1, fit parabola.
- 3 new unit tests in `parabolic_tests` module.

`pancetta-research/src/decoder.rs`: `with_sync_time_interpolation(on)`
builder. eval.rs: `--sync-time-interpolation` flag.

### Sanity

Build clean. 192 tests pass (was 189 + 3 new parabolic_tests).

---

## Iter 2: hb-044 part 2 — symbol extraction wire + eval

### Implementation

`lookup_time_interp` helper function. Used in both
`extract_symbols_from_spectrogram` (single-threaded) and
`par_extract_symbols_from_spectrogram`. When dt=0, exact original
behavior (no perf cost). When dt≠0, linear interpolation between
adjacent time bins in the spectrogram.

### Sweep result (curated-hard-200 + synth-clean)

| Tier | Metric | baseline | hb-044 ON | Δ |
|---|---|---:|---:|---:|
| curated-hard-200 | recovered | 4365 | **4249** | **−116** |
| curated-hard-200 | novel | 952 | 925 | −27 |
| curated-hard-200 | decode_rate | 0.50898 | 0.49545 | −0.01353 |
| synth-clean | SNR@50% (dB) | −20.0 | **−20.0** | = |
| synth-clean | SNR@90% (dB) | −18.0 | **−20.0** | **−2 dB** |
| synth-clean | −20 dB bin | 5/6 | **6/6** | +1 |

### Analysis

**WSJT-X-Improved's claim validated on synth-clean.** SNR@90%
improvement of 2 dB at the boundary cell is exactly the "sub-sample
DT refinement gives 1-2 dB sensitivity" finding. Real signal.

**But hard-200 regression is also real and larger in magnitude.**
−116 recovered (−2.7%) — the refinement is over-aggressive on
real-world multi-slot WAVs. Hypotheses for why:
1. Refinement may push some candidates AWAY from the true peak when
   noise creates spurious gradients at neighboring time bins.
2. Score inflation from refinement (refined ≥ integer) may cause
   noisier candidates to displace better ones in the top-300 cap.
3. The hard-200 WAVs have many candidates per slot; even a tiny
   per-candidate noise injection compounds.

### Disposition

**SHELVE hb-044 as-is.** Composite weight on hard-200 (0.5) >
synth-clean (0.3), so the regression dominates.

**Spawn hb-068**: conditional/scaled variants. Possible follow-ups:
1. Apply refinement only when sync_score > threshold (high-confidence
   candidates).
2. Apply 0.5× scaling on delta (half-step instead of full).
3. Reject refinement when |delta| > 0.3 (only accept tiny refinements).
4. Apply refinement only to candidates that would be DROPPED by the
   integer-bin score; never displace high-confidence ones.

---

## Iter 3+4: hb-046 two-stage scheduling

### Implementation (iter 3 — v1 subset)

`pancetta-research/src/decoder.rs::Ft8Decoder` gains
`two_stage_first_config: Option<Ft8Config>` field + `with_two_stage(on)`
builder. When on, runs a cheap-pass first (sync_cap=100, no OSD,
iters=25), then standard pass, unioning dedup'd by message text.
`--two-stage` CLI flag.

### Sanity v1 (curated-hard-200)

baseline: 4365 rec / 952 novel
two-stage v1: 4365 rec / 952 novel → **Δrec=0, Δnov=0**

**The cheap pass is a strict SUBSET of std (smaller cap, weaker OSD,
fewer iters).** Anything cheap finds, std also finds. v1 is
algorithmically incapable of adding decodes.

### Implementation (iter 4 — v2 NMS-on cheap pass)

Pivoted: cheap pass with NMS ON (production has NMS OFF per hb-019).
Different candidate POPULATION, not strictly weaker. Cap=200 to
bound cost.

### Sanity v2 (curated-hard-200)

two-stage v2: 4365 rec / 952 novel → **STILL Δrec=0, Δnov=0**

NMS-on cheap pass produces a different candidate set, but **dedup
is by message TEXT** — both passes decode the same callsigns into
the same message strings, so the union doesn't add anything unique.

### Disposition

**SHELVE hb-046 — architecture mismatch (mr-007 retroactively).**

The WSJT-X-Improved "two-stage" benefit is LATENCY: nzhsym=41 lets
the decoder start working on partial slot data (before full 50
symbols received). pancetta is OFFLINE eval — we always have the
full slot. The cheap-then-thorough pattern doesn't add sensitivity
in our context; both passes converge on the same decoded messages.

This is mr-001's THIRD architecture-mismatch finding (after hb-045
and hb-047). The mr-007 audit at harvest time would have caught
this with a sharper check — JTDX's "subpass" is conceptually
different from WSJT-X-Improved's "two-stage", and JTDX-style
subpasses (per mr-002) might actually attach. Note for future
harvests: WSJT-X-Improved release-note "two-stage" ≠ JTDX
"subpass".

---

## Iter 5: hb-053 revisit hb-034 (OSD-3 + filter)

### Sweep result (curated-hard-200)

| Config | rec | novel |
|---|---:|---:|
| baseline (OSD-2, no filter) | 4365 | 952 |
| OSD-2 + filter | 4364 | 811 |
| **OSD-3 + filter** | **4363** | **811** |

OSD-3 + filter vs OSD-2 + filter: **−1 real, ±0 novel.** The filter
already kills the +novels OSD-3 added (down to identical 811);
OSD-3 still loses 1 real decode vs OSD-2.

### Disposition

**hb-034 stays SHELVED.** The original (batch-2) finding that OSD-3
loses 1 real decode is ROBUST to FP filtering. The filter
neutralizes the FP cost but the recall loss persists.

Notably this differs from hb-035 (BP iters=100 + filter = +11 real,
-134 novels — net WIN) and hb-014 (gate=6 + filter = same recall as
prod, -132 novels — defensible). hb-034 confirms that not every
shelved hypothesis becomes attractive with the filter; the filter
rescues hypotheses whose primary failure mode was FP cost, not
recall loss.

---

## mr-003: academic LDPC literature audit (background agent)

Ran in parallel during iter 1. Surveyed academic literature 2020-
2026. Headline: **field is active and on-target** for pancetta.
Short-block LDPC + BP + OSD is a recognized research niche
(driven by 5G NR control channels, CCSDS deep-space, quantum codes).

5 candidate hypotheses harvested with mr-007 audit applied:

- **hb-063 (Layered/WR-LBP scheduling)** — replace flooding-schedule
  BP with sequential layered scheduling. ~2× faster convergence.
  Could enable cutting `ldpc_iterations: 50` → 25 with same FER,
  freeing wall-clock budget. Clean attach to decoder.rs BP loop.
  arXiv:2410.13131 + Hocevar 2004.
- **hb-064 (DIA-augmented OSD with iteration trajectories)** —
  refine existing `neural_osd.rs` to use per-iteration BP LLR
  trajectories as features (not just final LLRs). Sliding-window
  classifier for early TEP termination. Paper reports 97% TEP
  reduction at SNR=2dB on (128,64). Strong fit — refines our
  existing DIA. arXiv:2404.14165.
- **hb-065 (Adaptive Gaussian-Elimination removal in OSD)** —
  skip GE on many OSD calls via two early-decision conditions.
  Profile pancetta's OSD-2 first to confirm GE dominates; if it
  does, meaningful CPU savings. arXiv:2206.10957.
- **hb-066 (BP-RNN diversity ensemble)** — N parallel BP variants
  targeting distinct trapping sets. Needs plumbing; plan-sized.
  arXiv:2206.12150.
- **hb-067 (mBP offset parameter)** — fixed offset added to BP
  messages before OSD; claim order-(m-1) OSD reaches order-m
  performance. Could let pancetta drop OSD-2 → OSD-1.
  arXiv:2306.00443.

Plus 1-2 "interesting but not pancetta-relevant" (quantum LDPC,
long-code 5G NR) flagged as skip.

Recommendation: **hb-063 first** (lowest risk, biggest potential
budget headroom). hb-065 (after profile confirmation) and hb-067
(cheap one-iter sweep) next. hb-064 is plan-sized (training
pipeline). hb-066 deferred.

---

## Batch 7 cumulative impact

- **0 graduations** (hb-044 SHELVED, hb-046 SHELVED, hb-034 SHELVE
  confirmed)
- **1 conditional WIN on synth** (hb-044 SNR@90% −2dB) — spawns
  hb-068 for scaled variants
- **2 architecture-mismatch shelves** via retroactive mr-007 audit
  (hb-046 third such finding after hb-045 + hb-047 from mr-001)
- **5 new candidate hypotheses** in bank (hb-063..hb-067) from
  mr-003 with mr-007 audit applied at harvest
- **1 new variants hypothesis** (hb-068) spawned from hb-044 finding

**Production behavior unchanged.** Composite still at 0.5545.

**The pattern across batches 4-7 with mr-007 retroactive audits:**

| External source | Candidates | Architecture-fit shelved |
|---|---:|---:|
| mr-001 (WSJT-X-Improved) | 5 | 2 (hb-045 baseline, hb-047 passband) + hb-046 now |
| mr-002 (JTDX) | 5 | 0 (mr-007 applied at harvest — none made it through) |
| mr-003 (academic LDPC) | 5 | 0 yet (mr-007 applied at harvest) |

mr-007 working as designed when applied at harvest time. The mr-001
harvest predates mr-007, so 3 of its 5 candidates ended up shelved
on architecture mismatch.

**Counters: exploitation_run 38 → 43 (5 iters), current_ratio
0.085.** Wild card debt still growing; mr-003 partially addresses
by bank refill with NEW exploration targets.

## Workflow

Fifth batch under new discipline. Branch
`iter/2026-05-25-batch-7-mr001-followups`. mr-003 ran as background
Explore agent in parallel. Single push at batch end. No data-loss
incidents.
