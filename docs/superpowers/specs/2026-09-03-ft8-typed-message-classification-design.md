# PAN-51 — Classify decoded QSO messages from Ft8Message, not rendered text (Design Spec)

**Status:** DRAFT
**Date:** 2026-09-03
**Ticket:** [PAN-51](https://linear.app/hagale/issue/PAN-51) — filed while designing PAN-49, whose root cause this fixes structurally

## 1. Problem

`pancetta-ft8` decodes each FT8 frame into a richly-typed `Ft8Message`
(`pancetta-ft8/src/message.rs`), including `standard_type:
Option<StandardMessageType>` — a decode-time classification into one of
eight well-known message shapes (`Cq`, `Reply`, `ReplyWithR`, `Report`,
`ReportWithR`, `Rrr`, `Final73`, `RR73`), derived directly from the raw
77-bit payload structure.

The coordinator's real decode path
(`pancetta/src/coordinator/qso.rs:3441-3446`) already receives this typed
`Ft8Message` — it's sitting in `DecodedMessage.message`, right alongside
the rendered `DecodedMessage.text: String` — but discards it, taking only
`text` and re-classifying via `pancetta_qso::utils::parse_ft8_message`,
which regex-matches the RENDERED TEXT against a fixed pattern list
(`QSO_PATTERNS` in `pancetta-qso/src/exchange.rs`).

This is a lossy, duplicate re-parse of information the FT8 layer already
determined precisely, and no wire-protocol or message_bus change is
needed to fix it — the structured data already crosses the boundary; it's
just thrown away one line later.

**PAN-49** was a direct symptom: a real, valid `StandardMessageType::
ReplyWithR` decode (`"<to_call> <from_call> R <grid>"`) had no matching
regex in `QSO_PATTERNS`, so it fell through to `MessageType::NonStandard`
and the QSO never advanced. It was worked around with a separate,
additive contest-profile reclassification step
(`pancetta-qso/src/contest/profile.rs` + a hook in `qso_manager.rs`) that
runs AFTER classification and re-labels an "otherwise unclassifiable"
decode if it looks contest-shaped. That workaround stays — see §5 — but
this design closes the actual gap at its source: `ReplyWithR` decodes
correctly on the first pass under this design (§3).

Every future message shape "requiring a new hand-written regex" is the
same problem recurring.

## 2. Scope

**One real production call site changes:**
`pancetta/src/coordinator/qso.rs:3446`, where `raw_text` is currently
built from `decoded_msg.text.clone()` and handed to `parse_ft8_message`.

**Nothing else changes:**

- `pancetta_qso::MessageType` (the enum) is untouched. It's also the
  OUTBOUND message-generation type (`MessageExchange::generate_message`/
  `generate_response`) — it must keep serving that role regardless of how
  inbound classification works.
- `MessageExchange::parse_message` / `QSO_PATTERNS` (the regex list) stay
  exactly as they are today. They remain the only classification path
  for:
  - The ~10 unit tests in `exchange.rs` that call `parse_message(text)`
    directly.
  - `sim.rs`'s high-fidelity round-trip harness (`sim-hifi` feature),
    which is deliberately testing the encode → decode → rendered-text
    round-trip and needs to parse text.
  - `autonomous.rs:1424`'s collision-detection use, which operates on
    `DecodedMessageInfo.message_text` — a pancetta-qso-internal,
    text-only struct with no `Ft8Message` available at that point.
- The PAN-49 contest-reclassification step in `qso_manager.rs` keeps
  running after classification, unchanged (§5).
- No `Cargo.toml` changes anywhere. `pancetta-qso` does not gain
  `pancetta-ft8` as a dependency; the new conversion function lives in
  the `pancetta` coordinator crate, which already depends on both.
- No `message_bus`/wire-protocol change — `DecodedMessage.message:
  Ft8Message` already crosses the boundary today.

This is additive: a second, narrower classification path used only where
a real `Ft8Message` is actually in hand, not a replacement of the general
text parser.

## 3. The conversion function

New function in the `pancetta` coordinator crate (exact module TBD at
implementation time — likely alongside the other `qso.rs` decode-handling
helpers):

```rust
fn ft8_message_to_qso_type(
    message: &pancetta_ft8::Ft8Message,
    rendered_text: &str,
) -> pancetta_qso::MessageType
```

`rendered_text` is passed in (from `DecodedMessage.text`, already
computed) rather than re-derived via `Ft8Message`'s `Display` impl — one
render, not two, and it guarantees the `NonStandard` fallback carries
exactly what was actually logged/displayed for this decode, not a
freshly re-rendered copy that could in principle diverge if `Display`
ever changes.

It matches on `message.standard_type` and constructs the corresponding
`MessageType` variant directly from `Ft8Message`'s already-parsed fields
— no regex, no re-parsing. Verified against the CURRENT `parse_message`/
`QSO_PATTERNS` behavior (including the `RRR`-vs-`RR73` regression test at
`exchange.rs:1148-1161`, which the mapping below deliberately mirrors) so
this is not a guessed table:

| `Ft8Message.standard_type` | Message shape | → `qso::MessageType` |
|---|---|---|
| `Some(Cq)` | `"CQ <call> <grid>"` | `Cq { callsign: from_callsign, grid: grid_square }` |
| `Some(Reply)` | `"<to_call> <from_call> <grid>"` | `CqResponse { calling_station: to_callsign, responding_station: from_callsign, grid: grid_square }` |
| `Some(ReplyWithR)` | `"<to_call> <from_call> R <grid>"` | `ContestReply { to_station: to_callsign, from_station: from_callsign, grid: grid_square, is_ack: true }` — see note below |
| `Some(Report)` | `"<to_call> <from_call> <report>"` | `SignalReport { to_station: to_callsign, from_station: from_callsign, report: signal_report }` |
| `Some(ReportWithR)` | `"<to_call> <from_call> R <report>"` | `ReportAck { to_station: to_callsign, from_station: from_callsign, report: signal_report }` |
| `Some(Rrr)` | `"<to_call> <from_call> RRR"` | `FinalConfirmation { to_station: to_callsign, from_station: from_callsign }` — same variant as `RR73`, matching today's regex (`exchange.rs:114-115,819-823`), which treats plain `RRR` and `RR73` identically |
| `Some(Final73)` | `"<to_call> <from_call> 73"` | `SeventyThree { to_station: to_callsign, from_station: from_callsign }` |
| `Some(RR73)` | `"<to_call> <from_call> RR73"` | `FinalConfirmation { to_station: to_callsign, from_station: from_callsign }` |
| `None` | anything else | `NonStandard { text: rendered_text.to_string() }` |

`GridSquare` and `SignalReport` (`pancetta-qso/src/states.rs:15,18`) are
plain type aliases (`String`, `i8`) — field mapping is a direct copy, not
a parse.

**`ReplyWithR` → `ContestReply` note:** `qso::MessageType::ContestReply`'s
own doc comment already describes exactly this shape (`"K1ABC W9XYZ R
EN37"`, citing PAN-49). Today, NOTHING in `parse_message`/`QSO_PATTERNS`
ever constructs `ContestReply` from inbound text — the general regex list
has no pattern for it, which is the literal PAN-49 bug. Under this
design, `ReplyWithR` maps to it directly and correctly the first time; no
separate contest-profile-matcher pass is needed to catch this specific
case anymore (though that matcher still exists and still runs for
genuinely contest-specific formats it alone recognizes — see §5).

**`None` — trust the decoder, no text-regex fallback.** If
`standard_type` is `None`, the message is classified `NonStandard`,
full stop. The decoder's own classification runs against the raw payload
structure at decode time — strictly more information than a regex can
ever recover from rendered text — so `None` should mean the message
genuinely isn't one of the eight shapes, not "the regex list hasn't
caught up yet." (That gap is exactly PAN-49's failure mode, and this
design closes it rather than reproducing it as a fallback path.) No
evidence surfaced during investigation that `QSO_PATTERNS` currently
classifies anything the decoder's own `standard_type` misses.

