# Algorithm spec: JTDX napwid frequency-window AP gating

## Source attribution
- Origin: JTDX (https://github.com/jtdx-project/jtdx)
- File paths (for traceability, NOT to be quoted):
  - `lib/ft8b.f90` (the `loutapwid` flag computation at ~line 946,
    the various `dfqso.lt.napwid` guards at ~lines 586, 599, 649,
    743, 1452, 1456, and the inner-AP-cascade `cycle` statements
    gated on `loutapwid` at ~lines 993, 999, 1109, 1115, 1272)
  - `mainwindow.cpp` (where `napwid` is set per HF/VHF/UHF band:
    line 3323-3325)
  - `commons.h` (where `napwid` appears in the decode parameters
    struct)
- License: GPL-3.0
- Reader date: 2026-06-08

**Status note**: this spec is included **for reference**. Earlier
pancetta research (Batch 30+, see CLAUDE.md context: hb-062 and
hb-103 wired into autonomous TX) has shown that pancetta's AP
cascade is not a current source of FPs at the operating threshold.
Therefore the `napwid` gating mechanism is not a recommended port
*today*. The spec exists so that if pancetta ever expands its AP
cascade (e.g. adopts JTDX-style AP types 3-30) and FPs become an
issue, this is the documented JTDX precedent for restricting them.

## Purpose

JTDX has a large set of "additional priors" (AP) that the
bp/OSD decoder uses to brute-force partial codewords when a hard
decode fails. The full AP set covers ~31 message templates:
- Type 1: CQ ??? ???
- Type 2: MyCall ??? ???
- Type 3: MyCall DxCall ???
- Types 4-6: MyCall DxCall {RRR, 73, RR73}
- Types 11-14: nonstandard MyCall variants
- Types 21-24: Hound-mode variants
- Type 31: CQ DxCall (Grid)
- Types 35-36: ??? DxCall {73, RR73}
- Types 40-44: empty/compound MyCall variants

AP types 3 and above are *not* CQ-shaped — they assume both the
operator's callsign *and* the partner's callsign are present in the
message. When the candidate's audio frequency is far from either the
operator's RX frequency (`nfqso`) or TX frequency (`nftx`), these
templates are extremely unlikely to be right: a random station on
the other side of the band has no reason to be saying the operator's
callsign. Yet the AP cascade still tries them, which costs CPU and
risks FPs (the LDPC+AP combination can find spurious "valid"
codewords that match the AP template even when no real signal is
there).

The `napwid` mechanism is JTDX's solution: a frequency half-window
around RX and TX inside which AP types 3-30 are allowed. Outside the
window, those AP types are skipped entirely. AP types 1, 2, 31, 35,
36 (the CQ-shaped templates) remain available everywhere.

## Algorithm description (PROSE ONLY)

### Inputs

- `napwid`: integer Hz, the half-window around RX and TX.
- `f1`: audio frequency of the current candidate.
- `nfqso`: QSO partner audio frequency (operator's RX freq).
- `nftx`: operator's TX audio frequency.
- The various QSO state flags (`lqsosig`, `lmycsignal`, `lapmyc`,
  `ldxcsig`, etc) that determine which AP types are eligible to be
  tried at all.

### Outputs

- A boolean `loutapwid` per candidate: true means the candidate is
  outside the AP window and AP types 3-30 should be skipped.
- A handful of conditional behaviour changes when the candidate is
  inside the window (e.g. lower `ndxt` threshold for the DXCall
  signal, deeper OSD pass count).

### Steps

After symbol extraction and the various signal-pattern probes (CQ,
MyCall, DXCall pattern detection), `ft8b` computes:

`loutapwid = (|f1 - nfqso| > napwid) AND (|f1 - nftx| > napwid)`

That is, the candidate is "outside the AP window" only if it is
outside the window around *both* RX *and* TX. A candidate sitting on
the partner's frequency is in window; a candidate at the operator's
TX freq is in window; anything else on the band is out of window.

The inner AP cascade then guards the AP-3-through-30 branches with:

`if (iaptype > 2 AND iaptype < 31 AND loutapwid) cycle`

This skips AP types 3-30 entirely when the candidate is out of
window. AP types 1, 2 (CQ and MyCall templates that *only* require
the operator's callsign, not the partner's) and AP types 31, 35, 36
(DXCall templates used for casual DX hunting, not active-QSO
templates) remain available across the full band.

Several other branches use `napwid` more narrowly:

- The `lqsosig` / `lmycsignal` detection thresholds get a relaxed
  `ndeep = 4` (vs default 3) when `dfqso < napwid` or
  `|f1 - nftx| < napwid` and `lapmyc` is true.
- The DXCall search uses a relaxed `ndxt > 4` threshold (vs
  default `ndxt > 5`) when `dfqso < napwid`, allowing the AP cascade
  to fire on weaker DXCall signal patterns inside the window.
- The QSO-end check (looking for 73 / RR73 / RRR endings) is gated
  on `dfqso < napwid` — only candidates inside the window are tested
  for "end of QSO" patterns. This is because outside the window the
  candidate is presumed to be from a different station who would not
  be ending *the operator's* QSO.
- The DXCall-search inside `stophint` mode (idle: not actively in a
  QSO) gets a deeper `ndeep = 4` only when `dfqso < napwid`.
- The QSO-candidate averaging (`lqsocandave`) requires
  `dfqso < napwid/2.0` — a stricter half-window for the most
  aggressive averaging path.

### Numerical constants (facts, not expression)

`napwid` is set per band class in `mainwindow.cpp`:

- **HF** (`m_freqNominal < 30 MHz`): `napwid = 5 Hz`.
- **VHF** (`30 MHz ≤ m_freqNominal < 100 MHz`): `napwid = 15 Hz`.
- **UHF** (`m_freqNominal ≥ 100 MHz`): `napwid = 50 Hz`.

(Note: the task description in this reader thread's brief mentioned
"napwid (default 75 Hz)". The released JTDX source has 5/15/50,
not 75. The value the operator sees in normal FT8 operation on HF
is **5 Hz**. The 75 Hz figure may have been from an earlier JTDX
version or from a different mode; what ships today is 5 Hz on HF.)

Related thresholds inside the AP window:

- AP-half-window for `lqsocandave`: `napwid/2.0` = 2.5 Hz on HF.
- Deep-decode bump inside window: `ndeep` 3 → 4.
- DXCall search relaxation inside window: `ndxt > 4` (vs 5).
- DXCall search relaxation in idle mode inside window: `ndeep = 4`
  is set only when `dfqso < napwid`.

### Edge cases

- The bands the operator is *not* tuned to don't affect `napwid`
  because JTDX recomputes `napwid` per slot from the current dial
  frequency `m_freqNominal`. Operators jumping from HF to VHF mid-
  session get the new `napwid` on the next decode call.
- `napwid` is symmetric around both RX and TX. The "in-window" set
  is the *union* of two windows: `|f1 - nfqso| < napwid` OR
  `|f1 - nftx| < napwid`. When split mode is off (RX = TX), this
  collapses to a single window.
- `napwid` does NOT gate AP types 1, 2 (CQ/MyCall) or 31, 35, 36
  (DXCall templates used for DX hunting). Those run across the full
  decode band regardless. JTDX considers those templates safe to
  fire band-wide because they don't assume an active QSO partner.
- The Hound-mode AP types (21-24) are *not* gated by `napwid` —
  Hound mode uses a different code path that always considers the
  operator's TX freq as the relevant locus.
- When `dfqso < napwid/2.0` (the inner half-window), additional
  averaging paths activate (`lqsocandave`, isubp1 = 9..11). These
  are CPU-expensive but only fire on candidates very close to the
  partner.

## Conflict with pancetta's existing mechanisms

Pancetta's AP cascade (in `pancetta-ft8/src/decode/ap.rs` and
related) currently runs a smaller set of AP types than JTDX:
- CQ template (analogous to JTDX type 1).
- DX call template (partial analogue to type 31).
- Hash-based callsign templates (analogous to types 35-36).

Pancetta does **not** currently run the analogues of JTDX's AP types
3-6 (`MyCall DxCall ???` family) because:
- pancetta-qso's `is_plausible` filter rejects messages that don't
  match the operator's QSO state (cross-validation via callsign
  trust set, hb-062).
