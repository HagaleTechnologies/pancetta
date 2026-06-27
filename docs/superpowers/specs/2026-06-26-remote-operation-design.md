# Remote operation — pancetta server + Panino client (DESIGN)

**Date:** 2026-06-26 (decisions converged with operator same day)
**Status:** Design agreed at the architecture level; implementation not started.
**Client name:** **Panino** (the handheld Italian sandwich that wraps/serves the
pancetta core; "pan-" echoes pancetta).

## 1. Goal

Operate and watch the FT8 station **from anywhere network-connected**, while the
rig-coupled core stays on the machine wired to the radio.

- **`pancetta` (server/core)** runs headless on the miniPC at the rig: audio I/O,
  DSP, FT8 decode, QSO engine, hamlib/CAT, logging — **and now also exposes a
  remote API**.
- **Panino (client)** runs elsewhere — phone, tablet, laptop, browser — for
  display + control over the network. Separate repository.

## 2. Firm decisions (operator, 2026-06-26)

1. **Augment, never replace the local TUI.** The remote work is **purely
   additive** and runs in parallel; the existing TUI must keep working unchanged.
   (See §3 invariant.)
2. **No autonomous mode over remote.** Panino is *attended manual* operation.
   The unattended/autonomous engine stays local at the rig (also the cleanest
   remote-TX legal posture — §7).
3. **Repo split.** `pancetta` (this repo) exposes the API/server. **Panino lives
   in a separate repository**, built by a peer session against the documented
   API.
4. **Client name = Panino.**
5. **Client tech = React Native (+ React Native Web)** delivered at a
   **`cqdx.io` sub-path** — one codebase → iOS + Android + web; do NOT shoehorn
   the control UI into the existing cqdx content app (§8).
6. **Transport = WebSocket for all clients** (§5).
7. **Tiered, station-paired auth** — view via cqdx login is fine; *control*
   (anything that can TX) requires a station-paired credential independent of
   cqdx, plus transmit-arm (§7).
8. **v1 = a focused workflow, not TUI parity** (§10.1).

## 3. Why feasible + the additive invariant

The seam already exists: the core runs headless (`--headless`), and the TUI is
already a **client over a message boundary** — `TuiCommand` up, `TuiMessage` +
snapshots down, centralized in `pancetta/src/coordinator/tui_relay.rs`. Today
those ride **in-process channels**; remote = serializing the *same* messages
over a network transport.

**INVARIANT (must hold throughout):** the remote gateway is a NEW component
beside `tui_relay` on the same bus. The local TUI keeps its in-process path
untouched. Extracting the message types into `pancetta-protocol` must be a
*move/derive* that the TUI keeps consuming exactly as today — zero behavior
change to the existing TUI. If a protocol change would alter TUI behavior, it's
wrong.

**Latency enabler:** FT8 is 15-second-slot paced. Control/display tolerates
hundreds of ms latency with no functional impact; audio + decode stay local;
only control + display data crosses the network. The waterfall is ~1 Hz `f32`
data. Latency does not gate this design.

## 4. Architecture

```
   ┌──────────────────── miniPC (at the rig) — `pancetta` server ───────────────┐
   │  audio · DSP · FT8 decode · QSO engine · hamlib/CAT · logging               │
   │                       ── coordinator / message bus ──                       │
   │   tui_relay (in-proc, UNCHANGED)          remote_gateway (NEW)              │
   │        │                                        │                            │
   │   local TUI (unchanged)            WS server + TLS + tiered auth + fan-out  │
   └────────────────────────────────────────────────┼───────────────────────────┘
                                                     │ network (LAN / VPN / internet)
                                          ┌──────────┴──────────┐
                                          │   Panino (RN + RN-Web)│  served at cqdx.io/<subpath>
                                          │  iOS · Android · web  │
                                          └───────────────────────┘
```

## 5. Protocol & transport

### 5.1 WebSocket for all
- **Bidirectional, persistent, full-duplex** over one connection — matches our
  model (event/snapshot stream down, commands up).
- **Browser-native** — RN-Web/web speak WS with no extra runtime; gRPC would
  need grpc-web + an Envoy-style proxy just to reach browsers. One transport for
  web + RN + (potentially) the Rust TUI = one server + one path to secure/test.
- **Type safety without gRPC:** define the protocol once in Rust
  (`pancetta-protocol`) and generate TypeScript types for Panino (e.g. `ts-rs`).
- **Encoding:** JSON for v1 (debuggable, web-native). If the waterfall stream
  ever needs it, move *that one stream* to binary (CBOR) over the same WS.

### 5.2 `pancetta-protocol` crate (new, in this repo)
Promote `TuiCommand`, `TuiMessage`, and the snapshot structs to a **stable,
versioned, serde-serializable** protocol crate — the single source of truth,
shared by the server and any Rust client, and the basis for generated TS types.

- **Commands (client→server):** the relevant `TuiCommand` surface (frequency,
  call/answer, CQ, etc. — v1 subset in §10.1).
- **Events/deltas (server→clients):** decoded messages, QSO state changes,
  DX-Hunter updates, band/dial+split, S-meter/SWR, TX-policy, pending-call
  queue, (later) waterfall.
- **Snapshot-on-connect:** a `StateSnapshot` aggregate (current band/dial+split,
  active QSOs + ladder, recent decodes, DX-Hunter rows, TX policy) sent in full
  on connect, then live deltas.
