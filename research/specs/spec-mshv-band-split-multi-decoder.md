# Algorithm spec: MSHV band-split multi-thread FT8/FT4 decoder

## Source attribution

- Origin: MSHV (Hrisimir Hristov, LZ2HV)
- File paths (for traceability; no code quoted or paraphrased line-by-line):
  - `src/HvDecoderMs/decoderms.h` — class layout, six `DecoderFt8 *DecFt8_0..5`
    members, six `pthread_t th0..th5` workers, shared static buffers
    `static_dat0..5`, the `s_thr_used` count.
  - `src/HvDecoderMs/decoderms.cpp` — orchestration: per-worker entry points
    `StrtDec0..StrtDec5`, the POSIX-thread trampolines `ThrDec0..ThrDec5`,
    the band-split / bandwidth-floor / boundary-nudge dispatch body
    (the second branch of the `SetDecode` body after the input has been
    de-meaned and faded in).
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

Run the per-15-second-period FT8 decoder (or FT4 / experimental FT2 decoder)
in parallel over disjoint sub-bands of the audio passband. Each worker
sees the full-period audio buffer (a verbatim copy) but only emits
candidates whose carrier falls inside its assigned sub-band. The
orchestrator deduplicates candidates across workers by message-text +
frequency tolerance before publishing decode results to the UI / network.

This trades a fixed amount of memory (six 184k-sample int buffers, ~5 MB
per copy in the audio path plus per-worker scratch) for end-to-end decode
wall-clock proportional to `band_width / nthreads` instead of `band_width`.
On a 200-3200 Hz operator band and a 6-core host the wall-clock budget per
period drops from ~3000 Hz of search to ~500 Hz of search per core,
absorbing the deeper-pass / OSD cost.

## Inputs

- `dd[NMAX]`: real-valued 12-kHz-sampled audio for the whole period, with
  `NMAX = 180_000` samples for FT8 (15 s at 12000 Sa/s), `c_arrrs = 184_000`
  samples actually staged (the trailing few seconds tolerate noise tail).
  FT4 uses 96000 samples (8 s); FT2 uses 48000 samples (4 s).
- `s_f00, s_f01`: the user's audio-band passband endpoints in Hz. Default
  `s_f00 = 200`, `s_f01 = 3200`. Operator-adjustable.
- `s_nfqso_all`: the user's RX-frequency cursor in Hz. Default `1270.46`.
- `s_thr_used`: an integer in `[1, 6]`, clamped at construction time. Set
  via the `SetThrLevel(int i)` slot.
- `s_mode`: the active mode enum: `11 = FT8`, `13 = FT4`, `18 = FT2`.

## Outputs

- Per-worker: the same emit-decoded-text signal each `DecoderFt8` instance
  emits in single-thread mode (`EmitDecodetTextFt(QStringList)`), routed
  through `SetDecodetTextFtQ65` for orchestrator-side dedup.
- Side effect: `have_dec0_..have_dec5_` booleans, used by `TryEndThr` to
  detect "all workers complete" and tear down the period.

## Steps (prose)

### 1. Worker preallocation (one-time at `DecoderMs` construction)

Six `DecoderFt8` instances, each given a distinct `id` from 0 to 5, are
allocated up front. Equivalent fan-outs exist for `DecoderFt4` and
`DecoderFt2`. Each instance owns its private scratch buffers (FFT
workspace, candidate arrays, sync templates) but shares all
QSO-progress / hash / contest-type state with the others via `static`
class members on `DecoderFt8` (so that e.g. the partner-call hash table
is consistent across workers).

### 2. Period entry: validate input and decide on serial-vs-parallel

When a new period of audio is delivered through `SetDecode`, the
orchestrator:

1. Rejects buffers shorter than the per-mode minimum (FT8: 6 s of 12-kHz
   samples; FT4: 4 s; FT2: 2 s) and silently returns.
2. Sets a global `thred_busy` flag immediately to drop overlapping period
   requests.
3. Computes the noise floor over the buffer and applies a small dB
   normalization so the worker-side dynamic range is stable across
   periods.
4. If the mouse-driven re-decode flag is set (right-click "decode again"
   at a specific frequency), forces `nthr = 1` — the whole band is
   collapsed to a narrow window of `±25 Hz` around the requested
   frequency, which would not benefit from band-splitting.
