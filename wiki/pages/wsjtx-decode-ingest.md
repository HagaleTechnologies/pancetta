# What will bite you about WSJT-X decode ingest?

PAN-381's decode listener (`pancetta/src/coordinator/wsjtx_udp/ingest.rs`) lets a
same-host WSJT-X feed its Decode(2) datagrams into Band Activity and DX Hunter,
tagged `EXT`. It is display and scoring only.

**The dial comes only from Status.** A Decode carries no frequency, only an audio
offset. The listener keeps the last non-zero Status(1) dial per sender Id and drops
every Decode from an Id that has not sent one. Never fall back to pancetta's own
dial or to 14.074 MHz: `decode_view_dial_mhz` treats a 0 dial as 14.074 MHz, so a
guessed or missing dial mis-bands every decode on a station without CAT. A Close
from that Id forgets its dial.

**Display-only is structural.** External decodes go only on a clone of
`ft8_to_tui_tx`, never on the message bus. The QSO engine and Autonomous read no
provenance and can key transmit, so a bus send would let a WSJT-X decode answer a
caller or pounce. The source guard test `ingest_source_names_no_bus_or_tx_path`
fails if `ingest.rs` names `MessageType`, `send_message`, a TX request or
`send_to(`. Keep it that way.

**The native decoder's send blocks.** `ft8.rs` sends on `ft8_to_tui_tx` with a
blocking `send` into a channel bounded at 500. If the listener filled that channel
it would stall native decoding. So the listener only uses `try_send` (a full channel
counts as "display queue full" and drops) and caps intake at 250 decodes per 15 s
window.

**Own-Id echo.** A companion that forwards traffic can bounce pancetta's own
Decode broadcasts back. A datagram whose Id equals `instance_id` is dropped
silently, and validation refuses `decode_listen == destination`.

**Free text.** `Ft8Message::from_text` takes the second word of any text as a
from-call (`HELLO WORLD TEST` gives `WORLD`). The listener clears the from-call,
to-call and grid when the resolved call fails `is_plausible_callsign`, keeping the
text. Free text then shows in Band Activity but creates no DX Hunter row, matching
the parser-typed native path. An unresolved `<...>` is left alone; DX Hunter
already ignores it.

Headless and `--replay` never start the listener. Both config keys are
restart-required. Live qualification against a real WSJT-X is pending.
