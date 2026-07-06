# Decoder speed overhaul: mechanical fixes + budget-governed anytime decoding

**Date:** 2026-07-06
**Status:** Approved (operator-reviewed design, this session)
**Scope crates:** `pancetta-ft8` (primary), `pancetta-config`, `pancetta` (coordinator),
`pancetta-tui`
**Prior art in-repo:** `docs/decoder-analysis.md` (2026-07-03 efficiency/research pass),
hardware-tier classification (`pancetta/src/coordinator/tier.rs`, hb-216 S2)

## 1. Motivation and evidence

The native Rust decoder is ~25x slower than C ft8_lib wall-clock on the same window —
and the honest single-thread CPU gap is **~43x** (240.8 ms vs 5.6 ms on
`pancetta-ft8/tests/fixtures/wav/wsjt/210703_133430.wav` at exactly `WINDOW_SAMPLES`;
the familiar "25x"/143 ms figure is Rayon-parallel wall time, which burns ~1.34x extra
CPU to hide the gap across cores). Root-caused 2026-07-06 by profiler
(macOS `sample`, symbolicated release build) plus one-variable-at-a-time config
ablations. The gap decomposes into four measured multiplicative factors:

| # | Factor | Multiplier | Detail |
|---|--------|-----------|--------|
| 1 | Extra recall passes | 1.79x | coherent multipass ×3 (hb-079/080) + cross-cycle averaging (hb-056/74/75) + joint-pair retry (hb-086) — deliberate recall spend ft8_lib doesn't have |
| 2 | LDPC iteration cap | 1.88x | 100 iterations vs C's 25; noise candidates burn all 100 |
| 3 | Candidate count | 4.17x | 200 sync candidates vs C's 50; cost is linear in candidates |
| 4 | Implementation gap | 3.07x | like-for-like config: 17.2 ms vs 5.6 ms |

Profile highlights driving the design:

- At production config, **`fast_atanh` calls libm `ln()` per check-node edge per BP
  iteration ≈ 33% of ALL decoder CPU** (`decoder.rs` `fast_atanh`). ft8_lib (MIT,
  vendored) uses a 4-multiply Padé rational. A live swap measured **240.8 → 193.0 ms
  (−20%)** with unchanged decodes on the fixture.
- At like-for-like config the residual 3.07x is: **Costas sync scoring ~44%**
  (~170-read f64 kernel per swept cell over a jagged `Vec<Vec<Vec<f64>>>`; C reads a
  contiguous u8 waterfall), **f64-complex rustfft + per-bin f64 `log10` ~24%** (C uses
  f32 *real* FFT), BP ~13%, neural-OSD inference ~7%.
- Factors 1–3 are recall features and search breadth — the "better than ft8_lib" spend.
  They must not be cut; they must become **schedulable**.

Goal (operator framing): **better AND faster** — full current recall retained,
wall-clock reduced ~4–6x by implementation fixes, and the remaining depth-vs-time
trade turned into an explicit runtime choice.

## 2. Goals and non-goals

**Goals**

1. Land the mechanical efficiency fixes (Section 4) — each independently shippable and
   gated.
2. Restructure `decode_window` into a **budget-governed anytime decoder** (Section 5):
   stages ordered by expected yield, a deadline checked between work items, graceful
   partial results.
3. Give the operator a **decode-effort control surface** (Section 6): config +
   TUI runtime cycling + hardware-tier-probed default.

**Non-goals (explicit)**

- **No GPU backend in this spec.** Follow-on spec, written only if re-measurement
  after this work shows a Mac-Mini-class "Max" preset still budget-limited. The
  anytime architecture must not preclude a future GPU stage executor.
- **No recall-feature removal.** Multipass, cross-cycle, joint-pair, a7, neural OSD
  all stay; they become budget-schedulable stages.
- **No FT4 C-decoder stopgap removal here.** Retiring
  `decode_window_ft8lib_protocol` for FT4 is a separate decision after this ships.
- **No remote-gateway/protocol change.** Decode effort is station-local; not exposed
  over rig-api v1.
- No change to decode *semantics* (message types, SNR estimation, dedup rules).

## 3. Licensing / clean-room constraint (standing, restated)

- **ft8_lib is MIT** (vendored at `pancetta-ft8/vendor/ft8_lib`). Its techniques and
  constants (e.g. the Padé atanh coefficients in `ft8/ldpc.c`) may be used directly
  with attribution in comments.