5. Otherwise calls the band-split planner (Step 3).

### 3. Band-split planner: floor-decrement on per-thread bandwidth

A nominal worker count `nthr = s_thr_used` is taken. A per-mode minimum
sub-band width is enforced:

- FT8 (`s_mode == 11`): `limit = 300 Hz`
- FT4 (`s_mode == 13`): `limit = 400 Hz`
- FT2 (`s_mode == 18`): `limit = 500 Hz`

Then a cascade decrements `nthr` whenever `band_all / nthr < limit`:

```
band_all = s_f01 - s_f00
for k in {6, 5, 4, 3, 2}:
    if nthr == k and band_all/nthr < limit:
        nthr -= 1
thrsum = band_all / nthr     # final per-worker bandwidth
```

The cascade is written as five sequential `if` statements rather than a
loop in MSHV's source; the net effect is the same.

Notes:
- The minimum-bandwidth floor is per-mode because FT4 / FT2 carriers
  are wider in tone spacing and the candidate search needs more margin
  on each side.
- The cascade can collapse `nthr` all the way back to 1 (in which case
  Step 6 takes the single-thread path).
- The cascade does NOT consider symbol-rate or per-decoder cost, only
  band width.

### 4. Boundary-nudge around the user's RX frequency

The user's RX frequency `s_nfqso_all` is computed as a fraction of the
operator passband and would otherwise sometimes land exactly on a
sub-band boundary, where the candidate-search FFT roll-off would
penalize it. To avoid this:

- Initialize the per-mode "correction" constant: FT8 `corf = 25 Hz`,
  FT4 `corf = 50 Hz`, FT2 `corf = 100 Hz`.
- Tentatively place each interior boundary at the uniform position
  `_f01_ = _f00_ + thrsum`, `_f02_ = _f01_ + thrsum`, ..., `_f06_ = ...`.
- For each tentative boundary, if `|s_nfqso_all - boundary| < 11 Hz`,
  subtract `corf` from the boundary (shifting it left by 25/50/100 Hz).
  Otherwise reset the per-edge correction `tcorf = 0` so subsequent
  edges aren't compounded.
- The next boundary is computed from the *un-shifted* previous boundary
  (`_f02_ = _f01_ + thrsum + tcorf`), which makes the shift local to one
  edge — the sub-band on the high side of the nudged edge picks up the
  shifted 25/50/100 Hz of bandwidth, so no signal is dropped.

The effect: the user's RX cursor always sits at least ~12 Hz from any
boundary, well inside one worker's search range.

### 5. Per-worker overlap correction

When a worker is invoked, its lower edge is the previous worker's upper
edge minus an overlap constant:

- FT8: `CORFT8 = 60 Hz` — each worker actually scans
  `[_f0(k-1)_ - 60 Hz, _f0(k)_]` Hz.
- FT4: `CORFT4 = 100 Hz`
- FT2: `CORFT2 = 200 Hz`

The first worker (decid=0) gets `[s_f00, _f01_]` with no left-overlap.
This overlap absorbs the candidate-search FFT side-lobes and makes the
later orchestrator dedup robust: a strong signal near the boundary is
allowed to be detected by both adjacent workers; the dedup pass below
collapses to one emission.

### 6. Buffer fan-out and POSIX thread spawn

If `nthr == 1`: spawn a single `pthread_t th` running the
`ThreadDecode` trampoline, which calls `StrtDecode` (the original
single-worker entry).

If `nthr > 1`:
1. Memcpy the prepared `static_dat0` buffer to each of `static_dat1`,
   `static_dat2`, ..., `static_dat(nthr-1)` for as many samples as the
   mode uses (`c_arrrs`). This is a full-period verbatim copy per
   worker; MSHV does NOT share the buffer read-only.
2. For each used worker index `k` in `[0, nthr-1]`, set `have_dec(k)_ =
   false` and `end_dec(k)_ = false`.
3. Spawn `pthread_t th(k)` for each used worker, each pointing at
   `ThrDec(k)`, which is a static trampoline that calls into
   `StrtDec(k)` with the worker's buffer and `(_f0(k)_, _f0(k+1)_)`
   sub-band edges.
