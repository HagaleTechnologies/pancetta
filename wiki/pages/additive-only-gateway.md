---
id: additive-only-gateway
title: What will bite you about the additive-only remote gateway?
kind: gotcha
status: current
maintainer: agent
sources:
  - pancetta/src/coordinator/mod.rs
  - pancetta/src/coordinator/remote_gateway/mod.rs
  - pancetta/src/coordinator/ft8.rs
verified:
  commit: eac7aab
  date: 2026-07-07
links:
  - remote-operation
---
The remote gateway is wired **additive-only**: every feed to it is a *new* bus
send gated behind the `gateway_enabled` atomic, and the pre-existing `→Tui`
sends are left byte-for-byte untouched. The invariant is that turning the
gateway on (or off) can never change local TUI behavior — the `pancetta-tui`
diff for the whole feature is empty. If you "refactor" a shared send to serve
both destinations, or move a `→Tui` emit inside a gateway branch, you couple the
gateway to the TUI and break the guarantee that a remote client cannot perturb
the local station.

## Symptom

Local TUI decode/QSO/status rendering changes (or regresses) depending on
whether the gateway is enabled — a coupling that should be impossible. Zero
overhead when off is also part of the contract; a non-gated send breaks it.

## Where the invariant lives

- `pancetta/src/coordinator/mod.rs:693` — the `gateway_enabled: Arc<AtomicBool>`
  field (doc comment at `mod.rs:689` describes the additive-emit contract).
- `pancetta/src/coordinator/ft8.rs:1119` — a representative gated additive send
  (`if gateway_enabled.load(...)`); the same pattern repeats in `hamlib.rs`,
  `qso.rs`, `autonomous.rs`, `tui_relay.rs`.
- `pancetta/src/coordinator/remote_gateway/mod.rs` — the read-only component
  itself; it only reads the bus.

Full digest: `docs/DECISIONS/remote-operation.md`; see [[remote-operation]].

## Gotcha: RF-absolute conversion uses the RX dial, never split TX

When enriching an audio-baseband bus payload to an RF-absolute wire value
(`DecodedMessage.frequency_offset`, `SpectrumRow.audio_bin_start_hz`), the
gateway always adds the **RX dial** (`operating_frequency_hz`), never
`split_tx_frequency_hz`. The audio the decoder/waterfall actually samples is
the RX passband — split only changes what gets *transmitted*, not what's
heard/displayed. A 2026-07-22 build request (dispensa Q-0024, the `spectrum`
serverEvent) worded this as "convert using dial + split TX frequency," which
is imprecise; `translate::spectrum_row_to_event` (and the pre-existing
`decoded_to_view`) both only use the RX dial. See `pancetta-protocol`'s
`Spectrum` DTO / `pancetta/src/coordinator/remote_gateway/translate.rs`.
