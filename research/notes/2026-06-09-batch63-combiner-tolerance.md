# Batch 63 — hb-244 with widened cache-key tolerance

SNR ladder: [-17.0, -18.0, -19.0, -20.0, -21.0, -22.0] dB (2500 Hz BW). Same Batch 62 synthetic.

| Config | TPs recovered |
|---|---:|
| (1) Standalone (combiner OFF) | 3/6 |
| (2) Combiner ON, tol=0 | 3/6 |
| (3) Combiner ON, tol=1 | 3/6 |
| (4) Combiner ON, tol=2 | 3/6 |

Gains: tol=1 vs tol=0: +0, tol=2 vs tol=0: +0, tol=2 vs tol=1: +0.

**Widening provides no lift on this synthetic**: the issue is not cache-key drift alone. Possibilities: (a) sync stage fails entirely at the failed SNRs (no candidates to combine), (b) freq drift exceeds 2 bins, (c) the LDPC convergence requires more than 2-3x LLR accumulation.
