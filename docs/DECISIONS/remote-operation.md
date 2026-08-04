<!-- Covers: remote operation (gateway, station agent, split/tune). Content moved from CLAUDE.md 2026-07-07.
     For current behavior trust the code and specs in `docs/superpowers/specs/`. -->

# Remote operation

## Arbitrary-frequency tune + rig-level split (RX≠TX)

`coordinator/{mod.rs,hamlib.rs,tui_relay.rs}`, `pancetta-hamlib/src/{rig.rs,rigctld.rs,mock.rs}`, `pancetta-qso/src/qso_manager.rs`, `pancetta-tui/src/{tui_runner.rs,app.rs,ui/mod.rs}`: a single `split_tx_frequency_hz: Arc<AtomicU64>` atomic (0 = simplex; nonzero = split TX dial Hz) sits beside the RX-dial `operating_frequency_hz`; the **effective TX dial** is `split != 0 ? split : rx_dial`. The Rig trait gains `set_split`/`set_split_freq` (rigctld short-form `S`/`I`, allow-listed, `OPERATOR-CONFIRM(split)`), driven by `RigControlMessage::SetSplit { enabled, tx_frequency }` (hamlib loop sets VFO B TX freq + split-on, RX stays VFO A). Completed-QSO RF is stamped against `effective_tx_dial(rx_dial, split)` at all three stamp sites so split QSOs log the real TX RF. TUI: **Shift+F** opens a modal (RX dial MHz + optional TX split MHz; blank = simplex) → `TuiCommand::SetSplit`; a required **once-per-session** modal warns when the computed TX RF falls outside the US ham bands (`pancetta_core::Band::from_frequency`; acknowledge-to-proceed, never blocks — region-aware band plans are a deferred TODO); a "SPLIT TX x.xxx" title-bar chip shows when active. Split auto-clears on band change (manual `=`/`-` and autonomous band-hop), since the split TX freq is band-specific. Spec: `docs/superpowers/specs/2026-06-25-arbitrary-freq-split-design.md`.

## Read-only remote gateway (Panino client, v1)

