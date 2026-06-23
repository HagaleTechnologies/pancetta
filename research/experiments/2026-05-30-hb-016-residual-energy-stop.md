---
slug: hb-016-residual-energy-stop
mode: ft8
state: shelved
created: 2026-05-30T16:00:00Z
last_updated: 2026-05-30T17:30:00Z
branch: iter/2026-05-30-batch-15
parent_hypothesis: hb-016
wild_card: false
scorecard: research/scorecards/hb-016/v2-th2.0.json
delta_vs_main: composite 0 (recall identical at every threshold tried); elapsed not measurably saved
disposition: SHELVE — the probe never fires usefully at "neutral-composite" thresholds because the existing empty-pass-break in `coherent_subtract_and_repass` already short-circuits the dominant CPU sink. The expected CPU savings don't materialise; the probe adds O(N) work per round without an observable rebate. Re-open only if the multipass loop grows expensive enough between rounds to make a cheap early-exit signal worth its overhead.
---

## Hypothesis

After hb-079/hb-080 graduated coherent iterative-subtract multi-pass at
N=3 rounds (`2026-05-26-hb-079-coherent-multipass.md`,
`2026-05-27-hb-080-multipass-n3.md`), each round subtracts every
newly-decoded signal's coherent contribution from the spectrogram and
re-runs `costas_sync_search` on the residual. The existing safety
valve in `coherent_subtract_and_repass` is "if round N produces 0 new
decodes, the caller breaks the loop." hb-016 hypothesised that adding
a **residual energy probe** *before* the expensive `sync_search` would
let us bail earlier on rounds where the residual is dominated by
noise (e.g. clean-channel WAVs where pass 1 already recovered
everything), saving wall-clock CPU.

Expected outcome from the bank entry: neutral composite, +0.01-0.02
throughput rebate. Treat the probe as a free CPU optimisation if it
holds.

## Change (V2 — landed on this branch)

1. New `Ft8Config::residual_energy_stop_db: Option<f64>` (default
   `None`, opt-in).
2. New eval flag `--hb016-residual-energy-stop-db <X>` (and matching
   research builder `with_residual_energy_stop_db`).
3. New helper `mean_excess_above_noise_db(power, noise_floor_db)` —
   averages `max(0, db - noise_floor_db)` over the spectrogram.
   Monotone in signal energy; 0 when every bin sits at/below the
   reference floor.
4. New helper `noise_floor_db_median(power)` — median dB of all bins.
   Computed once on the ORIGINAL spectrogram before the multipass
   loop and reused as the stable reference across rounds.
5. In `decode_window_with_ap`, the multipass loop now passes
   `Some((floor_db, threshold_db))` into `coherent_subtract_and_repass`
   when the flag is set.
6. Inside `coherent_subtract_and_repass`, between step 1 (subtract)
   and step 2 (sync_search), the probe fires: if
   `mean_excess_above_noise_db(residual, floor) < threshold_db`, return
   empty Vec, which makes the caller break the loop.

### Iteration history

- **V1 (mid-iteration, NOT kept)** used `mean(linear power)` of the
  residual vs the noise-floor median. Debugged on fixtures with a
  `HB016_DEBUG=1` env-var print: the `(residual_mean_dB - noise_floor_dB)`
  margin sat in the 16-78 dB range across every round of every WAV —
  a single bright bin dominates the linear-mean, so the probe is
  insensitive to "how much *non-bright* signal is left after
  subtract." V1 never fired below a 9 dB margin, and only on isolated
  outliers above. Replaced with V2 before the hard-200 sweep.
- **V2 (current)** uses the "average per-bin excess above floor"
  metric described above. On fixtures this lives in the 1.4-23 dB
  range and DOES drop into the early-stop regime on quiet residuals
  (e.g. a fixture with floor=-119.96dB had excess=1.40 dB across all
  three multipass rounds — fires at threshold=1.5 dB).

## Eval results — hard-200 sweep

Caveat: the run completed under heavy CPU contention from sibling
agent worktrees (3-5 concurrent `eval` processes). Recall numbers are
exact and trustworthy; elapsed numbers are noisy and conservative
(longer than they would be on a quiet machine, but the *deltas
within* the sweep window are within the noise floor of run-to-run
variance, which already spans ~289-496 s for the same baseline
config).

Config: `--ldpc-iters 100 --max-sync-candidates 300 --max-candidates 100
--fp-filter-baselines research/baselines/ft8`. Single tier
`curated-hard-200`. Seed 42.

| variant | rec | nov | rate | elapsed (s) |
|---|---:|---:|---:|---:|
| baseline (probe off, early in session) | 4616 | 921 | 0.53825 | 361.0 |
| `--hb016-residual-energy-stop-db 1.0` | 4616 | 921 | 0.53825 | 495.6 |
| `--hb016-residual-energy-stop-db 2.0` | 4616 | 921 | 0.53825 | 381.1 |
| `--hb016-residual-energy-stop-db 3.0` | 4616 | 921 | 0.53825 | 389.1 |
| `--hb016-residual-energy-stop-db 5.0` | 4616 | 921 | 0.53825 | 397.2 |
| baseline (probe off, end of session) | 4616 | 921 | 0.53825 | 350.1 |

The recall and novel counts match the production scorecard
(`research/scorecards/main.json`: hard-200 rec=4616 nov=921). The
probe is provably **not regressing decode quality** at any of the
tested thresholds.