**Open item for implementation, not architecture:** confirm
`Ft8Message.signal_report`/`grid_square` are always `Some` when
`standard_type` implies they must be present (e.g. `Report` without a
`signal_report` would be a decoder invariant violation, not something
this conversion should silently paper over) — decide the right behavior
(panic vs. defensive `NonStandard` fallback) against the actual decoder
guarantees at implementation time.

## 4. Call site change

`pancetta/src/coordinator/qso.rs:3441-3446`, roughly:

```rust
// Before:
let raw_text = decoded_msg.text.clone();
let parsed = pancetta_qso::utils::parse_ft8_message(&raw_text, ...);

// After:
let parsed = ft8_message_to_qso_type(&decoded_msg.message, &decoded_msg.text);
```

Exact surrounding code (error handling, what `parse_ft8_message`'s other
parameters were doing) to be confirmed against the live file at
implementation time — `parse_ft8_message` is a thin wrapper
(`pancetta-qso/src/lib.rs:477-483`) around `parse_message`, so the
replacement should be a straightforward substitution, but the diff needs
verifying against the current call site, not assumed from this spec.

## 5. What stays separate: contest reclassification

PAN-49's contest-profile matcher (`pancetta-qso/src/contest/profile.rs`,
hooked in `qso_manager.rs`) is a SEPARATE, ADDITIVE step that runs after
classification and reclassifies an "otherwise unclassifiable" decode if
it looks contest-shaped (multiplier/serial-number exchanges,
contest-specific formats `Ft8Message.standard_type` has no concept of at
all). This design does not touch it. `ReplyWithR` no longer needs it
(§3), but genuinely contest-specific shapes (numeric serial exchanges,
etc.) still do, and nothing here changes that path's behavior.

## 6. Testing

- New unit tests directly on `ft8_message_to_qso_type`, one per
  `standard_type` variant plus the `None` case — construct a synthetic
  `Ft8Message`, assert the exact `MessageType` produced. Mirrors this
  codebase's established pattern of testing extracted pure decision
  functions directly rather than only through a full decode pipeline
  (e.g. `should_run_ft8lib_seed_union`, `finalize_native_and_seed_
  candidate_union` in `pancetta-ft8/src/decoder.rs`).
- A dedicated regression test reproducing PAN-49's exact original
  scenario (`ReplyWithR` from a real or synthetic decode) asserting it
  now produces `ContestReply { is_ack: true, .. }` directly from this
  conversion — proving the fix is structural, not another regex patch.
- No changes needed to existing `exchange.rs`/`sim.rs` tests — they
  exercise `parse_message`, which is untouched.
- Coordinator-level: confirm (via an existing or new integration test
  around `qso.rs`'s decode-handling path, if one already covers this
  call site) that a live decode still produces the correct `MessageType`
  end-to-end through the new path.

## 7. Non-goals

- Not touching `MessageType` as an enum, its outbound-generation role, or
  `parse_message`'s signature/behavior.
- Not attempting to unify the three classification paths (general regex,
  this new typed conversion, contest profile matcher) into one — they
  serve genuinely different inputs (arbitrary text, a real `Ft8Message`,
  contest-specific reclassification) and forcing one abstraction over
  all three would be exactly the kind of premature unification the
  codebase's `MessageType`/`ContestExchange` split already avoids.
- Not addressing PAN-50 (contest scoring) — unrelated, separate ticket.
