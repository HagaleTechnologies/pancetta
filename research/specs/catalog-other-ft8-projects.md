# Catalog: Other FT8 / Weak-Signal Projects in the Ecosystem

**Scout survey — 2026-06-08**
**Purpose:** Identify FT8 / weak-signal projects with something to teach pancetta that we haven't yet deep-dived.

**Already covered (excluded):** WSJT-X mainline, WSJT-X Improved (DG2YCB), JTDX, wsjtr (Bodiya), ft8mon (rtmrtmrtmrtm), MSHV, JS8Call, MAP65 (Batch 43 SHELVED), wsjt-z, WSJT-CB, FT-Activ8.

---

## Tier-A: Worth a follow-on clean-room agent

### A1. PyFT8 (G1OJS / Alan Holmes)

- **URL:** https://github.com/G1OJS/PyFT8
- **Companion:** https://github.com/G1OJS/MiniPyFT8 (300-line minimised reference)
- **License:** GPL-3.0 (derives constants/tables from WSJT-X)
- **Language:** Python (numpy/scipy)
- **Last updated:** v3.1.0 May 2026 — actively maintained, 81 releases, 1,511 commits on main
- **Description:** All-Python FT8 transceiver written from scratch (audio → spectrogram → symbols → bits → error correction) rather than ported from Fortran/C. Author explicitly designed it Python-idiomatic ("wide and short rather than thin and long") to avoid the nested-loop structure of the Fortran reference, which means it's full of vectorised-numpy reformulations of the standard pipeline. Self-reports ~70-100% of WSJT-X decodes on quiet bands, less on crowded bands.
- **Notable mechanisms to extract:**
  - Vectorised reformulations of FT8 sync/demod/LDPC stages (Python idiom forces different mathematical reformulation vs Fortran loops — algebraically equivalent but exposes structure)
  - 300-line MiniPyFT8 reference is essentially a tutorial implementation — useful as a clean reading reference for any pancetta investigator who wants the "minimum mathematical contract" of FT8 decoding
  - Author maintains active research code in a `research/` subfolder — worth scanning for hypothesis-shaped experiments
- **Worth follow-on?** **YES — moderate priority.** Different enough in style that vectorisation tricks may surface optimisations or simplifications we haven't seen in Fortran/C/Rust ports. Low risk of duplicating wsjtr territory.

### A2. SDRangel libft8 (Edouard Griffiths, F4EXB)

- **URL:** https://github.com/f4exb/sdrangel (libft8 in `ft8/` folder)
- **License:** GPL-2.0
- **Language:** C++
- **Last updated:** v7.24.0 (March 2026) — actively maintained, releases monthly
- **Description:** SDRangel's FT8 plugin uses a forked libft8 derived from ft8mon (AB1HL). The author claims it's "on par or slightly better than WSJT-X." Has independent maintenance trajectory from upstream ft8mon since at least 2024 — Apr 2024 added Gray decoding when decoding from magnitudes, Mar 2024 added encoding to library, Apr 2024 generalized soft decode on magnitudes to any-bits-per-symbol.
- **Notable mechanisms to extract:**
  - **Gray-decoded-from-magnitudes** (Apr 2024) — pancetta's decoder works on magnitude spectrograms; SDRangel's specific recipe for converting magnitude bins to Gray-coded soft bits may differ from ft8mon mainline and is worth comparing
  - **Generalised soft-decode-from-magnitudes for any bits-per-symbol** — design pattern that might inform a more flexible decode pipeline
  - Type 0.1 (DXpedition) decompacted into two separate messages — UX-level, but might inform pancetta's compound-callsign handling (current open follow-up)
  - Callsign verification used to reduce false OSD detections — pancetta already does hb-062/hb-103 sibling work here; comparison would validate or invalidate
- **Worth follow-on?** **YES — high priority.** Independent fork of ft8mon trajectory we've already deeply mined; cheap clean-room because the codebase is well-organised under one `ft8/` folder.

### A3. WB2FKO "Synchronization in FT8" paper (Mike Hasselbeck)

