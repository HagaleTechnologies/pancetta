# Batch 62 — hb-244 soft combiner repeat-heavy probe

Message: `CQ K5ARH EM10`. SNR ladder: [-17.0, -18.0, -19.0, -20.0, -21.0, -22.0] dB (2500 Hz BW). Each reception synthesized at the same audio frequency offset.

| Config | TPs recovered |
|---|---:|
| (1) Standalone (combiner OFF, fresh decoder per reception) | 3/6 |
| (2) Persistent decoder, combiner OFF | 3/6 |
| (3) Persistent decoder, combiner ON (hb-244) | 3/6 |

**Combiner gain** (3) - (2): +0
**Persistent-only gain** (2) - (1): +0 (sanity check)

**hb-244 inert on this synthetic**: combiner doesn't change the recovery profile. Either the signal repeats are too high-SNR (combiner not needed) or too low-SNR (no usable LLR evidence). Try a different SNR ladder.