- **WSJT-X, wsjtr, JTDX, ft8mon, MSHV are GPL.** No pancetta code may be written by
  someone (human or agent) who has read their source. If any task in this initiative
  needs a GPL-derived algorithm, use the established clean-room firewall
  (Reader Agent → prose spec → Implementer Agent who never opens GPL source), per the
  documented extraction policy. **None of the work specified here requires GPL
  material** — every fix is either ft8_lib-derived (MIT) or pancetta-original.

## 4. Phase 1 — mechanical fixes

Each fix is small, independently landable, and independently gated. Gating classes:
**[BIT-EXACT]** = existing ~295-test suite + timing benchmark must pass, no A/B needed;
**[A/B]** = numerics change, must pass the standing research-harness gate
(hard-200/1000 A/B, bootstrap CI, ΔFP ≤ 2×ΔTP, elapsed-time hard gate).

- **F1. Padé `fast_atanh`** [A/B] — replace the `ln`-based `fast_atanh` with the
  ft8_lib Padé rational (MIT; cite `vendor/ft8_lib/ft8/ldpc.c` in the comment).
  Measured −20% total wall on the fixture. A/B required because the approximation
  saturates near |x|→1 (max |LLR| ≈ 2.3 vs the ln-clamp's ≈ 8.4), which
  LLR-magnitude consumers (whitening, `min_llr` features, OSD parity gate) can see.
  If the A/B regresses recall, fallback variant: piecewise — Padé for |x| ≤ 0.95,
  `ln` form above — re-A/B; that keeps ~all the speed (large-|x| edges are rare
  mid-iteration).
- **F2. BP housekeeping** [BIT-EXACT] — in `belief_propagation_with_features`:
  (a) do not zero-init or per-iteration-copy the 25×174 trajectory array unless the
  caller can consume a trajectory (it is only read on BP *failure* for neural-OSD /
  capture); build it lazily or only populate on the failure path. (b) Return
  `[f32; 174]` (or write into a caller buffer) instead of `Vec<f32>` at all ~6
  return sites (decoder-analysis A4).
- **F3. Flatten the spectrogram** [BIT-EXACT] — replace
  `power: Vec<Vec<Vec<f64>>>` (and the `complex` twin) with one contiguous
  allocation + index arithmetic (`(t * freq_osr + sub) * num_bins + bin`), preserving
  f64 in this step so values are bit-identical. Update the Costas kernel,
  `lookup_time_interp`, subtract paths, and all other readers. This attacks the 44%
  Costas share via cache locality and removed pointer-chases.
- **F4. f32 spectrogram + real-input FFT** [A/B] — switch the spectrogram pipeline
  (FFT, power, complex plane, `log10`) from f64-complex to f32-real
  (`FftPlanner::<f32>` or `realfft`; `log10f`). Roughly 4x on the ~24% FFT/log
  share plus halved memory bandwidth everywhere downstream. Depends on F3 (do the
  layout change once, then the precision change is a type swap + A/B).
- **F5. `costas_half_loop_disabled = true` default** [A/B] — Batch 92 groundwork
  already proved the half-loop is redundant at `TIME_OSR ≥ 2`; flip the default
  behind the A/B it never got. ~2x on the Costas kernel share.
- **F6. Small hot-path items** [BIT-EXACT] — OSD reliability-sort key precompute
  (A7b, runs even at depth 0), remaining per-candidate heap allocs surfaced by the
  profile (LLR `to_vec` copies at OSD entry, `maybe_whiten_llrs` temporaries).
- **F7. Commit the profiling harness** — `pancetta-ft8/examples/profile_decode.rs`
  (modes `native`/`native-fresh`/`ft8lib`, `ABL_*` ablation env vars) becomes a
  permanent example, documented in the crate README, so every fix above ships with a
  before/after number from the same harness. Also add a doc note (or debug assert)
  that `decode_window` callers must pass exactly `WINDOW_SAMPLES`: feeding the full
  15 s WAV was measured to cost 4–5x (t0 sweep scales with time slack).

Expected combined effect (measured pieces): production-config single-thread
~240 ms → **roughly 60–100 ms** before Phase 2, at identical-or-A/B-verified recall.

## 5. Phase 2 — the anytime decoder

### 5.1 Stage pipeline

`decode_window_with_ap_scoped_partner` (the real implementation everything funnels
into) is restructured from an inline monolith into an ordered sequence of stages.
Order = expected decode yield per ms, mandatory floor first:

| Stage | Content | Class |
|-------|---------|-------|
| S0 | preprocess + spectrogram + Costas sync search + rank | **floor** (always runs) |
| S1 | top-`floor_candidates` (default 50) candidates, BP at `floor_iters` (default 25), AP0 path, dedup | **floor** (always runs) |
| S2 | remaining ranked candidates (up to `max_sync_candidates`), same shallow BP | budgeted |
| S3 | **BP escalation**: candidates from S1/S2 whose BP failed but look convergent get continued to deep iterations (see 5.2) — including the OSD/neural-OSD attempt for escalated candidates | budgeted |
| S4 | cross-cycle averaging pass (hb-056/74/75) | budgeted |
| S5 | coherent multipass subtract+repass rounds (hb-079/080), round-at-a-time | budgeted |
| S6 | joint-pair retry (hb-086) | budgeted |
| S7 | a7 template pass + fourth-pass-after-a7 (when enabled) | budgeted |

Stage order within a pass loop is preserved from today's semantics (cross-cycle
integrates un-subtracted data before multipass subtracts, etc.) — the restructure
changes *when work stops*, not *what order work happens in*.

