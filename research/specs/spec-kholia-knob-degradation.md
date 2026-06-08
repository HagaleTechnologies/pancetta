# spec-kholia-knob-degradation

Provenance: kholia/pico_ft8_xcvr (MIT-licensed; fork of aa1gd/pico_ft8_xcvr).
Prose-only extraction per clean-room discipline. All numbers are
parameter values copied as facts; this document does not reproduce
code.

Repo: real-time FT8 decoder running on a Raspberry Pi Pico (RP2040,
dual Cortex-M0+, 264 KiB SRAM, no FPU, no L1/L2 cache). The author
explicitly tightens four upstream constants from Karlis Goba's
ft8_lib to fit the Pico's memory and per-slot compute budget, and
documents the A/B effects in the README. This is the cleanest source
we have found that pairs a quantified degradation set with its measured
recall impact on the same WAV file.

## 1. The four downsized knobs (versus upstream ft8_lib)

The decoder configuration header (`ft8/decode_ft8.h`) holds five
named constants that the author labels with "Original was X" comments.
Read together they form a coherent compute-budget package:

* `kLDPC_iterations`: pico value **10**, upstream value **20**.
  This is the cap on belief-propagation sweeps inside the sum-product
  LDPC decoder. Each iteration scales linearly with the parity-check
  matrix (FTX_LDPC_M = 83 rows times FTX_LDPC_N = 174 columns of float
  messages plus the same matrix of extrinsic LLRs); halving it halves
  the inner-loop work. Mechanism: the LDPC routine still exits early
  on a clean parity check (`errors == 0`), so the cut only matters on
  marginal candidates that need extra sweeps to converge.

* `kFreq_osr`: pico value **1**, upstream value **2**. Frequency
  oversampling ratio in the waterfall. At 2 the FFT runs at twice the
  symbol-rate bin resolution and the waterfall stores two phase-offset
  bin grids per slot. At 1 the FFT length and waterfall page both shrink
  by 2x. Mechanism: a 2x frequency-osr matters for candidates whose
  carrier sits between two symbol-rate bins; collapsing to 1 forces
  the sync search onto the coarse grid.

* `kTime_osr`: pico value **1**, upstream value **2**. Time
  oversampling ratio. At 2 the waterfall has two staggered symbol
  start-times per slot; at 1 only one. The same shrink factor (2x)
  applies to waterfall storage. Mechanism: a 2x time-osr lets the
  sync search find candidates whose DT clock-offset puts the FT8
  Costas array half a symbol off the nominal grid.

* `kMax_candidates`: pico value **30**, upstream value **120**. Cap
  on candidates returned by `ft8_find_sync`. At 120 the decoder
  attempts LDPC on the 120 highest-sync-score windows in the slot; at
  30, only the top 30. Mechanism: low-score true signals below rank
  30 are silently dropped before they reach LDPC.

* (Listed for completeness, not a recall knob.) `kMax_decoded_messages`:
  pico value **14**, upstream value **50**. The author explicitly
  notes this is sized to match a 4x4 membrane keyboard, not to save
  compute. The decoder will exit early once 14 messages are decoded
  even if more candidates remain in the list.

A separate sample-rate knob lives outside this header:

* `sample_rate_`: pico value **6000 Hz**, library default **12000 Hz**.
  Set in `decode_ft8.h` and used by `rx_ft8.cpp` to program the ADC
  clock divider (`adc_set_clkdiv(48000000 / sample_rate_)`). At 6 kHz
  the FFT length `nfft = block_size * kFreq_osr` halves again (block
  size = sample_rate / 6.25 Hz symbol rate), and the waterfall
  per-block byte count halves with it.

## 2. Author's documented A/B measurements

The README contains three back-to-back runs on the same 15-second WAV
file (`tests/191111_110700.wav`), with reference WSJT-X-style output
showing how many messages each configuration recovered.

