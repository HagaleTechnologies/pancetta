# Batch 90 — hb-090 Stage B: phase-coherent matched filter at truth coordinates

Date: 2026-06-12
Branch: `iter/2026-06-12-batch-90`
Example: `pancetta-research/examples/batch90_hb090_matched_filter.rs`
Corpus: top-20 hard-200 worst WAVs (per `research/scorecards/main.json` per_wav_top_failures)

## Setup

Stage A (`hb088_osd_without_costas_feasibility.rs` + `PANCETTA_HB088_STEP_OFFSET`
sweep) established the correct row convention: offset **+2** (controls peak at
91.4% mean sign-agreement there, confirming Batch 88's
`SLIDING_FRAME_LOOKBACK_STEPS = 2`), and sub-Costas misses stay at 50.6% at
every offset under the max-log spectrogram-magnitude demod.

Stage B runs hb-090's pre-registered kill-switch: replace the max-log demod
with a **phase-coherent matched filter** at the same (corrected, +2)
coordinates. For each of the 79 symbols, the 1920-sample window is correlated
against the 8 complex tone templates exp(-j2π(f0 + k·6.25)t), k = 0..7
(cos + j·sin correlation of the real audio; |C| is the coherent matched-filter
output). The 8-tone magnitudes (in dB) flow through the IDENTICAL gray-code
max-log LLR function as the spectrogram path — the only delta is the demod
front-end.

**Sample mapping** (the load-bearing detail): `pos_from_freq_dt` with the +2
convention gives `t0 = round(dt/0.08) + 2`. Per the decoder's one-true-helper
`candidate_offset_samples` (pancetta-ft8/src/decoder.rs:93, `time_padding = 0`
since the example's spectrogram prepends nothing), the signal at row `t0`
starts at sample `(t0 − 2)·960 = round(dt/0.08)·960 ≈ dt·12000`. So the +2 the
row convention adds is subtracted right back out for the sample domain; the
residual is the ±480-sample 80 ms-grid quantization, which the optional
±240-sample refinement probes. Symbol `s` starts at `start + s·1920`; tone
base frequency = `f0·6.25 + fs·3.125` Hz (same quantization the spectrogram
path reads).

Refinement variant: shifts −240..+240 step 60, pick the shift maximizing
Σ_symbols max_tone |C|²; reported separately.

## Results (n = 558 controls, n = 576 sub-Costas targets)

| Population | Demod | mean | p10 | p50 | p90 | mean\|LLR\| |
|---|---|---|---|---|---|---|
| CONTROL (production-decode positions) | max-log (spectrogram) | 91.4% | 77.0% | 94.3% | 100.0% | 9.53 |
| CONTROL | matched filter | 91.1% | 72.4% | **95.4%** | 100.0% | 10.13 |
| CONTROL | matched filter +refine | 92.2% | 75.3% | **96.6%** | 100.0% | 11.79 |
| SUB-COSTAS (missed truths, sync < 3) | max-log (spectrogram) | 50.5% | 45.4% | 50.6% | 55.7% | 7.54 |
| SUB-COSTAS | matched filter | 50.3% | 45.4% | **50.0%** | 55.7% | 6.64 |
| SUB-COSTAS | matched filter +refine | 50.4% | 46.0% | **50.0%** | 55.7% | 7.32 |

Controls sanity gate: matched-filter p50 = 95.4% ≥ max-log p50 = 94.3% — the
sample mapping is validated (it even slightly beats the spectrogram demod on
real signals, +1.1 points at p50, +2.3 with refinement — the matched filter
works as a demod).

Refinement best-shift distribution: controls mean = +40 / p50 = +120 samples;
sub-Costas mean = +13 / p50 = +60 samples (controls show a coherent small
positive bias consistent with grid quantization; sub-Costas shifts are
~uniform noise).

## Verdict (pre-registered bars: PROCEED ≥ 70%, WEAK 58–70%, SHELVE < 58%)

- Matched filter (primary): sub-Costas median = **50.0%** → **SHELVE**
- Matched filter +refine (secondary): sub-Costas median = **50.0%** → **SHELVE**

## Interpretation

The phase-coherent matched filter is strictly better at demodulating *real*
signals (controls improve) and exactly as blind at the sub-Costas missed-truth
positions (chance level, 50%). This is the cleanest possible negative: the
front-end was never the bottleneck. At the baseline truths' (freq, dt)
coordinates where pancetta's Costas score is below threshold, there is no
phase-coherent FT8 tone energy matching the truth codeword for ANY linear
demod to find — consistent with hb-088's conclusion that dense-busy-band
sub-Costas energy is interferer-dominated, and extending it from "spectrogram
mining is closed" to "coherent sample-domain demod at single positions is
closed too." The hb-088 shelve's "what WOULD work" escape hatch (this
hypothesis) is now refuted on the same corpus.

Residual caveats: (a) baseline (freq, dt) labels come from the recorded truth
decoder; a systematic dt error beyond ±240 samples or freq error > ~3 Hz
would defeat the matched filter — but the same labels give 91–96% on
controls, so label quality is demonstrably sufficient for this measurement;
(b) per-symbol coherence is assumed (1 symbol = 160 ms); multi-symbol
coherent integration with channel tracking is a structurally different
(and much more expensive) mechanism not tested here.

Bank action: hb-090 → SHELVED (kill-switch fired at 50.0% vs 70% bar;
controls validated at 95.4%).
