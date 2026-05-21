---
slug: aggressive-decoding-audit
mode: ft8
state: shelved
created: 2026-05-21T00:00:00Z
last_updated: 2026-05-21T00:00:00Z
branch: experiment/ft8/aggressive-decoding-audit
parent_hypothesis: hb-020
wild_card: true
scorecard: (n/a — audit only, no code change)
delta_vs_main: 0
disposition: SHELVED — flag is dead code; cleanup spawned as hb-032
---

## Hypothesis

`Ft8Config::aggressive_decoding: bool` is documented as "Enable
aggressive decoding (more CPU, better weak signal performance)" but
the production config (`main.json` and every scorecard config snapshot)
shows it set to `false` and the field's actual behavior is unknown.
Per the hb-020 protocol: "grep all callsites in pancetta-ft8/src/.
If it does nothing, document and shelve. If it unlocks real behavior,
eval it on."

## Change

None — audit only.

## Result

**The flag is dead code.** Exhaustive search across the repo:

```
pancetta-ft8/src/decoder.rs:100:    /// Enable aggressive decoding (more CPU, better weak signal performance)
pancetta-ft8/src/decoder.rs:101:    pub aggressive_decoding: bool,
pancetta-ft8/src/decoder.rs:127:            aggressive_decoding: false,
```

Three references in pancetta-ft8/src/, all in the same struct
declaration: field decl, doc comment, default. **Zero reads.** Setting
`aggressive_decoding = true` has no effect on the decode pipeline.

**Surrounding cargo-cult on the dead flag:**

1. `pancetta-ft8/tests/integration_tests.rs::test_decoder_configuration_variants`
   sets the flag to `true` but asserts only `result.is_ok()` — never
   asserts any behavioral difference vs the default config. The test
   passes regardless of what the flag does.

2. `pancetta-ft8/benches/decoder_benchmark.rs::aggressive` benchmark
   bundles `aggressive_decoding=true` with `max_candidates=100,
   min_snr_db=-25.0`. But those companion settings are already at the
   default values (`Ft8Config::default()` has `max_candidates=100,
   min_snr_db=-25.0`). The "aggressive" benchmark is bit-identical to
   the "default" benchmark — comparing the same config against itself.

3. `pancetta-ft8/README.md` (lines 135-140 and 256) documents
   `aggressive_decoding` as a real feature with usage examples.
   Misleading: the documented behavior does not exist.

4. `pancetta-ft8/examples/enhanced_spectral_analysis.rs:29` enables
   the flag in a "showcase" example. Also misleading.

5. `pancetta-ft8/SPECTRAL_ANALYSIS_ENHANCEMENTS.md:122` references it
   as one of "all enhancements" — also misleading.

All scorecards (main.json, sweep/*.json, history/*.json) capture the
config with `aggressive_decoding: false`, so the harness has never
exercised it (which would have been a no-op anyway).

## Disposition

**SHELVED.** Per the bank's hb-020 protocol, "if it does nothing,
document and shelve." Confirmed: the flag does nothing. No code change
in this commit — just the journal + bank update.

The cleanup (removing the dead field + fixing the misleading docs,
test, benchmark, and example) is a separate scope. Spawned as **hb-032**
in case the operator wants to land the cleanup, or as a candidate to
be repurposed into a real "fast vs deep" preset (synergizes with
hb-031's fast-path mode).

## Learnings

- **The field is a footgun.** A developer reading the README, the
  example, or the integration test might reasonably believe that
  `aggressive_decoding = true` changes decoder behavior. It does not.
  Anyone running a benchmark comparing "default" vs "aggressive" gets
  identical numbers, which is silently misleading. Memory-grade
  surprise — would not have been caught without this audit.

- **"Aggressive" is the natural name for hb-031.** The hb-001 sweep
  showed pass 1 alone gets 98.8% of multi-pass at 10% of compute
  cost. The natural inversion is to call THAT mode "fast" and the
  current default "aggressive". This dead field is the right place
  to wire that toggle when hb-031 lands.

- **Wild-card audits pay for themselves even when the answer is
  "shelve."** This took ~10 minutes of investigation and surfaced a
  documentation/code-coherence gap that's been sitting in the
  repository for an unknown duration. Cleanup follow-up is well-scoped.

- **One sanity check that wasn't worth doing:** running the eval with
  `aggressive_decoding=true`. The grep result was decisive on its
  own; no eval would have produced a different number. Saved ~10 min
  of compute.

## Follow-ups added to hypothesis bank

- **hb-032 (new)** — Remove or repurpose the dead
  `aggressive_decoding` field. Three options for the cleanup:
  (a) delete the field + all referencing code (cleanest; minor
  breaking API change but pre-OSS-publish, so acceptable);
  (b) repurpose to drive a "fast | balanced | deep" preset
  (synergizes with hb-031);
  (c) deprecate the field with a `#[deprecated]` attribute and
  document it as a no-op. Recommended path: (b) when hb-031 lands;
  (a) before OSS publish if hb-031 doesn't land. Priority ~0.40.
  Estimated effort: 0.5 sessions.

## Reproducing

```bash
# Confirm the audit:
grep -rn "aggressive_decoding" pancetta-ft8/src/

# Should show only:
#   decoder.rs:100  (doc comment)
#   decoder.rs:101  (field decl)
#   decoder.rs:127  (default value)
#
# No reads anywhere.
```
