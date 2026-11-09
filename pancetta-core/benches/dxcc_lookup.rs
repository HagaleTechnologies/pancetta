//! PAN-284: one `entity_for_callsign` lookup must stay under 2 µs.
//! CI runs this and checks the means with scripts/ci/check-criterion-budget.sh.
use criterion::{criterion_group, criterion_main, Criterion};
use pancetta_core::dxcc::entity_for_callsign;
use std::hint::black_box;

const CALLS: &[&str] = &[
    "W1AW",
    "K5ARH",
    "G8KHF",
    "JA1XYZ",
    "DL1ABC",
    "VK3ABC",
    "PP0ZF",
    "EA8/G8BCG",
    "<N7RLK>",
    "QZ9ZZ",
    "kh6xyz",
    "ZS6ABC",
];

fn bench(c: &mut Criterion) {
    let _ = entity_for_callsign("W1AW"); // build the index outside the timed loop
    let mut group = c.benchmark_group("dxcc_entity_for_callsign");
    for call in CALLS {
        group.bench_function(*call, |b| b.iter(|| entity_for_callsign(black_box(call))));
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