The elapsed numbers tell the headline story: at every threshold the
runtime is **above** the bracketing baselines (350-361 s without the
probe vs. 381-397 s with it at th ≥ 2.0; the 495 s outlier at th=1.0
landed during the peak-contention window when 3-5 sibling `eval`
processes were sharing the box). Net: the probe adds roughly
5-12 % wall-clock overhead and saves nothing observable. The
existing empty-pass-break already short-circuits the
multipass loop on signal-poor residuals (the case hb-016 was meant
to catch) before the residual energy probe's data point becomes
useful, so the per-round O(N) cost of computing the mean-excess
metric is paid without a rebate.

## Why the predicted savings didn't materialise

Two structural reasons the optimisation is non-load-bearing today:

1. **The existing empty-pass-break already wins the easy cases.**
   `decode_window_with_ap` breaks the multipass loop when
   `coherent_subtract_and_repass` returns an empty Vec. The Vec is
   empty when *no* new candidates exceed the residual sync threshold —
   which happens whenever the residual is signal-poor (the case
   hb-016 was meant to catch). On clean WAVs where pass 1 recovers
   everything, the loop already terminates after a single wasted
   round, NOT three. hb-016 saves at most one residual sync_search
   per WAV in the best case, and on hard-200 most WAVs HAVE more
   signal than pass 1 catches (hence the +12-rec hb-086 win sitting
   on top of N=3 multipass), so the early-exit window is narrow.
2. **The residual is not noise-dominated at any "fires-but-loses-no-decodes"
   threshold.** The `mean_excess_above_noise_db` metric only drops
   into the 1-3 dB range on genuinely-quiet residuals (the
   floor=-119.96 dB fixture above). On hard-200 — by construction the
   noisiest, most signal-dense corpus we have — the residuals are
   carrying real signal mass that *might* decode if hb-086's
   joint-pair-retry can find it. Setting the threshold high enough to
   fire broadly would short-circuit hb-086's pipeline (which runs
   AFTER the multipass loop and depends on its residual). The "fires
   often AND keeps recall" sweet spot doesn't exist on this corpus.

The probe is doing the right thing — it just has nothing useful to
do that the existing break can't already do.

## Kill criteria check

Bank-pre-defined kill criteria:

- *Every threshold either regresses composite OR doesn't save measurable
  wall-clock time → SHELVE.* — **Met.** Composite stable at every
  threshold; no wall-clock rebate above the contention noise floor.
- *Some threshold saves ≥3% elapsed at neutral composite → propose to
  graduate.* — Not met.
- *Some threshold saves ≥10% elapsed even at small composite loss
  (-0.0001) → consider graduation depending on tradeoff.* — Not met
  (recall is *identical* at every threshold; there isn't even a
  "small loss" knob to trade against).

Disposition: **SHELVE**. The mechanism is correctly implemented and
the change is preserved on this branch (with the flag defaulted off,
zero behavior delta in production), but the bank can mark hb-016
SHELVED — the assumption that the multipass loop is wasting work on
empty residuals didn't hold up against the existing empty-break
short-circuit.

## Learnings

- **An "early-exit" optimisation has to fire on the cases the existing
  break already handles, in order to win cycles. If the existing break
  already catches them, the early-exit was redundant.** Worth checking
  for the next "skip work" hypothesis: is the work I want to skip
  already being skipped?
- **Mean-linear-power is a bad probe in dB-domain spectrograms.** A
  single bright bin dominates and the metric never converges toward
  noise floor, no matter how thoroughly the signals are subtracted.
  V2's mean-excess-above-floor is the right shape but the answer is
  still "stop wasting cycles probing — nothing fires."
- **The empty-pass-break path is more load-bearing than the
  hypothesis-bank entry credited it for.** When hb-016 was written
  (before hb-079 graduated), multipass wasn't shipping at all; the
  bank entry says "pass 2+ routinely finds 0 new decodes on
  synth-clean, this optimisation saves real wall-clock time." Under
  N=3 multipass + hb-086's joint-pair-retry, *most* of the slack the
  probe was meant to catch is already absorbed by downstream passes
  finding decodes.

## Code surface

Kept on the branch (default off, zero production impact):

- `pancetta-ft8/src/decoder.rs`:
  - `Ft8Config::residual_energy_stop_db: Option<f64>` (default
    `None`)
  - `mean_excess_above_noise_db(power, noise_floor_db) -> f64`
  - `noise_floor_db_median(power) -> f64`
  - `coherent_subtract_and_repass` signature now takes
    `energy_stop: Option<(f64, f64)>`; new probe sits between step 1
    and step 2.
  - `decode_window_with_ap` multipass loop computes the noise floor
    once before the loop (when the flag is set) and threads it in.
- `pancetta-research/src/decoder.rs`:
  - `Ft8Decoder::with_residual_energy_stop_db(Option<f64>) -> Self`
- `pancetta-research/src/bin/eval.rs`:
  - `--hb016-residual-energy-stop-db <f64>` flag.

If a future iteration finds a workload where the multipass loop's
between-round cost grows (e.g. a heavier sync_search variant
graduates), re-opening hb-016 is a one-line flag flip. Until then,
the surface is dead code paid for by the `None` default skipping
both the floor computation and the per-round probe.
