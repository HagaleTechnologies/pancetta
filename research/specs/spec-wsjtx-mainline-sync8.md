# Algorithm spec: WSJT-X mainline sync8 — coarse sync candidate generator

## Source attribution

- Origin: WSJT-X mainline (K1JT et al., upstream)
- Repository: https://sourceforge.net/p/wsjt/wsjtx/ci/master/tree/
- Mirror: https://github.com/saitohirga/WSJT-X
- File (traceability only; NOT quoted): `lib/ft8/sync8.f90`
- Companion constants file: `lib/ft8/ft8_params.f90`
- Companion helpers: `lib/ft8/sync8d.f90`, `lib/ft8/get_spectrum_baseline.f90`,
  `lib/ft8/baseline.f90`
- License: GPL-3.0
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

`sync8` is the *coarse* sync stage of the WSJT-X mainline FT8 decoder. It
walks a full 15-second audio buffer, computes a 2D sync surface over
`(frequency_bin, time_lag)`, picks the highest-scoring peaks, and emits an
ordered list of candidates that the inner decoder (`ft8b`) will refine and
attempt to decode. The candidate list is the only way candidates ever enter
the inner loop; if a signal is missed here, it cannot be recovered downstream.

This routine is what wsjtr's `sync.rs` and ft8mon's analogous stage are
translations/ports of. Reading the original is useful because (a) wsjtr's
docs paraphrase several details, and (b) ft8mon diverges in at least one
parameter (candidate dedup window). The original is the reference.

## Inputs

- `dd` — real-valued audio buffer, length `NMAX = 15 * 12000 = 180,000`
  samples at 12 kHz sample rate.
- `nfa`, `nfb` — frequency search bounds in Hz (typical operator setting:
  `nfa=200`, `nfb=3000`; the routine clamps to `[100, 4910]`).
- `syncmin` — minimum normalized sync score for a candidate to survive
  (caller passes 1.3, 1.6, or 2.0 depending on pass and depth — see
  ft8_decode spec).
