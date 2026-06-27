# Remote operation — client/server architecture design (DRAFT)

**Date:** 2026-06-26
**Status:** DRAFT for operator review — not yet approved; no implementation.
**Author context:** drafted while the station was operating live; intentionally a
design surface to react to, not a build plan.

## 1. Goal

Let the operator run and watch their FT8 station **from anywhere network-
connected**, while the rig-coupled core stays on the machine physically wired to
the radio.

- **Core ("server")** runs headless on the miniPC at the rig: audio I/O,
  soundcard, DSP, FT8 decode, the QSO engine, hamlib/CAT control, logging.
- **Client(s)** run elsewhere — phone, laptop, tablet, browser — and provide
  display + control over the network.

## 2. Operator constraints (decisions already made)

1. **No autonomous mode over remote, by default.** A remote client is *attended
   manual* operation. The unattended/autonomous engine stays a local,
   at-the-rig capability. (Also the cleanest posture for remote-TX legal
   responsibility — see §7.)
2. **Repo split.** `pancetta` stays the local/server (core). Client app(s) live
   in a **separate repository (or repos)**.
3. **Client app needs its own name.** TBD. Cured-meat lineage candidates
   (pancetta ↔ ham): **Salumi** (a board of many cured meats ≈ many client
   platforms), **Speck** (short, app-y; small/portable + "a tiny remote view"),
   Prosciutto, Guanciale, Capicola, Porchetta. Decide before client repo
   creation.

## 3. Why this is feasible (the seam already exists)

The hard architectural boundary is already present in the codebase:

- **The core already runs headless** (`--headless`); the rig-coupled components
  are already server-side.
- **The TUI is already a client over a message boundary**, not co-mingled with
  the core:
  - **Commands up:** `TuiCommand` (e.g. `SetFrequency`, `SetSplit`, `StartQso`,
    `CallStation`, `RespondToCaller`, `StartCq`/`StopCq`, `TogglePtt`, `StopTx`,
    `ToggleTune`, `ToggleTxFreqMode`, `SelectDevice`, `OperatorEmergencyStop`,
    `Quit`).
  - **Events/snapshots down:** `TuiMessage` + snapshot structs (decoded
    messages, `ActiveQsosSnapshot` incl. the pending-call queue, DX-Hunter rows,
    band activity, waterfall, S-meter/SWR, `TxPolicyUpdate`, `SplitUpdate`).
  - Translation is centralized in `pancetta/src/coordinator/tui_relay.rs`
    (`TuiCommand` → bus, bus → `TuiMessage`).
- Today those messages ride **in-process channels**. Remote operation =
  serialize the *same* messages over a network transport. The conceptual API
  already exists; we're hardening and transporting it.

### The key enabler: FT8 cadence makes latency a non-issue
FT8 is **15-second-slot paced**. The control/display path tolerates hundreds of
ms of latency with zero functional impact (works fine over cellular). Audio and
decode stay **local** to the rig; only control + display *data* crosses the
network. The waterfall is ~1 Hz of `f32` data — cheap to downsample/compress.
Unlike remote CW/SSB, audio latency is irrelevant here. **Latency does not gate
this design.**

## 4. Architecture

```
   ┌─────────────────────────── miniPC (at the rig) ───────────────────────────┐
   │  audio I/O · DSP · FT8 decode · QSO engine · hamlib/CAT · logging          │
   │                        ── coordinator / message bus ──                     │
   │   tui_relay (in-proc)            remote_gateway (NEW)                       │
   │        │                                  │                                 │
   │   local TUI (optional)         WS/gRPC server + auth + TLS                  │
   └────────────────────────────────────────┼──────────────────────────────────┘
                                             │  network (LAN / VPN / internet)
                ┌────────────────────────────┼────────────────────────────┐
                │                │                │                │        │
            web app          iOS app        Android app       macOS/Win   (Rust TUI
          (via cqdx.io)     (native/RN)     (native/RN)        app          as a
                                                                            client)
```

- A new **remote gateway** component sits beside `tui_relay` on the bus: it
  bridges the same command/event streams to/from remote clients over a network
  transport, with auth + TLS + multi-client fan-out.
- Clients are thin presentation layers: subscribe to the event/snapshot stream,
  send commands. No DSP/decode/rig logic client-side.

## 5. Protocol design

### 5.1 Foundation: serialize the existing UI boundary
Promote `TuiCommand`, `TuiMessage`, and the snapshot structs to a **stable,
versioned, `serde`-serializable** protocol. Many config types already derive
serde; the UI/relay types largely do not yet. This is the bulk of the work — and
doing it cleanly also tightens the current in-proc boundary (no shared-memory
shortcuts leaking across it).

- **Wire format:** JSON for v1 (debuggable, web-native); consider a compact/
  binary encoding (CBOR/msgpack) later for the waterfall stream if bandwidth
  warrants.
- **Transport:** WebSocket for v1 (browser-native, bidirectional, simplest path
  to a web client). gRPC is an alternative for native clients but adds friction
  for browsers; WebSocket keeps one transport for all clients.

### 5.2 Message classes
- **Commands (client → server):** the `TuiCommand` surface, each carrying the
  authenticated session + (for TX-affecting commands) the control-operator
  token (§7).
- **Events/deltas (server → clients):** decoded messages, QSO state changes,
  band activity, DX-Hunter updates, S-meter/SWR, TX policy/split status,
  pending-call queue, waterfall frames.
- **Snapshot-on-connect (server → one client):** a newly-connected client must
  receive a **full current-state snapshot** (active QSOs, current band/dial +
  split, config-relevant settings, recent decodes, TX policy) *then* the live
  delta stream. Define a `StateSnapshot` aggregate for the handshake.

