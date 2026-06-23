# Batch 77 — Slow-tier LLR whitening re-verified with ft8_lib truth

hard_1000, ft8_lib truth, Slow-tier preset (`max_decode_passes=1, osd_depth=Some(1)`).
Re-verification of Batch 56 (+14 TPs / -8 FPs under pancetta truth).

| Config | Decodes | TPs | FPs | Precision |
|---|---:|---:|---:|---:|
| Slow OFF | 18099 | 14493 | 3606 | 0.8008 |
| Slow + whitening ON | 18105 | 14494 | 3611 | 0.8006 |

Δ TPs: +1 | Δ FPs: +5

**Mixed under ft8_lib truth**: TPs +1, FPs +5. Small-delta regime; apply bootstrap-CI policy before any default change.

## Verdict (post-run)

Batch 56's headline (+14 TPs / -8 FPs) was pancetta-truth optimism: under
ft8_lib truth the Slow-tier effect is **inert** (+1 TP / +5 FPs on a
14,493-TP / 3,606-FP base; both deltas well inside sample noise). No
bootstrap CI needed because no default change rides on the sign: both the
old and new measurements support the same decision — whitening stays
unconditional default-ON, no tier-conditional flip. The Fast-tier benefit
(-717 FPs, Batch 69) remains the double-verified, load-bearing
justification; the Slow tier neither helps nor hurts.

Pattern note: this is the third pancetta-truth small-positive delta to
shrink toward zero under ft8_lib truth (hb-244 +8 → +0, B56 +14 → +1),
while the one large delta (whitening Fast-tier -713 FPs) held. Consistent
with truth-source bias inflating small TP deltas specifically.
