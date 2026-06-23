# Catalog: MSHV (Hrisimir Hristov, LZ2HV)

## Summary

MSHV is a WSJT-X derivative written in C++/Qt by Hrisimir ("Christo") Hristov,
LZ2HV. Author's distinctive claim to fame in the FT community is the
**Multi-Answering Auto-Sequencing protocols** ("MA DXpedition" and "MA Standard"),
which let one station run multiple concurrent QSOs in a single 15-second slot
on a single radio — the same idea pancetta is pursuing under the name
"multi-stream TX." MSHV is the most widely deployed implementation of that
pattern outside the WSJT-X "Fox/Hound" mode and is the de-facto European
contest / DXpedition tool for FT8.

The codebase only implements the modes K1JT modes already cover (FSK441,
FT8, FT4, JT65, JT9-via-Q65, JTMS, MSK144, MSK40, Q65, PI4, ISCAT, JT6M, and
its own experimental FT2). It explicitly carries the upstream WSJT-X
copyright notice and acknowledges the algorithms are originally K1JT's. The
distinguishing surface area is therefore at the **integration / sequencing /
contest** layer, not in low-level demodulation.

## License status

- **License**: GNU GPL v3.0 (the COPYING.txt and license.txt are the GPLv3
  text; each source file carries the "May be used under the terms of the GNU
  General Public License (GPL)" header).
- **Source available**: yes, at `https://github.com/LZ2HV/MSHV` (primary) and
  SourceForge mirror. C++ with Qt 5.6.3 (per README, statically linked
  libraries embedded in the binary).
- **Contamination risk for pancetta (MIT/Apache-2.0)**: high if copied
  literally. Standard clean-room rules apply: algorithms and parameter
  values are facts and may be extracted into prose specs; specific code
  expression may not be ported. The version-2.76 README explicitly notes
  that for one experimental round of new functions, "source code of this
  version will not be present" because the upstream feature wasn't yet
  finalised in WSJT-X — i.e. LZ2HV himself respects the boundary between
  shared algorithms and pre-release expressions of them.

## What MSHV does that mainline WSJT-X does NOT

Drawn from reading `README.txt` (which contains the full version history
back to 2.27) plus headers, decoder dispatch, and the multi-answer widget
implementation. References below are file paths only (clean-room: no code
quoted, paraphrased line-by-line, or directly translated).

### 1. Six-thread band-split FT8/FT4/FT2 decoder (the headline performance feature)

- One `DecoderFt8` (and parallel `DecoderFt4`, `DecoderFt2`) instance is
  preallocated per worker slot, with `decid = 0..5`. A user-facing
  "Threads" control sets `s_thr_used` between 1 and 6.
- On each new audio period the static input buffer is **copied** (verbatim
  duplicate of the 184k-sample int array — `static_dat1..static_dat5`) and
  one POSIX `pthread_t` is spawned per slot. Each worker decodes its own
  sub-band of the 200-3200 Hz audio range and writes results back over Qt
  signals that the orchestrator de-duplicates.
- The audio split is uniform with two soft adjustments:
  - **`limit` floor on the per-thread bandwidth**: if `band / nthr <
    300 Hz` (FT8) the worker count is auto-decremented. FT4 floor is
    `400 Hz`; FT2 floor is `500 Hz`. This means "6 threads" only actually
    becomes 6 threads once the user has opened the band wide enough.
  - **`corf` boundary nudge around the QSO RX frequency**: when a planned
    sub-band edge falls within `±11 Hz` of the user's RX frequency the
    edge is shifted by `corf = 25 Hz` (FT8) / `50 Hz` (FT4) / `100 Hz`
    (FT2) so a wanted signal never sits exactly on a worker boundary.
- File reference: `src/HvDecoderMs/decoderms.cpp` (the `StrtDec0..StrtDec5`
  workers, `ThrDec0..ThrDec5` POSIX thread trampolines, and the dispatch
  body at `SetDecode`/`thrsum` block); `src/HvDecoderMs/decoderms.h` (the
  `DecoderFt8 *DecFt8_0..5` member fan-out and `pthread_t th0..th5`
  declarations).
- **Why pancetta cares**: this is the closest analogue in the ham-radio
  open-source world to pancetta's planned "decoder workers per band slice"
  setup. Pancetta currently runs a single decode pass; MSHV demonstrates a
  shipped, validated design where the worker count is auto-trimmed by
  bandwidth and worker boundaries respect the user's RX frequency.
  Spec: `spec-mshv-band-split-multi-decoder.md`.

### 2. Multi-Answering Auto-Sequencing ("multi-stream TX") — Standard and DXpedition modes

- Two distinct modes:
  - **MA DXpedition**: TX only ever on the user's parity (even or odd
    period). Up to `MAXSL = 6` simultaneous answer slots (5 since 2.10,
    raised to 6 in 2.71). For 2.76's "Super Fox" extension, slot count
    can reach 9: five `RR73` plus four "Reports" plus one CQ on a free
    slot.
  - **MA Standard**: TX on either parity, with the caveat that if TX in
    second period the limit is "only one slot" — i.e. duplex slots are
    only allowed for the first-period side of the QSO. This is a
    deliberate concession to the asymmetric collision pattern between two
    operators on the same band.
