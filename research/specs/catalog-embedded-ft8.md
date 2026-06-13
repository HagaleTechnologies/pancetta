# Catalog: Embedded / microcontroller FT8 decoder implementations

Survey of open-source FT8 decoder implementations targeting embedded
hardware (Pi Pico, Teensy, ESP32, STM32). Compiled 2026-06-08 by
reader thread. The motivation: embedded ports often contain
**efficiency tricks** that aren't documented in the WSJT-X or ft8mon
mainlines because the embedded developers had to make them work in
~200 KB of RAM and a sub-200-MHz CPU. These tricks may be relevant
to pancetta's "scoped_fast_path" line (hb-091, hb-216) where the
goal is to keep up on slower hardware (M2 Air, future MiniPC
deployment).

## Summary table

| Project | URL | License | Base | Hardware | Notable tricks | Follow-on agent? |
|---|---|---|---|---|---|---|
| kgoba/ft8_lib | github.com/kgoba/ft8_lib | **MIT** | original | "fast microcontroller" generally; tested STM32F7 | core lib that all the others derive from; ~200 KB RAM target | Yes — baseline for comparison |
| aa1gd/pico_ft8_xcvr | github.com/aa1gd/pico_ft8_xcvr | **MIT** | ft8_lib (kgoba) | Raspberry Pi Pico (Cortex-M0+ dual-core, ~200 KB SRAM) | (1) incremental FFT — never store full 15s of samples; (2) dual-core split (RF control vs DSP); (3) overclock | Yes — strong candidate for tricks porting |
| kholia/pico_ft8_xcvr | github.com/kholia/pico_ft8_xcvr | **MIT** | aa1gd fork | Pi Pico | (1) LDPC iterations 20 → 10; (2) max msgs 50 → 14; (3) FFT oversampling 2 → 1; (4) sample rate 12 kHz → 6 kHz still decodes | Yes — quantifies what knobs are safe to dial down |
| Rotron/Pocket-FT8 | github.com/Rotron/Pocket-FT8 | unspecified ("no warranty") | ft8_lib | Teensy 3.6 + Si4735/Si5351 | (1) ADC at 6400 sps, processing at 3200 Hz; (2) 2048-pt FFT with 3.125 Hz bin spacing | Maybe — license is ambiguous |
| wcheng95/Mini-FT8 | github.com/wcheng95/Mini-FT8 | **MIT** | ft8_lib | M5 Cardputer (ESP32-S3) | UAC audio path; autoseq logic credited to N6HAN | Maybe — recent (2025), worth checking for new tricks |
| WB2CBA/W5BAA-FT8-POCKET-TERMINAL | (search hit) | unspecified | (likely ft8_lib) | Teensy + TFT touchscreen | not surveyed in depth | Skip — overlapping with Pocket-FT8 |
| pavel-demin/ft8d | github.com/pavel-demin/ft8d | **GPL-3.0** | WSJT-X Fortran | Red Pitaya (Zynq SoC) | Fortran→C CRC; hardcoded constants → parameters; complex samples at 4 ksps input | Skip — GPL contamination risk; not a structural improvement |
| kholia/Pico-FT8-TX | github.com/kholia/Pico-FT8-TX | (likely MIT, fork lineage) | ft8_lib | Pi Pico 2 W | TX-only beacon; NTP time sync | Skip — TX-only |
| etherkit/JTEncode | github.com/etherkit/JTEncode | (likely GPL, Arduino-style) | independent | Arduino class | TX/encode only | Skip — not decode |
| e04/ft8js | github.com/e04/ft8js | (matches upstream) | ft8_lib via WASM | Browsers | Useful for parity-check / corpus prep but not a hardware port | Skip — not embedded |
| G1OJS/MiniPyFT8 | github.com/G1OJS/MiniPyFT8 | **GPL-3.0** | independent Python | desktop Python | 300-LOC pure-Python FT8 decoder | Maybe — "what's the minimum that works" reference, even if GPL means clean-room |

## Detailed notes on the high-value entries

### kgoba/ft8_lib (MIT, baseline)

Karlis Goba's library is the de-facto MIT-licensed FT8 reference for
embedded use. Pancetta should treat it as a **license-compatible
reference**: paraphrase encouraged (per clean-room feedback for
MIT/BSD/Apache sources). The decoder runs in ~200 KB RAM and has
been demonstrated on STM32F7. Pancetta's decoder is generally
considered more capable (per the MEMORY note that pancetta is at
"123.7% of ft8_lib"), so kgoba is less interesting as an algorithm
source and more interesting as **the upstream that the embedded
ports derive from**.

### aa1gd/pico_ft8_xcvr (MIT) — strong candidate for tricks

Real-time FT8 decode on a 4 USD Pi Pico (Cortex-M0+, ~200 KB SRAM,
~150 mA). The README explicitly calls out the engineering tricks:

1. **Incremental FFT**: "It is not possible to store a 15 second
   duration of samples, and thus samples are collected in ~1 second
   intervals, processed with the FFT, and only the FFT data is
   stored." This is the classic streaming-spectrogram pattern.
   Pancetta already does this in `pancetta-dsp` — sanity-check the
   memory profile against aa1gd's numbers.
2. **Dual-core split**: one core handles RF/transceiver control,
   the other DSP. Pancetta is multi-threaded on desktop but the
   coordinator-side split (decode vs. TX scheduling vs. CAT
   control) is a related design question.
3. **Overclock**: aa1gd notes the Pico cannot keep up at stock
   clock; overclocking is needed. This is a **canary**: if a Pico
   (200 MHz overclocked Cortex-M0+) can run real-time FT8, the
   absolute floor for FT8 decode is well below desktop-class
   compute. Pancetta's hb-091 scoped-fast-path line should
   compare against this floor to validate its tier thresholds.