- `nfqso` — the operator's "QSO frequency" in Hz; candidates within ±10 Hz
  of this frequency get sorted to the head of the output list (priority
  boost for the operator's tuned target).
- `maxcand` — output buffer capacity (caller passes `MAXCAND = 600`).

## Outputs

- `candidate(3, maxcand)` — an array of up to `maxcand` candidate triples,
  each containing `(frequency_Hz, dt_seconds, normalized_sync_score)`.
- `ncand` — actual count of candidates written.
- `sbase` — per-frequency-bin spectral baseline (in dB units, ~3.125 Hz
  resolution). Used downstream by `ft8b` for SNR estimation.

## Numerical constants (facts, not expression)

These are pulled from `ft8_params.f90` and the body of `sync8.f90`:

- `NSPS = 1920` — samples per symbol at 12 kHz.
- `NSTEP = NSPS/4 = 480` — coarse time-sync step (4 steps per symbol; one
  spectrogram column every 40 ms).
- `NN = 79` — total channel symbols per FT8 frame.
- `NS = 21` — total sync symbols (3 Costas arrays × 7 tones).
- `NFFT1 = 2 * NSPS = 3840` — FFT length for the coarse spectrogram. The
  bin width is `12000 / 3840 = 3.125 Hz`.
- `NH1 = NFFT1 / 2 = 1920` — number of non-negative-frequency bins.
- `NHSYM = NMAX/NSTEP - 3 = 375 - 3 = 372` — number of spectrogram columns
  produced.
- `JZ = 62` — half-range of the lag sweep, in spectrogram column units.
  Comment in the original: "2.5 s / 0.16 s per symbol × 4 samples/symbol
  = 62.5 lag steps in 2.5 s". So the lag sweep covers ±2.5 s relative to
  the nominal start.
- `MAXPRECAND = 1000` — internal pre-candidate buffer (before final
  pruning and ranking).
- Costas 7×7 tone pattern: `(3, 1, 4, 0, 6, 5, 2)`. **NOTE**: the source
  comment in `ft8b.f90` says this is *flipped* relative to the original
  FT8 sync array — meaning the array as written here is the "decoder's
  view" (correlated against received tone indices), not the literal
  transmitter pattern.
- `jstrt = 0.5 / tstep` — the lag origin is offset by 0.5 s so that
  lag-zero corresponds to a TX that started at the slot's nominal 0.5 s
  offset (FT8 convention: "TX starts 0.5 s into the slot").
- Per-symbol oversampling: `nssy = NSPS / NSTEP = 4` (4 spectrogram
  columns per symbol).
- Per-symbol frequency oversampling: `nfos = NFFT1 / NSPS = 2` (2 FFT
  bins per symbol-frequency-spacing of 6.25 Hz).
- `fac = 1.0 / 300.0` — input scaling before FFT (signal hygiene; keeps
  FFT magnitudes in a sane range).
- Pre-FFT input shape: a 1920-sample window of `dd` followed by a 1920-
  zero pad to reach NFFT1 = 3840. So the FFT is single-symbol window
  zero-padded ×2 — this gives the 2× frequency oversampling.
- Baseline percentile: `npctile = nint(0.40 * iz)` — the 40th-percentile
  bin (of the lag-maxed sync surface, sorted) becomes the normalization
  baseline. Two parallel baselines are computed:
  - `red` baseline uses lag range ±`mlag = 10` columns (±0.4 s — tight
    search for the "real" peak)
  - `red2` baseline uses lag range ±`mlag2 = JZ = 62` columns (full ±2.5 s
    — for slot-edge / wide-offset captures)
- Dedup window after candidate generation: `|fdiff| < 4.0 Hz` AND
  `|tdiff| < 0.04 s`. The lower-scoring candidate is zeroed.
- Operator-frequency priority window: candidates within ±10 Hz of
  `nfqso` get sorted to the head of the output list.

## Algorithm description (prose only)

### Phase 1: build the symbol-spectra grid

The audio buffer is sliced into overlapping 1920-sample windows stepped by
`NSTEP = 480` samples. Each window is zero-padded to 3840 samples and run
through a real-to-complex FFT, then the magnitude-squared values for the
first 1920 non-negative-frequency bins are saved. This produces an
`s(NH1, NHSYM) = s(1920, 372)` power spectrogram with 3.125 Hz frequency
resolution and 40 ms time resolution.

A running sum across columns gives `savg` (the average spectrum), which is
then passed (along with the raw audio) to `get_spectrum_baseline`. That
helper computes a separate, longer-FFT spectral baseline `sbase` (used by
`ft8b` later for SNR estimation; see baseline subsection below).

### Phase 2: build the sync2d surface

For each frequency bin `i` in `[nfa/df, nfb/df]` and each lag `j` in
`[-JZ, +JZ]`, compute the following two quantities:

**`sync_abc` (full 3-Costas metric).** Sum across the 7 Costas tones of the
expected power at the right `(frequency, time)` offsets for the three
Costas arrays at symbol positions 0, 36, 72 (in symbols, expressed as
`nssy * 36 = 144` and `nssy * 72 = 288` spectrogram-column offsets). The
"expected" frequency for tone `n` is `i + nfos * icos7(n)` (i.e. the base
bin shifted by the Costas pattern × the 2-bin-per-symbol oversampling).

For each of the three Costas arrays, also compute an "all 7 tones at the
same time positions" power sum `t0a / t0b / t0c` — this serves as a local
noise / signal-plus-noise baseline. The numerator is the signal-at-the-
right-tones; the denominator is what's left over after removing the
signal contribution, divided by 6 (to get an average per-non-signal-tone).
Specifically: `t0 = (t0_all - t_signal) / 6`. Then `sync_abc = t / t0`.

**`sync_bc` (partial 2-Costas metric).** Same calculation but using only
Costas arrays 2 and 3 (symbols 36+ and 72+). This is the slot-edge rescue
metric — when the leading edge of the audio is missing or corrupted, the
first Costas array is unreliable but the latter two are intact. This is
the mechanism wsjtr's `spec-wsjtr-sync-bc.md` covers, and the *origin* of
that mechanism is here in WSJT-X mainline.

**Final cell value:** `sync2d(i, j) = max(sync_abc, sync_bc)`. The max
keeps both pathways "warm" — the partial metric never displaces a strong
full metric (because if full is strong, partial is at most equal), but it
rescues edge cases.

A subtle but load-bearing detail: the loop over Costas arrays for
`sync_bc` always accumulates `tb` and `tc` (the second and third arrays)
regardless of whether `m` is in range — only the first array gets the
`m >= 1 .and. m <= NHSYM` gate. This means at extreme negative lags
where the first Costas is out of window entirely, the full metric
collapses while the partial metric still works.

### Phase 3: per-bin peak lag picking and normalization

Two parallel passes over the surface, one with a tight lag window
`±mlag = ±10`, one with the full lag window `±mlag2 = ±62`:

For each frequency bin `i`, find the lag with the largest sync score
within the window, and store the (lag, score) pair as `(jpeak[i], red[i])`
or `(jpeak2[i], red2[i])` respectively.

Then sort the `red` and `red2` arrays separately. The 40th-percentile value
becomes the normalization base — every per-bin peak score is divided by
this base. This is what makes `syncmin` (e.g., 1.3) a meaningful
"signal-relative-to-typical-noise-bin" threshold; the 40th-percentile
captures the "ambient" sync level across frequency.

### Phase 4: pre-candidate emission

Walk the frequency bins in *decreasing* order of normalized sync score.
For each bin:

1. If the tight-lag normalized score `red[n]` clears `syncmin` (and is not
   NaN), emit a pre-candidate at `(n * df, (jpeak[n] - 0.5) * tstep,
   red[n])`. The `(jpeak - 0.5) * tstep` is the dt in seconds; the `-0.5`
   shift maps the lag-origin convention back to "dt relative to slot
   0.5 s start".
2. If the wide-lag peak landed at a *different* lag than the tight-lag
   peak (`|jpeak2 - jpeak| > 0`), AND `red2[n]` also clears `syncmin`,
   emit a *second* pre-candidate from the wide-lag pathway.

So a single frequency bin can yield two candidates if both pathways agree
the signal exists but disagree on where in time it sits — this is the
mechanism for picking up signals at unusual time offsets without
sacrificing tight-window sensitivity.

Stop after `MAXPRECAND = 1000` pre-candidates.

### Phase 5: near-duplicate suppression

Pairwise compare all pre-candidates. If two are within `|Δf| < 4 Hz` AND
`|Δdt| < 0.04 s`, zero the lower-scoring one's score (it gets filtered out
later by the `>= syncmin` check). This dedup happens *before* the
operator-frequency priority sort, so a strong candidate near nfqso doesn't
get suppressed by a slightly stronger candidate elsewhere.

