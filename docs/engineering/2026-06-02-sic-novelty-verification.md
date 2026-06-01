# SIC novelty verification: is pancetta's coherent iterative-subtract multi-pass novel for FT8?

**Date:** 2026-06-02
**Branch:** `iter/2026-06-02-sic-novelty-verification`
**Audit type:** read-only literature review (no code, no bank, no scorecards changed)
**Verdict:** **(a) NOT NOVEL — already published and shipping in WSJT-X mainline.**
**Confidence:** Very high (~95%).

---

## 1. What pancetta does (precise mechanism)

See `/tmp/pancetta-sic-mechanism.md` for the one-page technical
description. In one paragraph:

Pancetta retains the complex spectrogram (hb-075). After the first
decode pass, for each decoded message it derives the candidate
alignment, accumulates the complex FFT bins at the 21 Costas reference
positions to estimate a unit phase rotor `r = acc/|acc|`, then for
every (symbol, expected-tone) bin computes the orthogonal real
projection onto the rotor axis (`signal_est = Re(bin·conj(r))·r`) and
subtracts it (`residual = bin − signal_est`). It re-runs Costas sync
on the residual spectrogram and decodes new candidates. This is
iterated up to N=3 rounds (hb-080), each round subtracting only the
previous round's new decodes, with early-stop when no decodes are
added. A post-loop joint-pair-retry (hb-086 V1) re-runs LDPC against
the residual at every original-but-not-yet-decoded sync candidate.

Source: `pancetta-ft8/src/decoder.rs`:
- `coherent_subtract_and_repass` (line 2392) — the iteration body
- `subtract_decode_coherent` (line 4405) — per-symbol ML projection subtract
- `estimate_candidate_phase_rotor` (line 4567) — Costas-anchored rotor estimation
- `compute_costas_complex_accumulator` (line 4544) — the 21-symbol sum

Journals: `research/experiments/2026-05-26-hb-079-coherent-multipass.md`,
`2026-05-27-hb-080-multipass-n3.md`, `2026-05-28-hb-086-joint-pair-retry-v1.md`.

---

## 2. Lit search log

### Tier A: FT8-specific decoders

#### A1. WSJT-X mainline — **PRIOR ART, complete match**

**Source 1: `lib/ft8/subtractft8.f90`** in WSJT-X master tree.
URL: <https://sourceforge.net/p/wsjt/wsjtx/ci/master/tree/lib/ft8/subtractft8.f90>

The subroutine builds a complex reference waveform from the decoded
tone sequence (`gen_ft8wave(itone, 79, 1920, 2.0, 12000.0, f0, cref, …)`),
heterodynes the input against it via `camp(i) = dd(j)·conjg(cref(i))`,
FFT-lowpass-filters the complex amplitude (this **estimates the
time-varying channel response** — amplitude fade and phase drift),
then reconstructs `z = cfilt(i)·cref(i)` and subtracts:

```
dd(j) = dd(j) − 2.0·real(z)
```

This is **structurally identical** to pancetta's
`Re(bin·conj(rotor))·rotor` subtract, with two differences:
- WSJT-X operates on the **time-domain waveform** `dd`, pancetta
  operates on the **complex spectrogram bins**.
- WSJT-X estimates a **time-varying** complex channel response
  `cfilt(i)` (one value per audio sample, low-pass filtered to a
  ~few-Hz bandwidth), pancetta estimates **one constant rotor per
  candidate** (from the 21 Costas symbols).

The `2.0·real(z)` factor in WSJT-X is the time-domain equivalent of
pancetta's `Re(bin·conj(rotor))·rotor`: project the complex
reconstruction onto the real axis of the analytic signal, double it
because we are subtracting from a real waveform. Pancetta's
spectrogram-domain version doesn't need the factor of 2 because
positive-frequency complex bins were not symmetrised in the first
place.

