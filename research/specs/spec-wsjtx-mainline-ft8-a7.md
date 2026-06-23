# Algorithm spec: WSJT-X mainline ft8_a7 — cross-sequence (deferred chronological) decoder

## Source attribution

- Origin: WSJT-X mainline (K1JT et al., upstream)
- Repository: https://sourceforge.net/p/wsjt/wsjtx/ci/master/tree/
- Mirror: https://github.com/saitohirga/WSJT-X
- File (traceability only; NOT quoted): `lib/ft8/ft8_a7.f90` (~378 LOC)
- Companions: invoked from `lib/ft8_decode.f90`; calls
  `lib/ft8/ft8_downsample.f90`, `lib/ft8/sync8d.f90`.
- License: GPL-3.0
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

The "a7" pass is FT8's cross-sequence (cross-slot) decoder. It uses
*decodes from the previous 15-second slot* as a priori constraints to
re-attempt decodes in the current slot at the same frequency. The
typical use case: in slot N you decoded `K5ARH W1XYZ FN42`; in slot
N+1 there's a weak signal at the same frequency that's clearly W1XYZ
replying but too weak for normal decode. The a7 pass enumerates all
plausible "next-message-in-the-QSO" hypotheses (RRR, RR73, 73, grid,
+SNR, -SNR, R+SNR, R-SNR), brute-force scores each against the soft
demod, and picks the best.

Wsjtr has a partial port (`spec-wsjtr-cross-sequence-a7.md`); this is
the canonical source. Mainline's version has 206 hypotheses per call
which wsjtr's docs may not enumerate exhaustively.

## Inputs (`ft8_a7d`)

- `dd0` — raw audio (15 s @ 12 kHz).
- `newdat` — long-FFT-redo flag.
- `call_1`, `call_2` — the two callsigns from the previous-slot decode.
- `grid4` — the grid from the previous-slot decode, or blank/RR73 if
  none/non-grid.
- `xdt`, `f1` — saved time and frequency offsets from the previous-
  slot decode.
- `xbase` — local baseline power (from `sbase`).

## Outputs

- `nharderrors` ≥ 0 on success, -1 on failure.
- `dmin` — soft distance for the best hypothesis.
- `msg37` — the best-matching message (37 chars).
- `xsnr` — SNR estimate.

## Numerical constants

- `MAXMSG = 206` — number of message hypotheses enumerated.
- `dmin > 100.0` → reject (low confidence).
- `dmin2 / dmin < 1.3` → reject (second-best is too close to best —
  ambiguous decode).
- SNR formula: `xsnr = max(-24.0, 10*log10(pbest/xbase/3e6 - 1) - 27)`.
- Hypothesis generator scans:
  - `i = 1`: bare `call_1 call_2`.
  - `i = 2`: `... RRR`.
  - `i = 3`: `... RR73`.
  - `i = 4`: `... 73`.
  - `i = 5`: `CQ <call_2>` (with grid4 appended if not RR73).
  - `i = 6`: `call_1 call_2 grid4`.
  - `i = 7..206`: signal reports `±SNR` (with and without "R" prefix).
    The SNR range is `isnr = -50 + (i-7)/2`, alternating
    `+/-SNR` (i odd) and `R+/-SNR` (i even). With 200 iterations of
    `i ∈ [7, 206]`, the SNR range covers approximately `-50` to `+50`
    in 1-dB increments, each in both "send-snr-only" and "R-prefix"
    forms.

The "non-standard call" branch (`std_1` or `std_2` is false) wraps the
non-standard call in `<…>` brackets per FT8's hashed-call convention.

## Algorithm description (prose only)

### Step 1: standard front-end (fine sync + per-symbol DFTs)

This block is *almost identical* to `ft8b`'s steps 1-5 (downsample,
fine dt/frequency refinement, per-symbol DFTs). The only differences:

- The sync quality check (`nsync <= 6`) is **commented out** in the
  source — a7 doesn't bail on weak sync. The rationale: the previous-
  slot decode already established that there's a signal at this
  `(f1, xdt)`; we trust it even if the current slot's sync is weak.
- Final `xdt` is reported as `(ibest - 1) * dt2 - 0.5` (the `-0.5` is
  applied right here, not by the caller).

### Step 2: four bit-metric variants (identical to ft8b)

Same as ft8b's step 7 — variants A, B, C, D with `nsym ∈ {1, 2, 3}`
and the bit-by-bit-normalized D variant. Same scale factor 2.83 → LLR.

### Step 3: brute-force hypothesis enumeration

For `imsg = 1` to `206`:

**Build the candidate message string `msg`:**

The message-builder logic is contest-aware (`std_1` and `std_2` flags
indicate whether each call is a "standard" amateur callsign or
something hashed).