### Phase 6: ordering and output

The output `candidate` array is filled in two phases:

1. **Operator-frequency priority insertion:** walk the pre-candidates in
   their original order; any with `|f - nfqso| <= 10 Hz` and still
   surviving the syncmin gate get written to the output array first.
   Their pre-candidate slot is zeroed so they aren't re-emitted in the
   next phase.
2. **Score-sorted insertion:** sort remaining surviving pre-candidates by
   score descending, then append. Stop at `maxcand`.

Important detail: in the score-sorted phase, the frequency is written as
`abs(candidate0(1, j))` — i.e., negative frequencies are folded to
positive. (Pre-candidate frequencies are computed as `n * df` where `n` is
a positive bin index, so this is defensive but practically always
positive.)

### Phase 7: spectrogram rescaling for visualization

Just before returning, `s` is rescaled to `s = (20.0 / max(s)) * s` so that
peak power is 20 (dB scale prep). This is for the GUI waterfall, not the
decoder math, but it *does* mutate the input — readers should be aware
the spectrogram is no longer the raw value after `sync8` returns.

## What wsjtr's docs paraphrase or miss

These are the load-bearing details a strict-clean-room read of WSJT-X
mainline catches that wsjtr's docs (`docs/jt9r.md`, related specs in
`research/specs/`) glossed:

1. **`sync_bc` is `max(full, partial)`, not "partial only when full
   fails".** wsjtr's `spec-wsjtr-sync-bc.md` is correct on this point;
   I confirm it.
2. **Two parallel baselines** (`red` and `red2`) at *different lag
   windows* (`±10` vs `±62`), each independently normalized at the 40th
   percentile. Wsjtr collapses this to one in places. The wide-lag
   pathway is specifically what catches signals with unusual dt; the
   tight-lag pathway is the "normal" detector. Both run; both contribute.
3. **The lag-origin convention is `jstrt = 0.5 / tstep`** — i.e., lag 0
   means TX started at slot+0.5 s, the FT8 standard. The output dt is
   `(jpeak - 0.5) * tstep`, NOT `jpeak * tstep`. The `-0.5` is a
   half-spectrogram-column offset, not the 0.5-s slot offset. The 0.5-s
   slot offset is folded into `jstrt`.
