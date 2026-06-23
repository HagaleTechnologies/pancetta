---
slug: hb-089-residual-accumulation
mode: ft8
state: shelved
created: 2026-06-01T00:00:00Z
last_updated: 2026-06-01T00:00:00Z
branch: iter/2026-06-01-hb-089
parent_hypothesis: hb-089 (spawned from mr-008 ideation, territory A)
wild_card: false
scorecard: (n/a — diagnostic-only; kill switch fired before implementation)
delta_vs_main: 0 (no production change)
disposition: SHELVE — bank-stated kill switch unsatisfiable on the actual corpus (no callsign repeats in 15 s WAVs); task-stated SNR-delta fallback (overlap-averaged magnitude across sub-windows — Phase C 2026-06-02 rename from "Welch averaging"; see claim 7 in docs/engineering/2026-06-02-engineering-substance-audit.md) measured mean Δ +0.013 dB across 28 missed truths (median +0.007 dB, p75 +0.042 dB), two orders of magnitude below the 2 dB PROCEED gate.
---

## Hypothesis

After hb-079 multipass + hb-086 V1 joint-pair-retry saturate, the
remaining missed truths sit in a "cleaned residual" that should be
amenable to Q65-style multi-window coherent accumulation. The claim
(mr-008 ideation, territory A) is that averaging the residual
spectrogram across N=3-5 same-slot sub-windows pulls signals below the
single-window noise floor.

## Mechanism (as bank-stated)

"For each MISSED truth whose callsign appears in 2+ same-slot
sub-windows, measure per-position residual SNR before and after
5-window coherent accumulation. PROCEED if median SNR improvement ≥ 2
dB AND LLR sign-agreement with truth codeword ≥ 70%."

## Mechanism (as task-stated)

"Extract 3 sub-windows of the residual (after hb-079 multipass) at
e.g. 0-13.5s / 0.5-14s / 1-14.5s of the same slot; phase-align via the
candidate's rotor; coherent-average. Measure SNR delta at missed-truth
coordinates. PROCEED if ≥2 dB on average."

## Corpus structural finding (CRITICAL)

