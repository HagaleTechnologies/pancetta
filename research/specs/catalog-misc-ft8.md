# Catalog: miscellaneous FT8 ecosystem projects

Brief catalog of FT8-adjacent forks and apps surveyed for potential
algorithmic borrowing. Each entry is a quick triage: what is it, what
license, are there novel mechanisms, is it worth a follow-on reader
agent?

- Reader date: 2026-06-08
- Reader: clean-room reader thread (this session)

## WSJT-Z (sq9fve)

- Home: https://github.com/sq9fve/wsjt-z
- License: GPL-3.0
- Language: C++ (with the Fortran kernel inherited from upstream WSJT-X)
- Identity: WSJT-X fork with extended automation; supports FT8, FT4,
  FT2, JT4/JT9/JT65, Q65, MSK144, WSPR, Echo, FreqCal.
- Stars: 30 at reader date.

**Notable mechanisms (algorithm-level)**:

1. **Multi-threaded FT8 decoder** (selectable Auto / 1–12 threads).
   The host fans out per-band-segment or per-candidate-batch decode
   work across worker threads, with early-decode dedup eliminating
   the duplicate decodes that arise when two threads happen to land
   on overlapping candidate sets. Pancetta already runs the decoder
   on its own tokio task; the meaningful idea here is the
   **early-decode dedup** — a hash-keyed set of "this payload has
   already been emitted this slot, drop the duplicate" that runs
   across whatever parallelism the host has. Worth a small spec.
2. **JTDX-derived OSD on `ndepth=2`** and **lowered sync thresholds
   for weak signals** — these are JTDX features that pancetta has
   already catalogued (the three-method magnitude sweep family in
   `spec-jtdx-*.md`). Same lineage, same mechanism, no new spec
   needed.
3. **Cached filter lists / reduced regex compilation / tuned Fortran
   release flags** — engineering wins, not algorithmic novelty.

**Worth a follow-on reader agent?** *Yes, narrowly* — one short spec
on the multi-threaded early-decode dedup pattern would round out
pancetta's catalogue. The dedup keying choice (payload-hash vs.
candidate-bin-hash) is the load-bearing detail. The automation
features (Auto CQ, Auto Call, Pounce, priority call queue, band-hopper
scheduling) are interesting for pancetta's autonomous operator layer
but not for the decoder; if the operator wants those mined, that is a
separate ask under `catalog-automation-patterns-*.md`.

## WSJT-CB (vash909)

- Home: https://github.com/vash909/WSJT-CB
- License: GPL-3.0
- Language: C++ (WSJT-X kernel)
- Identity: WSJT-X-Improved fork shaped for 27 MHz CB / 11 m operation.
- Stars: 5 at reader date.

**Notable mechanisms**:

- Explicit 11 m band table (26.965–27.405 MHz) — config asset, not
  algorithm.
- **Extended `Radio::is_callsign(...)`** so CB callsign shapes are
  accepted (e.g., `1A1`, `26AT101`, `999ZZ999`, `1AT1000`, `999ZZ/ZZ`).
  This is a callsign-validation regex / tokeniser change, not a
  decoder change.
- Removal of features not relevant to CB operations (UI surgery).
- Strengthened packaging / branding (build hygiene).

**Worth a follow-on reader agent?** *No*. The fork is a CB-localised
re-shell of WSJT-X-Improved. The callsign extensions are not
applicable to pancetta's amateur-radio scope, the decoder is upstream
WSJT-X's, and the small CB community is not in pancetta's mission
band.

## FT-Activ8 (bodiya, Brian Bodiya KC1WIH)

- Home: https://github.com/bodiya/FT-Activ8
- License: GPL-3.0
- Language: Kotlin (Android UI) + Rust (decoder core)
- Identity: FT4/FT8 operating software for Android, built on **wsjtr**
  (the Rust FT8 implementation pancetta also draws from).
- Stars: 5 at reader date.

**Notable mechanisms**:

The README and DESIGN.md are explicit that the project **does not
modify the wsjtr decoder**. The Rust core is consumed via JNI through
wsjtr's `ft8-engine` crate C FFI. The Android-specific value is in
session management, CAT control, GPS time sync, and battery-aware
scheduling — not in algorithm changes.

CHANGELOG.md confirms: the only algorithmic claim is "multi-pass
signal subtraction with configurable decode depth (1–3)" in v0.6.0,
which is upstream WSJT-X / wsjtr SIC, not a fork-specific
contribution.

**Worth a follow-on reader agent?** *No for decoder algorithms.*

There are arguably interesting *mobile-engineering* patterns:

- 15-second decode/transmit batch cycle with wake-lock + foreground
  notification.