Standard pattern:
- `i = 1`: `call_1 call_2` (bare).
- `i = 2`: `call_1 call_2 RRR`.
- `i = 3`: `call_1 call_2 RR73`.
- `i = 4`: `call_1 call_2 73`.
- `i = 5`: `CQ call_2` (or `CQ DX call_2` if call_1 indicates DX call
  pattern) plus `grid4` if present and not 'RR73'.
- `i = 6`: `call_1 call_2 grid4` (only if std_2 — non-standard calls
  can't have grids).
- `i ∈ [7, 206]`: signal reports. `isnr = -50 + (i-7)/2`.
  - Odd `i`: `call_1 call_2 ±SNR` (e.g., `+05`, `-12`).
  - Even `i`: `call_1 call_2 R±SNR`.

Non-standard call handling:
- If `std_1 == .false.`: wrap call_1 in `<…>` brackets in the message.
- If `std_2 == .false.`: wrap call_2 in `<…>` brackets, only for some
  message indices.
- If neither is std: special handling per `i` to ensure the right call
  is the "subject" (hashed call gets brackets).

**Special quirk for `i = 5` with `call_1 starting with "CQ"`:** the
message is forced to `"QU1RK call_2"` — this is a sentinel that the
final filter rejects, ensuring CQ messages aren't mis-handled.

**Encode the candidate message:**
- `genft8(msg, i3, n3, msgsent, msgbits, itone)` — source-encode the
  message into 77 bits and 79 channel symbols.
- `encode174_91(msgbits, cw)` — apply LDPC + CRC encoding to get the
  full 174-bit codeword.

**Compute signal power for SNR estimation:**
- `pow = sum(s8(itone(i), i)^2)` for i=1..79 — power at the candidate's
  tone positions. The candidate with the highest `pow` is closest to
  the signal.

**Compute four soft distances:**
For each of the four LLR variants (a, b, c, d):
- Hard-decision the LLR: `hdec = (llr >= 0)`.
- XOR with the candidate codeword: `nxor = hdec XOR cw`.
- Soft distance: `da = sum(nxor * |llra|)`.

Take the minimum: `dm = min(da, dbb, dc, dd)`. This is the best
distance any of the four variants achieves for this candidate.

If `dm < dmin`: update best hypothesis. Save `msgbest = msgsent`,
`pbest = pow`, and the `nharderrors` count from the *winning* variant
(whichever of a/b/c/d achieved `dm`).

### Step 4: ambiguity check

After all 206 hypotheses:
- Find the second-smallest `dm` (`dmin2`).
- If `dmin > 100` → reject (no hypothesis fits well enough).
- If `dmin2 / dmin < 1.3` → reject (best and second-best are too
  close — the signal is consistent with multiple incompatible
  messages, so we can't be sure).

The `1.3` ratio test is the load-bearing reliability gate. Without
it, a7 would emit a high false-positive rate.

### Step 5: SNR estimate

- `arg = pbest / xbase / 3e6 - 1.0`.
- If `arg > 0`: `xsnr = max(-24.0, 10*log10(arg) - 27.0)`.
- Else: `xsnr = -24`.

The `3e6` and `-27` calibration constants match `ft8b`'s baseline-SNR
formula.

### Step 6: post-filter rejections

- If the message is `"CQ … "` with no grid and `std_2 == .true.` →
  reject (bare CQ without grid isn't useful as a cross-slot decode).
- If the message starts with `"QU1RK "` → reject (the sentinel from
  Step 3 case `i = 5` with CQ-prefixed call_1).

### Step 7: save state for next slot

`ft8_a7_save` is called by the caller (`ft8_decode`) on every
successful decode. It stores:
- `dt0(decode_index, slot_parity, current_or_previous) = dt`
- `f0(...) = f`
- `msg0(...) = trimmed message ("call_1 call_2 [grid]")`

The third index `0/1` is "previous tally for this slot parity" /
"current tally for this slot parity". On slot transition (per
`ft8_decode`'s startup logic), the current → previous shift happens.

**Important detail: cross-sequence duplicate prevention.** When saving
a new decode at slot N+1 frequency `f`, the routine walks the
previous-slot decodes at the same parity. If any of them are at the
same frequency (±3 Hz) AND their message contains the same `call_2`
fragment, the previous-slot entry is flagged as `f0 = -98.0`
("do not use for a7"). This prevents the next slot's a7 pass from
re-decoding the same exchange twice.

Skip-conditions for saving (don't save the decode to the a7 table):
- Message contains `/` (compound callsign — too complex for a7).
- Message contains `<` (hashed call already — handle differently).
- First word is `CQ_` with underscore (non-standard CQ — Field Day,
  state QSO party, etc., have different a7 patterns).

## What wsjtr's docs paraphrase or miss

1. **206 message hypotheses, not "8 or 10".** wsjtr's
   `spec-wsjtr-cross-sequence-a7.md` covers the structural cases
   (RRR, RR73, 73, grid, CQ) but doesn't enumerate the 200 SNR-report
   hypotheses (`i = 7..206`). Those are why mainline catches signal
   reports as cross-sequence decodes.
2. **The 1.3 ratio reliability gate is critical.** wsjtr's spec
   mentions a confidence check; the *exact* threshold is
   `dmin2/dmin >= 1.3`. Pancetta should match this.
3. **`dmin > 100` is the absolute reject threshold** — the absolute
   floor below which no hypothesis is good enough, independent of
   ambiguity.
4. **The `nsync <= 6` early bail is commented out in a7.** Normal
   `ft8b` rejects weak sync immediately; a7 trusts the previous-slot
   sync. This is the mechanism that lets a7 catch signals too weak
   for `ft8b`.
5. **a7 uses all FOUR LLR variants** (a/b/c/d) per hypothesis and
   takes the minimum distance. ft8b runs them as *separate passes*
   (one variant per pass, returning on first success). a7 evaluates
   all four per hypothesis, picking the best. This is a key efficiency
   trade-off and changes how the LLR variants combine.
6. **The non-standard-call wrapping rules are intricate.** When
   call_1 OR call_2 is hashed (non-standard), the wrap-in-brackets
   logic per-hypothesis is conditional on which call is hashed and
   what hypothesis index `i` is. There are 6 distinct sub-cases just
   for the call-wrapping logic.
7. **The `QU1RK` sentinel** is used to forcibly skip the `i = 5`
   case when call_1 starts with "CQ" — the message is built with
   "QU1RK" as a marker that the post-filter rejects. This is a
   purpose-built rejection mechanism, not an accident.
8. **Cross-slot duplicate prevention is at the *save* path, not the
   *decode* path.** When you save slot N+1's decodes, you scan back
   to slot N and mark any matches as "do not use". So when slot N+2's
   a7 pass runs, the already-handled slot-N decodes are skipped.
9. **The save logic is parity-keyed** (`jseq = mod(nutc/5, 2)`) — even
   vs odd slots are tracked separately. This matches FT8's "TX-on-even"
   vs "TX-on-odd" cadence: a station calling CQ on even slots has its
   call appear in even-slot decodes; its replies appear in odd slots.
   Pancetta's QSO state machine already tracks slot parity — verify
   the a7-equivalent uses it.
10. **`ft8_a7` table sizes:** `MAXDEC = 100` per `(slot_parity,
    current/previous)` slot. So up to 200 cross-slot context entries
    are retained per parity over the most recent two slots.

## Conflict with pancetta's existing mechanisms

- Pancetta does not currently implement cross-sequence/a7 decoding.
  Per MEMORY: "hb-173 (within-QSO context, deferred-chronological)"
  in the bank — this spec is precisely that hypothesis.
- The 206-hypothesis enumeration is the load-bearing piece. If pancetta
  implements a simplified version with only the structural cases
  (RRR/RR73/73/grid), it'll catch the structural cross-slot decodes
  but miss the SNR-report cross-slot decodes (which are common in
  active QSOs).
- The integration is at the QSO state machine level: when pancetta's
  state machine is in "WaitForReport" state for partner W1XYZ, it
  could feed `(call_1=mycall, call_2=W1XYZ, grid4=...)` into the a7
  decoder for the next slot's audio at W1XYZ's frequency.

## Estimated Rust port effort

- a7 front-end (reuse ft8b's sync + demod): ~50 LOC of new glue.
- 206-hypothesis enumerator + message-builder: ~250 LOC (the
  non-standard-call branching is the bulk).
- Four-variant minimum-distance scorer: ~50 LOC.
- a7 save/load state machine: ~150 LOC.
- Total: ~500 LOC of new Rust, plus integration with pancetta's QSO
  state machine.
- Sessions: 2-3.

## Implementation notes for the implementer thread

- The a7 front-end (downsample, fine sync, per-symbol DFT) is the
  same as ft8b's. Refactor that block out of ft8b first; share it
  between ft8b and a7.
- The 206-hypothesis loop is naturally parallel — each hypothesis is
  independent. Rayon over the hypothesis space; this gives a
  ~4-8× speedup.
- The non-standard-call (`<call>`) handling needs `stdcall` (verify
  callsign matches standard FT8 format) — pancetta-qso almost
  certainly has an equivalent.
- The duplicate-prevention save logic requires tracking previous-slot
  decodes by `(slot_parity, frequency_bin, call_fragment)`. Pancetta
  could use a `HashMap<(SlotParity, FreqBin), Vec<DecodeEntry>>`.
- Reliability gates (`dmin > 100`, `dmin2/dmin < 1.3`) must be
  exact — they're tuned to avoid false positives at the expense of
  some real cross-slot decodes.
- Skip `/` and `<` in the source message for save-path (don't save
  compound or hashed calls). Match mainline exactly here.
- Test fixture: a pair of consecutive 15-second WAVs where the
  signal is decodable in slot N (gives `call_1 call_2 grid`) and
  marginal in slot N+1 (only a7 with the slot-N context should
  catch it).