The bank-stated kill switch presumes WAVs ≥30 s with cross-slot
repeats — the ideation prose ("FT8's 90 s curated WAVs naturally
contain 5 same-slot sub-windows") references the wrong corpus shape.

The actual `research/corpus/curated/ft8/hard_200` WAVs are **15.0 s
each** (one FT8 slot). They contain ~30-70 DISTINCT callsigns per
slot at different freq bins; NO callsign repeats within a WAV. The
bank-stated criterion ("callsign appears in 2+ same-slot sub-windows")
yields ZERO eligible truths.

That mismatch alone is sufficient to SHELVE under the bank's own
definition.

## Fallback measurement (task-stated SNR delta)

Examples binary `hb089_residual_accumulation_diagnostic` measures:

1. Decode top-5 hard-200 worst WAVs with production `Ft8Decoder`.
2. Build "residual audio" by re-modulating each production decode at
   its (text, freq, dt) and subtracting via least-squares amplitude
   fit. (Time-domain analog of `coherent_subtract_and_repass`'s
   complex-spectrogram subtraction.)
3. For each MISSED truth: encode its text to recover 79 tone-symbols,
   then compute SNR_dB = mean(signal-tone-dB) − mean(noise-tone-dB)
   at the truth's (freq_bin, time_step, freq_sub) on:
     (a) baseline: single-window spectrogram of the residual audio
     (b) Welch: 3-sub-window power-averaged spectrogram, sub-window
         length 156 000 samples (= 13.0 s), offsets {0, 0.25, 0.50}s
4. Report Δ = (b) − (a).

> **Naming note (Phase C 2026-06-02):** the column labeled `snr_welch`
> below is the overlap-averaged magnitude across the 3 sub-windows at
> the bin coordinate. The "Welch" label is loose — Welch (1967) is a
> PSD estimator via averaged-windowed periodograms; what was actually
> measured is per-bin magnitude averaging at fixed coordinates. The
> diagnostic + conclusion are correct; only the column label is loose.
> See docs/engineering/2026-06-02-engineering-substance-audit.md
> (claim 7).

### Per-WAV results (top-5 hard-200)

```
      sha  truth    rec missed   snr_base  snr_welch      Δ_dB
 566cf9fc     70     30     40       0.57       0.62     0.052
 eb762156     66     26     40       0.32       0.33     0.013
 9e5b9243     65     27     38      -0.33      -0.31     0.014
 24acc713     62     27     35       0.61       0.63     0.015
 c328dfb5     65     29     36      -1.18      -1.20    -0.023
```

### Aggregate

- Missed truths probed: 28 (across the 5 WAVs; some missed truths fall
  outside the spectrogram's time-step bounds and are skipped)
- mean Δ SNR:    +0.013 dB
- median Δ SNR:  +0.007 dB
- p25 / p75:     −0.013 / +0.042 dB
- Kill switch:   ≥ 2 dB on the mean → **NOT MET** (off by ~150×)

## Verdict

**SHELVE** under both decision paths:

1. The bank-stated kill switch is unsatisfiable in the actual corpus
   (zero callsign repeats in 15 s WAVs).
2. The task-stated SNR-delta fallback measures essentially zero
   improvement (+0.013 dB mean, well below the 2 dB gate).

## Why the mechanism doesn't work (theoretical)

Overlap-averaged-magnitude across sub-windows (the mechanism this
experiment evaluated; Phase C 2026-06-02 rename from "Welch's method"
— Welch is the PSD estimator from which this borrows the
windowing/overlap idea but is *not* what we implemented) reduces
spectral estimate variance by a factor up to ~T_total / T_window when
sub-windows are non-overlapping. With SUBWINDOW_LENGTH = 13.0 s and
total audio = 15.0 s, the sub-windows overlap by ≥85%, so the noise
samples are highly correlated across sub-windows. Averaging correlated
noise samples does not reduce noise power. The theoretical best-case
SNR improvement is 10·log10(15.0 / 13.0) ≈ 0.6 dB — and that requires
fully decorrelated sub-windows, which the actual 85% overlap prevents.

For a TRUE multi-window coherent gain, the sub-windows must be either:
(a) drawn from independent slots of the same callsign (= existing
    hb-074/075/079 cross-cycle path — only works on multi-slot WAVs);
    or
(b) drawn from non-overlapping samples (= sub-1s segments, too short
    to hold a 12.64 s FT8 message).

Neither is satisfied here. The mechanism is structurally inapplicable
to single-slot FT8 decode geometry.

## What this RULES OUT

- Any "same-slot sub-window averaging" variant of hb-074/075/079
  (regardless of coherent vs power Welch averaging) cannot help on a
  15-second-slot corpus.

## What this DOES NOT rule out

- Multi-slot residual accumulation. If pancetta's coordinator
  retained the LAST few subtract-residuals from previous slots and
  did per-callsign cross-cycle averaging on the OPERATIONAL audio
  buffer (where the same station IS transmitting in N consecutive
  slots), there is real signal-coherent gain. This is the
  hb-074/075/079 lineage and is already in production; what is new
  is doing it on the RESIDUAL rather than the raw spectrogram. That
  variant would need a different corpus (e.g., curate a multi-slot
  "operational replay" tier from `~/.pancetta/recordings/`) to
  evaluate.
- hb-090 (phase-coherent matched filter at truth coordinates) — same
  ideation territory, different mechanism family; not refuted here.

## Files

- `pancetta-research/examples/hb089_residual_accumulation_diagnostic.rs`
  (diagnostic binary; runs in <30 s on top-5 hard-200)

## Branch

`iter/2026-06-01-hb-089`. No production code touched.
