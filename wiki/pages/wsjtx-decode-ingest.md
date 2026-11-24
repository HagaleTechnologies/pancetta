# What will bite you about WSJT-X decode ingest?

PAN-381's listener (`pancetta/src/coordinator/wsjtx_udp/ingest.rs`) shows a
same-host WSJT-X's live Decode(2) datagrams in Band Activity and DX Hunter,
tagged `EXT`. These are the traps.

**The dial comes only from Status.** A Decode carries an audio offset, not
an RF frequency. The listener keeps the last non-zero `DialFrequency` from
each sender Id's Status(1) and drops decodes from an Id that has not sent
one ("before WSJT-X sent a Status" in the summary line). Never fall back to
pancetta's own dial or to 14.074 MHz: `decode_view_dial_mhz` already maps a
dial of 0 to 14.074 MHz, so a missing dial would silently put every
external decode on 20 m. A zero-dial Status or a Close forgets the Id.

**Display only is structural.** External decodes go on a clone of
`ft8_to_tui_tx` and nowhere else. The QSO engine, Autonomous, alerts,
PSKReporter, the remote gateway and the WSJT-X re-emit read no decode
provenance, and the first two can key transmit, so a decode put on the
message bus would be answered. `ingest_source_names_no_bus_or_tx_path`
fails if `ingest.rs` names `MessageType`, `send_message` or `send_to(`, but
it is a text check on non-comment lines: it cannot catch a new bus method
name. The listener still holds a `MessageBus`, and its only legitimate use
is the `remote.wsjtx` Diagnostics line through `super::emit_diagnostic`.

**Operator keys are gated by dial.** Space and Callers Enter on an external
decode refuse with a status line unless WSJT-X's dial is within 10 Hz of
pancetta's dial (`same_dial`), the mode matches, and the rig readback is
not a band change (`is_band_change`). Pancetta never retunes to WSJT-X's
dial (PAN-435, PAN-443). Relaxing the check would let a call go out where
pancetta never heard the station. The rule and its reasons are in
`docs/DECISIONS/tui.md`, "WSJT-X decodes are matched by dial (PAN-443,
2026-11)".

**The native decoder blocks on the same channel.** `ft8.rs` sends native
decodes with a blocking `send` on the bounded `ft8_to_tui_tx`. If the
listener filled that channel, native decoding would stall. So the listener
only ever uses `try_send` (a full channel counts as "display queue full"),
and it forwards at most 250 decodes per fixed 15 s window, so a hostile
local sender cannot starve the native path.

**FT4 slot starts arrive at whole seconds.** WSJT-X writes an FT4 slot
start such as 12:00:07.5 as `120007`, so `slot_parity_from_qtime` rounds a
Decode's `Time` to the nearest slot start. Plain division would put that
decode in the previous slot, with the wrong parity.

**Your own datagrams come back.** A companion app that forwards traffic can
echo pancetta's own WSJT-X-role datagrams to the listener. Anything whose
Id equals pancetta's `instance_id` is dropped silently as echo, and echo
drops never reach the summary line. So an `instance_id` set to `WSJT-X`
(WSJT-X's own default Id; pancetta's default is `WSJT-X - pancetta`)
silently drops every real WSJT-X decode. No test covers that case.
`decode_listen` must also differ from `destination`, or pancetta would
receive its own broadcast; validation compares only a `destination` that
parses as an IP address and port.

**Free text needs a guard.** `DecodedMessage::from_ft8lib` happily reads
`HELLO WORLD TEST` as a from-call of `WORLD`. The listener clears the
from-call, to-call and grid when the sender call fails
`is_plausible_callsign`, so free text shows in Band Activity but makes no
DX Hunter row. Non-ASCII characters are stripped before the native
sanitizer, because a datagram can carry C1 controls that a real FT8 decode
never does.

**Loopback is the only authentication.** UDP sources are not
authenticated. Validation and the start path both refuse a non-loopback
`decode_listen`, and each datagram's source is checked again. Port 0 and the
`destination` clash are refused by validation only.

**Headless and `--replay` bind nothing.** Headless has only a debug drain
on the relay channel, and replay would mix live WSJT-X traffic with
replayed decodes. Live qualification against a real WSJT-X is pending.
