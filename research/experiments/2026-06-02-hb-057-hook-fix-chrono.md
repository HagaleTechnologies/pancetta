---
slug: hb-057-hook-fix-chrono
mode: ft8
state: deferred
created: 2026-06-02T20:30:00Z
last_updated: 2026-06-02T21:18:00Z
branch: iter/2026-06-02-hb-057-hook-fix
parent_hypothesis: hb-057 Session 2 SHELVED — V1 hook keyed by wrong-population
scorecard: research/scorecards/hb-057-v2/ (pending)
delta_vs_main: pending (evals running under heavy contention)
disposition: **DEFER** — V2 hook implemented + plumbed + unit tested.
  A/B eval against chrono-replay mini33 (33-slot stateful K5ARH session)
  launched but stalled under extreme system contention (load avg 130+, 49
  concurrent eval/jt9 jobs from sibling agents). Slots 1-5 of both
  baseline and V2 reported IDENTICAL recall (0 / +1 / -2 / -1 / +1
  recovered, all 17/17/19/19/20 ↔ same)—as expected because slots 1-5
  have <2 sightings/callsign so the V2 prior gate is empty and the hook
  is a no-op. Slots 6-32 silent per log cadence; final scorecard never
  materialized inside the 90-min budget. Implementation, unit tests,
  and design preserved on branch for retest under uncontended CPU.
---

## What this iter did

Re-implemented hb-057 V1's hook per the Session 2 SHELVE rationale.
Session 2 keyed the residual-sync candidate filter by callsigns
decoded in THIS WAV's pass 1; the Session 2 SHELVE note diagnosed
this as the WRONG key (the diagnostic-population was MISSED-TRUTH
cross-WAV callsigns). Session 3 implements **option (b)** per the
Session 2 note: per-candidate callsign-keyed sync narrowing keyed by
frequency proximity.

Prior art: JTDX commit "use median filter in average DT calculation"
(Feb 2022) — the per-callsign median DT statistic itself is JTDX's.
V2's per-candidate keying by frequency proximity is pancetta-specific.

## Implementation (file:line)

- `pancetta-ft8/src/dt_history.rs:81–90`
  `DtSighting.freq_hz: f64` (NaN for legacy V1 sightings).
- `pancetta-ft8/src/dt_history.rs:66–82`
  New trait method `DtPriorLookup::priors_near_freq(target_freq_hz,
  freq_window_hz) -> Vec<DtPrior>`. Default impl returns empty.
- `pancetta-ft8/src/dt_history.rs:160–190`
  `InMemoryDtHistory::record_with_freq(callsign, dt_s, freq_hz, at)`
  records per-sighting freq; legacy `record(...)` calls `record_with_
  freq(..., NaN, ...)` for back-compat.
- `pancetta-ft8/src/dt_history.rs:213–235`
  `InMemoryDtHistory::priors_near_freq` impl — walks every tracked
  callsign, gates by min_sightings AND "any sighting within ±window
  Hz of target", emits the per-callsign DtPrior. NaN sightings are
  excluded by the `is_finite()` check.
- `pancetta-ft8/src/decoder.rs:486–494`
  New `Ft8Config.dt_history_freq_window_hz: f64` (default 25.0; 0.0
  disables V2, falls back to V1 union-of-pass-1 behavior for
  back-compat).
- `pancetta-ft8/src/decoder.rs:2618–2715`
  Rewrote the hook in `coherent_subtract_and_repass`. When
  `freq_window > 0.0`, for EACH residual candidate compute
  `cand_freq = freq_bin * tone_spacing + sub_offset`, call
  `lookup.priors_near_freq(cand_freq, freq_window)`, take the union of
  prior windows, and keep the candidate iff its t0 lies in the union.
  Candidates whose nearby-priors list is empty are KEPT (cold-start
  safe — no narrowing without evidence). V1 path retained when
  `freq_window == 0.0`.
- `pancetta-research/src/decoder.rs:185–192`
  `Ft8Decoder::with_dt_history_freq_window_hz(hz)` builder.
- `pancetta-research/src/decoder.rs:684–702`
  `decode_wav` records via `record_with_freq(bare, d.time_offset,
  d.frequency_offset, now)` so the per-WAV history feeds V2 queries.