- pancetta's hb-103 sibling-API filter at `SHIP_CONSERVATIVE`
  threshold already prunes the bulk of low-confidence decodes.

So the `napwid` gating is targeting AP types pancetta doesn't yet
run. If pancetta ever adds JTDX-style AP types 3-6, this spec
becomes immediately relevant.

The `napwid` mechanism's other use — the deeper-decode bumps
(`ndeep = 4`, `ndxt > 4`) inside the window — has a partial
pancetta analogue: the per-candidate decision of how many bp
iterations to run is currently uniform across the band. A pancetta
port of *that* part of the mechanism (the windowed deeper decode)
could be useful as a CPU-budgeting tool: spend extra cycles on
candidates near the active QSO partner, not on the rest of the
band.

The `lqsocandave` half-window mechanism (`dfqso < napwid/2.0`)
overlaps with pancetta's existing partner-aware mechanisms (the
relaxed-sync-near-partner spec) and provides a useful precedent for
how aggressive averaging should scale with frequency distance.

## Estimated Rust port effort

(Only counted because the user explicitly asked for the spec; not a
recommended port today.)

- ~30 LOC to add a `Napwid` config struct with band-class defaults
  (HF=5, VHF=15, UHF=50) and a runtime accessor in pancetta-config.
- ~50 LOC to add a `is_in_ap_window(candidate)` method on
  `pancetta-ft8::decode::Context` that checks `|f1 - rx| < napwid OR
  |f1 - tx| < napwid`.