* **Run A — upstream-style knobs, 12 kHz sample rate.** Binary:
  `./decode_ft8`. Sample rate 12000 Hz, block size 1920, subblock
  size 960, N_FFT 3840. Result: **14 decoded messages**.

* **Run B — upstream-style knobs, 6 kHz sample rate** (WAV resampled
  with `sox -r 6000`). Binary: `./decode_ft8`. Sample rate 6000 Hz,
  block size 960, subblock size 480, N_FFT 1920. Result: **14 decoded
  messages**, the same 14 callsigns and (almost) the same SNR/DT/freq
  triples as Run A.

* **Run C — pico-tightened knobs** (`kLDPC_iterations=10`,
  `kFreq_osr=1`, `kTime_osr=1`, `kMax_decoded_messages=14`), 12 kHz
  sample rate. Binary: `./pc`. Sample rate 12000 Hz, block size 1920,
  subblock size 1920, N_FFT 1920. Result: **8 decoded messages**.

The author phrases Run C as the "with following settings" run and
explicitly contrasts it with the "with upstream values" header on
Run B.

Reading the three rows as a small ablation table:

* **Sample rate 12 kHz → 6 kHz at upstream osr (Run A vs Run B):**
  zero recall loss on this WAV. 14 → 14. The author treats this
  silently; the implication is that for slot-rate FT8 (symbol rate
  6.25 Hz, max usable bandwidth roughly 3 kHz), 6 kHz is already
  past Nyquist for the FT8 passband and so loses essentially nothing.
  This is the cleanest result in the README: **6 kHz vs 12 kHz at
  matched osr is free.**

* **Upstream osr → osr=1 at 12 kHz (Run B baseline → Run C):** 14 →
  **8**. A roughly 43% recall drop on a single WAV. The author does
  not factor the loss between the three knobs that change between
  Run B and Run C (`kLDPC_iterations`, `kFreq_osr`, `kTime_osr`, and
  `kMax_decoded_messages`). The largest mechanical change is osr=2
  → osr=1 in both axes; halving frequency-osr and time-osr together
  shrinks the sync search space by 4x and forces the search onto a
  coarser grid in both dimensions. The author's tone is matter-of-
  fact about the cost: "with following settings" introduces the
  tightened block, and the lower decode count immediately follows
  without apology.

The author's commentary on the strategic order is not spelled out
prose, but the **physical layout** of the knob block tells a clear
story. The four knobs are grouped together at the top of
`decode_ft8.h` with "Original was X" inline comments on the three
expensive ones; `kMax_decoded_messages` is annotated as the keyboard
limit. The author keeps the LDPC iteration count at half of upstream
(10, not less), keeps the candidate cap at a quarter of upstream
(30, not less) and only zeroes the oversampling (osr=1 in both axes).
The pattern reads as: **shave linearly where the cost is linear (LDPC
iterations, candidate count) and only collapse the oversampling axes
to 1 when the platform forces the issue, because osr=1 is the cliff.**

## 3. Smooth-vs-cliff classification (inferred from the three runs)

The README does not present a sweep, only the three endpoints. Pairing
the numbers with the inner-loop math gives a defensible split:

* **Smooth knobs (linear cost, gradual recall loss):**
  - `kLDPC_iterations` 20 → 10. The LDPC loop's early-exit on a
    clean parity check means the cap mainly matters on weak signals;
    halving the cap is a soft cut.
  - `kMax_candidates` 120 → 30. Reduces the tail of low-score
    candidates that occasionally succeed in LDPC.

* **Cliff knobs (lose information the rest of the pipeline cannot
  recover):**
  - `kFreq_osr` 2 → 1. Drops the sub-bin frequency search; carriers
    between symbol-rate bins now compete with neighbours at full
    coarse resolution.
  - `kTime_osr` 2 → 1. Drops the half-symbol DT search; signals with
    non-zero clock offset cannot land on a finer grid.
  - `sample_rate_` 12 kHz → 6 kHz. **In this corpus, not a cliff**
    — the 14 → 14 result on Run B is the empirical evidence. A
    further drop (6 kHz → 3 kHz) would clip the FT8 passband and
    become a hard cliff, but kholia does not test that.