4. Each worker entry calls a fixed `usleep((17 + k) * 1000)` (i.e.
   17 ms, 18 ms, 19 ms, ...) immediately on entry. This staggers the
   workers by single-digit milliseconds so that the FFTW (or whichever
   FFT lib) thread-locked plan allocator doesn't contend.

### 7. Per-worker decode and dedup-via-emit

Each worker runs the standard `ft8_decode` body (or `ft4_decode` /
`ft2_decode`) with sub-band edges `(f0a, f0b)` set from
`(_f0(k)_ - CORFT8, _f0(k+1)_)`. The worker emits one
`QStringList` per accepted decode through `EmitDecodetTextFt`. The
orchestrator's `SetDecodetTextFtQ65` slot:

1. Looks up each emission in a per-period duplicate table
   `dup_amsgs_thr[MAXDUPMSGTHR]`, `dup_afs_thr[MAXDUPMSGTHR]` keyed on
   `(message_text, frequency_int)` with a tolerance of `f1tool = 6 Hz`
   for FT8 / `10 Hz` for FT4. `MAXDUPMSGTHR = 240` (raised from 120 in
   2.70).
2. Drops duplicates silently.
3. Inserts unique messages into the dedup table and forwards to the
   display / PSK Reporter / UDP broadcast.
4. The dedup table is reset (`ResetDupThr`) at the end of each period.

### 8. Period teardown

Each worker, on exit, sets its `end_dec(k)_ = true` and calls
`TryEndThr`. `TryEndThr` checks "are all `end_dec(0..nthr-1)_` true?";
if so it tears down the period's UI state, resets `dup_*` tables, and
clears `thred_busy`. This pattern makes the orchestrator stateless
between periods: any failed worker just delays the teardown but does
not corrupt state.

## Numerical constants (facts, not expression)

- Worker count maximum: `MAXWORKERS = 6` (compile-time, encoded as
  `s_thr_used` clamped to `[1, 6]` and as six declared `pthread_t`).
- Per-worker bandwidth floor:
  - FT8: `300 Hz`
  - FT4: `400 Hz`
  - FT2: `500 Hz`
- Boundary nudge trigger distance: `11 Hz` (i.e. nudge if the user RX
  cursor is within ±11 Hz of a planned boundary).
- Boundary nudge magnitude (`corf`):
  - FT8: `25 Hz`
  - FT4: `50 Hz`
  - FT2: `100 Hz`
- Per-worker left-overlap (`CORFT*`):
  - FT8: `60 Hz`
  - FT4: `100 Hz`
  - FT2: `200 Hz`
- Per-worker initial sleep: `17 + decid` ms (i.e. 17, 18, 19, 20, 21,
  22 ms) to stagger FFT-plan setup.
- Duplicate-message tolerance:
  - FT8: ±6 Hz on carrier frequency
  - FT4: ±10 Hz on carrier frequency
  - (Identical message text required.)
- Duplicate table size: `MAXDUPMSGTHR = 240`.
- Audio range: default `s_f00 = 200 Hz`, `s_f01 = 3200 Hz`.
- Single-thread fallback triggers: right-mouse re-decode (any mode), or
  Super Fox mode, or `nthr` decremented to 1 by the floor cascade.

## Edge cases

- **Right-mouse re-decode**: the operator clicks a single signal on the
  waterfall to re-decode. This forces `nthr = 1` and narrows the band
  to ±25 Hz around the clicked frequency. The six-thread machinery is
  skipped entirely.
- **Operator narrows passband below `nthr * limit`**: the floor cascade
  silently reduces `nthr`. The user is not warned. The "Threads"
  spinbox UI keeps reading the original value but the actual decode
  uses fewer workers.
- **Boundary nudge underflow**: if the user RX cursor is within ±11 Hz
  of `s_f00` (the leftmost passband edge), the nudge subtracts 25 Hz
  from a boundary that might fall below `s_f00`. The code does not
  clamp; the worker's `nfa = fmax(100, f0a)` clamp at the inner FT8
  decoder is the safety net.
- **Worker leak on early exit**: each worker calls `pthread_detach`
  inside its trampoline. Memory is reclaimed by the OS on worker exit
  regardless of orchestrator state.
- **Super-Fox mode collapses to 1 thread**: `f_dec_sfox` is checked
  before the multi-thread fan-out and forces `nthr = 1`. Super Fox is
  inherently a "decode many simultaneous DXpedition signals from one
  buffer" decoder, not a band-split one.