- ~80 LOC to gate AP types 3-6 (when added) on
  `is_in_ap_window(candidate)`.
- ~100 LOC of tests.
- 1 implementation session, contingent on the AP types being
  implemented first.

Total: ~260 LOC, 1 session, *prerequisite*: JTDX-style AP types
3-30 must exist in pancetta first.

## Implementation notes for the implementer thread

- **Do not port this spec today.** Pancetta's AP cascade is not
  currently FP-limited at the operating threshold; adding the gating
  mechanism without the underlying AP types it gates is a no-op.
- If a future hypothesis (e.g. an hb-300 series on AP-3-6) graduates,
  this spec is the JTDX precedent for restricting those types.
  Port the gating mechanism *in the same change* that adds the AP
  types — never ship the AP types band-wide first and gate later,
  because the FP signature on the un-gated path will pollute the
  scorecard baseline.
- The HF default of 5 Hz is tight. Combined with FT8's 6.25 Hz
  symbol spacing and pancetta's existing 3 Hz near-partner zone in
  the relaxed-sync spec, 5 Hz is essentially "the partner and
  nothing else". This is intentional — JTDX is making the trade
  that AP types 3-6 should *only* fire on the partner, not on
  band-adjacent stations.
- The mechanism is independent of pancetta's existing sender-callsign
  verification (which lives at `pancetta-qso/src/qso_manager.rs`
  per the 2026-04-29 security review): one is a structural filter on
  who can advance the QSO state, the other is a structural filter on
  what messages the decoder is allowed to *find*. Both can ship
  together with no overlap.
- The `napwid/2.0` inner half-window for `lqsocandave` is an
  interesting precedent for any future pancetta partner-averaging
  mechanism: the doubled-tightening of the locus when running the
  most aggressive averaging suggests JTDX considers the inner
  half-window the "definitely my partner" zone vs the outer window
  as the "probably my partner" zone.