### 5.2 BP escalation ladder (replaces the flat 100-iteration cap)

- S1/S2 run BP at `floor_iters = 25`. On failure, record the candidate's final
  parity-error count and (cheaply) its BP message state.
- S3 sorts failed candidates by parity errors ascending and **continues** BP (from
  saved `c2v` state — ~2.3 KB per candidate, retained only for candidates below an
  escalation ceiling) up to `deep_iters` (default 100, "Max" preset may raise it,
  e.g. 300 — the documented recall lever).
- Escalation eligibility threshold (parity errors ≤ N) is set empirically on the
  research harness; starting proposal N = 24 (BP failures that ever succeed by
  iteration 100 almost always show low residual parity error at 25 — the plan
  includes a measurement task to confirm and pick N with data).
- **Recall invariant:** with unlimited budget, S1+S3 at (25 → continue to 100) must
  decode a superset-or-equal set vs today's flat 100-iteration BP on the eval
  corpus. Continuation-from-saved-state is mathematically identical to having run
  the higher cap in one shot (layered BP state is the `c2v`/posterior pair), so this
  should hold exactly; the harness A/B verifies.

### 5.3 Budget governor

- New `DecodeBudget` passed into the decode entry points:
  `deadline: Option<Instant>` + telemetry accumulator. `None` = unlimited (research
  harness, tests, `Max`).
- Checked **between work items only** — per candidate in S2/S3, per round in S5,
  per pass in S4/S6/S7 — never inside a BP iteration loop or FFT. Overshoot is
  therefore bounded by one work item (~1–2 ms after Phase 1).
- On expiry: stop scheduling stages, run the (cheap, existing) dedup/finalize, return
  results decoded so far. Decode NEVER returns empty because of budget: S0+S1 are
  floor and always complete.
- The **coordinator** computes the deadline each window:
  `deadline = window_ready_at + min(configured_budget, mode_ceiling)` where
  `mode_ceiling` is protocol-aware (FT8 vs FT4 slot timing; exact ceilings chosen at
  plan time from the DSP cadence — decode must finish before the pipeline needs the
  thread back).
- Telemetry: per-stage elapsed, items processed/skipped, `budget_exhausted: bool`,
  threaded into the existing decode-metrics path and surfaced in the TUI status bar
  (Section 6) and the tracing log (`target: "decode.budget"`).

### 5.4 Determinism

Wall-clock budgets make production results mildly nondeterministic run-to-run (a
slow scheduler moment can drop a tail candidate). Accepted for production (each
15 s slot is already unique on-air). **The research harness, CI, and all tests run
with `DecodeBudget::unlimited()` or pinned work counts** — evals stay exactly
reproducible. No wall-clock reads on the eval path.

### 5.5 Parallelism note

S1/S2 keep the existing Rayon `par_iter` over candidates (budget checked in the
chunk driver between candidates via a shared deadline read — workers finish their
current candidate). The measured 1.34x parallel CPU inflation is mostly pool
wake/latch overhead around small serial sections; Phase 1's shrinking of serial BP
rounds reduces it naturally. No new parallelism is introduced in this spec.

## 6. Phase 3 — effort control surface

### 6.1 Config

New `[decoder]` section in `pancetta-config`:

