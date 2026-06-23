---
slug: aggressive-decoding-removal
mode: ft8
state: won
created: 2026-05-24T00:00:00Z
last_updated: 2026-05-24T00:00:00Z
branch: experiment/ft8/multipass-profile (chained)
parent_hypothesis: hb-032
wild_card: false
scorecard: (n/a — cleanup only)
delta_vs_main: 0 (cleanup; no behavior change)
disposition: WIN (cleanup) — removed dead Ft8Config::aggressive_decoding field + all references
---

## Hypothesis

hb-032 (spawned from hb-020 audit on 2026-05-21): the
`Ft8Config::aggressive_decoding: bool` field has 3 references in
pancetta-ft8 source (field decl, doc comment, default value) and zero
reads in the decode pipeline. Setting it to true has no behavioral
effect. Surrounding cargo-cult code in tests, benches, and docs
treats it as a real feature.

hb-020 audit (2026-05-21) confirmed the field is dead. hb-032's
recommendation: either (a) delete the field + all referencing code,
(b) repurpose for hb-031's fast|balanced|deep preset, or (c)
deprecate as a no-op. The bank entry recommended (b) when hb-031
lands; otherwise (a).

hb-031 (max_decode_passes default 3 → 1) DID land before this cycle.
Repurposing was on the table — but a fresh preset API would be
cleaner than retrofitting a single-bool flag, and "aggressive" is
ambiguous (does it mean "more candidates" or "more LDPC iters" or
"longer subtract window"?). Going with option (a): pure deletion.

## Change

Removed `Ft8Config::aggressive_decoding` and all references:

- **`pancetta-ft8/src/decoder.rs`**: Removed field declaration
  (line 119-120) and Default impl (line 217).
- **`pancetta-ft8/tests/integration_tests.rs`**: Removed
  `aggressive_config.aggressive_decoding = true;` from
  `test_decoder_configuration_variants`; renamed local var
  `aggressive_config` → `high_sensitivity_config` to match what
  the bundled settings actually do (more candidates, wider SNR
  window — no "aggression" anywhere).
- **`pancetta-ft8/benches/decoder_benchmark.rs`**: Same rename in
  the "aggressive" benchmark → "high_sensitivity" benchmark. The
  pre-rename benchmark was bit-identical to "default" because the
  flag did nothing; the rename makes the surface match the actual
  knobs.
- **`pancetta-ft8/README.md`**: Removed the field from the example
  code block and from the config-parameters table.
- **`pancetta-ft8/SPECTRAL_ANALYSIS_ENHANCEMENTS.md`**: Removed the
  `aggressive_decoding: true` line from its example.

Verification: `grep -rn aggressive_decoding` returns only entries in
research journals (this one + the older hb-020 audit). No live code
or doc references.

## Result

Tests: 189 lib + 35 integration/transmit tests pass (no behavior
change, just removed a no-op).

```
test result: ok. 189 passed; 0 failed
test result: ok. 7 passed; 0 failed   (integration_tests)
test result: ok. 10 passed; 0 failed  (loopback_qso scaffolding)
test result: ok. 11 passed; 0 failed  (encoder/modulator)
test result: ok. 7 passed; 0 failed   (cross-decode)
```

Build: clean (`cargo build -p pancetta-ft8`).

## Disposition

**WIN (cleanup).** Public API surface shrunk by one bool field.
Documentation no longer lies about a feature that doesn't exist.
Benchmark name now describes what it actually measures.

This is a minor breaking-API change. Any external consumer
constructing `Ft8Config` with named-field syntax that sets
`aggressive_decoding: ...` will fail to compile. The fix is
trivial — delete the line. The struct supports `..Default::default()`
which most callers use, so impact is bounded.

## Learnings

- **Renaming reveals truth.** The "aggressive" benchmark and test
  name suggested the flag was the actual knob being benchmarked.
  Renaming to "high_sensitivity" makes the actual knobs (more
  candidates, wider SNR) visible in the symbol name.
- **Dead flags accumulate cargo-cult.** A single unused boolean
  produced wrong information across docs (README, SPECTRAL...),
  tests (integration_tests.rs), benches, and examples. Cleaner
  to delete the dead source than patch each downstream mention.
- **Wait until the natural replacement materializes before
  repurposing.** Option (b) (repurpose for fast|balanced|deep
  preset) was tempting, but designing a proper preset API needs
  its own iter — and conflating "remove dead flag" with "design
  new API" muddies the commit. If/when we want presets, hb-NNN
  can design them from scratch with clean names.

## Follow-ups added to hypothesis bank

- **hb-032 → CLOSED (WIN).** Field removed; no further work.
- **Future preset hypothesis (no new hb-id yet):** if operational
  experience suggests a fast|balanced|deep preset would be
  valuable, design it as a fresh enum (`Ft8Profile::{Fast,
  Balanced, Deep}`) that builds an Ft8Config — not a retrofit of
  a single bool.

## Reproducing

```bash
cargo test --features transmit -p pancetta-ft8
grep -rn aggressive_decoding pancetta-ft8/  # should return nothing
```