- Two data structures drive the sequencer:
  - **"Queue" list (`LsQueue`)**: callers detected but not yet engaged.
    Default queue length is 5, configurable 0-50. Auto-sort is selectable
    `Off / Distance / S/N (dB)` — the "Distance" mode uses the
    `HvQthLoc` Maidenhead-to-km calculator on the operator's grid + the
    detected caller's grid.
  - **"Now" list (`LsNow`)**: callers actively in a slot. The TX message
    generator walks this list and emits one slot per row per slot
    period.
- TX-message-id encoding inside the slot is a small enum:
  `0 = "click to someone else"`, `1 = "+rpt"`, `2 = "R+rpt"`,
  `3 = "RR73"`, `4 = "CQ %M %G4"`. The state machine in `DecodeMacros`
  picks the right tx_id per row given the QSO progress of that
  correspondent.
- **`Free text msg` cap**: if a free-text macro is in flight, the
  generator constrains the slot fan-out to "max 4 correspondents +
  1 CQ-on-free-slot = 6 total", and disables the free-text menu if
  more than 4 are active.
- **CQ-on-free-slot**: a per-mode boolean (`cb_tx_cq_on_free_slot`); when
  enabled, MSHV emits a CQ on any otherwise-unused slot rather than
  staying silent.
- **"Special MSG"** combined-RR73-and-report message
  (`A1AB RR73; B2CD <MYCALL> +05`) merges two state transitions
  (closing-one-QSO-while-acknowledging-a-new-caller) into a single
  message — a real efficiency win when running with many callers, since
  it removes one TX cycle from the average sequence length.
- **CQ-type selector** includes activity tags `CQ MDX, CQ DX, CQ UP, CQ
  IOTA, CQ POTA, CQ SOTA, CQ BOTA, CQ WWFF`, regional `CQ AF/AN/AS/EU/NA/
  OC/SA/JA`, plus `CQ QRG / CQ END / TIME / Free Msg`. Contest
  acknowledgement (`SOTA, POTA, BOTA, IOTA`) was added in 2.71.
- File reference: `src/HvTxW/hvmultianswermodw.h` (`MAXSL`, `MultiAnswerModW`,
  `ListA`, `HvSpinBoxSlots`, `HvSpinBoxMTP` schema) and
  `src/HvTxW/hvmultianswermodw.cpp` (the `DecodeMacros`, `MakeSMsg`,
  `gen_msg`, `RefreshLists`, `SetAutoSort` bodies).
- **Why pancetta cares**: pancetta's `SmartFrequencyAllocator` and
  multi-stream TX (`pancetta/src/coordinator/tx.rs`) are the natural
  recipient of the queue / now / slot decomposition. MSHV is the
  canonical example of "TX-side slot scheduling that interleaves
  greetings, reports, R-reports and RR73 across N callers" and is worth a
  prose spec on its own.
  Spec: `spec-mshv-multi-answer-sequencer.md`.

### 3. Per-slot A Priori (AP) decoding wired through the sequencer

- The FT8 decoder's `ft8apset` builds the `apsym2[]` known-bit array from
  the operator's own callsign and the current QSO partner's callsign.
- In MAM mode, MSHV calls `SetMAMCalls` with the *full live list of every
  caller in the Now list*, so each worker thread can attempt AP for every
  active correspondent, not just the "primary" QSO partner. This is the
  multi-stream analogue of WSJT-X's single-correspondent AP slot.
