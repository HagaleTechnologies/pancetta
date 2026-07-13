# WSJT-X-Compatible UDP: Emit + Consume — Design Spec

**Date:** 2026-07-13 (operator green-lit: "Let's definitely build out the ability to
emit WSJT-X style UDP. (and to consume it!)")
**Protocol contract:** `docs/superpowers/specs/2026-07-13-wsjtx-udp-protocol-notes.md`
(clean-room, byte-level, permissive-source-verified).
**Plan:** `docs/superpowers/plans/2026-07-13-wsjtx-udp-plan.md`.

## Why

The competitive audit found the WSJT-X UDP protocol is the lingua franca of the FT8
companion ecosystem: GridTracker, JTAlert, and every major logger consume it. Emitting
it makes the entire existing companion stack light up for pancetta users with zero
work on their side; consuming it (Reply/HaltTx) makes GridTracker on another machine a
point-and-click remote head for pancetta. This is the single biggest ecosystem unlock
available.

## Target scenarios

1. **Same-machine companions:** pancetta + GridTracker/logger on one host (unicast
   localhost or multicast loopback).
2. **Cross-machine GridTracker (operator's explicit ask):** pancetta on machine A (the
   rig MiniPC), GridTracker on machine B — decodes/status/QSOs flow A→B; the operator
   double-clicks a station in GridTracker on B and pancetta on A calls it.
3. **Loggers:** QSOLogged + LoggedADIF so Log4OM/N3FJP/HRD/CQRLOG-class loggers ingest
   pancetta QSOs live.

## Architecture

New coordinator component in the `pancetta` main crate:
`pancetta/src/coordinator/wsjtx_udp/` (`mod.rs` component + `codec.rs` wire format).
NOT in `pancetta-protocol` (that crate is the contract-pinned dispensa rig-api and
must not be touched). The codec is hand-rolled big-endian in the exact style of the
PSKReporter IPFIX encoder (`pancetta-dx/src/pskreporter.rs:~700-783`) — hand-rolled
`to_be_bytes` IS house style; no serde/bincode.

One tokio `UdpSocket`, bound ephemeral, `connect`-less (needs `send_to`/`recv_from`
since multicast dest ≠ reply source): sends to the configured destination, polls the
same socket for inbound requests (companions reply to our source addr:port — protocol
notes §1). Multicast destinations set `IP_MULTICAST_IF` + TTL per notes §1/§6.

### Emit taps (all additive — the remote-gateway invariant applies verbatim)

| Message | Source | Mechanism |
|---|---|---|
| Heartbeat(0) | 15 s timer | component task |
| Status(1) | coordinator atomics: `operating_frequency_hz`, `active_protocol_mode`, `tx_offset_hold_hz`, `tx_policy`, `ptt_active`, `last_decode_timestamp`, config callsign/grid (all mapped in the integration survey) | sample on a 1 s timer + emit only on change; always one at startup (GridTracker gates instance validity on first Status) and after each Heartbeat |
| Decode(2) | new `ComponentId::WsjtxUdp` + one gated additive send in the decoder fan-out after `ft8.rs:1147`, mirroring the remote-gateway block byte-for-byte in structure | `wsjtx_enabled: Arc<AtomicBool>` gate |
| Clear(3) | band change (dial moved to a different band per Status sampling) | component task |
| QSOLogged(5) + LoggedADIF(12) | `QsoManager::subscribe()` broadcast (`QsoEvent::QsoCompleted { metadata }`) — third subscriber alongside the ADIF writer and the upload subscriber; ADIF record text via the existing `AdifProcessor::qso_to_adif` + `generate_record` | component task |
| Close(6) | cooperative shutdown | component task |

Lifecycle follows `start_pskreporter_component` exactly: `start_wsjtx_udp_component`
called from `run()`, disabled ⇒ spawn a bus-drain task, enabled ⇒ config snapshot +
`create_channel(ComponentId::WsjtxUdp)` + one tokio task + handle pushed onto
`named_task_handles`.

### Consume (inbound requests)

Processed only when `accept_udp_requests = true` (master switch, default **false**) AND
the datagram passes source filtering AND the Id matches ours:

| Message | Action |
|---|---|
| Reply(4) | match against the retained decode ring; CQ/QRZ ⇒ the remote-initiation path below; non-CQ ⇒ honored for targeting only, never TX (protocol notes §5) |
| HaltTx(8) | `abort_current_tx.store(true)`; if `!auto_tx_only` also `QsoMessage::CancelAllQsos` + `StopCq`. **Always honored under `accept_udp_requests`** — stopping TX is the safe direction and must not require the initiation consent |
| Replay(7) | re-emit the retained ring as Decode(2) `New=false`, then a Status |
| Heartbeat/Clear/Configure/FreeText/Location/HighlightCallsign/SwitchConfiguration/AnnotationInfo | parse, log at debug, ignore in v1 (Configure/FreeText are candidate v2 features) |

Every honored AND every refused inbound request emits a retained
`DiagnosticEvent { target: "remote.wsjtx", .. }` so the operator can audit exactly
what a companion app did or tried to do (Shift+D).

### Remote QSO initiation (Reply → on-air TX) — the security model

A GridTracker double-click is **remote TX initiation** and is treated with the same
posture as the station-agent remote path. Five gates, ANDed, all fail-closed:

1. `[network.wsjtx_udp].accept_udp_requests` (master, default false).
2. `[network.wsjtx_udp].allow_tx_initiation` (default false) — Reply may initiate only
   when this explicit consent knob is set.
3. Source filtering: unicast destination ⇒ inbound accepted only from that host;
   multicast ⇒ inbound accepted only from hosts in
   `[network.wsjtx_udp].allowed_request_hosts` (empty list = refuse all requests, log
   the source so the operator can add it).
4. `TxPolicy::allows_initiation()` pre-check (friendly refusal + diagnostic, same as
   the TUI CallStation path at `tui_relay.rs:992`) — advisory; the hard gates below
   still apply.
5. The QSO is created via `QsoMessage::StartQso { remote_origin: true }` — the single
   existing seam (`station_agent/mod.rs:526` is the model) that makes every emitted
   frame `TxOrigin::Remote`, which the TX worker arm-gates fail-closed
   (`remote_tx_permitted`, `tx.rs:417-427`) and parity-admits. **No remote frame is
   ever `TxOrigin::Local`** (repo invariant).

**Arm-gate decision (recommended: option A).** `TxOrigin::Remote` frames require the
shared `remote_tx_arm` (`ApplicationCoordinator.remote_tx_arm`, seeded from
`[network.station_agent].remote_tx_enabled` at `coordinator/mod.rs:863`) to be armed,
else they are silently dropped-and-audited.
- **Option A (recommended):** `allow_tx_initiation = true` additionally seeds the same
  `remote_tx_arm` at startup (exactly as `remote_tx_enabled` does), with a loud audit
  line naming the wsjtx-udp channel. Rationale: the arm is the operator's last-line
  "I consent to remote TX" bit, channel-independent; both channels still carry their
  own upstream consent + filtering; zero changes to safety-critical `tx.rs`.
- **Option B:** a dedicated per-channel ArmState checked in the TX worker — cleaner
  isolation, but it modifies `tx.rs` gate logic (safety-critical, heavily tested) for
  marginal benefit while both channels remain operator-consent-seeded booleans.
- Option A ships in v1; **a dispensa question must be filed** (per CLAUDE.md
  cross-repo policy and the 5-check remote-TX security spine) describing the new
  listener and the shared-arm decision, before the Reply-initiation task merges. If
  dispensa review rejects the shared arm, Option B is the fallback and only the
  seeding + one gate callsite change.

Additional caps: Reply-initiated QSOs are subject to the same duplicate check, parity
admission, and N0CALL refusal as every other QSO (they flow through the identical
`StartQso` path — nothing bypasses the QSO engine).

### Config

New `[network.wsjtx_udp]` section in `NetworkConfig` (template:
`RemoteGatewayConfig`, `network.rs:202-213`; §5-guardrail merge test extended):

```toml
[network.wsjtx_udp]
enabled = false                     # master: emit nothing, bind nothing
destination = "127.0.0.1:2237"      # unicast host:port or multicast group:port
multicast_interface = ""            # LAN IP to send multicast from ("" = OS default)
multicast_ttl = 3                   # only used for multicast destinations
instance_id = "WSJT-X - pancetta"   # the protocol Id field
accept_udp_requests = false         # master switch for ALL inbound processing
allow_tx_initiation = false         # Reply(4) may start a QSO (arm-gated, see spec)
allowed_request_hosts = []          # required for multicast inbound; unicast infers
```

### Invariants (added to the plan's global constraints)

- All emit taps are additive-only; `pancetta-tui` behavior stays byte-identical;
  existing bus sends untouched.
- `enabled = false` (default) ⇒ no socket is ever bound, no bytes emitted.
- No remote frame as `TxOrigin::Local`; arm gate fails closed; HaltTx is always
  honored under the master switch (stop-direction asymmetry).
- The codec lives in the main crate; `pancetta-protocol` untouched.
- Golden-vector tests (real captured packets, Apache-2.0-attributed) gate the codec.

## Non-goals (v1)

- JTAlert-specific version mimicry beyond a plausible Heartbeat Version string.
- Consuming Configure(15)/FreeText(9)/Location(11) — parse-and-log only.
- WSPRDecode(10), AnnotationInfo emission, IPv6, Fox-mode Status enum values.
- FT4 Decode emission until one on-air FT4 glyph capture confirms `"+"` (protocol
  notes §8 flag 7); v1 emits Decode only in FT8 mode, Status always.

## Acceptance

1. Codec golden tests pass against the captured WSJT-X vectors.
2. Same-host GridTracker shows pancetta's decodes/status live and its map populates
   (operator visual check).
3. Cross-host drill (operator, meatspace): GridTracker on machine B receives decodes
   from A; double-click on a CQ in GridTracker → pancetta on A calls the station,
   arm-gated, visible in the QSO panel and the `remote.wsjtx` diagnostics; GridTracker
   "halt TX" button stops TX on A within one slot.
4. With `accept_udp_requests = false` (default), a hostile LAN host replaying valid
   Reply packets produces zero TX and an audit trail.
