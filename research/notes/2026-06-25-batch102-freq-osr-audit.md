# Batch 102 — FREQ_OSR knob audit (hb-225 mechanism, current baseline)

**Verdict: NEGATIVE for the naive-oversampling path. hb-225 priority LOWERED
0.55 → 0.30; its specific (fixed-resolution sub-bin Costas grid) mechanism is
NOT refuted but is now lower-confidence.**

## Why this probe
hb-225 (ft8mon 2-D sub-bin Costas grid) targets the band-middle recall hole via
finer-than-current sub-bin frequency search. Batch 45's sub-Hz freq-dither probe
corroborated the *mechanism* (+33 TPs at N=200) — but that predates the dt-align
fix (B88), hash-normalized scoring (B87), and decode_origin. Probe-baseline
discipline: re-verify on the CURRENT baseline before the ~250 LOC cached-FFT
grid. The decoder already does sub-bin freq search at `FREQ_OSR = 2` (3.125 Hz)
+ hb-044 parabolic refinement, so the cheapest direct knob is `FREQ_OSR` 2 → 4
(1.5625 Hz sub-bins).

## Result (raw_530_full, N=50, ft8_lib truth, hash-normalized)

| arm | decodes | TP | FP | precision | miss | wall |
|-----|--------:|---:|---:|----------:|-----:|-----:|
| FREQ_OSR=2 (baseline) | 1129 | 928 | 201 | 0.8220 | **2.42%** | 15.8s |
| FREQ_OSR=4 (probe)    |  844 | 777 |  67 | 0.9206 | **18.30%** | 11.3s |

FREQ_OSR=4 LOSES 151 TPs and pushes miss 2.42% → 18.30%. Precision rises (FPs
fall to 67) but that's the wrong trade — it's rejecting real signals, not noise.

## Mechanism
Raising FREQ_OSR doubles the spectrogram FFT (`nfft = block_size·freq_osr`) and
splits each tone's energy across 4 sub-bins instead of 2. That rescales the dB
power map, the 40th-percentile noise-floor estimate, the Costas sync scores, and
everything the OSR=2-tuned thresholds (sync min, OSD gate=6, hb-062/103 FP
filters) consume. The pipeline is calibrated for OSR=2; the knob can't be lifted
in isolation. **Identical shape to hb-224** (ft8mon osd gate=70 port regressed
because pancetta's downstream is tuned for gate=6).

## What this does and doesn't say about hb-225
- It KILLS the naive "just oversample frequency finer" path — a dead end.
- It does NOT cleanly test hb-225's *actual* proposal, which keeps the OSR=2
  spectrogram resolution and adds sub-bin search OFFSETS within the coarse
  Costas grid (cached-FFT bin-rotation), without changing the FFT size or the
  energy normalization. That mechanism is untouched here.
- But the regression shows how sensitive recall is to anything that perturbs the
  OSR=2 normalization, which raises the bar for hb-225: the 4×4 grid would have
  to surface candidates WITHOUT disturbing the tuned scoring — non-trivial.

Combined with Batch 45's +33 being pre-hash-norm and pre-dt-fix, hb-225 is no
longer a high-confidence +30-50 TP bet. Lowered to 0.30; re-open only with a
band-middle-specific corpus + a fixed-resolution sub-bin probe (not a knob bump).

Probe: `pancetta-research/examples/batch102_freq_osr_audit.rs` (kept; rebuild the
decoder per FREQ_OSR arm). Production unchanged (`FREQ_OSR = 2`).
