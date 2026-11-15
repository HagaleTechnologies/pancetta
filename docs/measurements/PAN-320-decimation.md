# PAN-320 decimation A/B — qualification record

Status: **pending station captures**. The comparison harness, filter and
CPU budget all exist and are tested; the station recordings that decide
the default have not been supplied yet, so the shipped default remains
the original subsampler. This record is the implementation checkpoint,
not a completed station qualification.

Companion JSON: `docs/measurements/PAN-320-decimation.json` (schema v1,
`status: pending_station_captures`, measured totals `null`,
`selected_default: subsample`).

## What is missing

Tony's external acquisition step (sophon station; the raw48 recorder is
PAN-393): one capture per band condition — quiet, busy, and a TX-desense
slot — as 48 kHz PCM16 WAVs of exactly 720000 frames (15 s), plus a v1
manifest carrying each file's SHA-256, condition and lead seconds
(0.5 or 1.0). A `_tx` filename suffix is provenance, not proof of desense;
the condition label in the manifest is authoritative. Until those files
arrive, no measurement recorded here may select the shipped default, and
synthetic fixtures never count toward station totals.

Once present, run:

```sh
cargo run --release -p pancetta-research --bin decimation-ab \
  -- --manifest /data/pan320/captures.json --output docs/measurements/PAN-320-decimation.json
```

and commit the regenerated JSON plus this page. The shipped default flips
to the FIR **iff** its total merged decode count is strictly greater; a
tie keeps subsample regardless of SNR.

## Settings (fixed)

| Item | Value |
|---|---|
| Input | channel 0 of each raw48 capture, i16 / 32768, never averaged, never resampled |
| Subsample arm | production loop: counter starts 0, retains indices 3, 7, 11, … |
| FIR arm | 31-tap Kaiser low-pass (β ≈ 3.9754, cutoff 6 kHz midpoint), unity DC gain, zero-initialized history |
| FIR group delay | 15 input samples (0.3125 ms), compensated in reported per-message timing and the window anchor |
| Decoder | fresh `Ft8Decoder` per arm per capture, `Ft8Config::default()`, no cross-file state |
| Decode prefix | lead + 13 s (13.5 s at lead 0.5, 14 s at lead 1.0) |

Measured response of the shipped taps (10 Hz grid, direct DTFT): DC gain
1.0 within 1e-6; passband 0–4 kHz within ±0.082 dB; stopband 8–24 kHz at
or below −39.4 dB. Pinned by `pancetta-dsp` tests
(`tests/decimation.rs::fir31_response_and_unity_gain`), so the historical
mis-specified-FIR failure mode (86 vs 1 decodes) cannot recur silently.

## Metrics (once captures exist)

| Metric | Definition |
|---|---|
| merged_decodes | Unique decoded message text per capture per arm after the decoder's merge, highest-SNR kept per duplicate text; summed across captures, never deduplicated across slots |
| SNR distribution | Finite `snr_db` of those unique decodes: n, min, p10, median, p90, max (nearest-rank); nonfinite values counted separately, excluded from quantiles only |
| paired SNR delta | FIR minus subsample `snr_db` for texts present in both arms of the same capture; FIR-only and subsample-only messages reported separately (survivor-bias guard) |
| decode delta | FIR total merged_decodes minus subsample total; the *only* default-selecting metric |

Counts are detection comparisons, not ground-truth recall claims.

## CPU cost (measured locally, not CI evidence)

Criterion bench `decimation_48k_12k/fir31_15s` (720000 input samples in
512-frame chunks, 180000 outputs): **mean 1.13 ms** per 15 s window on
this container (Ryzen 9 5950X, plain release codegen) against the 5 ms
budget — AC7's scenario, with CI enforcement configured beside the PAN-284
DXCC budget. Final CI evidence comes from the actual CI run.

## Consequences of enabling the filter

Filtered levels change the waterfall appearance, the RMS meter and the
12 kHz decode-window recordings, which all consume the decimated buffer;
thresholds are unchanged. The PAN-393 raw48 capture is fed pre-filter and
is unaffected by design.