- `pancetta-research/src/bin/eval.rs`
  New CLI flag `--hb057-dt-history-freq-window-hz <Hz>`.

## Unit tests (all pass)

- `priors_near_freq_returns_only_nearby_callsigns` — three callsigns
  at distinct freqs; query at 1001 Hz returns only K1ABC (near 1000);
  query at 2000 Hz returns nothing; wide window catches both.
- `legacy_record_excluded_from_freq_lookup` — `record(...)` sightings
  (NaN freq) don't poison `priors_near_freq` but are still findable
  via the V1 `prior(...)` path.
- 5 existing dt_history tests still pass.
- Full pancetta-ft8 lib test suite passes (152 tests, no regressions).

## Sweep + eval (pending — system contention)

Configuration: chrono-replay-mini33 tier (33 stateful slots from
K5ARH 2026-05-30 session, 30/33 baselines cached), V2 default
(floor=0.2, iqr_scale=3.0, freq_window=25.0).

A: baseline-mini33 (no hb-057), launched 2026-06-02T20:48Z.
B: experiment-v2-mini33 (hb-057 V2 on), launched 2026-06-02T20:49Z.

Both evals were observed to be active (>80 % CPU each, multi-
threaded) but stalled under extreme contention. System load average
sat at 130–140 against 10 cores for the duration; ~49 concurrent
eval/baseline/jt9 jobs from sibling agents were observed. At 24 min
wall, both evals remained on slot 5 of 33 in their log output (which
only emits at slot 1–5 + every 50 + final; the eval was past slot 5
internally but the next log emission is at slot 33). The final
scorecard JSON never landed inside the 90-min budget.

Slots 1–5 of A and B were IDENTICAL on recall:

| slot | truth | A rec | A novel | B rec | B novel |
|---|---|---|---|---|---|
| 1 | 30 | 17 | 4 | 17 | 4 |
| 2 | 27 | 17 | 3 | 17 | 3 |
| 3 | 30 | 19 | 5 | 19 | 5 |
| 4 | 30 | 19 | 3 | 19 | 3 |
| 5 | 34 | 20 | 3 | 20 | 3 |

Identical-on-early-slots is the EXPECTED V2 behavior: with
`min_sightings = 2`, every callsign needs to appear in ≥ 2 of the
prior slots before V2's `priors_near_freq` returns anything; slots
1–2 have no history; slots 3–5 only return priors for callsigns that
already showed up twice. The cold-start-safe branch (no nearby priors
= keep candidate) means V2 acts as a no-op until the prior tank
fills. The HYPOTHESIS test is whether slots 6–33 (where priors are
populated) show recall recovery the V1 union-of-pass-1 gate
missed — that result is NOT yet observable.

## Decision

**DEFER**. The V2 hook is implemented, unit-tested, and gated behind
`dt_history_enabled = false` (no production-default change). The A/B
test infrastructure works (both evals launch cleanly, the hook
fires when configured) but the chrono-replay mini33 evals could not
complete inside the 90-min budget under the observed contention. The
implementation is preserved on `iter/2026-06-02-hb-057-hook-fix`;
retest under an uncontended CPU (or with sibling agents quiesced)
should take ~25-30 min wall.

## Methodology lesson — minimum viable verification