**Source 2: `lib/ft8/ft8d.f90`** — the WSJT-X FT8 driver. Multi-pass
loop:

```fortran
npass = 3
if (ndepth.eq.1) npass = 2
do ipass = 1, npass
  ...
  call sync8(dd, ...)
  do icand = 1, ncand
    call ft8b(...)
  enddo
enddo
```

WSJT-X calls `subtractft8` on every successful decode (both early
and across passes), then re-runs `sync8` + `ft8b` on the modified `dd`
waveform on the next pass. **This is the same algorithm pancetta runs
in `coherent_subtract_and_repass`, iterated up to N=3 rounds.**

**Source 3: Franke, Somerville, Taylor, "The FT4 and FT8 Communication
Protocols," QEX July/August 2020.**
URL: <https://wsjt.sourceforge.io/FT4_FT8_QEX.pdf>

Direct quote from the paper:

> "[A] channel response model is applied to the ideal reference signal
> to reconstruct a nearly noiseless version of the received signal's
> waveform, including channel-induced amplitude fading and phase
> variation. The reconstructed signal is then subtracted from the
> received data, i.e.
> `s'(t) = s(t) − 2·R[g(t)·r(t)]`
> where R[·] takes the real part of its argument, and s'(t) is the
> audio waveform after subtracting the decoded signal. This
> subtraction process can uncover weaker signals that occupy the same
> frequency slot as the subtracted strong signal. The weaker signals
> can often be decoded on a second decoding pass, after all signals
> decoded in the first pass have been subtracted."

And:

> "If at least one signal is decoded and subtracted in the first
> decoding pass, the remaining audio waveform is re-analyzed. New
> candidates are identified and steps 1 through 3 are carried out for
> each one. If at least one new signal is decoded and subtracted in
> the second pass, a third pass will sometimes yield decodes missed in
> the first two passes. Multi-pass decoding has proven very effective:
> the approach is often able to decode two or three signals at the
> same or nearly the same frequency."

This is published in 2020 — five+ years before hb-079. **The QEX
paper's formula `s'(t) = s(t) − 2·R[g(t)·r(t)]` is mathematically
identical to pancetta's `residual = bin − Re(bin·conj(rotor))·rotor`**;
the only difference is the domain (time vs spectrogram) and
generality of the channel estimate (time-varying vs constant rotor).
The 3-pass iteration is also explicitly described.

#### A2. WSJT-X-Improved (Uwe Risse DG2YCB) — has multi-pass and "4th pass after a7"

URL: <https://sourceforge.net/projects/wsjt-x-improved/files/>

Release notes (v3.0.0+) reference "a7 decoding technology, sub-sample
DT refinement, and a 4th pass" — extension of the same multi-pass +
subtract framework. Not novel mechanism; just one more pass.

#### A3. JTDX — has multi-pass with "subpass" option

URL: <https://stationproject.blog/2019/03/01/jtdx-feature-rich-software-for-ft8-and-other-jt-modes/>

JTDX adds "use subpass" which "searches the audio spectrum a second
time" and is a multi-pass enhancement. Per WSJT-X-Improved release
notes, JTDX also incorporates additional passes and OSD on ndepth=2,
all built on the same `subtractft8` + multi-pass spine.

#### A4. kgoba/ft8_lib (the C library pancetta depends on) — does NOT have multi-pass

URL: <https://github.com/kgoba/ft8_lib>

Verified locally: `pancetta-ft8/vendor/ft8_lib/ft8/{decode.c,decode.h}`
contains **zero** matches for `subtract`, `multipass`, `multi.pass`,
`residual`, `iterate`, `cancel`, or `sic` (case-insensitive). kgoba's
decoder is single-pass. So pancetta's multi-pass IS a delta vs
ft8_lib — but it's also a re-implementation of WSJT-X's, which has
shipped since FT8 was released.

#### A5. rtmrtmrtmrtm/weakmon (Robert Morris AB1HL, Harvard CS) — independent Python implementation, also has multi-pass coherent subtract

URL: <https://github.com/rtmrtmrtmrtm/weakmon/blob/master/ft8.py>

Robert Morris's independent FT8 implementation has:

```python
npasses = subpasses + 1
for pass_ in range(0, npasses):
    ...
