---
slug: resolve-isolated-novels
mode: ft8
state: shelved
created: 2026-05-23T00:00:00Z
last_updated: 2026-05-23T00:00:00Z
branch: experiment/ft8/resolve-isolated-novels
parent_hypothesis: hb-039
wild_card: false
scorecard: (n/a — probe output only)
delta_vs_main: diagnostic; refines hb-024's precision bound
disposition: SHELVED — ~97% of isolated callsigns are singletons (likely FPs); pancetta precision ≈ 87.7%
---

## Hypothesis

hb-024 (2026-05-23) found that 64.6% of pancetta's novel decodes on
hard_1000 had callsigns continued in jt9 truth elsewhere (likely real),
but 35.2% were "isolated" (callsign never seen in 1121 jt9 baselines).
Isolated novels are ambiguous: rare DX OR LDPC+CRC false positives.

hb-039: self-consistency check. If pancetta finds the same isolated
callsign in MULTIPLE WAVs across the corpus, it's likely a real
(but jt9-missed) station. If each isolated callsign appears in
exactly one pancetta WAV, FP suspicion is strong (random noise
should produce different "callsign-shaped" garbage in different WAVs).

## Change

Pure research probe — no production behavior changed.

- `pancetta-research/examples/resolve_isolated_novels.rs` — sibling
  to `cross_validate_novels.rs`. Loads jt9 baselines, decodes hard_1000
  with pancetta, tracks per-callsign distinct-WAV appearances in
  pancetta novels, classifies isolated callsigns by self-consistency.

## Result

```
Total unique callsigns in pancetta novel decodes: 3086
  - in jt9 truth (continued, validated by hb-024): 740 (24.0%)
  - NOT in jt9 truth (isolated):                   2346 (76.0%)

Self-consistency of isolated callsigns (# distinct pancetta WAVs):
    1 (singleton): 2278  ← 97.1% of isolated
                2:   11
              3-5:   52
             6-10:    2
              10+:    3

Isolated singletons (likely FPs):                 2278 (97.1% of isolated)
Isolated multi-appearance (likely real rare DX):    68 ( 2.9% of isolated)
```

**97.1% of isolated callsigns appear in exactly one pancetta WAV.**
This is the classic FP signature: LDPC+CRC random convergences would
generate different "callsign-shaped" garbage in different noise WAVs,
rarely repeating exactly. The 68 multi-appearance callsigns are
likely real stations that jt9 missed every time.

## Refined precision estimate (combining hb-024 + hb-039)

Per-decode picture on hard_1000:
- Recovered (jt9-matched): 4326 (cap=300 production)
- Novel-continued (real, callsign seen elsewhere): 1572
- Novel-isolated, multi-appearance (likely real rare DX): ~26 estimated
- Novel-isolated, singleton (likely FPs): ~830 estimated

Real-decode total: ~5924  •  Estimated FPs: ~830
**Pancetta precision: ~87.7%** — matches hb-024's worst-case bound.

## Disposition

**SHELVED** with refined diagnostic. No production change.

Combined with hb-024, we now have:
- **Recall:** pancetta finds ~5924 / 28104 jt9-truth + ~5924 union ≈ 21% of all CRC-valid signals on hard_1000 corpus. Conservative; underestimates due to jt9 also missing real signals.
- **Precision:** ~87.7% (12.3% FP rate).

These are concrete operational numbers — useful for sizing future
work. Per-WAV: about 4.3 real decodes + 0.8 FPs.

## Learnings

- **Singleton-callsign-in-novels is a strong FP signature.** 97%
  of isolated novel callsigns appear in exactly one WAV. This could
  be used as an in-corpus FP filter (post-hoc cleanup) but not as
  an in-decoder filter (it requires cross-WAV context).

- **Pancetta's FP rate is concrete at ~12%, not catastrophic.** The
  parity gate ≤4 (decoder.rs:3163) is doing real work — it's
  catching MOST FPs. The remaining 12% slip through CRC-14 collision
  + OSD-2 trials.

- **Future FP-filter work has a concrete target.** hb-014 (parity
  gate sweep) is now better-motivated: tightening from ≤4 to ≤3
  might drop the FP rate to ~5% at cost of some real decodes.
  hb-034 (OSD-3 audit) is less urgent — even the current OSD-2
  outputs roughly correctly.

- **The 68 multi-appearance isolated callsigns are the rare-DX-or-
  bug category.** Worth checking if any are syntactically valid but
  semantically impossible (e.g., a prefix that doesn't exist in
  any country's allocation). That'd be a separate diagnostic.

## Follow-ups added to hypothesis bank

None new. The result tightens the case for hb-014 (parity gate
sweep) which is already in the active bank. No fresh hypothesis
needed beyond what's already queued.

## Reproducing

```bash
cargo run --release -p pancetta-research --example resolve_isolated_novels
```

Self-contained, deterministic. ~10 min runtime.
