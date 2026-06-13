# Batch 92 — Costas half-loop plateau: measured, redundant for SCORING, load-bearing for RECALL

**Branch**: `iter/2026-06-12-batch-92` · **Date**: 2026-06-12
**Origin**: Batch 88 residual #1 (`research/notes/2026-06-12-batch88-dt-audit.md`, "Residuals")

## Question

`compute_costas_score_groups` (pancetta-ft8/src/decoder.rs) takes `max` over
`half ∈ {0,1}`. With `TIME_OSR = 2` the outer t0 sweep already visits
half-symbol offsets, so `score(t0) = max(g(t0), g(t0+1))` — a two-step
plateau. Hypothesis (Batch 88): the half loop is redundant and removing it
sharpens time localization (kills the −960 early-emission bucket) without
losing candidates.

## Redundancy verification (claim checked, not trusted)

- The kernel's only time dependence is `time_idx = t0 + symbol_idx*TIME_OSR
  + half`; all four neighbor lookups and every bounds check are pure
  functions of `time_idx`. Therefore `g(t0, half=1) ≡ g(t0+1, half=0)`
  exactly.
- The caller (`costas_sync_search_with_threshold_and_partner`) sweeps
  `for t0 in 0..=max_time_step` in steps of 1, so every half=1 value is
  re-captured at the next t0. **The scoring redundancy is real.** Executable
  proof: unit test
  `test_costas_half_loop_disabled_plateau_identity_and_sharpening` asserts
  `score_off(t0) == max(g(t0), g(t0+1))` across a t0 range (passes).
- `TIME_OSR` is a compile-time `const = 2` — no configuration can make it 1.
  The flag is still guarded (`costas_half_loop_disabled && TIME_OSR >= 2`)
  so the half loop survives any future TIME_OSR=1 build, where it is NOT
  redundant.
- One true edge: `g(max_time_step+1)` is reachable only via half=1 at the
  last t0; flag-ON drops that single far-edge position.

## Implementation

- `Ft8Config::costas_half_loop_disabled: bool`, default **false**
  (probe-baseline discipline). When true, the kernel evaluates half=0 only.
  Zero-diff when false (asserted in the unit test: default decoder ==
  explicit flag-false decoder).
- A/B harness: `pancetta-research/examples/batch92_costas_half_loop.rs`
  (raw_530_full, ft8_lib truth, hash-normalized matching per Batch 87 rule,
  thread-parallel).
- dt audit: `batch88_dt_audit.rs` gained `--half-loop-off` and `--only-a`.

## A/B results — raw_530_full, ft8_lib truth, hash-normalized

| Config | Slots | Decodes | TP | FP | truth | miss% | wall |
|---|---|---|---|---|---|---|---|
| half-loop ON (baseline) | 200 | 4213 | 3563 | 650 | 3660 | 2.65% | 29.6s |
| half-loop OFF (flag=true) | 200 | 4138 | 3499 | 639 | 3660 | 4.40% | 35.7s |
| **Δ (200)** | | **−75** | **−64** | **−11** | | | +6.1s |
| half-loop ON (baseline) | 2066 | 46978 | 38607 | 8371 | 39668 | 2.67% | 449.5s |
| half-loop OFF (flag=true) | 2066 | 46087 | 37972 | 8115 | 39668 | 4.28% | 328.3s |
| **Δ (2066)** | | **−891** | **−635** | **−256** | | | **−121.2s** |

The 200-slot signal replicates at scale: **−635 TPs (−1.6%)** on the full
corpus. The flag-ON config IS faster at scale (−27% wall — the kernel
halving is real; the 200-slot +6.1s was scheduling variance), but the
recall cost rules it out.

## dt audit Part A (synthetic ground truth), before/after

Byte-identical. Both configs:

```
Part A delta: n=64 mean=-105.0 median=+0.0 sd=274.5 min=-480.0 max=+480.0 (samples)
hist[-2880..2880, 240/bucket]: -720:4 -480:15 -240:12 +0:16 +240:16 +480:1
```

The early-emission population does **not** shrink — the reported dt
distribution is invariant to the flag. The mean −105 skew is therefore NOT
caused by the plateau tie-break at the candidate-emission site: on these
strong synthetic signals the same candidate wins and downstream refinement
produces the same reported dt either way.

## Mechanism: why removal LOSES TPs

The plateau makes each strong signal emit TWO candidates 960 samples apart
(t*−1 and t*; `nms_enabled` is false by default, so both survive into the
budget of 200). The time-domain decode path's fine timing search is only
±720 samples (±3/8 symbol, 240-sample steps; decoder.rs `time_deltas`), so
the pair jointly covers a ~2400-sample alignment span vs ~1440 for a single
candidate. LDPC sometimes converges from only one of the two alignments —
the half loop is redundant for *scoring* but functions as a free
adjacent-alignment retry for *extraction*. Removing it deletes that retry:
−64 TPs at 200 slots / −635 at 2066 slots with no compensating dt benefit.
The kernel halving does buy −27% wall at scale — if sync-search cost ever
becomes a binding constraint (Slow tier), the honest trade is this flag vs
other speed knobs, with the −1.6% TP cost on the label.

## Verdict (pre-registered)

- ΔTP ≥ 0? **NO** (−64 at 200 slots, −635 at 2066). ΔFP ≤ 0? yes (−11 /
  −256). dt improves? **NO** (byte-identical).
- → **Do not ship ON.** The flag stays default-OFF; the mechanism is
  shelved with this note as the measurement record (bank: hb-251). The
  redundancy *analysis* was correct (executable assertion in-tree), so the
  flag is kept rather than reverted — it documents a verified-but-harmful
  knob and guards any future TIME_OSR change.
- Batch 88's residual is now closed: the −960 candidate-level skew is real
  but harmless (absorbed downstream), and the plateau itself is
  load-bearing for recall.

## Discipline

- touch-before-build, "Compiling pancetta-ft8" confirmed; cargo fmt run.
- `cargo test --features transmit -p pancetta-ft8`: all pass (388 lib tests
  incl. the new one; exit 0).
- clippy: no new warnings from this change (one `field_reassign_with_default`
  in the new example fixed; remaining warnings pre-existing).
