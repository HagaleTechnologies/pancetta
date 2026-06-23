# hb-228 — JTDX 3-method spectral sweep: SHELVED-NULL (2026-06-18)

**Mechanism:** run the Costas sync search over three magnitude compressions of
the same FFT — power (`|X|²`, the default), linear (`|X|`), sqrt (`|X|^0.5`) —
and UNION the candidates, on the premise (from JTDX) that each compression
surfaces a different candidate population and widens recall.

**Implementation:** `Ft8Config::three_method_spectral_sweep_enabled` (default
OFF). `compute_spectrogram_with(audio, MagnitudeTransform)` parameterizes the
per-bin compression; on pass 0 the decode path builds the sqrt + linear maps,
runs `costas_sync_search_partner` on each, extends the power-map candidates,
dedups by `(time_step, freq_bin, freq_sub)` keeping the best sync score, then
restores the `max_sync_candidates` cap. Probe:
`pancetta-research/examples/hb228_three_method_sweep.rs`.

**Measurement (raw_530_full, ft8_lib truth, delta vs `Ft8Config::default()`):**

| N | baseline TP / dec | hb-228 TP / dec | Δ TP | Δ FP | wall |
|---|---|---|---|---|---|
| 50 | 893 / 1129 | 893 / 1129 | **+0** | **+0** | 15s → 17s |
| 200 | 3427 / 4213 | 3427 / 4213 | **+0** | **+0** | 62s → 67s |

Byte-identical decode counts at both tiers. The +2s/+5s wall increase confirms
the extra sqrt+linear FFT+sync passes ARE executing (this is a real null, not a
no-op-bug).

**Why it's null (the finding):** in pancetta's Costas sync formulation, the sync
peak LOCATIONS are invariant to magnitude compression. A Costas correlation peak
sits at the same `(time, freq)` whether the spectrogram stores `|X|²`, `|X|`, or
`|X|^0.5` — the compression rescales the *scores* but does not move *where* the
peaks are. So the union dedups straight back to the same candidate set as the
power map, and any non-coincident extra candidates that survive are decode-dead
(they yield no CRC-valid codeword the power map missed, hence +0 TP AND +0 FP).
JTDX's benefit presumably comes from a different sync/aggregation formulation;
it does not transfer here.

**Verdict: SHELVED-NULL on raw_530_full.** Same shape as hb-117 (decoder is
scale-invariant → gain ensembles add nothing) and hb-090 (the "what would work"
energy doesn't exist at the candidate coords). The gated flag + probe are kept
dormant (default-OFF, zero production impact) so the mechanism can be cheaply
re-tested on a different corpus (e.g. real-Doppler / storm tiers) where peak
locations might smear enough for compression to matter — but on the standard
corpus the premise does not hold.

**Bank:** hb-228 → SHELVED-NULL. Top remaining open: hb-225 (sub-bin Costas
grid — corroborated +33 TP via Batch-45 freq-dither, genuinely moves peak
*locations*, unlike compression), hb-243 (downsampler, the real ~1-2 dB WSJT-X
gap, plan-sized), hb-237 (cross-sequence A7).
