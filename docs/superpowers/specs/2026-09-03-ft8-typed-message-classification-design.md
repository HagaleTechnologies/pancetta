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
pub fn ft8_message_to_qso_type(
    message: &pancetta_ft8::Ft8Message,
    rendered_text: &str,
    our_callsign: &str,
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
| `None` | i3=4 NonStdCall (compound/DXpedition call) | fall back to `pancetta_qso::utils::parse_ft8_message(rendered_text, our_callsign)`; `NonStandard { text: rendered_text.to_string() }` only if that yields nothing — see below |

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

**`None` — i3=4 NonStdCall, NOT "unrecognized shape."** *(Corrected in the
final whole-branch review; the original draft of this section claimed
`standard_type` is populated for every decoded message and that `None`
should always mean `NonStandard`. That premise was factually wrong and
would have silently reverted the shipped PAN-17 fix.)*

`standard_type` is assigned in exactly one place:
`parse_type1_standard` (`pancetta-ft8/src/message.rs`), the handler for
i3=1/2 `MessageType::Standard` payloads. An i3=4
`MessageType::NonStdCall` payload — a compound / DXpedition callsign such
as `YS/WE9G`, where one call is an exact 58-bit base-38 pack and the
other a 12-bit hash render (`<K5ARH>` resolved, `<...>` unresolved) —
goes through `parse_nonstd_call`, which sets
`to_callsign`/`from_callsign`/`contest_exchange` and **never touches
`standard_type`**.

In production, exactly two message types reach this function:
`is_plausible` gates every decode-acceptance point in `decoder.rs` and
rejects `FreeText`/`Telemetry`/`Contest`/`FieldDay`/`RTTYRoundup`/
`DXpedition`/`Unknown`, while `Extended` is never produced. So `Standard`
(always `Some(..)`) and `NonStdCall` (always `None`) are the only
survivors — and `is_plausible` has a dedicated `NonStdCall` shape check
added precisely so real compound callsigns are accepted (PAN-17).

Therefore `None` here means "not an i3=1/2 Standard payload," i.e. a
legitimate compound-callsign CQ, reply, or RR73/RRR/73 close — messages
that are perfectly classifiable from their rendered text. That is exactly
what `CALL_TOKEN` and `normalize_callsign_token`
(`pancetta-qso/src/exchange.rs`) were widened/added for in PAN-17.
Mapping them to `NonStandard` would make every compound-callsign QSO
invisible to the QSO engine again (the "TX-only, not a working QSO" gap)
and would also drop `normalize_callsign_token`'s bracket stripping, so a
resolved hash render would latch as `<K5ARH>` — which the round-2
unencodable-message watchdog then (correctly) rejects, self-sabotaging
the QSO.

The `None` arm therefore falls back to the existing, already-correct,
already-tested text parser. PAN-49's structural fix is unaffected: it
lives entirely in the `Some(ReplyWithR)` arm, which no longer depends on
`QSO_PATTERNS` at all. The fallback's own `NonStandard` result (and any
parse error) is collapsed to the verbatim `rendered_text`, because
`parse_message` uppercases its input and would otherwise substitute a
different string than what was actually logged.

`our_callsign` is threaded in solely to construct the `MessageExchange`
the text parser hangs off. The parse path (`parse_message` →
`parse_cq_message`/`parse_qso_message`) never reads it — only the
`generate_*` half of `MessageExchange` does — so it has no effect on
classification, but it keeps the call honest rather than passing a
placeholder.

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
let parsed = ft8_message_to_qso_type(&decoded_msg.message, &decoded_msg.text, &our_callsign);
```

`our_callsign` is already in scope in the decode loop (it is passed to
`maybe_auto_resend_73` a few lines below), so this is a one-line addition.

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
- Dedicated i3=4 (`standard_type == None`) tests covering the three
  NonStdCall shapes a compound-callsign station actually sends — CQ,
  reply carrying a resolved hash render (asserting the brackets are
  stripped), and RRR/RR73/73 closes — plus the unresolved `<...>`
  placeholder. These are the PAN-17 regression guard for the `None` arm;
  each one fails if `None` maps straight to `NonStandard`.
- No changes needed to existing `exchange.rs`/`sim.rs` tests — they
  exercise `parse_message`, which is untouched.
- Coordinator-level: `pancetta/tests/loopback_qso.rs` (encode → modulate
  → decode → classify) must call `ft8_message_to_qso_type`, NOT
  `utils::parse_ft8_message` — otherwise the flagship E2E suite exercises
  code the coordinator no longer runs and cannot catch a regression in
  this function at all. The function is `pub` and re-exported from
  `pancetta_lib::coordinator` for this reason. Both the standard-callsign
  legs and the PAN-17 compound-callsign legs are repointed; the compound
  legs are what give the `None` arm end-to-end coverage.
  (`test_loopback_pan_49_contest_r_grid_ack_advances_qso` deliberately
  keeps using the text parser: it pins the contest-profile
  *reclassification* path, which needs a `NonStandard` input.)

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