- Audio ring-buffer with 15-second window extraction.
- TX audio pre-generation to allow clean playback handoff.
- Single-pass decoder mode for thermally-constrained operation
  (explicitly noted as a knob, not as dynamic thermal management —
  thermal-driven depth adjustment is listed as unimplemented).
- A session-based API for callsign-hash / decode-history / cross-
  window context, separated cleanly from the decoder.
- CI-V protocol implementation for Icom IC-7300 (CAT, frame
  encoding, timing).
- Auto-sequencer state machine that is pure logic with no Android
  dependencies.

These are useful patterns if pancetta ever does a mobile/embedded
target. Pancetta-hamlib + pancetta-qso already cover most of this on
desktop. No urgent algorithm-level follow-up.

## FT8 Decoder / FT8 Radio (Dhiru Kholia, kholia@gmail.com)

- Support repo: https://github.com/kholia/DigitalRadioReceiverSupport
- Play Store: `com.bunzee.digitalradioreceiver` and
  `com.bunzee.ft8radio`
- License: support repo is unspecified; binary apps include
  `ft8_lib` (kgoba, MIT) and FreeDV PSK Reporter (LGPL-2.1).
- Identity: Android FT8 decoder/transceiver apps built on
  `ft8_lib`.

**Notable mechanisms**: The decoder is upstream `ft8_lib` (kgoba) —
the same MIT-licensed reference implementation pancetta uses as a
pre-`pancetta-ft8` baseline. The README notes the app "does NOT
compute (WSJT-X) SNR presently, and it reports back `candidate
scores` instead" — i.e., the SNR estimator is simpler than WSJT-X's,
not improved. No fork-specific algorithm changes.

**Worth a follow-on reader agent?** *No.* It is downstream of the
same `ft8_lib` pancetta already exceeds (per the "123.7% of ft8_lib"
status note in pancetta's memory).

## FT8CN (N0BOY)

- Home: https://github.com/N0BOY/FT8CN
- License: GPL-3.0 (verify if pursued — published as a free Android
  app)
- Identity: Android app that turns phone/tablet into an FT8 station
  controller. Free, unrestricted-use.

**Notable mechanisms**: Not surveyed in depth. Reputed to use a
Java/Kotlin port of `ft8_lib`-style decoding. Likely no algorithmic
novelty vs upstream.

**Worth a follow-on reader agent?** *No, unless* the operator
specifically wants a Chinese-language ham UI reference. Decoder-wise
this is the same lineage as FT-Activ8 / FT8 Radio.

## FT8RX (Sascha Wittkowski)

- Listing: AppBrain `com.swi.ft8dx`, paid (~$3).
- License: closed-source.
- Identity: Android FT8 decoder.

**Worth a follow-on reader agent?** *No.* Closed source; can't be
read; can't be cleanroomed.

## Other GitHub `ft8` topic finds (no triage)

A topic search on `ft8` yields scattered projects. The ones not
surveyed here either (a) match an already-catalogued upstream
(wsjt-x, ft8_lib, ft8mon, JTDX) without claiming improvements, or
(b) are tooling / GUI / SDR-front-end work unrelated to the
decoder algorithm. None warrant a reader agent at this time.

## Summary table

| Project       | Maintainer        | License | Lang     | Decoder novel? | Follow-on? |
|---------------|-------------------|---------|----------|----------------|------------|
| WSJT-Z        | sq9fve            | GPL-3.0 | C++/F90  | Multi-thread + dedup | **YES (narrow)** |
| WSJT-CB       | vash909           | GPL-3.0 | C++/F90  | No (CB callsign tokeniser only) | No |
| FT-Activ8     | bodiya (KC1WIH)   | GPL-3.0 | Kotlin/Rust | No (uses wsjtr unchanged) | No |
| FT8 Decoder / FT8 Radio | Kholia  | mixed   | C/Java   | No (ft8_lib unchanged) | No |
| FT8CN         | N0BOY             | GPL-3.0 | Java/Kotlin | Unlikely | No |
| FT8RX         | Wittkowski        | closed  | unknown  | Unknown | No (closed) |

## Recommended follow-on (if any)

One narrow reader pass on **WSJT-Z's multi-threaded decoder + early-
decode dedup** would close out the FT8-host-fork survey cleanly.
That spec would document:

1. How WSJT-Z distributes per-band-segment / per-candidate-batch
   work to the thread pool.
2. What hash key is used for the early-decode dedup.
3. What state the dedup set lives on (per-slot, per-thread, or
   shared).
4. Any synchronisation choices worth noting.

Estimate: one reader session, ~120 LOC of resulting spec, then
~80 LOC of Rust port (pancetta already has multi-threading, so this
is mostly the dedup pattern).