`pancetta/src/coordinator/remote_gateway/{mod.rs,translate.rs}`, `pancetta-protocol` crate, `pancetta-config/src/network.rs`: a **default-OFF, localhost-bound** axum WebSocket component that serves the read-only remote view (decodes + QSO progress + scalar status) to WebSocket clients (panino) using the `pancetta_protocol` wire types. **NO control / NO remote-TX** in v1 — inbound `clientCommand` frames are accepted but ignored (logged); the control/relay/auth path is gated behind dispensa ADR-0002 (Accepted) and is not built here. Started in `start_remote_gateway_component` (mirrors `start_pskreporter_component`: disabled → drain channel, enabled → bus-pump + axum-serve tasks). The pure `translate.rs` maps bus payloads → protocol types (`ActiveQsoSnapshotItem`→`QsoProgress`, `DecodedMessage`→`DecodedView` enriched with worked/needed/atno from `CachedStationLookup`, etc.). Live feed via **additive dual-destination** bus sends gated on a cached `gateway_enabled: Arc<AtomicBool>` (zero overhead when off): decode fan-out (`ft8.rs`), `ActiveQsosSnapshot` ×3 (`qso.rs`), Frequency+SignalStrength (`hamlib.rs`), Split-clear (`autonomous.rs`), Mode (`tui_relay.rs`, see below) — the existing `→Tui` sends are **untouched** (additive-only invariant; `pancetta-tui` diff empty). On connect: `Welcome{StateSnapshot}` (full state, sent every reconnect) then `event` deltas; `protocolVersion = 1`. v1 emits `decoded`/`activeQsos`/`frequency`/`signalStrength`/`split`/`mode`/`spectrum` (NOT `dxHunter` rows — client builds DX Hunter from the decode stream; NOT live `txStatus`/`txPolicy`). **`mode` field** (2026-07-06, dispensa Q-0027 — fulfills the Q-0009 promise made when FT4 landed): `StateSnapshot.mode: String` (station-wide operating mode, "FT8"/"FT4"/"FT2"; `#[serde(default)]` ⇒ "FT8" for back-compat) seeded from the live `active_protocol_mode` atomic at gateway startup, plus a new `ServerEvent::Mode` pushed on every successful Shift+M switch (`tui_relay.rs`'s `CycleOperatingMode` handler, gated through `relay_to_gateway` same as Frequency/Split) so already-connected clients see a live mode change without reconnecting. Additive/optional on the wire — no schema-breaking change. **`spectrum` event** (2026-07-22, dispensa Q-0024, rig-api v1.2): `ServerEvent::Spectrum{spectrum: {binStartHz, binWidthHz, magsDb, seq, timestamp}}`, event-only (not carried in `StateSnapshot` — consumers key off `seq` and tolerate gaps). Source is `pancetta-ft8::decoder::generate_waterfall_data` — raw, pre-normalization dB (distinct from the TUI's own 0-1 color-mapped waterfall, which is untouched). `ft8.rs` forwards one `MessageType::SpectrumRow` (native ~2.93 Hz/bin resolution, audio-baseband `audio_bin_start_hz`) per waterfall row (~4 per FT8 decode window) to the gateway, gated by `display_feed_enabled` exactly like the `DecodedMessage` relay. The pure `translate::spectrum_row_to_event` pair-averages bins down to ~5.86 Hz/bin (matching panino's fixture default) and converts `audio_bin_start_hz` → RF-absolute by adding the live dial frequency (mirrors `decoded_to_view`'s enrichment); `handle_bus_msg` assigns `seq` from a per-display-feed-session `AtomicU64`. Config: `[network.remote_gateway]` (`enabled`=false, `bind_addr`=`127.0.0.1:4080`). Plain `ws://` localhost only (wss/TLS deferred to Sub-plan C). Cross-repo: dispensa ADR-0003 (rig-API) + ADR-0009 (this v1 implemented profile) + `contracts/rig/rig-api.v1.schema.json`. Spec: `docs/superpowers/specs/2026-06-26-remote-operation-design.md`; plan: `docs/superpowers/plans/2026-06-27-remote-gateway.md`.

## Station agent — remote rig control (P0–P2 safety core + P3 relay/pairing wire landed offline; the FINAL TX authority)

