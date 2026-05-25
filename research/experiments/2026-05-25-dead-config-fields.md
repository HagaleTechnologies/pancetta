---
slug: dead-config-fields
mode: ft8
state: graduated
created: 2026-05-25T20:30:00Z
last_updated: 2026-05-25T20:30:00Z
branch: iter/2026-05-25-batch-10
parent_hypothesis: hb-060, hb-061
wild_card: false
scorecard: n/a (cleanup — no behavior change)
delta_vs_main: none (dead fields, never read)
disposition: GRADUATE hb-060 + hb-061 — remove dead Ft8Config::enable_multithreading and frequency_range fields.
---

## Hypothesis

hb-060 / hb-061 (mr-004 source-drift audit 2026-05-25): `Ft8Config`
carries two `pub` fields that are declared, defaulted, and set in
tests/benches/examples but **never read** in the decode pipeline —
mirrors of the already-removed `aggressive_decoding` (hb-032) and
`min_snr_db` (hb-049):

- `enable_multithreading` — the parallel decode in `par_try_ap_decode`
  uses rayon unconditionally; the flag is never consulted.
- `frequency_range` — the actual search bounds are the hardcoded
  `MIN_FREQ_BIN..max_freq_bin` in `costas_sync_search`; the field is
  never consulted.

## Change

Removed both fields, their `Default` entries, and every referencing
site:

- `pancetta-ft8/src/decoder.rs`: field decls + Default + the
  `test_ft8_config_default` assert on `enable_multithreading`.
- `pancetta-ft8/tests/integration_tests.rs`: dropped the
  `minimal_config.enable_multithreading = false` line.
- `pancetta-ft8/benches/decoder_benchmark.rs`: removed the entire
  `single_thread` benchmark — it set the dead flag and was therefore
  bit-identical to the `default` benchmark (both multithreaded), so it
  measured nothing and was misleading.
- `pancetta-ft8/examples/enhanced_spectral_analysis.rs`: dropped the
  `frequency_range: 300.0` override.
- `pancetta-ft8/README.md` + `SPECTRAL_ANALYSIS_ENHANCEMENTS.md`:
  removed both fields from the config examples and the field table.

Minor breaking API change (two `pub` fields deleted) — acceptable
pre-OSS-publish, same call as hb-032 / hb-049.

## Result

Builds clean across all ft8 targets (lib, tests, benches, examples).
Tests: 193 lib + 11 integration pass. No behavior change (the fields
were never read), so no eval needed and the composite is untouched.

## Decision

**GRADUATE both.** Two dead `pub` fields gone; the misleading
`single_thread` benchmark gone.

## Learnings / follow-ups

- That's four dead `Ft8Config` fields now retired across hb-032
  (aggressive_decoding), hb-049 (min_snr_db), hb-060
  (enable_multithreading), hb-061 (frequency_range). mr-004's
  per-field grep is the right periodic hygiene check; worth re-running
  after any config-surface churn.
- No new hypotheses.
</content>