The 14 → 8 jump between Run B and Run C therefore lives almost
entirely in the osr knobs, with a smaller residual contribution
from the LDPC and candidate caps. The README does not isolate those
contributions, so this attribution is inference, not measurement.

## 4. Cross-reference with pancetta's hb-216 Slow-tier preset

Pancetta's Slow-tier configuration today is `max_decode_passes = 1`,
`osd_depth = Some(1)`. These are pancetta-specific knobs (multi-pass
decode with subtract-and-redecode, plus OSD post-processor depth);
they do not have direct equivalents in kholia's port, which uses a
single-pass ft8_lib decoder with no OSD. Conversely, kholia's four
knobs live in three buckets relative to pancetta:

* **LDPC iterations.** Pancetta's LDPC iteration cap (looking at
  `pancetta-ft8`) is the closest analogue to `kLDPC_iterations`.
  Kholia's data supports halving it on Slow tier as a smooth-cost
  knob with bounded recall impact. The mechanism is identical: a
  sum-product belief-propagation decoder that early-exits on a clean
  parity check.

* **Frequency/time oversampling.** Pancetta runs at WSJT-X-equivalent
  osr in the production decoder. Kholia's data flags osr=1 as the
  cliff. **Recommendation: do not drop pancetta's osr on Slow tier.**
  The cost saving is real (FFT length halves, waterfall page
  quarters), but Run C's 14 → 8 result puts the recall cost in the
  same order of magnitude as just turning the decoder off for that
  slot. If a future tier needed even more headroom, osr should be
  the last knob to touch.

* **Sample rate.** Kholia's Run A vs Run B result (6 kHz vs 12 kHz
  at matched osr, 14 → 14) is the genuinely novel finding for the
  hb-216 line. Pancetta runs at higher sample rates upstream of the
  decoder; if the FT8 waterfall stage is bottlenecked on FFT
  throughput on Slow-tier hardware, **downsampling the decoder's
  input chain to 6 kHz before the FFT is the most-defensible knob
  to add to the Slow-tier preset.** It costs nothing in kholia's
  corpus, halves FFT length, and is fully reversible (it is a sample
  rate constant, not a decoder algorithm change). The catch: kholia
  measures one WAV. A pancetta-side validation on the iter corpus
  would be needed before shipping. The hypothesis is strong but the
  evidence base is one data point.

* **Candidate cap.** Kholia's `kMax_candidates = 30` (vs upstream
  120) is much more aggressive than anything pancetta currently
  uses. Pancetta's candidate processing is structured differently
  (multi-pass with subtract), so a direct port is not meaningful.
  But the principle — cap the tail of low-sync-score candidates on
  Slow tier — could be a usable Slow-tier knob if pancetta's
  candidate list is currently uncapped or has a generous cap.

The headline addition for hb-216 Slow tier is therefore: **drop the
decoder-stage sample rate to 6 kHz** as a candidate knob alongside
the existing `max_decode_passes = 1` and `osd_depth = Some(1)`. The
LDPC iteration cap is a secondary candidate. The osr knobs are not
recommended.

## 5. Other novel mechanisms in kholia's port

Three implementation choices stand out as embedded-platform ideas
that could inform pancetta's tier line even though pancetta is not
memory-constrained:

* **Incremental FFT memory layout (`inc_extract_power` /
  `inc_collect_power`).** Kholia processes the slot one FFT block at
  a time as samples arrive from the ADC, and writes each block
  directly into a pre-allocated `mag_power` array sized
  `num_blocks * kFreq_osr * kTime_osr * num_bins` bytes (uint8_t,
  scaled to 0.5 dB resolution covering -120..0 dB). There is no
  intermediate float waterfall; the whole spectrogram lives in 8-bit
  log-power form, scaled as `(int)(2 * db + 240)` with clamp to
  0..255. The implication for pancetta: **the entire decode-pass
  waterfall can be stored as uint8_t at 0.5 dB resolution without
  measurable recall loss** (Run A vs Run B is in this same
  representation). On Slow tier this saves 4x memory bandwidth on
  the waterfall pages and could speed up cache-bound decoders on
  laptops with small L2.