`pancetta-agent` crate: `arm.rs`, `audit.rs`, `noise.rs`, `keys.rs`, `pairing.rs`, `capability.rs`, `relay.rs`, `session.rs`, `control.rs`; `pancetta/src/coordinator/{mod.rs,tx.rs}`; `pancetta/src/message_bus.rs`; `pancetta-config/src/network.rs`: the offline safety core for future remote operation. **`pancetta-agent`** ships (a) a Noise-IK E2E conformance layer (`noise.rs`), (b) an append-only **audit log** (`audit.rs`), and (c) the crown-jewel **armed-TX state machine** `ArmState` (`arm.rs`) — a **pure, clock-free** gate whose `tx_permitted(now_ms)` is the logical **AND** of armed ∧ tx-scope ∧ unexpired (TTL) ∧ heartbeat-fresh (dead-man, `HEARTBEAT_TIMEOUT_MS=30s`) ∧ local-consent ∧ ¬local-kill (adding a gate can only make it more restrictive). **Coordinator gate (the ONE change to the live TX path):** `MessageType::{TransmitRequest,MultiTransmitRequest}` carry a `TxOrigin` (`Local` **default** / `Remote`); every existing construction site is explicitly `Local`, so **local TX is byte-identical**. The coordinator holds `remote_tx_arm: Arc<Mutex<ArmState>>` (`remote_tx_arm()` accessor), seeded at startup with `set_local_consent(config.network.station_agent.remote_tx_enabled, now)`. The TX worker (`tx.rs`) gates **only** `origin == Remote` requests through `remote_tx_permitted(arm, now_unix_ms)` **before keying PTT** — if not permitted it drops exactly like the drop-stale-TX gate (no PTT, clears the TX strip, failed `TransmitComplete`, logs `target:"agent.tx"`). It **fails CLOSED on a poisoned lock** (the OPPOSITE of `tx_qso_is_live`'s fail-open — this is a safety gate) and **ANDs UNDER `TxPolicy`** (Disabled hard-mutes everything first). **Inert in P0–P2:** nothing arms it and nothing constructs a `Remote` request, and `[network.station_agent].remote_tx_enabled` **defaults OFF**, so `tx_permitted()` is always false — this phase adds the *gate*, not the ability to use it. Config `[network.station_agent]` (`enabled` [no runtime effect yet — transport unbuilt] + `remote_tx_enabled` [LOCAL operator consent] + `audit_log_path`), all default OFF/None. **P3 relay/pairing wire landed offline** in `pancetta-agent` (`relay.rs` relay.v1 frames + mockable `WsConn`; `session.rs` agent-leg auth + Noise-over-`env`; `pairing.rs` enroll client; `capability.rs` two-stage `verify_capability_token`→`verify_arm_grant`→`VerifiedArmGrant`; `control.rs` `map_client_frame`→`ControlAction`; `keys.rs` identity). **P3.4b — the live component (`pancetta/src/coordinator/station_agent/{mod.rs,net.rs}`)**: `start_station_agent_component` mirrors `start_remote_gateway_component` — **default-OFF + inert** (disabled OR unpaired OR missing relay/pairing URL → no-op drain task; local consent still seeded from config so the gate reflects `remote_tx_enabled` even when off). Enabled + paired: loads `AgentIdentity`+`PairedState` from `key_dir`, dials the relay (`net::RealWsConn`, tokio-tungstenite; pairing HTTP = `net::ReqwestPairingHttp`, both thin sync→async `block_on` bridges over the network-free agent traits), runs `AgentSession`, and dispatches decrypted control frames (`dispatch_action`): **`Arm{grant}`** → verify capability (pinned IdP) + client-signed grant (allow-list + `clientSig`) → `remote_tx_arm.lock().arm(verified)` (fail-closed: any verify error audits `TxDenied`, never arms); **`Heartbeat{arm_jti,seq}`** → `heartbeat(&arm_jti, seq, now)` — **P3.4c**: rejects a wrong `arm_jti` or non-monotonic `seq` WITHOUT sliding the window (a replayed heartbeat can't hold the arm past its dead-man; a fresh arm resets the seq); **`Disarm`**/session-teardown/socket-loss → `disarm()` (fail TX-off on control-channel loss, Part-97) + capped-backoff reconnect; **`Qsy`/`SetSplit`** → coordinator `RigControlMessage` (NON-TX rig control); **`TxRequest(callStation/answerCaller/startCq)`** → **P3.4c**: routed into the QSO engine as a **remote-origin** QSO (`QsoMetadata.remote_origin=true`), so every `TransmitRequest` it emits carries `TxOrigin::Remote` and is **arm-gated end to end** — creation is allowed but TRANSMISSION is gated: an unarmed remote operator's QSO is created yet every frame it emits is dropped by the TX
worker's arm gate — logged, surfaced to the local TUI Diagnostics overlay, AuditLog-appended,
and relayed to any connected remote client as a generic `error` event (dispensa Q-0051, all
three phases; see below). The origin derivation (`coordinator/qso.rs`: `origin = remote_origin ? Remote : Local`) covers the MessageToSend forward, keep-call, coalesce, and auto-73 resend sites; **no remote QSO frame is ever emitted as `Local`** (no arm bypass). `remote_origin` defaults false at every `QsoMetadata` site ⇒ local/TUI/autonomous QSOs byte-identical. **P3.4d — reconciled to the FROZEN `e2e-auth.v1` (dispensa `03bac8b`, Q-0014/Q-0015 resolved).** The arm frame is the `txArm` E2E inner frame `{type:"txArm", capabilityToken, grant}` carrying the `capabilityToken` (compact JWS) + `txArmGrant` as **siblings** — the token is verified as a SEPARATE input (against the pinned IdP key), NEVER read from inside the grant (`clientSig` signs only the canonical `txArmGrant`, which references the token via `capabilityJti`). `ControlAction::Arm{capability_token, grant}` maps the frame; a `txArm` missing either sibling is a **hard error** (`ControlError::MalformedFrame`, fail-closed), not a silent no-op. **`txEnabledUntil` (clock-2 TX enablement, epoch SECONDS):** the arm path additionally requires the capabilityToken to be TX-**enabled** — `capability::require_tx_enabled(cap, now)` refuses to arm unless `txEnabledUntil` is present AND `> now` (`CapError::NotTxEnabled`); an absent/expired enablement NEVER arms (status/qsy are unaffected — they don't call it). Minted only after a WebAuthn step-up, never client-asserted. **Short-TTL backstop** (in `verify_capability_token`): a non-enabled token is rejected if `exp − iat > 900s` (`TtlTooLong`); an enabled token skips the 900s cap but must satisfy `exp == txEnabledUntil` (`EnablementMismatch`) and `txEnabledUntil − iat ≤ 24h` (`EnablementTooLong`). **Arm-time best-effort deny-list:** `verify_arm_grant(…, revoked_jtis, …)` refuses a capability whose `jti` is on the deny-list (`Revoked`) FIRST (right after the enablement gate); the set is **EMPTY/inert in v1** (never blocks — the station-local TX-allow-list is the authoritative revoke; a future cqdx-fed deny-list, populated on (re)connect and fail-open when offline, is the documented seam on `ArmContext.revoked_jtis`). **`txDisarm{armJti}`** → `ControlAction::Disarm{arm_jti}`: fail-safe TX-OFF that ALWAYS disarms; `armJti` is a sanity match (a non-empty mismatch still disarms + `warn!`). Frozen-shape drift is guarded by field-name round-trip tests (camelCase `capabilityToken`/`armJti`, `type` consts) in `control.rs`. Read stream is minimal-v1 (decodes/status via `remote_gateway::translate`, opportunistic). `ComponentId::StationAgent` added. **End-to-end security proof** (`station_agent::tests::e2e_arm_over_noise_permits_remote_tx`): a scripted mock relay + client drive the REAL `AgentSession` through auth → Noise IK → an encrypted `Arm` → the shared `remote_tx_arm` becomes `tx_permitted`; negatives covered (consent-OFF never permits, un-allow-listed rejected, heartbeat-loss auto-disarms, replayed jti rejected). Case-3 (Remote request keys PTT when armed / dropped when not) stays proven by the existing P2.3 `coord_sim` remote-TX scenarios. Config `[network.station_agent]` gains `relay_url`/`pairing_api_url`/`key_dir`/`tx_allow_list` (all default None/empty; validation rejects enabled-without-URLs and remote_tx_enabled-with-empty-allow-list). Still **no autonomous-over-remote**; the agent remains the FINAL TX authority.

## WSJT-X-compatible UDP: emit + consume (2026-07-15)

`pancetta/src/coordinator/wsjtx_udp/{mod.rs,codec.rs}`, `pancetta-config/src/network.rs`
(`WsjtxUdpConfig`): a new default-OFF UDP companion-protocol component that emits
Heartbeat/Status/Decode/Clear/QSOLogged/LoggedADIF/Close for GridTracker/JTAlert/logger
interop, and consumes Reply/HaltTx/Replay when explicitly enabled. Spec:
`docs/superpowers/specs/2026-07-13-wsjtx-udp-design.md`; protocol reference:
`docs/superpowers/specs/2026-07-13-wsjtx-udp-protocol-notes.md`.

**Clean-room provenance.** The protocol reference notes were built entirely from
permissively-licensed sources — Apache-2.0 (`k0swe/wsjtx-go`), MIT (`bmo/py-wsjtx`),
BSD-3 (GridTracker, `schlatterbeck/wsjtx-srv`, `MarcFontaine/wsjtx-udp`), Qt's own GFDL
documentation, and WSJT-X's own release notes / user guide (documentation, not source).
WSJT-X's normative upstream reference (`Network/NetworkMessage.hpp`, GPLv3) was
**deliberately never read**, consistent with this repo's clean-room policy (README
§Provenance): every byte-level fact in the protocol notes traces to a permissive-license
citation, and facts that couldn't be verified that way are explicitly flagged (⚑) in the
notes' §8 and treated as best-effort rather than asserted. No GPL text or structure was
copied into pancetta.