- The decoder advances per-pass parameters `nappasses_2[nQSOProgress] =
  {2, 2, 2, 4, 4, 3}` and the `naptypes_2` 6×4 lookup table. Per the
  MSHV-specific comment in the source, depth-3 / decoder-depth=3 only
  runs the wider AP search if `|nfqso-f1| <= napwid` or `|nftx-f1| <=
  napwid` — i.e. AP is only tried near the user's RX or TX cursor unless
  ndepth is high.
- `napwid` ("AP width") is set to `60 Hz` in MSHV (default WSJT-X is `50
  Hz`). A 20% wider AP cone is one of the simplest knobs.
- File reference: `src/HvDecoderMs/decoderft8.cpp` near the
  `nappasses_2 / naptypes_2 / napwid` declarations; `ft8apset` is the
  builder.
- **Pancetta angle**: pancetta's `is_plausible` and hb-103 content-score
  paths already enforce a context-driven filter; MSHV's idea of feeding
  *every active correspondent's callsign* into the AP machinery in
  parallel is potentially worth a spec, since it captures the operator's
  intent at the multi-stream level rather than the single-QSO level.

### 4. "Var-decode" / SD-FT8 — soft-decision FT8 decoder (Beta as of 2.76.5)

- Per the 2.76.5 changelog: "New SD FT8 Decoder (Beta version), can be
  found in Menu Decode and his Parameters. Note, not recommended for slow
  speed PSs."
- Wired through `DecoderFt8::SetVarDecodeFtPar(bool f, int dcyc, int
  dsens)` and a static `s_use_var_dec` switch. When enabled, the regular
  `ft8_decode` early-outs into `ft8_decodevar(...)` which lives in
  `decoderft8var.cpp` (352 KB — the second-largest source file in the
  tree, after `decoderjt65.cpp`).
- The two integer parameters `dcyc, dsens` exposed in the UI map to
  "decoder cycles" and "decoder sensitivity"; explicit ranges and effects
  were not characterised in this catalog pass.