### 5.3 Versioning
Clients and server version independently → the protocol must be **versioned and
backward-compatible** (a `protocol_version` in the handshake; additive fields;
`#[serde(default)]` / non-exhaustive enums so an older client tolerates new
event types it doesn't understand).

## 6. Multi-client & control arbitration

The bus is effectively single-UI today. Remote means N simultaneous clients:

- **Many read-only viewers** + **at most one control operator** at a time.
- **Control handoff:** an explicit "take control" / "release control"
  request; the server grants the control token to one session; others are
  view-only. TX-affecting commands from non-control sessions are rejected.
- The local at-the-rig TUI can be the default/privileged control surface.

## 7. Remote-TX safety (the hard part — design carefully)

Keying a transmitter over the network is a different risk class than a local
TUI. Non-negotiables:

- **Authentication + TLS.** PTT is exposed to the network; all transport
  encrypted; clients authenticated (token/cert). No anonymous control.
- **Control-operator model (FCC §97.213 remote control)** layered on the
  existing §97.221 presence logic. One authenticated control operator;
  TX-affecting commands require the control token; everyone else read-only.
- **Explicit transmit-enable.** A deliberate, revocable "transmit armed" state
  for the remote control session — *not* armed by default on connect.
- **Fail-safe PTT.** A network drop / heartbeat-timeout while transmitting must
  **drop PTT** (reuse the existing TX/PTT watchdog). Default-to-safe.
- **No autonomous over remote** (per §2.1) keeps remote operation strictly
  attended, simplifying the control-operator story.
- **Audit:** log remote control actions (who keyed what, when) — extends the
  existing `qso.security` logging.

## 8. Client strategy — DECISION POINT (not yet decided)

cqdx.io (the operator's own first-party web service) reframes the client
question: **the web app could *be* the client**, delivered through cqdx.io,
instead of building 4–5 native binaries. Options:

| Option | Pros | Cons |
|---|---|---|
| **Web-only via cqdx.io** | One codebase → every desktop + mobile browser; no app-store friction; leverages the operator's existing service | Mobile-browser background limits; PWA limits on iOS; perf of in-browser waterfall to verify |
| **Native per-platform** | Best UX/perf/background behavior; push notifications | 4–5 codebases; app-store overhead |
| **Hybrid (React Native / RN-Web)** | One codebase → native-ish apps; RN-Web can *also* target web via cqdx.io | RN complexity; still some per-platform polish |

**Leading low-cost candidate to evaluate first:** web client via cqdx.io.

### 8.1 Must-investigate before deciding
1. **Is a web client fast enough?** FT8 cadence makes the *control/display* path
   fine, but verify: in-browser waterfall rendering (canvas/WebGL) perf; mobile-
   browser battery/throttling; **background-tab suspension** (iOS Safari throttles
   backgrounded tabs/PWAs hard — matters if you want it watching while
   backgrounded); iOS PWA limits (no real background, limited push).
2. **Does web actually make our lives easier vs native?** Weigh one-codebase-via-
   cqdx.io against native background operation, push notifications, and offline/
   reconnect robustness. RN/RN-Web is the middle path.

## 9. Repository structure

- `pancetta` (this repo) — **stays the local/server core**, gains the remote
  gateway component + the serialized protocol crate.
- **`pancetta-protocol`** (new crate, possibly published) — the versioned,
  serde wire types shared by server and any Rust client. The single source of
  truth for the protocol; non-Rust clients (web/RN) mirror it (or generate from
  it, e.g. via schema/`ts-rs`).
- **Client repo(s)** — separate repository for the client app (name TBD, §2.3).
  If web-via-cqdx.io wins, the client may live in/adjacent to the cqdx.io
  codebase rather than a standalone app repo.

## 10. Phasing / roadmap

- **Phase 1 — prove the seam.** Extract `pancetta-protocol` (serialize
  `TuiCommand`/`TuiMessage`/snapshots + handshake + versioning). Add the remote
  gateway (WebSocket server) on the bus. Convert the existing Rust TUI into a
  *network* client talking to the gateway (proves the whole path end-to-end on
  LAN, no UI rewrite).
- **Phase 2 — web client.** Build the browser client (via cqdx.io) consuming the
  protocol; covers desktop + mobile browsers immediately. Resolve the §8.1
  questions here with a real prototype (waterfall perf, mobile behavior).
- **Phase 3 — production remote.** Full remote-TX security model (§7), multi-
  client arbitration (§6), auth/TLS hardening, then native wrappers/apps (or
  RN/RN-Web) if web alone proves insufficient.

Each phase is independently useful and shippable; Phase 1 is the de-risking
step (it converts an in-proc boundary into a network one with the existing TUI
as the client, surfacing every serialization/handshake issue before any new UI
exists).

## 11. Non-goals / out of scope (v1)

- Streaming rig *audio* to the client (the decode is server-side; not needed for
  FT8 operation). Could be a later add for a remote "listen" feature.
- Autonomous operation over remote (explicitly excluded, §2.1).
- Multi-*rig* / multi-station fan-in (one core ↔ one rig for now).
- Replacing the local TUI (it remains a first-class local client).

## 12. Open questions for the operator

1. **Client strategy (§8):** start with web-via-cqdx.io, or commit to a hybrid
   (RN/RN-Web) from the outset?
2. **Transport:** WebSocket-for-all (recommended) vs gRPC for native + WS for web?
3. **Auth model:** reuse cqdx.io accounts/tokens for client auth, or a separate
   station-local credential?
4. **Client name (§2.3).**
5. **Scope of Phase 1:** is "Rust TUI as a network client over LAN" the right
   first milestone, or go straight to a minimal web read-only viewer?

---

*This is a draft. Nothing here is built. Next step after operator review:
converge §12, then a `writing-plans` implementation plan for Phase 1.*