**Follow-on agent: YES.** Worth dispatching a clean-room implementer
to read the aa1gd README + relevant sources and produce a Rust spec
of the streaming-FFT memory layout. Since MIT-licensed, code
structure may be referenced.

### kholia/pico_ft8_xcvr (MIT) — quantified knob audit

Fork of aa1gd that explicitly documents which decoder knobs can be
dialled down. From the README:

- LDPC iterations: 20 → 10 (still decodes)
- Max decoded messages: 50 → 14
- FFT oversampling: 2 → 1 (both frequency and time)
- Sample rate: 12 kHz → 6 kHz works

These are **measured graceful-degradation points** for ft8_lib on
Pico hardware. Pancetta's `Ft8Config` has analogous knobs
(`max_decode_passes`, `osd_depth`, FFT bin count). The hb-216 Slow-
tier preset (`max_decode_passes = 1`, `osd_depth = Some(1)`) is
already in this spirit. The kholia data is a useful **independent
validation** that aggressive knob settings still recover real
signals — though kholia targets ~150 mA Pico hardware, not desktop.

**Follow-on agent: YES.** A spec capturing kholia's measured
degradation curves (decode-rate as a function of each knob) is
worth producing. Pancetta's tier-thresholds line could be informed
by this.

### Rotron/Pocket-FT8 (license ambiguous) — DSP rate trick

Targets Teensy 3.6 (ARM Cortex-M4F, 180 MHz). Key trick from the
README: ADC at 6400 sps, processing at 3200 Hz, 2048-point FFT
yielding 3.125 Hz bin spacing. This is a 4× downsampling vs. the
standard 12000 sps FT8 convention.

The bin spacing of 3.125 Hz is **half** of FT8's symbol-rate of
6.25 Hz, so each FT8 tone covers two FFT bins — a valid trade
between bin spacing and FFT cost. Pancetta currently uses a
finer bin spacing (per `pancetta-dsp`); a coarser bin variant
could be a scoped-fast-path mechanism for Slow-tier hardware.

**Follow-on agent: MAYBE.** License ambiguity ("distributed in
the hope that it will be useful, but WITHOUT ANY WARRANTY"
without a named license) means even MIT-style use is risky. A
clean-room read of the rate-trick description is fine; reading
the source is not.

### wcheng95/Mini-FT8 (MIT) — recent ESP32 port

ESP32-S3 (M5 Cardputer) port of ft8_lib. README does NOT call out
efficiency tricks explicitly but credits N6HAN for "audio/DSP path
(UAC) and autoseq". The ESP32-S3 is significantly more capable
than the Pi Pico (240 MHz dual Xtensa LX7 + AI accelerator) so
optimisations may be less aggressive.

**Follow-on agent: MAYBE.** Recent (2025), MIT-licensed. Worth a
shallow look. The "autoseq" logic credited to N6HAN may be an
auto-sequencer comparable to pancetta's `auto_sequencer` —
cross-reference of state machine implementation could be useful.

### G1OJS/MiniPyFT8 (GPL-3.0) — minimalist Python reference

300-LOC pure-Python FT8 decoder with annotated waterfall and LDPC.
GPL-3.0 so clean-room only, but the **minimalism is the value**:
this is the smallest known correct FT8 decoder. Useful as a
"what's the irreducible algorithm" reference for explaining FT8
to people who haven't worked on it, and as a sanity-check on
decoder pipeline shape.

**Follow-on agent: MAYBE.** GPL means prose-only summary; the
value is illustrative not structural.

### pavel-demin/ft8d (GPL-3.0) — Red Pitaya port

Minimal subset of WSJT-X's Fortran ft8 decoder, repackaged for the
Red Pitaya SoC. GPL-3.0. The README notes Fortran→C CRC conversion,
hardcoded → parameter refactor, and complex-sample input at 4 ksps.

**Follow-on agent: NO.** GPL contamination risk and no structural
improvements over what's already known from WSJT-X / wsjtr.

## Top-level recommendations

1. **Dispatch a follow-on clean-room agent** for aa1gd/pico_ft8_xcvr
   and kholia/pico_ft8_xcvr (both MIT). Produce a spec capturing:
   - The streaming-FFT memory layout (sample collection in chunks,
     FFT-only storage).
   - The knob-degradation curves (LDPC iterations, FFT
     oversampling, sample rate) and their effect on decode rate.
   These directly inform pancetta's scoped-fast-path / tier line.
2. **Periodic re-survey** (6-month cadence) of the embedded FT8
   landscape; this space is evolving (the kholia and wcheng95
   ports are both 2024-2025).
3. **DO NOT** port from pavel-demin/ft8d or G1OJS/MiniPyFT8 (both
   GPL). Use as illustrative references only if at all.
4. **The Rotron/Pocket-FT8 DSP rate idea** (3.125 Hz bins via 3200
   Hz processing) is worth its own hypothesis-bank entry as a
   potential scoped-fast-path mechanism — even though the source
   license is ambiguous, the **idea** is described in prose in
   the README and is a fact, not expression. Implementer can build
   it from the description without reading source.

## Open questions for follow-on agents

- What's pancetta's current per-WAV decode wall-clock budget vs.
  kholia's measured Pico timings? If pancetta on a Slow-tier
  desktop is within 2× of a Pico decoder, the kholia degradation
  knobs are immediately useful. If pancetta is 100× faster, they're
  not (we're not in the "real-time on tiny hardware" regime).
- Is there an FPGA reference decoder (the pavel-demin work seems
  Zynq-class)? Not surveyed here; potentially relevant for the
  next embedded line.
