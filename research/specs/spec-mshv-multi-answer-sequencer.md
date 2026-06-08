# Algorithm spec: MSHV Multi-Answer Auto-Sequencing slot scheduler

## Source attribution

- Origin: MSHV (Hrisimir Hristov, LZ2HV)
- File paths (for traceability; no code quoted or paraphrased line-by-line):
  - `src/HvTxW/hvmultianswermodw.h` — `MAXSL` constant, `MultiAnswerModW`
    class shape, `ListA` (Queue and Now widget), `HvSpinBoxSlots`
    (slot-count input), `HvSpinBoxMTP` (max-time-periods input), the
    tx-id slot enum schema.
  - `src/HvTxW/hvmultianswermodw.cpp` — the `DecodeMacros` per-row state
    machine, `MakeSMsg` Special-MSG synthesizer, `SetAutoSort`,
    `RefreshLists`, `gen_msg`, the Queue/Now list orchestration,
    the CQ-on-free-slot and dupe-suppression logic.
  - `src/main_ms.cpp` — the two `SetMultiAnswerMod` / `SetMultiAnswerModStd`
    toggles wiring DXpedition vs Standard mode through to the decoder
    (`SetMultiAnswerMod(bool)`) and TX widget.
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

Manage up to `MAXSL = 6` simultaneous FT8/FT4 QSOs on a single radio in a
single time period, by:
- Maintaining a **Queue** of callers waiting for a slot,
- Maintaining a **Now** list of callers actively occupying a slot,
- Per-period emitting one FT8 message per Now-list row (each on its own
  audio-frequency carrier scheduled by the upstream TX path),
- Advancing each Now-list row through a small TX-message state machine
  (`+rpt → R+rpt → RR73`) tied to the per-correspondent QSO progress,
- Promoting Queue → Now when a slot frees,
- Optionally collapsing two adjacent state transitions ("close one QSO
  *and* greet a new caller") into a single Special-MSG to save a TX
  cycle.

This is the canonical contest / DXpedition "run many simultaneous QSOs
from one operator station" pattern. The same shape is what pancetta's
multi-stream TX in `pancetta/src/coordinator/tx.rs` wants to implement
in production.

## Inputs

- **DX list** (from decoder): a stream of decoded FT8 messages with
  `(from_call, to_call, payload, audio_freq_hz, snr_db, dt_s,
  grid_4char_or_none)`. The sequencer subscribes to this stream and
  decides whether each decode is a new caller, an in-progress
  correspondent's reply, or noise.
- **Operator config**:
  - `id_mshf` mode identifier (`0 = MAM disabled`, `1 = MA DXpedition`,
    `2 = MA Standard with Super-Fox tweaks`).
  - `nslot = SBslots->valueS()` ∈ `[1, MAXSL]`, configurable per mode
    (clamped to 4 for Super Fox + `s_msf_ftmsg`-true cases).
  - `SBqueueLimit->value()` ∈ `[0, 50]`, default 5: max Queue depth.
  - `SBmaxTP->value()`: max number of periods a Now-list row is allowed
    to occupy a slot before being kicked back to Queue or dropped.
  - `Cb_sort` ∈ `{Off, Distance, S/N}`: Queue auto-sort criterion.
  - `cb_tx_sm`: emit Special-MSG (combined RR73 + new-caller greeting)
    when possible.
  - `cb_tx_cq_on_free_slot`: emit CQ on any slot that would otherwise
    be silent.
  - `cb_otp_mamd_key` + 16-char OTP key: Super Fox authenticator
    (out of scope for pancetta).
  - `Cbcqtype` ∈ {`CQ`, `CQ MDX`, `CQ DX`, `CQ UP`, `CQ IOTA`, `CQ POTA`,
    `CQ SOTA`, `CQ BOTA`, `CQ WWFF`, `CQ AF/AN/AS/EU/NA/OC/SA/JA`,
    `CQ QRG`, `CQ END`, `TIME`, `Free Msg`}: type-tag on emitted CQs.
- **Operator's own state**: `my_call`, `my_grid`, `my_base_call`,
  whether the operator's callsign is "standard" or "compound" (slashed,
  per the 77-bit-protocol rules).

## Outputs

- One FT8 message string emitted per Now-list row per period, via the
  `MamEmitMessage(QString, bool, bool, bool)` signal. The four
  booleans flag (a) is-CQ-on-free-slot, (b) is-Special-MSG, (c)
  is-free-text, (d) is-OTP-key.