* **Dual-core split: ADC capture on core 0, FFT on core 1, via the
  hardware FIFO.** Core 0 runs `collect_adc` (DMA capture) and
  pushes a block-index token into the multicore FIFO; core 1's IRQ
  handler pops the token and runs `inc_extract_power` on the freshly
  captured buffer. The author's hard real-time constraint is
  documented in a comment: **"handler MUST BE under 160 ms"** —
  this is the per-block budget at the FT8 symbol rate of 6.25 Hz
  (one symbol = 160 ms). The pattern is a producer/consumer pipeline
  with a single-element queue, where the producer is bound to the
  ADC's DMA completion rate and the consumer must keep up
  block-for-block. Pancetta already does roughly the equivalent at
  the audio thread / decoder thread boundary; the explicit 160 ms
  per-symbol budget is the useful number to take away. **If Slow
  tier's decoder cannot meet 160 ms per symbol on a single core, the
  tier classifier should detect that and drop osr or sample rate to
  recover the budget**, with the trade-offs documented in section 3.

* **Overclock-and-vreg as a recall lever.** The author runs the
  Pico at 290.4 MHz against a stock 133 MHz (`set_sys_clock_khz(290400, true)`)
  with the core voltage explicitly raised to 1.30 V (`vreg_set_voltage(VREG_VOLTAGE_1_30)`).
  This is the inverse of the knob story: on the Pico, instead of
  degrading the decoder, the author overclocks the silicon by 2.2x.
  The relevance to pancetta is meta: **the tier classifier should
  probe at the configured clock, not at idle.** A laptop in power-
  save mode that probes Fast and then enters power-save during
  decode will end up running the Fast preset on Slow-tier wall-clock
  compute. The mirror of this is to detect frequency scaling and
  rerun the probe.

* **Hann window over 1.8x block_size (`make_window`).** The author
  uses a Hann window of length `1.8 * block_size` zero-padded to
  `nfft`, calling it "hand-picked and optimized" in an inline
  comment. The upstream `monitor_init` path uses a full-length
  Hann over `nfft`. This is not a degradation knob — it is a
  window-shape choice — but it is worth noting that a non-default
  window length is in the production firmware. No measured A/B is
  provided; the choice predates the kholia fork.

## 6. Bottom-line for hb-216 / pancetta tier line

* **Strongest takeaway:** 6 kHz vs 12 kHz at matched osr loses zero
  recall on kholia's WAV. This is the only piece of evidence in the
  source for a free knob. Worth adding to the Slow-tier preset as a
  candidate, with pancetta-side validation on the iter corpus.

* **Medium takeaway:** LDPC iteration cap halved (20 → 10) is the
  paradigmatic smooth-cost knob; bounded recall impact on marginal
  signals only, because the LDPC loop early-exits on clean parity.
  Candidate for a future Slow-tier or new Moderate-Slow intermediate
  tier.

* **Anti-takeaway:** osr=1 is the cliff. Run C's 14 → 8 result is the
  evidence. Pancetta's Slow-tier preset should not include an osr
  cut.

* **Cross-validation gap:** the kholia evidence base is one 15-second
  WAV file. The corpus-truth quality and SNR distribution are
  unknown. Before shipping anything tier-level, pancetta should
  re-measure on the iter corpus.

* **Free-ish ideas worth porting independent of the tier line:**
  uint8_t/0.5 dB waterfall representation; explicit 160 ms-per-symbol
  budget as the Slow-tier health gate; tier probe under realistic
  power-management state, not idle.
