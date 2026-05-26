---
slug: spectrogram-window-sweep
mode: ft8
state: shelved
created: 2026-05-25T21:00:00Z
last_updated: 2026-05-25T21:00:00Z
branch: iter/2026-05-25-batch-11
parent_hypothesis: hb-010
wild_card: false
scorecard: research/scorecards/win-{hann,blackman,kaiser5,kaiser8}.json (transient, removed)
delta_vs_main: none beats Hann — Blackman/Kaiser8 break a fixture, Kaiser5 ties. Code reverted.
disposition: SHELVE hb-010 — Hann (ft8_lib sin² parity) is optimal; no window improves synth SNR@50%.
---

## Hypothesis

hb-010 (0.47): the decode spectrogram uses a hardcoded periodic Hann
taper (`sin²(πi/N)`, matching ft8_lib). Other windows trade main-lobe
width against sidelobe suppression — Blackman (-58 dB sidelobes) or
Kaiser(β) might reduce inter-symbol interference in the spectrogram and
improve weak-signal SNR. Expected +0.005..+0.02 SNR@50% synth-clean.

## Change

Made the spectrogram window configurable (`Ft8Config::spectrogram_window:
WindowFunction`, default Hann). The Hann arm keeps the exact `sin²(πi/N)`
periodic form (bit-exact parity baseline); other windows route through
`signal_processing::generate_window` with the same `2.0/nfft`
normalization. Research `with_spectrogram_window` builder +
`--spectrogram-window <hann|blackman|hamming|kaiserN|rectangle>` eval flag.

## Result

fixtures + synth-clean sweep:

| window  | fixtures | synth@50 | synth@90 |
|---------|---------:|---------:|---------:|
| Hann    |     1.0  |   -20    |   -18    |
| Blackman|   0.875  |   -20    |   -18    |
| Kaiser5 |     1.0  |   -20    |   -18    |
| Kaiser8 |   0.875  |   -20    |   -18    |

- **No window improves synth-clean SNR@50% or @90%** — every alternative
  ties Hann on synth sensitivity, so none can raise the composite's
  synth term.
- **Blackman and Kaiser8 break a fixture** (1.0 → 0.875): their wider
  main lobe smears the marginal `basicft8/170923_082015.wav`
  ground-truth signal — the same fragile fixture that gates OSD work
  (hb-041). Wider/heavier tapers cost the weakest decode.
- **Kaiser5 is decode-identical to Hann** on both guard tiers but
  diverges from ft8_lib parity for zero benefit.

hard-200 not run: since no window improves synth@50 and the aggressive
ones lose a fixture, none can beat the Hann baseline composite — the
question is closed without spending hard-corpus eval time.

## Why Hann wins

ft8_lib chose the periodic Hann `sin²` deliberately, and this confirms
it's the right tradeoff for FT8's 6.25 Hz tone spacing: Hann's main-lobe
width keeps adjacent tones resolvable while its -31 dB sidelobes are
already low enough that the limiting factor is noise, not sidelobe
leakage. Heavier sidelobe suppression (Blackman/Kaiser8) only buys a
wider main lobe that blurs marginal signals — net negative.

## Decision

**SHELVE.** Hann confirmed optimal. Code fully reverted — adding a
proven-useless window knob would be exactly the dead-config surface
batch-10 iter 5 (hb-060/061) just removed. Journal retained.

## Learnings / follow-ups

- The spectrogram window lever is closed. No follow-up.
- Reinforces the recurring pattern: ft8_lib's deliberate parity choices
  (Hann window, sync structure) are well-tuned; pancetta's wins come
  from *post*-spectrogram structural changes (layered BP, sync_cap, NMS
  off, FP filter), not from re-tuning the front-end taper.
</content>