- **URL:** https://www.sportscliche.com/wb2fko/FT8sync.pdf (also TAPR DCC 2019 proceedings)
- **License:** N/A (paper)
- **Last updated:** 2019, revised
- **Description:** Deconstruction of the Fortran sync code with intuitive explanation of the 7×7 Costas Array and its implementation in FT8. Lays out the two-phase coarse/fine sync, the 40-ms / 3-Hz initial candidate accuracy, the coarse-search downsample-by-60, and the 32-samples-per-symbol fine stage. NOT covered by the wsjtr or ft8mon agents because those agents extracted mechanisms from code, not from this paper's narrative.
- **Notable mechanisms to extract:**
  - **First-principles rationale for each sync stage** — the paper explains *why* the WSJT-X design picks each parameter, which may inform pancetta's hb-218 capture-effect work (sync is the bottleneck there)
  - Costas-array intuition useful for any future joint-decoder design
- **Worth follow-on?** **YES — small dedicated agent.** Different deliverable: not a spec extraction but a "translate paper intuition into hypothesis-bank entries" pass. Cheap; could be folded into a hb-090/hb-218 redo.

---

## Tier-B: Worth catalog awareness, low-priority follow-on

### B1. weakmon (Robert Morris, AB1HL) — non-ft8mon Python sibling

- **URL:** https://github.com/rtmrtmrtmrtm/weakmon (file `ft8.py`, `ft8i.py`)
- **License:** Not specified; project from same author as ft8mon
- **Language:** Python/numpy/scipy
- **Description:** Python sibling of ft8mon, predates it. Uses Phil Karn's Reed-Solomon and convolutional decoders. The Python implementation is materially different in shape from the C++ ft8mon, so it's a useful cross-check on "did ft8mon's mechanism translate the Python correctly?" — *but* this is exactly the kind of cross-check the ft8mon deep-dive agent should have implicitly done.
- **Worth follow-on?** **MAYBE.** Only valuable if we suspect a mechanism in ft8mon was a translation artifact rather than the original author's intent. Defer until we have a specific question.

### B2. basicft8 (Robert Morris, AB1HL) — annotated reference

- **URL:** https://github.com/rtmrtmrtmrtm/basicft8
- **Companion page:** http://www.rtmrtm.org/basicft8/
- **License:** Not specified
- **Language:** Python
- **Description:** Deliberately simple, annotated demodulator. Intended as a teaching reference. Author notes it "misses many possible decodes" — by design. Useful as a clean read for anyone learning FT8 internals, but not a source of mechanism wins.
- **Worth follow-on?** **NO** for mechanism extraction. **YES** as onboarding reading for future pancetta agents.

### B3. Kozlenko/Lazarovych CNN paper (arXiv:2502.19097, Feb 2025)

- **URL:** https://arxiv.org/abs/2502.19097
- **Authors:** Mykola Kozlenko (Vasyl Stefanyk Carpathian National University), Ihor Lazarovych, Valerii Tkachuk, Vira Vialkova
- **Description:** Deep CNN demodulator for JT65A weak signals. Achieves interference immunity ~1.5 dB short of theoretical non-coherent orthogonal MFSK limit, across SNR -30 to 0 dB. **Targets JT65A, not FT8** — but FT8 is a sister MFSK protocol (8-tone vs 65-tone), so the demodulator architecture may transfer.
- **Worth follow-on?** **MAYBE.** ML-based demodulator is a category pancetta has not explored. Risk: bringing a 1.5-dB-short-of-theory neural front end into a system that's currently 5-10% of WSJT-X rate is a large mechanism shift. Defer until coverage-line (capture-effect / strong-signal miss) hypotheses are exhausted, then revisit as a wild_card.
- **Sibling paper:** arXiv:2502.16371 "Software defined demodulation of MFSK with dense neural network" — same author cluster, dense-NN variant. Also worth peripheral awareness.

### B4. CWSL_DIGI (alexranaldi / W2AXR)

- **URL:** https://github.com/alexranaldi/CWSL_DIGI
- **License:** GPL-3.0
- **Language:** C++
- **Last updated:** Oct 2023
- **Description:** Multi-band FT8/FT4/JT65/WSPR/FST4/FST4W/JS8 skimmer for Windows + Red Pitaya / QS1R. Delegates actual decoding to jt9 (WSJT-X command-line). Orchestration layer, not a decoder.
- **Worth follow-on?** **NO.** No novel decoder algorithm. Useful as architectural reference for any future multi-band pancetta skimmer mode but not for hypothesis-bank mining.

