//! PAN-320 CPU budget bench (AC7): the production FIR decimates one
//! 15 s window — 720,000 input samples in realistic 512-frame chunks —
//! into 180,000 outputs with a Criterion mean strictly under 5 ms.
//!
//! Group and case names are load-bearing: CI enforces the budget via
//! `scripts/ci/check-criterion-budget.sh target/criterion/decimation_48k_12k
//! 5000000`, which reads `target/criterion/<group>/<case>/new/estimates.json`.

use criterion::{criterion_group, criterion_main, Criterion};
use pancetta_dsp::decimation::DecimationFilter;
use std::hint::black_box;

/// Deterministic, non-zero 15 s of 48 kHz audio (two in-band tones + a
/// slow envelope): nothing the optimizer can elide, and representative of
/// the real window's energy.
fn window_15s() -> Vec<f32> {
    (0..720_000)
        .map(|i| {
            let t = i as f64 / 48_000.0;
            (0.4 * (2.0 * std::f64::consts::PI * 700.0 * t).sin()
                + 0.3 * (2.0 * std::f64::consts::PI * 2300.0 * t).sin()
                + 0.2 * (2.0 * std::f64::consts::PI * 25.0 * t).sin()) as f32
        })
        .collect()
}

fn bench_decimation(c: &mut Criterion) {
    let input = window_15s();
    let mut group = c.benchmark_group("decimation_48k_12k");

    // Correctness of the benched shape, asserted outside all timing: the
    // filter yields exactly floor(N/4) outputs for the 15 s window.
    {
        let mut fir = DecimationFilter::new();
        let mut probe = Vec::with_capacity(180_000);
        for chunk in input.chunks(512) {
            fir.process(chunk, &mut probe);
        }
        assert_eq!(probe.len(), 180_000, "15 s at 48 kHz -> 180000 at 12 kHz");
    }

    // Constructor coefficient design is outside the benchmark: one filter,
    // reset + clear + process inside each iteration, output capacity
    // reserved once outside the timed loop.
    let mut fir = DecimationFilter::new();
    let mut out = Vec::with_capacity(180_000);
    group.bench_function("fir31_15s", |b| {
        b.iter(|| {
            fir.reset();
            out.clear();
            for chunk in input.chunks(512) {
                fir.process(black_box(chunk), &mut out);
            }
            black_box(&out);
        });
    });
    assert_eq!(out.len(), 180_000);

    group.finish();
}

criterion_group!(benches, bench_decimation);
criterion_main!(benches);