4. **`fac = 1/300` input scaling before FFT.** wsjtr drops this since it
   normalizes elsewhere, but the original applies it pre-FFT so the
   spectrogram values are in a known range for the `20.0 / max(s)`
   visualization rescale at the end.
5. **The Costas array is the "flipped" version** (`3,1,4,0,6,5,2`), not
   the transmit-side pattern. Several derivative implementations
   independently rediscover this; the mainline comment in `ft8b.f90`
   explicitly says so.
6. **Operator-frequency priority is exactly ±10 Hz from `nfqso`** and
   inserts those candidates at the *head* of the output list — they get
   tried first by the inner decoder. Wsjtr's docs don't emphasize this
   ordering, but it's load-bearing for "operator tuned to this signal,
   give it the first decode attempt" UX.
7. **`MAXPRECAND = 1000` internal cap before final `MAXCAND = 600` cap.**
   Two-stage gating. The internal cap protects against pathological
   spectra; the external cap is what `ft8b` actually loops over.
8. **The frequency search bound clamping in `get_spectrum_baseline`**:
   `nfa` is forced to ≥100 and `nfb` is forced to ≤4910 before baseline
   fitting. This is invisible to the caller (the variables are passed
   by reference and modified). Pancetta should be aware of this when
   doing exotic band setups.

## Conflict with pancetta's existing mechanisms

Pancetta's decoder runs a single-baseline sync (tighter lag range, fewer
candidates). The hb-058 / FP-filter work is downstream of candidate
generation, so this spec is upstream of all current production filters
and complementary to them.

Pancetta-specific implications:

- Pancetta's `sync_bc` rescue (per `spec-wsjtr-sync-bc.md`) lines up with
  this. Mainline already does the `max(abc, bc)` selection; pancetta
  matching that is correct.
- The two-baseline pathway (`red` + `red2`) is **NOT** currently in
  pancetta. This is the most promising headroom item from this read —
  it's what specifically rescues large-dt signals (hb-091 / slot-edge
  bucket from MEMORY: "dt: slot-edge (negative dt) at 48.3% recall").
  Adding a wide-lag-window second pass at the candidate-generation
  stage (instead of only at the fast-path symbolic-sync stage) is a
  plan-sized hypothesis, call it hb-219 or similar.
- The 40th-percentile baseline normalization at per-bin granularity is
  the trick that makes `syncmin = 1.3` a portable threshold across
  bands and noise floors. Pancetta should verify our equivalent uses
  the same percentile (not, e.g., median or trimmed mean).
- The "second candidate per bin when wide-lag and tight-lag disagree"
  emission rule is subtle. Pancetta's current code emits at most one
  candidate per frequency bin; matching mainline would roughly double
  the candidate count in some scenarios. Whether this is good depends
  on downstream FP-filter capacity.

## Estimated Rust port effort

- `sync8` core (Phases 1-4): ~250-350 LOC of Rust, leveraging existing
  FFT and spectrogram infrastructure.
- `sync8d` (the per-candidate fine-sync correlator used by `ft8b`):
  ~80-120 LOC of Rust, see ft8b spec for context.
- Tests: spot-check against known WAVs; verify candidate ordering matches
  the "nfqso priority then score descending" rule.
- Sessions: 2-3, depending on how cleanly the existing pancetta sync
  infrastructure abstracts the two-baseline pathway.

## Implementation notes for the implementer thread

- The current pancetta sync lives in `pancetta-ft8/src/decoder/sync.rs`
  (verify path before writing). The two-baseline pathway should be a
  new branch, not a replacement; wire it through a feature flag during
  bring-up so corpus regression is bounded.
- The `(jpeak - 0.5) * tstep` dt conversion must use a *signed* lag.
  Watch for off-by-one in the `±62` sweep — mainline uses inclusive
  bounds.
- The `40th percentile` baseline can be implemented with a partial sort;
  no need to fully sort the per-bin scores.
- Tests for this should include a hand-constructed WAV with a signal at
  `dt = -1.5 s` (well into the wide-lag-only regime). Mainline should
  emit a candidate; pancetta-pre-change should miss it.
- Do NOT mutate the input spectrogram with the `20.0 / max(s)` rescale;
  pancetta's TUI uses the raw spectrogram. This is a mainline-specific
  GUI hack; skip it.