- Updates to `EmitMAMCalls(QStringList)` — the *complete current set* of
  Queue + Now callsigns, so the FT8 decoders downstream can feed each
  active callsign into per-thread A Priori (AP) decoding.
- `AddToLog(QStringList)` per completed QSO row.
- `EmitQSOProgressMAM(int, bool)` for the UI progress widget.

## Steps (prose)

### 1. New decode → Queue or Now decision

When the decoder delivers a new decoded message:

1. **Filter out noise**: messages not addressed to the operator's
   callsign (the simplest case: `to_call != my_call`) are sent to the
   "general DX" panel, not to the MAM sequencer.
2. **Lookup by callsign**: search Now first, then Queue. If the
   from-call is already in Now, this decode is the expected reply;
   advance that row's state (Step 3).
3. **If not in Now**:
   - If Now has fewer than `nslot` rows: promote directly to Now and
     start at state `+rpt`.
   - Else: insert into Queue (capped at `queueLimit` rows). If Queue is
     full, the message is dropped silently (no "queue overflow"
     warning).
4. **Insert format**: each Queue / Now row is a tuple of columns
   `(call, dB, RxdB, Dist, Grid, Freq, Time, IDrpt, TTry, GinTxT, ...)`.
   The `IDrpt` (tx-id, see Step 3) is initialized to `"1"` (= "+rpt")
   for new entries.

### 2. Queue auto-sort (per `Cb_sort`)

- **Off (`s_auto_sort = 0`)**: insertion order preserved. New callers
  appended at the bottom.
- **Distance (`s_auto_sort = 1`)**: sort by `Dist` column descending or
  ascending per the user's click on the column header. Distance is
  computed by `HvQthLoc` from the operator's grid to the caller's
  grid (`getDistanceKilometres` for km mode, `getDistanceMilles` for
  miles).