```

With a `subtract_v6()` that does FFT-domain bin modification
(`a[si] /= aa; a[si] *= (aa - ampl); samples = irfft(a)`) — **another
coherent (complex-domain) subtract**, independent of WSJT-X's
implementation. Default 3 total passes.

This is a strong piece of evidence: a serious independent
implementation (different language, different author, no shared code)
**also converged on coherent multi-pass subtract** — that's not
discovery, that's the canonical engineering solution for FT8.

### Tier B: FT8 academic literature

- **Franke-Taylor-Somerville QEX 2020** (covered in A1 source 3).
  Documents the exact mechanism. **Decisive prior art.**

- **Mike Hasselbeck WB2FKO, TAPR DCC 2019 paper on FT8
  synchronization** (<https://files.tapr.org/meetings/DCC_2019/2019-4-WB2FKO.pdf>).
  Doesn't discuss multi-pass subtract specifically; focuses on the
  sync8 algorithm. Out of scope for our question.

- **arXiv search** for "FT8 decoder successive interference
  cancellation" / "FT8 coherent subtract" — no FT8-specific papers
  surfaced. Adjacent papers found:
  - arXiv:2208.04256 "Coherent Time-Domain Canceling of Interference
    for Radio Astronomy" — confirms coherent time-domain SIC is
    canonical in radio astronomy.
  - arXiv:2512.18427 "On the Limits of Coherent Time-Domain
    Cancellation of Radio Frequency Interference" — analyses the
    SAME class of algorithm in a different domain.

### Tier C: Multi-user detection / SIC literature

The broader literature is unambiguous:

- Verdú 1998, *Multiuser Detection*. Coherent SIC is **canonical
  textbook material** for multi-user channels going back to the
  late 1990s. The orthogonal-projection subtract used by both
  pancetta and WSJT-X is a standard maximum-likelihood single-user
  matched-filter detection step.
- IEEE Xplore documents on CDMA SIC (Patel-Holtzman 1994 and many
  followers): same mathematical structure (project onto known signal
  direction, subtract, redetect).

There is no chance the mechanism itself is novel as a signal-processing
technique. The question is only whether the **specific FT8 application**
is novel — and per A1+A2+A3+A5, it's not.

### Tier D: Other digital modes

- WSJT-X applies the same `subtractXX.f90` + multi-pass framework to
  JT65 (`subtract65.f90`), JT9, FT4 (`subtractft4.f90`), Q65
  (`subtract65.f90` shared), MSK144. The technique is the **standard
  WSJT-family decoder spine** since FT8 launched.

---

## 3. Verdict + supporting evidence

### Classification: **(a) NOT NOVEL — already published for FT8.**

Pancetta's hb-079/080/081/086-V1 is a re-implementation, in a
different domain (spectrogram vs waveform) and with a simpler channel
model (constant rotor vs time-varying), of WSJT-X's `subtractft8` +
multi-pass loop. The Franke-Taylor-Somerville 2020 QEX paper and the
WSJT-X source tree both predate hb-079 by 5+ years.

### Three pieces of evidence supporting (a)

1. **WSJT-X mainline `subtractft8.f90`**: complex reference signal
   reconstruction, conjugate heterodyne, channel-response estimation
   via lowpass-filtered complex amplitude, subtraction with
   `2·real(z)` factor. Same operation class as pancetta's
   `Re(bin·conj(rotor))·rotor`.
2. **QEX 2020 paper explicit formula and procedure**: `s'(t) = s(t) −
   2·R[g(t)·r(t)]`, 3-pass loop, all described in the paper as the
   FT8 decoder's standard mechanism.
3. **Independent re-implementations** (weakmon, JTDX, WSJT-X-Improved)
   all converged on the same technique — strong sign that this is
   THE canonical FT8 SIC algorithm, not anything novel.

### What pancetta does that the literature does NOT explicitly cover

- **Spectrogram-domain (not time-domain) subtraction.** WSJT-X
  subtracts from the time-domain `dd` array; pancetta subtracts from
  the per-bin complex STFT cells. This means pancetta's subtract is
  symbol-aligned and tone-aligned by construction (only the expected
  tone bin per symbol is modified), whereas WSJT-X's time-domain
  subtract affects all audio samples and relies on its lowpass
  filter to localise the effect to the correct frequency.
- **Costas-only rotor estimation.** Pancetta uses only the 21 Costas
  reference symbols (not the 71 LDPC-corrected data symbols) for
  rotor estimation. WSJT-X reconstructs the full 79-symbol waveform
  via `gen_ft8wave(itone, 79, ...)` — using ALL 79 symbols including
  the data symbols (whose values come from the post-LDPC decoded
  message). Different choice: WSJT-X gets a stronger channel estimate
  (more reference samples) at the cost of being more sensitive to
  LDPC errors near the threshold; pancetta gets a cleaner estimate
  immune to data-symbol errors but with less averaging.
- **Joint-pair-retry on residual sync candidates** (hb-086 V1). The
  post-loop pass that retries every ORIGINAL sync candidate against
  the residual, bypassing the residual sync threshold. This specific
  optimisation does not appear in WSJT-X's source tree (mainline
  always re-derives candidates from the residual via `sync8`).
  Possibly minor novelty here — but very narrow and well within the
  "engineering refinement" envelope of well-known SIC.

These are implementation details, not algorithmic novelty. None of
them is publishable as a new contribution.

### Confidence and remaining uncertainties

- **Confidence in (a): ~95%.** The QEX paper's explicit formula and
  the WSJT-X source code are uncontestable. We've directly observed
  the same algorithm in the canonical FT8 reference.
- **Residual uncertainty (~5%):** I haven't read every line of
  `subtractft8.f90` myself (only summarised quotes from WebFetch). It
  remains possible the WSJT-X implementation uses a different channel
  model that doesn't strictly match pancetta's ML projection — but
  the QEX paper's formula `s(t) − 2·R[g(t)·r(t)]` removes that
  uncertainty for the **published algorithm** even if implementation
  details differ.
- **Unsearched corner:** I didn't read the entire `ft8b.f90` (the
  per-candidate decoder driver) to see if it does coherent
  per-symbol subtract differently. That's a deeper-rabbit-hole detail
  — even if there's a variation there, the **published QEX paper and
  the `subtractft8.f90` filename + multi-pass loop** are sufficient
  prior art on their own.

---

## 4. Honest assessment

**Am I certain we've found everything?** No.

Gaps in the search I'm aware of:

1. I did not fully read JTDX source code line-by-line to see whether
   their subtract differs from WSJT-X mainline. JTDX is downstream of
   WSJT-X for the subtract code, so they likely use the same
   `subtractft8.f90` — but the exact version may differ.
2. I did not consult MSHV source code in detail (LZ2HV's fork). Same
   downstream-from-WSJT-X assumption.
3. The arXiv search for "FT8" + "SIC" returned no FT8-specific papers,
   but the search was English-language and Western-published. There
   may be Japanese, Russian, or Chinese amateur radio publications I
   haven't located.

**But these gaps don't matter for the verdict.** The QEX paper + the
WSJT-X source code are sufficient prior art on their own. Even if a
hypothetical Japanese-language ham radio journal published the same
technique two years before WSJT-X did, that would only strengthen the
(a) verdict, not change it.

The verdict is robust under the worst case of every uninspected
corner: it remains (a) NOT NOVEL.

---

## 5. Recommendation

### Case study: **NO.**

This is not novel. Don't write a case study; don't draft a QEX
article; don't draft a blog post claiming discovery. Any publication
would have to acknowledge the QEX 2020 paper and WSJT-X's
`subtractft8.f90` as prior art, at which point the contribution
collapses to a few implementation details that are not interesting
to anyone but the pancetta team itself.

### What IS appropriate to write up

- A short **technical README addition** or inline comment in
  `pancetta-ft8/src/decoder.rs` near `coherent_subtract_and_repass`
  citing:
  - Franke, Somerville, Taylor (QEX 2020) §"Multi-pass decoding"
  - `subtractft8.f90` in WSJT-X (`lib/ft8/subtractft8.f90`)
  - kgoba/ft8_lib for context (no multi-pass — that's the gap we
    closed vs our underlying dependency, not vs WSJT-X)

- An honest framing in research journals going forward: "pancetta
  implements coherent SIC + multi-pass per Franke-Taylor-Somerville
  2020, in the spectrogram domain. The +0.013 composite gain shows
  the technique is also worth implementing for ft8_lib-based
  decoders, which had not adopted it." That's accurate and avoids
  overclaim.

### Bank entry edits (if desired, separate iter)

- hb-079 journal could append a "post-hoc prior art" footnote citing
  the QEX paper and `subtractft8.f90`. The current journal says
  "coherent subtract is the right kernel" and "ML projection is
  canonical" — both true but understated; the technique is **literally
  what WSJT-X has done for years**. No falsehood, just a missed
  citation at the time. Worth correcting.

### Recovered framing for the user's "case study" question

The earlier "we rediscovered SIC" framing was overclaim in the wrong
direction (too strong), and the previous summary's apology for
overclaim was also somewhat misdirected. The accurate framing is:
**pancetta adopted a known, published, shipping FT8 decoder
technique that ft8_lib (our dependency) didn't have — and this gave
a large composite gain because the dependency was lacking it, not
because the technique was new.** Closing a gap, not opening a frontier.

---

## Appendix: search URL log

- <https://sourceforge.net/p/wsjt/wsjtx/ci/master/tree/lib/ft8/> — WSJT-X
  FT8 directory listing; confirmed `subtractft8.f90` exists.
- <https://sourceforge.net/p/wsjt/wsjtx/ci/master/tree/lib/ft8/subtractft8.f90>
  — coherent subtract source.
- <https://sourceforge.net/p/wsjt/wsjtx/ci/master/tree/lib/ft8_decode.f90>
  — multi-pass driver, `npass = 3` loop.
- <https://wsjt.sourceforge.io/FT4_FT8_QEX.pdf> — Franke-Taylor-Somerville
  2020. Formula `s'(t) = s(t) − 2·R[g(t)·r(t)]`, 3-pass description.
- <https://github.com/kgoba/ft8_lib> — single-pass FT8 lib, no subtract.
  Verified locally in `pancetta-ft8/vendor/ft8_lib/ft8/`.
- <https://github.com/rtmrtmrtmrtm/weakmon/blob/master/ft8.py> —
  independent Python FT8 decoder with `npasses = subpasses + 1` and
  `subtract_v6` FFT-bin subtract.
- <https://sourceforge.net/projects/wsjt-x-improved/files/> — WSJT-X
  Improved (DG2YCB), references "4th pass after a7" extension.
- <https://stationproject.blog/2019/03/01/jtdx-feature-rich-software-for-ft8-and-other-jt-modes/>
  — JTDX "use subpass" multi-pass option.
- <https://files.tapr.org/meetings/DCC_2019/2019-4-WB2FKO.pdf> — WB2FKO
  sync paper; doesn't cover multi-pass subtract.
- <https://arxiv.org/abs/2208.04256> — generic coherent time-domain
  cancellation prior (not FT8-specific, confirms SIC is canonical).