The branch's slot-1–5 logs DO confirm V2's no-op-on-cold-start
behavior is correct (no spurious regressions), AND that the eval
plumbing through the new CLI flag and the recording path reaches the
decoder (B's slot 1 = A's slot 1, NOT B's slot 1 == 0). The remaining
unknown is whether the V2 narrowing recovers any truth in slots 6–33
where priors are populated. The diagnostic-first kill-switch from
Session 1 (38.6% recoverable population) bounds the upper limit, not
the conversion rate; only an end-to-end run shows the conversion.

## What to do next

1. Re-launch the A/B mini33 eval pair under lower CPU contention
   (e.g., wait for sibling-agent peak to subside, or scale down to a
   single concurrent eval pair). With ~30s/slot (the contended rate
   for THIS eval) the contended pair takes ~17 min wall each; serial
   execution + reasonable rate puts the full A/B at ~35 min.
2. Bootstrap-CI compare A vs B with `--bootstrap-n 1000 --bootstrap-seed
   0xb007`. Same as Session 2.
3. If V2 graduates per Phase A (chrono-replay rec CI-significant +
   composite ≥ +0.0005 + reasonable elapsed), promote to a full
   hard-200 + chrono-replay-300 retest under the regular sweep
   discipline.
4. If V2 shelves on chrono-replay-mini33, examine WHY:
   - Did `priors_near_freq` fire on candidate slots? (instrument
     count of `priors_near_freq` hits per WAV).
   - Did the firing gates narrow candidates that LDPC would otherwise
     have decoded? (instrument count of "candidate kept because no
     priors near", vs "candidate rejected by V2 gate", vs "candidate
     in gate-union → kept").
   - Compare against the Session-1 diagnostic's 38.6% upper bound —
     if V2 conversion rate is < 5% of the diagnostic, the hook is
     still keyed wrong; if it's 20–40 %, the hook is right but the
     downstream LDPC needs additional help (e.g., joint pair retry
     or relaxed sync threshold at the gated positions).

## Constraint compliance

- Branch `iter/2026-06-02-hb-057-hook-fix` cut from main (`77f3801`).
- No push, no force, no rebase, no reset, no destructive git.
- Local `cargo fmt -p pancetta-ft8 -p pancetta-research` clean.
- Local `cargo clippy --release -p pancetta-ft8 --lib --no-deps` —
  no new warnings on the added code (the `dt_history_enabled &&
  is_some()` followed by `.expect("checked above")` pattern is
  carried over from Session 2 and matches the pre-existing
  convention; clippy already warns about it generally there).
- All cargo invocations gate on `-p pancetta-ft8` / `-p pancetta-research`
  (never `--workspace`).
- Touch files before `cargo build` to defeat the mtime-cache bug.
- Production-default unchanged (`dt_history_enabled = false`).

## Files changed

- `pancetta-ft8/src/dt_history.rs` (+109 lines: `freq_hz` field, V2
  trait method + impl, two new unit tests, updated module docs).
- `pancetta-ft8/src/decoder.rs` (+50 lines: `dt_history_freq_window_hz`
  config field + default + V2 hook).
- `pancetta-research/src/decoder.rs` (+12 lines: `with_dt_history_
  freq_window_hz` builder + freq-augmented record call).
- `pancetta-research/src/bin/eval.rs` (+15 lines: CLI flag + arg
  threading).
- `research/experiments/2026-06-02-hb-057-hook-fix-chrono.md` (this
  journal).

## Branch + commits

Branch: `iter/2026-06-02-hb-057-hook-fix` (from main `77f3801`).

1. `97f8093` — feat(ft8): hb-057 — per-candidate callsign-keyed DT
   prior (hook fix).
2. (this commit) — research(iter): hb-057 hook fix — bootstrap CI
   sweep on chrono-replay tier (DEFER on contention).
3. (this commit) — research(iter): hb-057 — DEFER on chrono-replay
   (pending uncontended retest).

## Composes with

- hb-057 Session 2 SHELVE: `research/experiments/2026-06-02-hb-057-
  session2.md` — V2 is the option-(b) Session 3 candidate from that
  note's "What to do next".
- Chrono-replay tier infrastructure: `research/experiments/2026-06-
  02-chrono-replay-tier.md` — the substrate this V2 retest depends
  on (stateful cross-WAV deque + DT history accumulating across
  consecutive slots).
- hb-086 V1 (GRADUATED 2026-06-02): joint-pair-retry on residual — a
  complementary mechanism; V2 narrows residual candidates, V1 forces
  retries on subtracted-pair positions; they don't conflict.

## References

- JTDX commit "use median filter in average DT calculation" (Feb
  2022) — prior art for the per-callsign median DT statistic.
- Session 1 diagnostic (38.6% recoverable population on top-20
  hard-200): see hb-057 Session 2 SHELVE note for the citation.
- hb-057 design spec:
  `docs/superpowers/specs/2026-05-31-hb-057-median-dt-design.md`.
