---
slug: min-snr-db-removal
mode: ft8
state: won
created: 2026-05-24T04:30:00Z
last_updated: 2026-05-24T04:30:00Z
branch: iter/2026-05-24-batch-3
parent_hypothesis: hb-049
wild_card: false
scorecard: (n/a — cleanup only, no behavior change)
delta_vs_main: 0
disposition: WIN (cleanup) — second dead pub field removed from Ft8Config. Mirror of hb-032.
---

## Hypothesis

hb-049 (spawned from hb-045 audit in this batch): the `min_snr_db`
field on `Ft8Config` and the `MIN_DECODE_SNR` const are dead code.
Field declared at decoder.rs:114, defaulted to -25.0, but never
read in the decode pipeline. Same pattern as the recently-removed
`aggressive_decoding` flag (hb-032).

## Change

Removed `Ft8Config::min_snr_db` + `MIN_DECODE_SNR` const + all
references:

- **`pancetta-ft8/src/decoder.rs`**: removed field decl, Default
  impl entry, MIN_DECODE_SNR const, and the assertion in
  `test_ft8_config_default`.
- **`pancetta-ft8/tests/integration_tests.rs`**: removed two
  references in `test_decoder_configuration_variants`.
- **`pancetta-ft8/benches/decoder_benchmark.rs`**: removed two
  references (high_sensitivity + minimal benchmarks).
- **`pancetta-ft8/README.md`**: removed from example code block
  and config-parameters table.
- **`pancetta-ft8/SPECTRAL_ANALYSIS_ENHANCEMENTS.md`**: removed
  from example code.
- **`pancetta-ft8/examples/enhanced_spectral_analysis.rs`**: removed
  from example config.

Verification: `grep -rn min_snr_db pancetta-ft8/` returns nothing
after the change.

Note: `pancetta-config/src/network.rs` has a separately-defined
`min_snr_db` field (line 67) for filtering displayed decodes. That
one is real and stays — different concept, different module.

## Result

Tests: 189 lib pass + 7 integration. Examples build clean.

## Disposition

**WIN (cleanup).** Public API surface shrunk by one bool field.
Documentation no longer lies about a feature that doesn't exist
(SNR-threshold gating of candidates). Minor breaking API change
for named-field constructors; `..Default::default()` callers
unaffected.

## Learnings

- **The hb-032 pattern is repeatable for dead pub config fields.**
  Two finds in the same audit pass would be ideal; mr-004 (quarterly
  source-drift audit) is now overdue and would have caught both
  aggressive_decoding (caught manually as hb-020) and min_snr_db
  (caught accidentally via hb-045 architecture audit).
- **Architecture-fit audits surface dead code as a side effect.**
  hb-045's audit didn't intend to find dead code, but the question
  "where does pancetta gate candidates by SNR?" returned "nowhere"
  — which IS the finding for hb-049.

## Follow-ups added to hypothesis bank

- **hb-049 → CLOSED (WIN).** Field removed; no further work.
- **Run mr-004 (source-drift audit) when bank thins again.**

## Reproducing

```bash
cargo test --features transmit -p pancetta-ft8
cargo build -p pancetta-ft8 --examples
grep -rn min_snr_db pancetta-ft8/  # should return nothing
```