- **Versioning:** `protocol_version` in the handshake; additive fields
  (`#[serde(default)]`); non-exhaustive enums so an older client tolerates
  unknown event types.

## 6. Multi-client & control arbitration
- Many **read-only viewers** + **at most one control operator** at a time.
- Explicit **take-control / release-control**; the server grants the control
  token to one session; TX-affecting commands from non-control sessions are
  rejected. The local TUI is the default privileged control surface.

## 7. Auth & remote-TX safety (tiered, station-paired)

Keying a transmitter over the internet is the highest-risk capability. Auth is
**tiered**, and the control tier is **independent of cqdx login** (a phished
cqdx email must never be able to key the rig):

- **View tier (read-only):** cqdx login (email-proof) is acceptable — low risk.
- **Control tier (anything that can TX):**
  - **Station-side device pairing** — Panino pairs to the miniPC via a one-time
    code generated *at the station* (like pairing a TV). The pairing secret
    lives on the station, never in email/cqdx.
  - **Strong, revocable per-device tokens**; **MFA** on the control role;
    optional **mutual-TLS / device certs**.
  - **Explicit transmit-arm** — a deliberate, revocable "armed" state; NOT armed
    on connect.
  - **Fail-safe PTT** — heartbeat timeout / disconnect drops PTT (reuse the
    existing TX/PTT watchdog).
  - **Audit log** of all remote control actions (extends `qso.security`).
  - FCC §97.213 remote-control-operator model layered on the existing §97.221
    presence logic.
- **TLS everywhere.** No anonymous control. No autonomous over remote (§2.2).

**Principle:** PTT requires (station-paired secret) + (explicit arm) + (control
token) — never an email login alone.

## 8. Client strategy — DECIDED: React Native at a cqdx.io sub-path
- **Chosen:** RN + **RN-Web** → one codebase targeting **iOS native, Android
  native, and web**, delivered under **`cqdx.io/<subpath>`** (reuse cqdx's
  domain/hosting/accounts for delivery) but as a **separate, purpose-built
  control app** — NOT retrofitted into the existing cqdx content pages.
- **Rejected:** shoehorning a real-time control surface into the existing cqdx
  web app (fights its content-site architecture).
- **Must verify during build:** RN-Web canvas/WebGL **waterfall** rendering perf
  (deferred feature, §10.2); mobile-browser background/throttling + iOS PWA
  limits for any background-watch use; RN build complexity.

## 9. Repositories
- **`pancetta` (this repo):** the server — adds `remote_gateway` + the
  `pancetta-protocol` crate + the documented WS API. (Local TUI unchanged.)
- **Panino (new, separate repo):** the RN/RN-Web client, built by a **peer
  session** against the documented API. May live in/adjacent to the cqdx.io
  codebase for the web deploy, but is its own app/repo.
- **Handoff:** after the server API exists here, produce a **handoff context
  doc** (protocol surface + generated TS types, v1 workflow, auth/pairing flow,
  endpoint, the Panino name) so the peer session can build Panino standalone.

## 10. Phasing

### 10.1 Phase 1 — server API + Panino v1 (focused workflow)
**Server (this repo):** `pancetta-protocol` + `remote_gateway` (WS + TLS +
tiered auth + station pairing + control arbitration + state-snapshot), exposing
the v1 command/event subset. TUI untouched.

**Panino v1 (peer repo) — DEFINED SCOPE:**
- **View:** **DX Hunter** list + **QSO progress** (the exchange ladder /
  current QSO state).
- **Control:**
  1. **Select a station from the DX Hunter list** and call it.
  2. **Answer a caller.**
  3. **Call CQ.**
  4. **Frequency select.**
  - All control behind the control-tier auth + transmit-arm (§7).
- **Out of v1:** full TUI parity, band activity panel, free-text, multi-stream
  management UI, etc.

### 10.2 Later phases
- **Waterfall** — a proper graphical waterfall (explicitly nicer than the ASCII
  TUI's). Validate RN-Web rendering perf (the §8 open item) when built.
- Broader TUI-feature parity as desired; native-app polish; push notifications.

## 11. Non-goals (v1)
- Streaming rig *audio* to Panino (decode is server-side; not needed for FT8).
- Autonomous over remote (§2.2).
- Multi-rig / multi-station fan-in.
- Replacing the local TUI (it stays first-class).
- Waterfall in v1 (deferred to §10.2).

## 12. Status of open questions
- Client strategy — **DECIDED** (RN/RN-Web at cqdx.io subpath).
- Transport — **DECIDED** (WebSocket for all).
- Auth — **DECIDED** (tiered, station-paired control tier; cqdx login for view).
- Client name — **DECIDED** (Panino).
- v1 workflow — **DECIDED** (§10.1).
- Remaining to settle at plan time: exact `StateSnapshot`/event schema, the
  station-pairing UX details, where Panino's repo lives relative to cqdx.io, and
  the precise `pancetta-protocol` command/event list for v1.

---

*Next step: a `writing-plans` implementation plan for the Phase-1 **server side**
(in this repo): `pancetta-protocol` + `remote_gateway` + v1 API, preserving the
TUI invariant. Then a handoff context doc for the peer session that builds the
Panino repo.*