- **S/N (`s_auto_sort = 2`)**: sort by `dB` column. Stronger signals
  first (or weaker first, per the user's column-header click).

When sort is active, the column header for the *other* metric is
hidden (`THvHeader->hideSection(1 or 3)`), so the user can only act on
the chosen criterion.

### 3. Per-row tx-id state machine (the "message-row enum")

Each Now-list row carries a `tx_id` in column 7. The decoder maps
incoming text patterns to tx-id transitions:

- `tx_id = 0`: "click to someone else" (user-driven; the operator
  clicked a station in the DX panel to start a manual QSO inside the
  MAM machinery)
- `tx_id = 1`: "+rpt" (the +SNR signal-report greeting)
- `tx_id = 2`: "R+rpt" (the R-confirmed signal-report)
- `tx_id = 3`: "RR73" (the QSO-close handshake)
- `tx_id = 4`: "CQ <my_call> <my_grid4>" (this row was a CQ-on-free-
  slot emission)

State transition is driven by incoming decode content:
- On row's outgoing `+rpt`, the expected reply is `R+rpt`: advance to
  `tx_id = 2` ("R+rpt").
- On row's outgoing `R+rpt`, the expected reply is `RR73` from the
  correspondent: advance to `tx_id = 3` ("RR73").
- On row's outgoing `RR73`, the QSO is closed and logged. The row is
  removed from Now; a new Queue entry is promoted to fill the slot.

The legality of the next tx-id is also gated on whether the
correspondent's callsign is "standard" (regular form, no slashes) or
"compound" (e.g. `LZ2HV/M`, portable, or `<hashed-non-standard>`).
Compound callsigns require the 77-bit-protocol bracketed format
(`<COMPOUND_CALL>`) and have a small per-case branch table in
`DecodeMacros` (the `noQSO` value passed by `isStandardCalls` selects
between 8 sub-cases).

### 4. Per-period message generation: walk Now top-to-bottom

For each `slot = 0..(LsNow.rowCount() - 1)` in the Now list:

1. Read the row's `tx_id` from column 7.
2. Run `DecodeMacros(slot, str(tx_id))`. This function:
   - Looks up the operator's call and the correspondent's call.
   - Detects standard / non-standard / compound on both sides
     (`isStandardCalls` returns booleans + a `noQSO` 0..7 enum).
   - Generates the FT8-77-bit-protocol formatted message string for
     this tx_id, e.g. `"K5ARH LZ2HV +05"` or `"K5ARH LZ2HV RR73"`.
   - Stores the gen-in-tx-time in the row's column 9 so that the
     RR73-removing logic at the end of the period knows when the row
     was last touched.
3. Emit the resulting string via `MamEmitMessage`. The downstream TX
   path schedules this string on the row's assigned audio frequency
   (which was set when the row was promoted from Queue to Now —
   typically the audio frequency the caller's decode was received on).

### 5. Special-MSG fold (the contest-efficiency trick)

When all of these are true:
- `f_tx_sm` (operator opted in via `cb_tx_sm`),
- the operator's TX-side parity matches the slot,
- the row currently being generated is at `tx_id = 3` ("RR73"),
- the Queue has at least one entry that is at `tx_id = 1` or `tx_id = 2`
  (a fresh caller awaiting greeting),
- the Now list is already at the slot maximum (so the freshly-promoted
  caller would otherwise have to wait one full period),

then MSHV instead emits a "Special MSG" — a compound message of the form

  `<closing_call> RR73; <new_caller_call> <MYCALL> +<rpt>`

generated via `MakeSMsg(hc0, hc1, mc, rpt)`. This collapses two
transitions ("send RR73 to A" and "send greeting to B") into one TX
cycle. The Now-list bookkeeping records the new caller as "already
greeted" via `t_list_i3b[]` so the next period emits the R+rpt rather
than re-greeting.

A subtle exception path (for the Super Fox 9-slot variant only): if
`id_mshf == 2`, the report-and-acknowledge counters `c_sf_rpt` and
`c_sf_r73` are tracked separately, and the Special-MSG fold is gated
on `c_sf_rpt >= 4 && c_sf_r73 < 5` so that the slot budget doesn't
overflow nine concurrent messages.

The Special-MSG case is only emitted in MA DXpedition mode
(`!f_multi_answer_mod_std`), not in MA Standard mode.

### 6. CQ-on-free-slot

If `cb_tx_cq_on_free_slot` is true and at the end of Step 4 there are
fewer than `nslot` Now-list rows emitted, the sequencer fills the
unused slots with `CQ <my_call> <my_grid4>` (or the selected
`Cbcqtype` flavor, e.g. `CQ DX <my_call> <my_grid4>`).

These CQ emissions are flagged `tx_id = 4` so that any *next-period*
decode that arrives addressed to the operator's call is matched
against the freshly-emitted CQs and the originating slot is reused
for the new caller (rather than queueing them).

### 7. End-of-QSO cleanup

When a row reaches `tx_id = 3` ("RR73") emission *and the
correspondent's RR73 has been received*, the row is removed from Now,
the QSO is logged via `AddToLog`, and the operator's QSO-progress
widget is updated. The row's slot is now available; the next
top-of-Queue promotes into it.

If the row has been in Now for more than `SBmaxTP->value()` periods
without progress (the per-correspondent stall timeout), the row is
removed from Now without logging.

### 8. Live AP broadcast

After every Queue/Now mutation, the sequencer emits
`EmitMAMCalls(QStringList)` with the complete current set of
callsigns. The decoder side (`DecoderMs::SetMAMCalls`, which fans
out to all six per-period `DecoderFt8` workers) updates the per-
worker AP hash table so that each worker can attempt AP decoding for
*any* active correspondent in its sub-band, not just the operator's
"primary" QSO partner.

### 9. MA Standard vs MA DXpedition mode differences

- **MA DXpedition** (`Multi_answer_mod` checked): TX strictly on the
  operator's own parity (first or second period of the 30-s slot,
  whichever the operator picked). Slots up to `MAXSL = 6`. Special-MSG
  fold available. Default CQ type is `CQ MDX` ("MSHV recommended
  identification for DXpeditions").
- **MA Standard** (`Multi_answer_mod_std` checked): TX on either
  parity, but with an asymmetric slot budget — if TXing in the second
  period, only one slot is allowed. The "first period" still gets up
  to `MAXSL` slots. This concession reflects the practical
  observation that two duplex operators on the same band tend to
  collide more often on the second period than the first.
- Both modes use the same Queue / Now / tx_id state machine; only the
  slot-count / parity / Special-MSG gating differs.

## Numerical constants (facts, not expression)

- Maximum simultaneous slots: `MAXSL = 6` (raised from 5 in 2.71).
  Super Fox extension allows up to 9 in special configurations.
- Default Queue limit: `5`. Configurable `[0, 50]`.
- Default slot count: `1`. Configurable `[1, MAXSL]`.
- Per-correspondent stall timeout: configurable via `SBmaxTP` (max
  number of periods a row may occupy a slot without state progress).
- Tx-id enum: `{0 = click-other, 1 = +rpt, 2 = R+rpt, 3 = RR73,
  4 = CQ-on-free-slot}`.
- Special-MSG eligibility: `cb_tx_sm` opt-in AND DXpedition mode AND
  Now.rowCount() ≥ `nslot` AND next-queued's tx_id ∈ {1, 2} AND
  closing row's tx_id == 3.
- Super Fox 9-slot exception: `c_sf_rpt >= 4 && c_sf_r73 < 5 &&
  LsNow.rowCount() >= 8` allows one extra compound RR73 in a single
  emission.
- 4-slot Free-Msg cap: with a Free-Msg active, slot count is clamped
  to 4 plus 1 CQ-on-free-slot.
- Queue sort tie-break: when a column header is hidden by the
  auto-sort, ties on the visible column fall back to insertion order
  (no secondary key).
- CQ types: 16 named CQ-tag variants (regional / activity / portable).

## Edge cases

- **Queue overflow**: silent drop. No "queue full" warning to the
  operator. Subsequent decodes from the dropped caller will re-trigger
  the queue insertion if a slot opens.
- **Caller decodes twice in one period at different audio freqs**:
  the decoder's `(message, freq, ±6 Hz)` dedup table catches this
  before the sequencer ever sees both copies. The sequencer assumes
  one decode per caller per period.
- **Caller goes silent mid-QSO**: the row stalls in Now at whatever
  tx_id it last reached. After `SBmaxTP` periods the row is GC'd. The
  slot is reused.
- **Same caller decodes again while already in Queue or Now**: the row
  is updated in place (new audio frequency, new SNR, new dt) — the
  call is not duplicated.
- **Compound / non-standard callsigns**: an 8-way branch table on
  `(my_call_is_std, his_call_is_std, noQSO)` selects which of the two
  sides gets the bracketed `<COMPOUND>` notation; this is fully
  state-machine-encoded in `DecodeMacros`.
- **Operator's call itself is compound**: same 8-way table; some
  branches require the operator's *base* call instead of the full
  compound call.
- **Slot count reduced mid-QSO**: rows already in Now beyond the new
  limit are not aborted; they continue to completion. New promotions
  from Queue stop until Now.rowCount() drops below the new `nslot`.
- **Operator switches DXpedition ↔ Standard mid-period**: the toggled
  mode takes effect at the next period boundary. In-flight slots
  continue under the old mode's rules.
- **OTP key emission (Super Fox)**: when `cb_otp_mamd_key` is set, the
  OTP key takes one slot (incrementing the slot budget by one). The
  key is emitted on the second slot when slot limit is 1, else on a
  free slot.

## Conflict with pancetta's existing mechanisms

Pancetta has architectural pieces in place but they don't yet form an
end-to-end sequencer:

- `pancetta-qso/src/qso_manager.rs` — the per-QSO state machine
  (`determine_state_transition`, `is_message_relevant`) that pancetta
  uses for one-at-a-time QSOs. The per-correspondent tx-id-style
  state machine inside MSHV's `DecodeMacros` is the per-QSO equivalent
  of pancetta's `QsoState`. **Reuse-as-is**: each MSHV "Now-list row"
  maps to one pancetta `QsoManager` instance running independently.
- `pancetta-qso/src/frequency.rs::SmartFrequencyAllocator` — the
  7-criterion soft scoring that picks an audio TX frequency per
  outgoing message. MSHV instead pins each row's frequency to the
  caller's RX frequency at promotion time. **Pancetta's allocator is
  more sophisticated** here; the implementer would keep it and only
  add the multi-row dispatch.
- `pancetta/src/coordinator/tx.rs` — the DX-slot-aware TX scheduler
  that owns parity. MSHV's "TX on operator parity" rule is already
  implemented in pancetta as the `tx_parity = opposite_of(dx_parity)`
  latch. **Pancetta is closer to MSHV Standard than MSHV DXpedition**
  because pancetta currently latches per-QSO parity from the partner;
  the DXpedition pattern (operator picks one parity for all slots)
  would be a config option layered on top.
- `pancetta-qso/src/autonomous.rs` — pancetta's autonomous operator
  with its `recently_responded_to` table and per-callsign 60-s
  cooldown. MSHV does not have a cooldown; it relies on the
  Queue / Now uniqueness. **Pancetta's cooldown is an additional
  defense** worth preserving when wiring multi-stream.

Conceptual mapping of MSHV components to pancetta modules for the
implementer thread:

| MSHV concept              | Pancetta home (new or existing)                    |
| ------------------------- | -------------------------------------------------- |
| Queue list                | new `MultiStreamScheduler::queue: VecDeque<...>`   |
| Now list                  | new `MultiStreamScheduler::active: Vec<QsoManager>` |
| `tx_id` enum              | reuse `QsoState` (already maps 1:1)                |
| `DecodeMacros`            | the existing per-QSO message generator             |
| `nslot`                   | new `multi_stream.max_slots: usize` config field   |
| Special-MSG fold          | new `multi_stream.allow_combined_rr73: bool`       |
| CQ-on-free-slot           | reuse `autonomous::cq_mode` per empty slot         |
| `EmitMAMCalls` AP fan-out | optional; pancetta's AP wiring is not yet shipped  |

There is no fundamental conflict — the spec lays down cleanly on top
of the existing `QsoManager` and TX scheduler.

## Estimated Rust port effort

- ~600-900 LOC of new code, plus ~200 LOC of integration changes
  inside `coordinator/tx.rs` and `coordinator/mod.rs`.
- 3-5 sessions:
  1. `MultiStreamScheduler` core (queue, active, promotion).
  2. Wire to the existing per-QSO state machine and TX message
     generator.
  3. Special-MSG fold (optional, can defer).
  4. Queue auto-sort (DXCC / distance / SNR / pancetta-priority).
  5. End-to-end loopback test (`pancetta/tests/loopback_qso.rs` analog
     with 3 simultaneous QSOs).

## Implementation notes for the implementer thread

- **Reuse pancetta's per-QSO state machine.** Do NOT duplicate the
  `+rpt → R+rpt → RR73` logic; instantiate one `QsoManager` per
  Now-list row. The MAM scheduler is a thin shell over a `Vec<QsoManager>`
  plus a Queue.
- **Use pancetta's priority scoring for the Queue sort criterion**,
  not MSHV's "Distance" / "S/N" choice. Pancetta's `priority.rs`
  already scores needed-DXCC, needed-grid, POTA/SOTA, rarity, with
  duplicate suppression and failure-backoff. MSHV's distance/SNR
  sort is the simpler operator-facing variant; pancetta's autonomous
  scoring is strictly more capable. Implementer: surface "use
  pancetta priority" as a sort mode and offer distance/SNR for
  compatibility with operators familiar with MSHV.
- **Frequency allocation per row**: at promotion time call the
  existing `SmartFrequencyAllocator::pick(...)` rather than mimicking
  MSHV's "pin to caller's RX frequency". The smart allocator already
  accounts for multi-stream non-collision.
- **Special-MSG fold is optional**: leave a `--special-msg` config
  toggle but ship with it off until the multi-stream basics are
  validated on-air.
- **Capacity defaults**: ship with `max_slots = 3` initially (not the
  MSHV `MAXSL = 6`), since pancetta has not yet validated
  multi-stream beyond two simultaneous slots in the loopback tests.
  Operator-configurable per `Cargo` config.
- **Loopback test**: extend `pancetta/tests/loopback_qso.rs` to drive
  three simulated callers at different audio frequencies through the
  full pipeline end-to-end, verifying that each completes a
  CQ→grid→+rpt→R+rpt→RR73 exchange within `max_periods` periods.
- **Cite as inspiration** in the eventual commit: "Multi-stream
  TX slot scheduling shape inspired by MSHV's Multi-Answer
  Auto-Sequencing protocol (MA DXpedition / MA Standard). Per-QSO
  state machine and frequency allocator are pancetta-native."
- **Out of scope for this spec**: Super Fox OTP-key handling, Cabrillo
  contest export, free-text 4-slot mode. These are MSHV-specific
  features that pancetta does not currently target.
- **Do NOT port** the static-class-member sharing pattern MSHV uses
  for cross-thread state (`DecoderFt8::s_MyBaseCall8` etc.) — Rust's
  ownership model makes this anti-pattern explicit. Use
  `Arc<RwLock<SharedQsoState>>` instead.
