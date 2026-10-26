# What will bite you about measuring impulsiveness on real recordings?

The PAN-162 instrument (`pancetta-research/src/impulsiveness.rs`) counts the
160 ms blocks (one FT8 symbol, 1920 samples at 12 kHz) whose mean power is
more than 3× the median of a 25-block window that is shifted to stay inside
the slot. A day's score is the mean of its slots' impulsive-block fractions.
The obvious simpler definitions were tried in a pilot and fail on real audio:

- **A slot-global median is fooled by drift.** Band-wide level drifts by 2×
  within a single 15 s slot on real off-air captures, so a global median
  flags the loud half of the slot.
- **Per-bin spectral metrics are fooled by dense bands.** Hopping 8-FSK tones
  fill more than half the bins on a busy 20 m slot, so signal-dominated
  blocks look impulsive. Their crest factor (~4-5 dB) gives them away;
  a real burst runs ~12 dB.
- **A window truncated at the slot edge flags next-slot onsets.** The slot's
  own signals end at ~13 s; a truncated window at the end sees mostly the
  quiet gap, so the next slot's transmissions starting at t ≥ 14.4 s look
  like bursts. Keeping the full 25-block window inside the slot fixes it.

Blind spot: in a very busy band, a burst weaker than the total signal power
is under-counted. The block size is FT8-specific; do not reuse it for FT4.

On the 62 vendored 12 kHz off-air WAVs almost every slot scores 0, which is
expected: none is known to be a storm capture. The synthetic burst plants
(`noise::ImpulseBurstPlant`) score ≈0.044 (batch101), ≈0.087 (storm) and
≈0.186 (severe), so a real day's score can be placed on the same scale.

Sources: `research/experiments/2026-10-25-pan162-impulse-llr-storm.md`,
`research/hypothesis_bank.md` hb-256. Code and docs win any conflict.