## Conflict with pancetta's existing mechanisms

Pancetta currently runs a single decode pass per period (see
`pancetta-ft8` — the decoder is invoked once per period over the full
passband). The hb-091 scoped fast-path and hb-216 hardware-tier
classifier exist to *speed up* the single pass on slower hardware, not
to fan it out. Wiring band-split would be an additive change:

- The `ApplicationCoordinator` would gain a worker count akin to MSHV's
  `s_thr_used`, derivable from `cpu_count` (e.g. `min(cpu_count - 1,
  6)`).
- The audio buffer for each period would be cloned `nthr` times into
  per-worker scratch.
- The per-worker decode invocation would set `(f0a, f0b)` in place of
  the current global `(passband_low, passband_high)`.
- The `DecodeResultPipeline` (whichever module owns dedup) would need
  a per-period dedup table keyed on `(message_text, freq_int, ±6 Hz)`
  to suppress the overlap-region double-emissions.
- The `is_plausible` / hb-103 content score filters would run on the
  union, unchanged.

The interaction with pancetta's hb-091 scoped fast-path is benign: each
worker independently consults the shared `scoped_fast_path:
Arc<AtomicBool>` (no contention since it's read-only during a period)
and the shared `Ft8Config: Arc<RwLock<...>>` (one `try_read` per worker
on entry).

The interaction with pancetta's `SmartFrequencyAllocator` is benign:
TX-side frequency allocation is independent of RX-side band-split.

Capture-effect (hb-218 bank entry): MSHV's band split does NOT solve
capture-effect within a sub-band — two signals 25 Hz apart will both
land in the same worker. The reduction in per-worker bandwidth might
marginally help by giving each worker more FFT bins per Hz on a fixed
FFT length, but MSHV does not appear to have a specific capture-effect
mitigation tied to the multi-thread architecture.

## Estimated Rust port effort

- ~400-600 LOC of new code in `pancetta/src/coordinator/` and possibly
  `pancetta-ft8/src/decode/` for the band-split entry point.
- 2-3 sessions (initial fan-out + dedup, then tuning of the
  bandwidth-floor / nudge constants under pancetta's eval harness).
- Use `std::thread::scope` (preferred) or `rayon` rather than reproducing
  the manual `pthread_create` / `pthread_detach` pattern. The MSHV
  staggered-sleep workaround is unnecessary in Rust because
  `rustfft`'s planner doesn't need a stagger.

## Implementation notes for the implementer thread

- The natural insertion point in pancetta is the function that
  currently calls into the single-pass `Ft8Decoder` for each period.
  Wrap that call site, not the decoder itself, so the single-pass
  decoder remains an internal primitive.
- The bandwidth-floor / boundary-nudge constants are MSHV-tuned for
  WSJT-X's reference candidate search. Pancetta's candidate search is
  different (neural OSD + scoped fast-path), so the constants should
  be re-tuned in a Reader-thread-style eval pass before being treated
  as final. Specifically:
  - Re-run the hard-200 eval at `nthr = 1, 2, 3, 4` and confirm
    decoded-message count is monotonically non-decreasing and that no
    capture-effect-adjacent truths are lost across sub-band boundaries.
  - Re-tune `corf` for pancetta's specific FFT length / window if the
    boundary-nudge causes measurable artifacts.
- Pancetta's eval harness in `pancetta-research/` can verify the
  end-to-end behavior (decoded message set, false-positive rate) on
  the existing 200-WAV corpus with the band-split wired vs.
  baseline.
- Memory: at 184k int samples per worker, six workers cost ~4.4 MB
  per period of audio in copies. Trivial on M4 / MiniPC. Worth
  noting in the design but not a constraint.
- Threading: use `std::thread::scope` so the workers are joined before
  the audio buffer drops out of scope. Avoid `tokio::spawn` for the
  decode workers themselves — these are pure-CPU and would block the
  tokio runtime; spawn a `tokio::task::spawn_blocking` once per period
  that drives the scoped worker pool internally.
- Cite-as-inspiration in the eventual commit: "Band-split decoder
  fan-out inspired by MSHV's six-thread architecture; constants and
  edge handling adapted to pancetta's FFT pipeline."