- File reference: `src/HvDecoderMs/decoderft8var.cpp` (not yet read in
  detail; not yet spec'd).
- **Pancetta angle**: potentially useful but high-effort. The OSD work
  pancetta already shipped (neural OSD) is comparable in spirit. Worth
  scheduling a Reader session specifically on `decoderft8var.cpp` if and
  only if pancetta's recall plateau motivates it. Defer.

### 5. Mode FT2 — 3.75-second-period FT8 variant (experimental, 2.76.5)

- New shorter-period FT mode, multi-thread by default ("Because period is
  very short 3.75s, In Menu Decode use as minimum 2-3 Threads"). Standalone
  modulation scheme; not interoperable with WSJT-X. Acknowledged to "ARI
  Caserta IU8LMC, Martino" — so this is the IU8LMC FT2 mode, hosted in
  MSHV.
- File reference: `src/HvDecoderMs/decoderft2.cpp` (99 KB).
- **Pancetta angle**: none for now. Pancetta is FT8-only.

### 6. Hardware-tier scaling controls

- The Decode menu has an explicit "Threads" spinbox (1-6), a "decoder
  depth" 1-3 (fast / normal / deep) — depth=1 is "BP only", depth=2 is
  "uncoupled BP + OSD", depth=3 is "BP+OSD" with widened AP.
- The 2.76.5 commit message specifically says "not recommended for slow
  speed PCs" about SD-FT8 — i.e. LZ2HV does not auto-classify hardware
  and instead surfaces the cost knobs to the operator.
- **Pancetta angle**: pancetta now auto-classifies hardware (hb-216 S2
  landed Batch 32) and adjusts `Ft8Config` per tier. The architectural
  difference is interesting — MSHV is operator-driven, pancetta is
  auto-driven. The MSHV approach is worth knowing about for the "expert
  mode" override path; not a spec target by itself.

### 7. Contest / Cabrillo support

- Activity selector in Macros: `FT Challenge`, `NCCC Sprint`,
  `ARRL International EME`, plus legacy `ARRL RTTY Roundup`, `EU VHF`,
  `Field Day`. Cabrillo export is one-click per activity type.
- 77-bit message protocol fully implemented for non-standard, compound,
  and standard callsigns; this is aligned with WSJT-X Improved.
- Special "FT Challenge" exchange format added 2024-11 (2.76).
- File reference: `src/HvTxW/HvMakros` (not opened in this pass).
- **Pancetta angle**: not currently a priority. Pancetta is not contest-
  focused. Document existence but do not spec.

### 8. Super Fox / Super Hound (DXpedition-tuned variants)

- Added 2.76.1 (2025-02-25). The Super Fox decoder lives in
  `src/HvDecoderMs/decodersfox.cpp` (103 KB) and is essentially LZ2HV's
  take on the WSJT-X "Fox" mode, with these MSHV-specific additions:
  - OTP-key verification for the DXpedition's TX stream (a 16-character
    key emitted on the second slot to authenticate the running station).
  - Random-noise generator for sensitivity probing.
  - Candidate sorting.
  - "Seeds tracking" (per 2.76.4 changelog).
- **Pancetta angle**: pancetta isn't planning to run DXpeditions in
  Super-Fox mode; this is out of scope unless the operator changes
  direction.

## What we can extract (algorithm-level, license-clean)

The following items have enough mechanism described in headers + dispatch
code + changelog + comments to produce a prose spec without copying any
expression. All would be safe targets for a Reader→Spec→Implementer
clean-room pass per
`~/.claude/projects/-Users-thagale-Code-pancetta/memory/feedback_clean_room_extraction.md`:

1. **`spec-mshv-band-split-multi-decoder.md`** — six-worker pthread fan-out,
   bandwidth-floor auto-decrement, ±11 Hz boundary nudge around RX
   frequency. *Authored in this pass.*
2. **`spec-mshv-multi-answer-sequencer.md`** — Queue / Now / Slot data
   model, tx_id enum, Special-MSG fold, CQ-on-free-slot. *Authored in
   this pass.*
3. **`spec-mshv-ap-multi-call-broadcast.md`** (NOT authored yet) — fan
   the active-correspondent list into per-worker AP machinery. Spec'able
   in one session.
4. **`spec-mshv-napwid-60hz.md`** (NOT worth a standalone spec — single
   constant change). Note in this catalog only.

## What we CANNOT extract cleanly (or not worth it)

- **`decoderft8var.cpp` (SD-FT8 / "Var" decoder)**: 352 KB of dense
  decoder math. Spec'able with a dedicated Reader session — possibly
  yielding the biggest single recall win for pancetta — but the size
  alone is a 2-3-session commitment to read carefully and produce a
  clean spec, and the changelog flags it Beta. Defer to a future
  hardware-and-headroom-driven session.
- **`decodersfox.cpp` (Super Fox)**: out of pancetta scope.
- **Contest format strings, Cabrillo exporters, language packs**:
  configuration data, not algorithm. Out of scope.
- **Qt UI layer**: tightly coupled to Qt 5; not portable. Out of scope.

## Whether further investigation would be useful

**Yes, conditionally:**

- The two algorithm specs already produced
  (`spec-mshv-band-split-multi-decoder.md`,
  `spec-mshv-multi-answer-sequencer.md`) are the highest-leverage extracts
  given pancetta's current direction (multi-stream TX, hardware-tier
  classifier). Implementer threads can take these forward without
  re-reading MSHV.
- A future Reader session on `decoderft8var.cpp` (the SD decoder) is
  worth scheduling **only after** pancetta has exhausted the
  Batch-30-32 line of FP-filter / capture-effect headroom, since this
  would be a decoder-internals change rather than a sequencer change.
  Likely 2-3 sessions for a clean spec.
- The MSHV `napwid = 60 Hz` and ndepth-conditional AP-near-RX-only gate
  are single-parameter facts that can be applied as quick experiments in
  pancetta if and when AP is fully wired into the autonomous responder.
  No standalone spec needed.

## Source attribution metadata

- Project: MSHV Amateur Radio Software
- Author: Hrisimir Hristov, LZ2HV
- Primary URL: `https://github.com/LZ2HV/MSHV`
- Mirror: `https://sourceforge.net/projects/mshv/`
- License: GPL-3.0 (per `COPYING.txt`, `license.txt`, and per-file headers)
- Language: C++ with Qt 5.6.3 (statically linked)
- Reader date: 2026-06-08
- Repository snapshot at read time: default branch `main`, last push
  2026-05-28, ~6.3 MB source tree, 10 stars / 3 forks (small upstream
  community).
