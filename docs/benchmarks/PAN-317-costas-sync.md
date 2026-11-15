# PAN-317 Costas sync sweep

The complete warmed search ran 3.280 times faster with four Rayon workers on
2026-11-12. Candidate positions, scores, refinements and order matched the serial
traversal bit for bit. This measures the sync search, not a whole decode window.

## Reproduce

```sh
cargo test --release -p pancetta-ft8 --lib pan317_four_worker_speedup -- --ignored --nocapture --test-threads=1
```

The ignored test times 21 alternating serial/parallel pairs, reversing which path
runs first on each pair. Both paths receive five warmups. Timing includes candidate
collection, ordered peak merging, optional per-bin selection, auxiliary baseline
emission, stable sorting, truncation and the configured NMS tail. Output consumption
uses `black_box`; bitwise equality checks run outside the timed regions. Spectrogram
construction, decoder construction and pool startup are excluded from both paths.

The default configuration has NMS disabled. The correctness matrix separately covers
NMS enabled and deferred, two baselines, partial scoring, per-bin selection,
interpolation controls, both half-loop settings and scoped partner relaxation.
The serial baseline retains the original nested-loop traversal and shares the
unchanged f64 kernels and serial tail with the parallel path. Per-bin group emission
is now canonical by ascending frequency bin instead of HashMap iteration order.

## Host and result

- CPU: AMD Ryzen 9 5950X, 16 physical cores, 32 logical CPUs.
- Affinity: CPUs 0–31. `nproc` reports 32.
- Cgroup v2 `cpu.max`: `600000 100000`, a six-CPU quota.
- Rust `available_parallelism`: 6. Explicit Rayon pool: 4 workers.
- Rust: `rustc 1.98.0 (88d9e12ae 2026-08-18)`, x86_64 Linux.
- Build: Cargo release profile, no competing build during measurement.
- Fixture: `pancetta-ft8/tests/fixtures/wav/jtdx/190227_155815.wav`, mono i16,
  12 kHz, resized to `WINDOW_SAMPLES` = 151,680 samples as in the profiling example.
- Configuration: `Ft8Config::default()`, full frequency scope, no deferred NMS.
  Chunk width is 32 frequency bins. No precision or scoring-order changes.
- Serial median: **10.160862 ms**.
- Parallel median: **3.097687 ms**.
- Ratio: **3.280×**, passing the required 2× on a qualifying host.

Raw test output:

```text
PAN-317: serial median 10.160862ms; parallel median 3.097687ms; speedup 3.280x; 21 pairs
available_parallelism=6, cgroup quota=Some(6.0)
test result: ok. 1 passed; 0 failed; 0 ignored
```

The previous planning container had a two-CPU quota. This implementation run has a
six-CPU quota and qualifies independently; the figures above are from this code,
not the prior research probe. Timing remains host sensitive, so the benchmark is
ignored in ordinary CI. Windows MiniPC and on-air behavior were not measured.

The ordering hazard is specific to stable score ties: indexed Rayon collection
must preserve `(freq_sub, t0, f0-chunk)` order, and peak merging must use strict `>`
to retain the earliest time step. Changing either rule can change NMS survivors.