```toml
[decoder]
effort = "auto"    # "auto" | "eco" | "standard" | "deep" | "max"
# budget_ms = 500  # optional explicit override; wins over `effort` when set
```

- Presets map to budgets + stage enablement. Starting values (plan refines with
  measured post-Phase-1 numbers):
  - `eco` — floor only (S0+S1). Bare-minimum hardware.
  - `standard` — budget ≈ 250 ms: S2+S3 usually complete on Moderate hardware.
  - `deep` — budget ≈ 1000 ms: everything usually completes (≈ today's behavior).
  - `max` — unlimited + raised `deep_iters` (e.g. 300) and any future depth levers.
  - `auto` — hardware-tier probe result maps Slow→eco, Moderate→standard, Fast→deep.
- **Subsumes** tier.rs's current ad-hoc `Ft8Config` rewrites (Slow-tier
  `max_decode_passes`/`max_sync_candidates` overrides are replaced by the eco
  preset; `scoped_fast_path` stays separate and untouched).
- Hot-reloadable. **Must include the `merge_with` lines + a
  `merge_with_carries_over_decoder_section` regression test** (per the 2026-07-05
  config-merge bug class).
- Validation: `budget_ms` if set must be > 0; unknown `effort` string rejected.

### 6.2 TUI

- A keybinding cycles effort live (exact key chosen at plan time from the free-key
  audit; plumbed exactly like Shift+M's mode switch: TUI → `TuiCommand` →
  relay → coordinator atomic → echo back as a status message).
- Status bar chip: active preset + last slot's actual decode ms + a marker when the
  budget clipped work (e.g. `DECODE: DEEP 412ms` / `DECODE: STD 250ms✂`).
- Runtime cycling only changes the *budget*, never requires decoder reconstruction —
  safe mid-QSO (unlike the mode switch, no active-QSO gate needed; state as an
  invariant: effort changes take effect at the next window).

## 7. Validation plan

1. **Bit-exact fixes (F2/F3/F6):** full `cargo test --workspace --features transmit`
   + fixture decode-count equality + before/after harness timing in the commit
   message.
2. **A/B fixes (F1/F4/F5):** standing research-harness gate, one variable per
   experiment, journaled per the research workflow.
3. **Anytime restructure:** (a) unlimited-budget superset-invariant vs pre-restructure
   baseline across the WAV fixture corpus and the harness eval set; (b) BP
   continuation-equivalence unit test (25+75 continued ≡ 100 flat on captured LLR
   cases); (c) budget-floor test (tiny budget still returns S1 results); (d) budget
   monotonicity smoke test on fixtures (larger budget ⊇ smaller, modulo documented
   subtract-order effects); (e) `coord_sim` + loopback QSO end-to-end unchanged.
4. **Control surface:** config parse/validation/merge_with tests; TUI relay
   round-trip test; tier-mapping unit test.
5. **On-air soak** (operator-gated) before flipping the default from current behavior
   to `effort = "auto"`: run `deep` (today-equivalent) vs `auto` across sessions and
   compare decode counts + timing telemetry.

## 8. Success criteria

- Production config (deep-equivalent recall) single-thread decode:
  **≤ 100 ms/window** post-Phase-1, **operator-tunable down to ≤ 25 ms floor** (eco)
  post-Phase-2, vs 240 ms today. (C ft8_lib reference: 5.6 ms at 50-candidate/
  25-iter/no-extras scope — pancetta's eco floor at the same scope should land
  within ~2x of it after F1–F5.)
- Zero recall regression at `deep`/`max` on the standing eval gates.
- FT4 slot timing comfortably met by `standard` on Moderate hardware — unblocking
  eventual retirement of the C stopgap (out of scope here).
- Operator can move across the eco↔max continuum at runtime and see the effect in
  the status bar within one slot.

## 9. Risks

- **decoder.rs is ~16K lines and the restructure touches the hot path.** Mitigation:
  stage extraction is mechanical (move existing blocks behind a stage driver),
  each stage move verified by the unlimited-budget invariant before the next;
  duplicated serial/parallel decode paths (known drift risk from decoder-analysis)
  are consolidated only where a stage boundary already forces it — no gratuitous
  rewrite.
- **F1 LLR-saturation recall risk** — explicitly A/B'd, with a piecewise fallback.
- **Budget starvation on very slow hardware** — floor stages are unconditional, and
  `auto` maps such hardware to eco where the floor *is* the plan.
- **Config-merge regression class** — addressed by construction (test required in
  the same task that adds the section).