**Arm-sharing decision: Option A shipped.** A GridTracker double-click (`Reply(4)`) is
remote TX initiation and is routed through the identical `TxOrigin::Remote` enforcement
the station-agent remote path already uses (`coordinator/tx.rs`'s `remote_tx_permitted`,
fail-closed on a poisoned lock, ANDed under `TxPolicy`). The design spec offered two
options for how `[network.wsjtx_udp].allow_tx_initiation` interacts with the shared
`remote_tx_arm`: **Option A** (shipped) — `allow_tx_initiation = true` seeds the SAME
`remote_tx_arm` the station agent's `remote_tx_enabled` seeds
(`remote_tx_arm_consent(station_agent_enabled, wsjtx_allow_tx_initiation)` — an OR of
both channels' local consent, applied at both `set_local_consent` call sites so neither
channel's seed clobbers the other's), with a loud startup audit diagnostic naming the
wsjtx-udp channel whenever it contributes the seed. **Option B** (not built) would have
added a dedicated per-channel `ArmState` checked separately in the TX worker. Rationale
for A: the arm is the operator's channel-independent "I consent to remote TX" last-line
bit — one shared surface is easier to audit and reason about than parallel arm gates that
must each be independently verified never to diverge, and both options converge on the
identical `TxOrigin::Remote` enforcement point regardless; each channel still carries its
own upstream per-message consent (`accept_udp_requests` + `allow_tx_initiation` +
source filtering, ANDed, same fail-closed posture as every other remote-TX gate) and
Option A required zero changes to the safety-critical `tx.rs` gate logic itself, only to
the seeding call sites. This decision was filed as a cross-repo question — dispensa
Q-0031 (<https://github.com/HagaleTechnologies/dispensa/pull/16>, merged) — per the
5-check remote-TX security spine and CLAUDE.md's cross-repo contract-first policy; the
merged PR is the concurrence record for shipping Option A.

**v1 non-goals.** No IPv6 (multicast destinations are IPv4-only; an IPv6 `destination`
falls back to non-multicast socket-option handling and — deliberately fail-closed for the
inbound gate — is still treated as a multicast-shaped peer for source filtering, never
the looser unicast-single-peer path). No cryptographic peer authentication of any kind:
inbound-request trust is LAN-scoped IP-source gating only (`request_allowed`: unicast
destinations infer the single configured peer host, multicast destinations require an
explicit non-empty `allowed_request_hosts` allow-list, empty = refuse everyone). This
matches WSJT-X's own upstream security posture for this protocol — the real WSJT-X
"Accept UDP requests" checkbox has no peer-auth story either, and the protocol has no
provision for one (no signing, no shared secret, no TLS). Pancetta's `Id`-field targeting
and the five-ANDed-gate model add real risk-reduction on top of that baseline, but the
transport-level trust boundary is deliberately the same as upstream's: a hostile actor
already on the operator's LAN, sourced from an allow-listed (or destination-matching)
host, is out of scope for v1 — consistent with treating the LAN itself as the trust
boundary, the same assumption WSJT-X, GridTracker, and JTAlert all make.

**Operator verification needed (meatspace, at-rig).** No in-repo running list for
at-rig action items was found for this branch (see CLAUDE.md's multi-agent-hygiene /
memory conventions for cross-session tracking), so the acceptance drill from the design
spec's §Acceptance items 2-4 is recorded here instead, for the operator to run once the
station hardware is available:

1. **Same-host GridTracker (spec item 2):** with pancetta and GridTracker on one
   machine (`destination = "127.0.0.1:2237"`), confirm GridTracker's Band Activity shows
   pancetta's live decodes and its map populates from Status — visual check.
2. **Cross-host decode flow (spec item 3, first half):** GridTracker on a second LAN
   machine, pancetta on the rig machine, multicast `destination` (e.g.
   `224.0.0.73:2237`) with `multicast_interface` set to the real LAN NIC — confirm
   decodes/status arrive on the second machine.
3. **Double-click call + HaltTx (spec item 3, second half):** with
   `accept_udp_requests = true`, `allow_tx_initiation = true`, and the second machine's
   IP in `allowed_request_hosts` (or unicast, which infers it), double-click a CQ in
   GridTracker and confirm pancetta calls the station — arm-gated, visible in the QSO
   panel and in the `remote.wsjtx` diagnostics (`Shift+D`). Then confirm GridTracker's
   "Halt TX" button stops pancetta's TX within one slot.
4. **Hostile-replay negative check (spec item 4):** with `accept_udp_requests = false`
   (the default), have a LAN host replay a previously-captured valid `Reply` packet at
   pancetta and confirm it produces **zero TX** and an audit trail entry (refused, not
   silently dropped) in `Shift+D`.

## Concurrent multi-client station agent (2026-07-20)

`pancetta-agent/src/multi_session.rs` (new), `pancetta-agent/src/relay.rs`,
`pancetta/src/coordinator/station_agent/{mod.rs,net.rs}`,
`pancetta/src/coordinator/{mod.rs,remote_gateway/mod.rs}`: the station agent now admits up to
`MAX_PEERS` (8 — the relay's own `MAX_CLIENTS` cap) concurrent, independently-Noise-sessioned
clients over the one relay websocket, instead of exactly one. Six implementation tasks landed,
all with clean reviews (no Critical/Important findings):

1. **`CAPACITY` terminal-code fix** — `relay.rs` was missing the 12th relay.v1 terminal code
   (sent by the relay on a 9th-client attempt); now handled like the other 11.
2. **Timeout-bounded websocket receive** — `WsConn::recv_text` (and `RealWsConn`) gained a
   bounded-wait variant so the session loop can interleave control-frame reads with other
   per-tick work instead of blocking indefinitely.
3. **`MultiPeerSession`** (`pancetta-agent`, new module beside `session.rs`) — demuxes the
   single relay leg's `env` frames by DO-authenticated `src` into per-peer Noise state
   (`HashMap<String, PeerState>`), admission-checks an unknown `src` against the station-local
   `tx_allow_list` *before* allocating any handshake state, and reports `Plaintext`/
   `PeerEstablished`/`PeerDown`/`Idle`/`Closed` to the caller. `AgentSession` is kept untouched
   as the tested single-peer reference.
4. **Coordinator switch to `MultiPeerSession`** with per-peer identity binding — `ArmContext`
   gained a `peers: HashMap<String, PeerCtx>` (each peer's own `hello_scopes`, rooted in the
   actual demuxed `src` of the frame that carried its capability token, never a session-global
   value or another peer's identity).
5. **One-controller-at-a-time, free grab** — `ArmContext::controller: Option<String>`. Rules:
   `takeControl` from any admitted peer always succeeds, disarming a displaced controller's live
   arm first (arms never transfer — the new controller must re-arm through the full grant
   verification); `releaseControl` from the controller clears it (disarms if armed); a
   control-mutating action from `controller == None` implicitly grabs it (so a legacy client that
   never sends `takeControl` still arms exactly as it always has); the same action from a
   non-controller while someone else holds control is refused with an audited error frame, never
   an implicit grab; controller `down`/session teardown/shutdown clears `controller` and disarms,
   a listener disconnecting disarms nothing.
6. **Shared display feed / relay read stream** — the gateway's bus→`ServerEvent` translation
   pump (`handle_bus_msg`) was hoisted into `remote_gateway::DisplayFeed`, started when *either*
   the localhost gateway or the station agent is enabled (`display_feed_enabled` generalizes the
   old `gateway_enabled` flag; TUI emit sites are untouched, additive-only). The station agent
   subscribes a `broadcast::Receiver<ServerEvent>` and, between control-frame reads, drains it
   and fans each event out as an encrypted `env` per established peer — the module doc's old
   "read stream (minimal v1)" aspiration is now real. `ServerEvent::ControlState` (already
   defined in rig-api.v1, no wire change) is sent per-peer on every controller/arm transition and
   on session establishment.

**Two deliberate safety asymmetries** (reviewed and intentional, not gaps):

- **`Disarm` is accepted from ANY established peer**, controller or not — fail-safe TX-OFF beats
  exclusivity, consistent with disarm-any's pre-existing posture.
- **`Heartbeat` is accepted from any peer** — `ArmState`'s existing `arm_jti` + monotonic-`seq`
  binding already guarantees only the armer's heartbeats can slide the dead-man window, so
  restricting heartbeat acceptance to the controller would add no safety and would risk
  spurious disarms if control changes hands mid-session.

No relay or cqdx-side wire changes: `controlState`, `error`, `takeControl`, and `releaseControl`
all already existed in rig-api.v1 — this work defines semantics for the previously no-op verbs.
Spec: `docs/superpowers/specs/2026-07-20-concurrent-multi-client-station-agent-design.md`
(Status: Implemented). Plan:
`docs/superpowers/plans/2026-07-20-concurrent-multi-client-station-agent.md`.

## `armedUntil` ceiling raised 10 min → 60 min (dispensa Q-0048), 2026-07-29

panino's operator hit this live: `armedUntil`'s v1 conservative 10-minute ceiling (originated as an
arbitrary `N` instantiation in dispensa ADR-0002 §Decision #5, never derived from a Part-97 number or
specific threat model) forced a full re-arm ceremony every 10 minutes during any real operating
session. Confirmed via `docs/fcc-part97-compliance.md`: §97.109(c) control-operator presence is what
the 5-15s dead-man heartbeat already enforces on a fast, continuous cadence; `armedUntil` is a
coarser, independent worst-case-exposure backstop, not doing compliance work the heartbeat isn't
already doing. cqdx concurred (2026-07-28) on 60 minutes as a fixed contract constant (not
station-configurable — matches the existing `MIN_HEARTBEAT_SEC`/`MAX_HEARTBEAT_SEC` pattern).

Coordinated three-sided bump, in-place `e2e-auth.v1` text/bound amendment (not a `v2` — no field
shape changes): dispensa's `contracts/auth/e2e-auth.v1.schema.json` `txArmGrant.armedUntil`
description → `<= 60 min`; pancetta's `MAX_ARM_MS` (`pancetta-agent/src/capability.rs`) →
`3_600_000`; panino's `TxArmController.arm()` `BOUNDS` ceiling updates separately on panino's side
(not this repo). None of the three should land independently — a mismatched trio would be a live
footgun (e.g. panino allowing 60 min while pancetta still rejected above 10). Full thread: dispensa
`questions/0048-relax-tx-arm-armeduntil-10min-ceiling.md`.

## Remote-TX arm-gate drop gains full visibility — local, audit, and wire (dispensa Q-0051), 2026-07-30

panino reported (dispensa Q-0051): a remote client's QSO could show `active` via `activeQsos` while
its transmissions were silently dropped by the Step 0a remote-TX arm gate (`tx.rs`), with only an
`info!`-level log line as any trace — no operator-visible signal anywhere, local or remote.
Confirmed both the single-TX (`tx.rs`'s `TransmitRequest` handler) and multi-TX bundle sibling drop
sites matched exactly: unlike their neighboring Step 0 (`TxPolicy::Disabled`) and Step 0b
(drop-stale-TX) gates — both of which already call `emit_diagnostic` — Step 0a called only `info!`.
Also found the QSO-creation-time comment in `station_agent/mod.rs` (`ControlAction::TxRequest`
handling) claimed the TX worker "audits" the drop; it didn't — the TX worker has no access to the
station agent's `AuditLog` at all today (constructed locally inside
`start_station_agent_component`, never a shared coordinator field).

**Phase A (this fix):** both Step 0a sites now call `emit_diagnostic` (`target: "agent.tx"`,
`DiagnosticLevel::Warn` — a security-relevant drop, not routine housekeeping) alongside the existing
log line, upgraded from `info!` to `warn!` to match. Surfaces immediately in the TUI's Diagnostics
overlay (Shift+D), matching the sibling gates' existing pattern exactly — no new abstraction, reuses
the generic `emit_diagnostic` helper already wired into ~13 other call sites. The stale
`station_agent/mod.rs` comment was corrected to describe what's actually true today.

**Phase B (audit event), shipped same day.** The TX worker had no access to the station agent's
`AuditLog` at all — it's constructed locally inside `start_station_agent_component`, never a shared
coordinator field. Fixed by hoisting construction up into `ApplicationCoordinator` itself: a new
`audit_log: AuditLog` field (+ `audit_log()` accessor, mirroring `remote_tx_arm()`), built once at
startup from `[network.station_agent].audit_log_path` (falling back to
`pancetta_agent::audit::default_audit_path()`), cheap to construct unconditionally since `AuditLog`
does no I/O until `append()`. `start_station_agent_component` now uses `self.audit_log()` instead of
building its own; the TX worker captures a clone the same way it already captures `remote_tx_arm`.
Both Step 0a drop sites now append `AuditKind::TxDenied` (operator callsign best-effort read from
the arm state, `None` when nothing to attribute), matching the existing `Arm`-rejection audit
pattern in `station_agent/mod.rs` exactly.

**Phase C (client-visible wire signal), shipped same day — no contract change.** New
`MessageType::TxDenied { reason, qso_id }` bus variant; both Step 0a sites call
`remote_gateway::relay_to_gateway` (the same additive dual-destination pattern already used from
`hamlib.rs`/`autonomous.rs`/`qso.rs`, 8+ existing call sites) to forward it to
`ComponentId::RemoteGateway`. `translate::server_event_from_bus` maps it to the already-frozen,
already-wire-shipped `ServerEvent::Error { component: "tx", message }` — the exact reuse proposed in
the original question, requiring zero `rig-api.v1` schema touch.

Full technical trail: dispensa
`questions/0051-remote-tx-dropped-by-arm-gate-is-invisible-to-clients.md`.

## StationAgent crash teardown is a definitive, attributed disarm (PAN-5), 2026-07-31

A StationAgent task death is a security-state transition, not merely a lost transport. Both the
live session's unwind guard and supervisor teardown disarm the shared `ArmState` with
`DisarmReason::ComponentCrash`, preserving an accurate audit attribution instead of reporting an
operator disarm. If the arm mutex is poisoned, teardown recovers the protected state, removes the
session, applies the disarm effects, and only then calls `clear_poison`; the remote-TX gate therefore
remains fail-closed until no live authorization remains.

The crash invalidates only remote-origin QSOs (`QsoMetadata.remote_origin == true`), reported as
`QsoFailureReason::ComponentCrash("StationAgent")`. Local QSOs do not depend on StationAgent and
continue unchanged. A restarted StationAgent keeps the same shared arm object but cannot inherit an
authorization: re-arming must pass through the normal capability and grant-verification path.
