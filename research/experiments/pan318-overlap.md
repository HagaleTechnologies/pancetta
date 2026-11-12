# PAN-318 experiment record: overlapping the reference and native decode passes

Date: 2026-11-12. Branch `PAN-318`, base `5108a01cc40cca2dbad0126332c71068d6e4d7b2`.
Host: this implementation container, Linux, 32 visible cores with a 2-CPU
cgroup quota (`/sys/fs/cgroup/cpu.max` = `200000 100000`). Debug profile
for the runs recorded here unless noted.

## What changed

`pancetta/src/coordinator/ft8.rs` now runs the ft8_lib reference pass and
the native AP pass concurrently through the shared helper
`pancetta-ft8/src/window_execution.rs::run_overlapped_passes`:

- two-party barrier: neither branch runs decode work before both are
  ready; each branch timestamps itself immediately before its decoder
  invocation (in-worker, never the parent's spawn instant);
- one `catch_unwind` per branch; a panic in either pass is logged,
  counted in `DECODE_PANIC_COUNT`, and the other pass's results survive;
- the scoped child is joined before the helper returns, so the existing
  `merge_ft8lib_first` only runs after both passes finished;
- the reference pass runs on a second long-lived `Ft8Decoder` (Send, not
  Sync — exclusive access moves into the child and back with the result),
  rebuilt in lockstep (`rebuild_decoder_pair`) and re-seeded every window
  from the native learned-callsign snapshot (`sync_reference_parser_seeds`),
  so its parser resolves i3=4 hash callsigns exactly as the sequential
  single-parser pipeline did;
- the waterfall moved from the pre-native prefix to after the joined
  pair: the native entry no longer pays the waterfall FFT or the ft8_lib
  pass. The generator and fan-out are unchanged (byte-identical rows);
  deriving the waterfall from the reference monitor's FFT is deferred.

`pancetta/src/coordinator/ft8/timing.rs` adds bounded histograms over
`ft8lib_ms` / `waterfall_ms` / `pre_native_ms` (microsecond samples,
fixed 1..2000 ms buckets + overflow, one debug distribution line every
32 windows on the `decode.budget` target).

## Measurements (assertions, not just prints)

`cargo test -p pancetta-ft8 --features benchmark,transmit --test pan318_overlap_tests`
(debug, this container, 2026-11-12):

- `fixtures_present_and_ft8lib_linked` — ft8_lib linked, all five
  production-sized fixtures present (191111_110200, 191111_110700,
  websdr_test2, websdr_test7, websdr_test12; each truncated to
  WINDOW_SAMPLES = 151,680).
- `standard_overlap_matches_sequential_fixtures` — **passed**: the
  overlapped merged exact-text set equals the sequential result on all
  five windows under the Standard 250 ms receipt-anchored budget.
- `unlimited_overlap_preserves_messages` — **passed**: full-field
  equality (every `DecodedMessage` field; decode `timestamp`,
  `decode_time_into_window` and BitVec heap addresses normalized as
  per-run noise) with unlimited budget on all five windows.
- `overlapped_native_entry_skips_the_waterfall_prefix` — **passed**:
  overlapped `pre_native` < sequential `pre_native` (which contains the
  waterfall the sequential order runs first).
- `window_execution` unit tests (debug): `parallel_passes_start_together`
  (in-worker start skew ≤ 5 ms, measured; a sequential implementation
  fails this by the first branch's 75 ms duration),
  `parallel_passes_join_before_merge`, `parallel_native_keeps_receipt_deadline`
  (a 120 ms reference pass does not postpone native entry),
  `parallel_pass_panics_are_isolated`, all **passed**.
- Coordinator lifecycle tests: `parallel_rebuild_is_atomic`,
  `parallel_reference_retains_learned_callsigns` **passed**; all 48
  `coordinator::ft8` tests green.

Baseline (sequential, prior-phase measurement on the same fixtures,
release, from the PAN-318 research): `pre_native_ms` 13–24 ms, of which
waterfall 4–8 ms and ft8_lib 8–15 ms; overlap (waterfall still first in
that probe) 6–11 ms. With the waterfall moved off the prefix, the
measured overlapped `pre_native` in this change is the scoped-free
config-read/barrier path only — the debug shape check above shows it
below the sequential figure on every fixture.

## Release qualification (ran 2026-11-12, this container)

`cargo test --release -p pancetta-ft8 --features benchmark --test pan318_overlap_tests -- --include-ignored --nocapture --test-threads=1`
— **6 passed, 0 failed** in 13.65 s (57 s wall incl. build):

- `standard_window_pre_native_budget` — **passed**: every measured
  window (5 fixtures × 3 warmed runs, Standard 250 ms, no scoped call)
  reached the native call in under 10 ms from receipt.
- `standard_window_start_skew` — **passed**: in-worker start skew
  within 5 ms on every measured window (5 fixtures × 3 warmed runs).
- All deterministic parity tests green in the release profile too.

## Pending

- Windows MiniPC / on-air qualification.
- Deriving the waterfall from the reference monitor FFT (plan Phase 2);
  today the legacy generator still runs, relocated after the joined pair.

## Commands

```sh
cargo test -p pancetta-ft8 --features benchmark,transmit --test pan318_overlap_tests
cargo test -p pancetta-ft8 --features benchmark,transmit --lib window_execution
cargo test -p pancetta --lib coordinator::ft8
scripts/check.sh   # repository gate (full lane)
```