### B5. ft8modem (Matt Roberts, KK5JY)

- **URL:** http://www.kk5jy.net/ft8modem/
- **License:** Likely permissive (author's site, unspecified)
- **Description:** Command-line scriptable FT8/FT4 modem. Feeds audio to `jt9` then prefixes decodes with `D:` for downstream automation. Delegates decoding entirely.
- **Worth follow-on?** **NO** for decoder mechanisms. **YES** as architectural reference for pancetta-tui or pancetta CLI mode design — clean separation of decode-pipe from policy.

### B6. SDRconnect FT8 module (Jan van Katwijk)

- **URL:** https://github.com/JvanKatwijk/sdrconnect-ft8-module
- **License:** GPL-2.0
- **Last updated:** Feb 2026
- **Description:** Standalone Qt6/C++ FT8 decoder for SDRconnect. Uses LDPC code "taken from Karlis Goba" with modifications (configurable iterations default 20). No claim of sensitivity advantage.
- **Worth follow-on?** **NO** for mechanism extraction (it's a ft8_lib derivative). Worth peripheral awareness if we ever investigate ft8_lib's iteration-count tuning.

### B7. FT8CN (BG7YOZ / N0BOY)

- **URL:** https://github.com/N0BOY/FT8CN
- **License:** MIT
- **Language:** Java (Android), C/C++ JNI decoder
- **Last updated:** Jan 2025
- **Description:** Android FT8 app. Acknowledges "lightweight operations instead of deep decoding" for battery/perf — i.e., deliberately less capable than WSJT-X. No sensitivity claim.
- **Worth follow-on?** **NO.** Negative-mechanism territory — they're trading sensitivity for compute.

### B8. PyFT8 sibling: DigiSkimmer (lazywalker)

- **URL:** https://github.com/lazywalker/DigiSkimmer
- **License:** GPL-2.0
- **Language:** Python
- **Last updated:** Jan 2021 — stale
- **Description:** KiwiSDR-based multi-band skimmer; delegates to jt9/wsprd. Orchestration layer.
- **Worth follow-on?** **NO.** Stale and no novel decoder.

### B9. Pavel Demin Red Pitaya FT8 transceiver

- **URL:** https://github.com/pavel-demin/ft8d + Red-Pitaya-notes
- **License:** Not specified; minimal Fortran rebuild
- **Last updated:** Jul 2025
- **Description:** Minimal extraction of K1JT/K9AN Fortran for embedded use. Uses 8-DDC FPGA front-end to produce 8 simultaneous .c2 files; CPU runs decoder. Innovation is hardware, not decoder.
- **Worth follow-on?** **NO** for decoder. The 8-DDC architecture is interesting for multi-band but orthogonal to pancetta's single-channel sensitivity work.

### B10. rtlsdr-ft8d (Guenael)

- **URL:** https://github.com/Guenael/rtlsdr-ft8d
- **License:** GPL
- **Last updated:** Oct 2024
- **Description:** RTL-SDR + ft8_lib glue. Documents real-world clock-stability issues (no-name dongles drift). Decoder is unmodified ft8_lib.
- **Worth follow-on?** **NO** for decoder mechanism. Worth peripheral awareness for any future pancetta dongle-stability test scaffolding.

### B11. Pocket-FT8 / pico_ft8_xcvr / Mini-FT8 (microcontroller class)

- **URLs:** https://github.com/Rotron/Pocket-FT8 (Teensy 3.6), https://github.com/aa1gd/pico_ft8_xcvr (RPi Pico), https://github.com/wcheng95/Mini-FT8 (M5 Cardputer / ESP32-S3), https://github.com/kholia/pico_ft8_xcvr (Pico fork)
- **License:** Mix (MIT in most)
- **Description:** All build on ft8_lib with adjustable LDPC iterations / oversampling. Tradeoff: compute-bound on M0/M0+, so iterations capped low. Negative-mechanism for sensitivity.
- **Worth follow-on?** **NO** for sensitivity mechanism. The "adjustable LDPC iterations" knob is already in pancetta and ft8_lib upstream.

### B12. hotpaw ft8d (Ronald Nicholson, iOS app)

- **URL:** https://www.hotpaw.com/, App Store iOS app
- **License:** Closed-source iOS app
- **Description:** iPhone FT8 decoder. Level-1 messages only (standard callsigns + 13-char free text). No auto-sequencing, no hashed-callsign support. Listens to external SSB radio's speaker.
- **Worth follow-on?** **NO.** Closed-source, deliberately simplified.

---

## Tier-C: Negative finds (looked, nothing useful)

- **fldigi** — does NOT support FT8 natively. Confirmed: covers PSK/RTTY/MFSK/etc. but not FT8.
- **Quisk** — SDR transmitter front-end, no FT8 decoder.
- **gr-satellites (Daniel Estévez)** — does FT8 *through* satellites (FO-29 experiments) but uses WSJT-X for the decode; no in-house decoder.
- **OpenWebRX (jketterl)** — adds FT8 background decode but delegates to wsprd/jt9. Orchestration only.
- **ICOM IC-7300 / IC-705 / Yaesu firmware FT8** — closed firmware; no published algorithm notes found.
- **TAPR DCC 2024 / 2025 proceedings** — 2024 conference was cancelled; 2025 not yet published.
- **No FT8-specific Rust crate beyond pancetta-ft8 and wsjtr** — confirmed.
- **No mature FT8 Go implementation** found.
- **ft8js** (e04) — WASM wrapper around ft8_lib; no algorithm changes.
- **M17 / FreeDV RADE V1** — adjacent space; weak-signal voice codec using ML (RADE V1 down to -2 dB SNR), but voice, not FT8. Architecturally interesting for future "ML in pancetta" thinking but not directly transferable.

---

## Top-3 Recommendations for Follow-On Agents

### #1 — SDRangel libft8 (Tier-A2)

**Why first:** Highest-confidence mechanism payoff. It's an *independently-evolved fork* of the ft8mon codebase we already mined deeply, with documented 2024 changes to Gray-decoding-from-magnitudes and soft-decode generalisation. Codebase is well-organised under one `ft8/` folder, GPL-2.0, actively maintained (v7.24.0 Mar 2026). A clean-room agent can diff SDRangel's libft8 against ft8mon mainline and surface only the deltas — surgical, bounded scope.

**Expected output:** 3-6 spec files in the existing `research/specs/` pattern, comparable in size to the ft8mon specs we have.

### #2 — PyFT8 / G1OJS (Tier-A1)

**Why second:** Different in style from any decoder we've mined (Python-vectorised, written-from-scratch). Active maintenance (May 2026 release, monthly cadence). Lower confidence of unique mechanisms than #1 but higher confidence of unique *reformulations* — vectorised numpy code naturally exposes algebraic structure that Fortran loops hide. Has a `research/` subfolder worth scanning for hypothesis seeds. MiniPyFT8 (300 LOC) is a tutorial-quality reference and may serve as a "minimum FT8 contract" document for future onboarding.

**Expected output:** 2-4 spec files plus 1-2 hypothesis-bank seed entries.

### #3 — WB2FKO Synchronization-in-FT8 paper (Tier-A3)

**Why third:** Cheapest agent (single PDF, no codebase), but addresses pancetta's open hb-218 capture-effect work which is sync-bound. Different deliverable shape: not algorithm extraction but *first-principles rationale documentation* for the sync stage. Useful as a companion when revisiting hb-090 (phase-coherent matched filter) and hb-218 (capture-effect joint decode). Can be folded into a hb-090/hb-218 plan agent rather than dispatched standalone.

**Expected output:** 1 design-rationale document or 2-3 hypothesis-bank entries informed by the paper.

---

## Notes on coverage gaps still uncatalogued

- **JT65/JT9/QRA64/Q65 decoders** — adjacent modes, sometimes share Fortran code with FT8. If pancetta ever adds mode coverage, the Q65 decoder (best weak-signal performance in WSJT-X family) is the prime study target. Not in scope today.
- **WSPR-specific weak-signal techniques** (Phil Karn sequential decoder + Fano + OSD waterfall in wsprd) — the OSD-gates-on-trusted-callsign-only trick is documented and may inform hb-103 v2.
- **Wavecom commercial FT8 decoder** — closed-source, no algorithm notes.

---

**Catalog file:** `/Users/thagale/Code/pancetta/research/specs/catalog-other-ft8-projects.md`
