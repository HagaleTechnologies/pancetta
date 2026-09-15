# What did the September 7 product assessment establish?

The [product assessment](../../docs/reviews/2026-09-07-product-assessment.md)
reviews baseline 17ffe360fbc1f1f2afac6615aad76d6df39f4c32, which is 24
commits ahead of the published v0.9.6 tag. Its 16 priorities cover station
workflow, market alternatives, usability, integration qualification and
distribution. They are product recommendations, not a merged implementation
plan or security findings.

The [evidence bundle](../../docs/reviews/2026-09-07-product-evidence/README.md)
preserves current renderer/parser probes. Compact views lose UTC in the
tested idle fixtures; the two GUIDE UDP recipes fail as standalone configs.
The report separates these reproductions from qualitative design judgments,
historical benchmarks and proposed acceptance targets.

Do not carry older review assumptions forward without checking source:
live rig switching/bookmarks and real TQSL invocation exist at this baseline.
The production FT8 component merges ft8_lib and native decoder output;
historical native-only curves do not measure that whole runtime.

Validation is qualified: the shipped-crate suite passed in the same session,
but the full required workspace suite failed in a research cache fixture.
Detailed security evidence remains in the separate private security artifact.
