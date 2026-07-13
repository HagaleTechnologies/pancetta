# WSJT-X UDP Emit + Consume — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pancetta emits the WSJT-X UDP companion protocol (Heartbeat/Status/Decode/Clear/QSOLogged/LoggedADIF/Close) so GridTracker/JTAlert/loggers light up unchanged — including on another machine — and consumes Reply(4)/HaltTx(8)/Replay(7) so GridTracker can remotely initiate (arm-gated, fail-closed) and halt QSOs.

**Architecture:** Per `docs/superpowers/specs/2026-07-13-wsjtx-udp-design.md`. New coordinator component `pancetta/src/coordinator/wsjtx_udp/{mod.rs,codec.rs}`; one ephemeral-bound tokio UdpSocket; additive-only emit taps; five ANDed fail-closed gates on remote initiation. Wire format per `docs/superpowers/specs/2026-07-13-wsjtx-udp-protocol-notes.md` (byte-exact tables; "the protocol notes" below). Every field list referenced from the notes §4 is normative — do not improvise fields.

**Tech Stack:** tokio UdpSocket (`socket2` only if `IP_MULTICAST_IF` needs it — check whether tokio's `UdpSocket` + `std::net` cover `set_multicast_if_v4`/`set_multicast_ttl_v4`; std's `UdpSocket` does, so bind std → convert with `UdpSocket::from_std`). Hand-rolled big-endian codec in the PSKReporter style. No serde for the wire.

## Global Constraints

- **Additive-only emits; TUI byte-identical.** No existing bus send is modified; the Decode tap mirrors the remote-gateway gated block after `pancetta/src/coordinator/ft8.rs:1147`.
- **`enabled = false` (default) ⇒ no socket bound, zero bytes emitted, drain task only.**
- **Remote initiation is arm-gated fail-closed**: Reply-initiated QSOs go through `QsoMessage::StartQso { remote_origin: true }` ONLY — never any path that could yield `TxOrigin::Local`. HaltTx is always honored under `accept_udp_requests` (stop direction).
- **`pancetta-protocol` is untouched** (contract-pinned cross-repo crate).
- **Task 7 (Reply initiation) must not merge before the dispensa question (Task 8 Step 1) is filed** — cross-repo policy for a new network listener that can initiate TX.
- Emit schema 2; accept 2–3; tolerate short datagrams (absent trailing fields) and ignore trailing bytes (protocol notes §3 parsing discipline).
- Every commit passes `cargo fmt --all` + `cargo clippy --workspace --features transmit`.
- Golden-vector bytes are from Apache-2.0 `k0swe/wsjtx-go` test data — keep the attribution comment where they appear.

---

### Task 1: `[network.wsjtx_udp]` config section

**Files:**
- Modify: `pancetta-config/src/network.rs` (new struct next to `RemoteGatewayConfig` at network.rs:202-213; field + merge line in `NetworkConfig` struct at :15-74 and `merge_with` at :1492-1513; validation in `validate_section` at :1305)
- Modify: `pancetta-config/src/lib.rs` merge-guard test (`merge_with_carries_every_field`, lib.rs:853) — add the new section

**Interfaces:**
- Produces: `pancetta_config::network::WsjtxUdpConfig` with exactly these fields (later tasks consume them by name): `enabled: bool` (false), `destination: String` ("127.0.0.1:2237"), `multicast_interface: String` (""), `multicast_ttl: u32` (3), `instance_id: String` ("WSJT-X - pancetta"), `accept_udp_requests: bool` (false), `allow_tx_initiation: bool` (false), `allowed_request_hosts: Vec<String>` ([]).

- [ ] **Step 1: Write the failing merge-guard extension** — add `assert_carries_all::<network::WsjtxUdpConfig>("WsjtxUdpConfig", &[], |a, b| a.merge_with(b));` to the guard test; run `cargo test -p pancetta-config --lib merge_with_carries 2>&1 | tail -3` → FAIL (type not found).

- [ ] **Step 2: Implement the struct**

```rust
/// WSJT-X-compatible UDP companion-protocol settings (GridTracker, JTAlert,
/// loggers). See docs/superpowers/specs/2026-07-13-wsjtx-udp-design.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsjtxUdpConfig {
    /// Master enable. Off ⇒ no socket is bound and nothing is emitted.
    pub enabled: bool,
    /// Destination `host:port` — unicast (e.g. "192.168.1.20:2237") or a
    /// multicast group (e.g. "224.0.0.73:2237").
    pub destination: String,
    /// Local interface IP to send multicast from ("" = OS default). Only
    /// used when `destination` is a multicast group.
    pub multicast_interface: String,
    /// Multicast TTL (GridTracker uses 3). Only used for multicast.
    pub multicast_ttl: u32,
    /// The protocol `Id` field. Companion apps route by it.
    pub instance_id: String,
    /// Master switch for processing ANY inbound request (Reply/HaltTx/...).
    pub accept_udp_requests: bool,
    /// Reply(4) may initiate a QSO. Also seeds the remote-TX arm gate —
    /// see the design spec's security model. Fail-closed everywhere.
    pub allow_tx_initiation: bool,
    /// For multicast destinations: hosts allowed to send requests. Empty ⇒
    /// all requests refused (and logged). Unicast infers the peer host.
    pub allowed_request_hosts: Vec<String>,
}

impl Default for WsjtxUdpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            destination: "127.0.0.1:2237".to_string(),
            multicast_interface: String::new(),
            multicast_ttl: 3,
            instance_id: "WSJT-X - pancetta".to_string(),
            accept_udp_requests: false,
            allow_tx_initiation: false,
            allowed_request_hosts: Vec::new(),
        }
    }
}

impl ConfigSection for WsjtxUdpConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        if self.enabled {
            self.destination
                .parse::<std::net::SocketAddr>()
                .map_err(|_| ConfigError::InvalidValue {
                    field: "network.wsjtx_udp.destination".to_string(),
                    value: self.destination.clone(),
                })?;
            if self.instance_id.trim().is_empty() {
                return Err(ConfigError::InvalidValue {
                    field: "network.wsjtx_udp.instance_id".to_string(),
                    value: self.instance_id.clone(),
                });
            }
        }
        Ok(())
    }

    fn merge_with(&mut self, other: Self) {
        self.enabled = other.enabled;
        self.destination = other.destination;
        self.multicast_interface = other.multicast_interface;
        self.multicast_ttl = other.multicast_ttl;
        self.instance_id = other.instance_id;
        self.accept_udp_requests = other.accept_udp_requests;
        self.allow_tx_initiation = other.allow_tx_initiation;
        self.allowed_request_hosts = other.allowed_request_hosts;
    }
}
```

Add to `NetworkConfig`: `#[serde(default)] pub wsjtx_udp: WsjtxUdpConfig,` + `self.wsjtx_udp.merge_with(other.wsjtx_udp);` in its `merge_with` + `self.wsjtx_udp.validate_section()?;` in `validate_section`. Note: `Vec<String>` and non-scalar defaults may need an `overrides` entry in the guard harness (see how existing enum/Vec fields are listed at lib.rs:853-912) — follow the harness's existing convention.

- [ ] **Step 3: Tests green** — `cargo test -p pancetta-config --lib 2>&1 | tail -3` (merge guard + validation), clippy clean.

- [ ] **Step 4: Commit** — `feat(config): add [network.wsjtx_udp] section (all-off defaults)`

---

### Task 2: Wire codec with golden-vector tests

**Files:**
- Create: `pancetta/src/coordinator/wsjtx_udp/codec.rs`
- Create: `pancetta/src/coordinator/wsjtx_udp/mod.rs` (just `pub mod codec;` for now)
- Modify: `pancetta/src/coordinator/mod.rs` (declare `pub(crate) mod wsjtx_udp;`)

**Interfaces:**
- Produces: `struct Writer(Vec<u8>)` with `u8/u32/i32/u64/f64/bool/utf8/qtime_ms/qdatetime_utc` push methods; `struct Reader<'a>` with matching `read_*` methods returning `Option<T>` (None = absent trailing field) plus `remaining()`; `enum OutMsg` (Heartbeat/Status/Decode/Clear/QsoLogged/Close/LoggedAdif) with `fn encode(&self, id: &str) -> Vec<u8>`; `enum InMsg` (Heartbeat/Clear/Reply/Close/Replay/HaltTx/Other(u32)) with `fn decode(buf: &[u8]) -> Option<(String /*id*/, InMsg)>`. Field lists EXACTLY per protocol notes §4; struct fields named after the notes' field names (snake_case).

- [ ] **Step 1: Write the failing golden tests** (real captured WSJT-X packets; bytes from Apache-2.0 k0swe/wsjtx-go test data — keep this attribution):

```rust
#[cfg(test)]
mod golden {
    use super::*;

    // Captured WSJT-X 2.2.2 heartbeat (test vector from k0swe/wsjtx-go, Apache-2.0).
    const HEARTBEAT: &str = "adbccbda00000002000000000000000657534a542d5800000003000000053\
                             22e322e3200000006306439623936";
    // Captured FT8 decode: "~", "JA2EJP N4BP 73", SNR -5, DT 0.2, DF 1302, new=true.
    const DECODE: &str = "adbccbda000000020000000200000006\
                          57534a542d58010259baf8fffffffb3fc99999a000000000000516000000017e\
                          0000000e4a4132454a50204e34425020373300 00";

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn heartbeat_round_trips_byte_exact() {
        let bytes = hex(HEARTBEAT);
        let (id, msg) = InMsg::decode(&bytes).expect("decodes");
        assert_eq!(id, "WSJT-X");
        let InMsg::Heartbeat { max_schema, version, revision } = msg else { panic!() };
        assert_eq!((max_schema, version.as_str(), revision.as_str()), (3, "2.2.2", "0d9b96"));
        // Re-encode must be byte-identical (same field order, schema 2, utf8 lengths).
        let out = OutMsg::Heartbeat { max_schema: 3, version: "2.2.2".into(), revision: "0d9b96".into() };
        assert_eq!(out.encode("WSJT-X"), bytes);
    }

    #[test]
    fn decode_msg_encodes_byte_exact() {
        let out = OutMsg::Decode {
            new: true,
            time_ms: 0x0259baf8,
            snr: -5,
            delta_time: 0.2,
            delta_freq: 1302,
            mode: "~".into(),
            message: "JA2EJP N4BP 73".into(),
            low_confidence: false,
            off_air: false,
        };
        assert_eq!(out.encode("WSJT-X"), hex(DECODE));
    }

    #[test]
    fn reader_tolerates_truncation_and_trailing_bytes() {
        let mut bytes = hex(HEARTBEAT);
        bytes.truncate(bytes.len() - 10); // drop revision → absent field, not error
        let (_, msg) = InMsg::decode(&bytes).expect("short datagram still decodes");
        let InMsg::Heartbeat { revision, .. } = msg else { panic!() };
        assert_eq!(revision, "");
        let mut bytes = hex(HEARTBEAT);
        bytes.extend_from_slice(&[0xde, 0xad]); // trailing bytes → ignored
        assert!(InMsg::decode(&bytes).is_some());
    }

    #[test]
    fn null_and_empty_strings_both_decode_to_empty() {
        let mut w = Writer::new();
        w.utf8_null(); // 0xFFFFFFFF
        w.utf8("");    // 0x00000000
        let mut r = Reader::new(w.as_slice());
        assert_eq!(r.read_utf8(), Some(String::new()));
        assert_eq!(r.read_utf8(), Some(String::new()));
    }

    #[test]
    fn qdatetime_utc_encoding() {
        // 2020-10-30 = JDN 2459153; 12:34:56.789 UTC = 45_296_789 ms; timespec 1.
        let mut w = Writer::new();
        w.qdatetime_utc(2459153, 45_296_789);
        let b = w.as_slice();
        assert_eq!(&b[0..8], &2459153u64.to_be_bytes());
        assert_eq!(&b[8..12], &45_296_789u32.to_be_bytes());
        assert_eq!(b[12], 1);
    }
}
```

Run: `cargo test -p pancetta --lib wsjtx_udp::codec 2>&1 | tail -3` → FAIL (nothing exists).

- [ ] **Step 2: Implement `Writer`/`Reader` primitives** per protocol notes §3 (u32 BE; utf8 = u32 length + bytes with `0xFFFFFFFF` null sentinel, decode both null and empty to `""`; bool = 1 byte accept-nonzero; QDateTime = JDN u64 + ms u32 + timespec u8, tolerate timespec 2 by also consuming an i32 on read; f64 BE). Reader methods return `Option`, `None` at end-of-buffer (the absent-field rule).

- [ ] **Step 3: Implement `OutMsg::encode`** — header (magic `0xadbccbda`, schema 2, type, id) then fields per protocol notes §4 OUT tables: Heartbeat(0), Status(1) all 21 fields in order, Decode(2), Clear(3) header-only, QsoLogged(5) all 17 fields, Close(6) header-only, LoggedAdif(12).

- [ ] **Step 4: Implement `InMsg::decode`** — verify magic, accept schema 2..=3, read type + id, then per §4 IN tables: Heartbeat(0), Clear(3) (optional window byte), Reply(4) (8 fields), Close(6), Replay(7), HaltTx(8) (`auto_tx_only: bool`), everything else → `InMsg::Other(type)` (bytes ignored).

- [ ] **Step 5: Tests green + clippy; commit** — `feat(wsjtx-udp): wire codec with captured-packet golden tests`

---

### Task 3: Component skeleton — socket, Heartbeat, Status, Close

**Files:**
- Modify: `pancetta/src/message_bus.rs:33` (add `ComponentId::WsjtxUdp` variant + its Display/name arm — grep the enum's match sites)
- Modify: `pancetta/src/coordinator/wsjtx_udp/mod.rs` (the component)
- Modify: `pancetta/src/coordinator/mod.rs` (call `start_wsjtx_udp_component` from `run()` next to the remote-gateway start at mod.rs:1482-1483; add `wsjtx_enabled: Arc<AtomicBool>` field next to the gateway's enabled flag)

**Interfaces:**
- Consumes (all mapped in the integration survey): `operating_frequency_hz: Arc<AtomicU64>` (mod.rs:647), `active_protocol_mode: Arc<AtomicU8>` (mod.rs:674, decode via `pancetta_config::OperatingMode::from_u8` + `mode_str` mod.rs:295), `tx_offset_hold_hz: Arc<AtomicU64>` (mod.rs:657), `tx_policy: Arc<AtomicU8>` (mod.rs:547, `TxPolicy::from_u8(..).allows_any_tx()` → TxEnabled), `ptt_active: Arc<AtomicBool>` (mod.rs:755, → Transmitting), `last_decode_timestamp: Arc<AtomicU64>` (mod.rs:794 — Decoding = within the last 2 s), config callsign/grid snapshot (PSKReporter pattern, psk_reporter.rs:61-62). Template: `start_pskreporter_component` (psk_reporter.rs:24) including its disabled-drain task (psk_reporter.rs:28-58).
- Produces: `start_wsjtx_udp_component(&mut self) -> Result<()>`; a `WsjtxState` struct sampling the atomics into a Status message; socket helper `open_socket(cfg: &WsjtxUdpConfig) -> Result<tokio::net::UdpSocket>`.

- [ ] **Step 1: Write the failing test for Status sampling** — pure fn `build_status(snapshot: &StatusSnapshot, cfg_ids: &StationIds) -> OutMsg` (StatusSnapshot = plain struct of the sampled values): assert dial/mode/txdf/de_call/de_grid land in the right fields, TxEnabled = `TxPolicy::Full|RespondOnly`, unset strings null, freq tolerance + TR period = `0xFFFFFFFF`, special op mode = 0. Run → FAIL.

- [ ] **Step 2: Implement `open_socket`** — bind `0.0.0.0:0` (std `UdpSocket`), if destination IP `.is_multicast()`: `set_multicast_ttl_v4(cfg.multicast_ttl)` + `set_multicast_if_v4(&iface)` when `multicast_interface` non-empty; `set_nonblocking(true)`; `tokio::net::UdpSocket::from_std`. Unit-test the multicast-detection branch with a parsed `224.0.0.73:2237` (no actual send needed).

- [ ] **Step 3: Implement the component task** — PSKReporter lifecycle shape: disabled ⇒ drain task on the `ComponentId::WsjtxUdp` bus channel; enabled ⇒ snapshot config, open socket, loop `while !shutdown.load(Ordering::Acquire)`: (a) 15 s Heartbeat tick; (b) 1 s Status sample → emit only when changed (keep `last_status: Option<Vec<u8>>`, compare encoded bytes) and always right after each Heartbeat; (c) drain the bus channel via `try_recv()` (Decode fan-in arrives Task 4); (d) `recv_from` poll with a short timeout for inbound (dispatch arrives Task 6). On loop exit: emit Close(6). Push handle onto `named_task_handles`. First Status must precede any Decode emission — emit one Status immediately after the socket opens (GridTracker instance-validity rule, protocol notes §5).

- [ ] **Step 4: Wire into `run()`** + tests green + clippy. Manual smoke: enable in a scratch config with destination `127.0.0.1:2237`, run `nc -u -l 2237 | xxd | head` (or a 10-line python listener) → observe magic `adbccbda` heartbeats every 15 s and a Status.

- [ ] **Step 5: Commit** — `feat(wsjtx-udp): component skeleton — socket, heartbeat, change-driven status, close`

---

### Task 4: Decode emission, retention ring, Clear, Replay

**Files:**
- Modify: `pancetta/src/coordinator/ft8.rs` (additive gated send after the remote-gateway block at ft8.rs:1147-1163)
- Modify: `pancetta/src/coordinator/wsjtx_udp/mod.rs` (ring + handlers)

**Interfaces:**
- Consumes: `pancetta_ft8::DecodedMessage` (message.rs:966 — `text`, `snr_db: f32`, `time_offset: f64`, `frequency_offset: f64`, `timestamp: SystemTime`); `MessageType::DecodedMessage` bus variant (message_bus.rs:126).
- Produces: additive fan-out block in ft8.rs gated on `wsjtx_enabled` (exact structural mirror of the gateway block — copy it, swap the component id and the gate); in the component: `decode_ring: VecDeque<(OutMsg, StoredKey)>` capped at 500 (matches the TUI channel bound), `StoredKey { time_ms: u32, delta_freq: u32, message: String }` for Reply matching (Task 7); Clear(3) emission when the sampled dial frequency changes band; Replay(7) → re-emit ring with `new=false` then a Status.

- [ ] **Step 1: Failing test** — pure fn `decode_to_msg(d: &DecodedMessage) -> OutMsg`: SNR rounds to i32, `time_ms` = ms-since-UTC-midnight of `timestamp`, `delta_freq` = `frequency_offset as u32`, mode glyph `"~"` **only when the active mode is FT8** (v1 emits Decode only in FT8 mode — protocol notes §8 flag 7), `message` = `text`. Run → FAIL; implement; PASS.

- [ ] **Step 2: The additive tap** — in ft8.rs after :1163, gated `if wsjtx_enabled.load(Ordering::Relaxed)`, send `MessageType::DecodedMessage(decoded_msg.clone())` to `ComponentId::WsjtxUdp` with the same fire-and-forget error handling as the gateway block. Thread the `Arc<AtomicBool>` in exactly like `gateway_enabled`.

- [ ] **Step 3: Component side** — on bus `DecodedMessage`: build `OutMsg::Decode`, push to ring (evict oldest past 500), emit. Band-change detection in the Status sampler: `band_of(dial_hz)` (integer MHz bucket is sufficient) change ⇒ clear ring + emit Clear(3). On `InMsg::Replay`: re-emit ring (`new=false`) + Status.

- [ ] **Step 4: Tests green** (`cargo test -p pancetta --lib wsjtx_udp 2>&1 | tail -3`), clippy, loopback suite still green (`cargo test -p pancetta --test loopback_qso 2>&1 | tail -3` — proves the tap perturbed nothing).

- [ ] **Step 5: Commit** — `feat(wsjtx-udp): decode emission via additive fan-out tap, retention ring, clear-on-band-change, replay`

---

### Task 5: QSOLogged + LoggedADIF

**Files:**
- Modify: `pancetta/src/coordinator/qso.rs` (subscribe wiring next to `start_adif_subscriber` at qso.rs:1142)
- Modify: `pancetta/src/coordinator/wsjtx_udp/mod.rs`

**Interfaces:**
- Consumes: `QsoManager::subscribe() -> broadcast::Receiver<QsoEvent>`; `QsoEvent::QsoCompleted { metadata: QsoMetadata }` (qso_manager.rs:356; `QsoMetadata` fields at states.rs:304: `our_callsign`, `their_callsign`, `frequency: f64` RF Hz, `mode`, `start_time`/`end_time: DateTime<Utc>`, `reports.{sent,received}`, `grids.{ours,theirs}`); `AdifProcessor::qso_to_adif` (adif.rs:467, stamps tx_pwr) + `generate_record` (adif.rs:1073) — the identical reuse the upload subscriber does at qso.rs:4572.
- Produces: a third broadcast subscriber task (pattern: `start_adif_subscriber`, qso.rs:4718) forwarding `(QsoLogged, LoggedAdif)` pairs to the wsjtx component (bus message to `ComponentId::WsjtxUdp` — add a small `MessageType::WsjtxQsoLogged { .. }`? NO: keep the bus untouched; instead pass the `broadcast::Receiver` into `start_wsjtx_udp_component` the same way the ADIF subscriber gets one at qso.rs:1142 — the coordinator owns both, no new bus variant needed).

- [ ] **Step 1: Failing test** — pure fn `qso_to_msgs(m: &QsoMetadata, station_power_watts: u32, adif_record: String) -> (OutMsg, OutMsg)`: DateTimeOn/Off = UTC QDateTimes from start/end (JDN via a small `unix_to_jdn` helper — test: 2020-10-30 → 2459153), TxFrequency = `frequency as u64`, reports/grids/calls mapped per protocol notes §4, TxPower = watts string, ADIF fragment = `<adif_ver:5>3.1.0\n<programid:8>pancetta\n<EOH>\n` + record. Run → FAIL; implement; PASS.

- [ ] **Step 2: Wire the subscriber** — in the component task, `tokio::select!`-style poll of the `broadcast::Receiver` alongside the existing loop arms; on `QsoCompleted`, render via the shared `AdifProcessor` (construct it exactly as the upload subscriber does) and emit both messages.

- [ ] **Step 3: Tests + clippy green; commit** — `feat(wsjtx-udp): QSOLogged + LoggedADIF on QSO completion (third broadcast subscriber)`

---

### Task 6: Inbound dispatch — gates, HaltTx, audit

**Files:**
- Modify: `pancetta/src/coordinator/wsjtx_udp/mod.rs`

**Interfaces:**
- Consumes: `abort_current_tx: Arc<AtomicBool>` (mod.rs:521 — the emergency-stop path uses it at tui_relay.rs:1344), `QsoMessage::CancelAllQsos` / `StopCq` (message_bus.rs:618/:606) sent to `ComponentId::Qso`; the `emit_diagnostic` pattern (tx.rs:358-380) with `target: "remote.wsjtx"`.
- Produces: `fn request_allowed(cfg: &WsjtxUdpConfig, src: IpAddr, dest: &SocketAddr) -> bool` (pure, tested): master switch AND (unicast ⇒ src == dest host; multicast ⇒ src ∈ allowed_request_hosts). `fn handle_inbound(...)` dispatching decoded `InMsg`.

- [ ] **Step 1: Failing tests for the gate** —

```rust
    #[test]
    fn request_gate_is_fail_closed() {
        let mut cfg = WsjtxUdpConfig::default();
        let dest: SocketAddr = "192.168.1.20:2237".parse().unwrap();
        let peer: IpAddr = "192.168.1.20".parse().unwrap();
        let stranger: IpAddr = "192.168.1.99".parse().unwrap();
        assert!(!request_allowed(&cfg, peer, &dest)); // master off
        cfg.accept_udp_requests = true;
        assert!(request_allowed(&cfg, peer, &dest));  // unicast: peer == dest host
        assert!(!request_allowed(&cfg, stranger, &dest));
        let mcast: SocketAddr = "224.0.0.73:2237".parse().unwrap();
        assert!(!request_allowed(&cfg, peer, &mcast)); // multicast + empty allowlist ⇒ refuse
        cfg.allowed_request_hosts = vec!["192.168.1.20".into()];
        assert!(request_allowed(&cfg, peer, &mcast));
    }
```

Run → FAIL; implement; PASS.

- [ ] **Step 2: Dispatch** — on `recv_from`: decode; drop bad magic silently; if id ≠ `instance_id` → debug-log, ignore; if `!request_allowed` → emit `DiagnosticEvent(remote.wsjtx, Warn, "refused request from <src>: <type>")`, ignore. Then: `HaltTx { auto_tx_only }` ⇒ `abort_current_tx.store(true)` always, plus `CancelAllQsos` + `StopCq` when `!auto_tx_only`, plus an Info diagnostic; `Replay` ⇒ Task 4 handler; `Reply` ⇒ Task 7 handler (until then: diagnostic "reply received but initiation not implemented"); `Heartbeat`/`Clear`/`Close`/`Other` ⇒ debug-log.

- [ ] **Step 3: Tests + clippy; manual check** — with `accept_udp_requests = false`, hand-send a valid HaltTx datagram (10-line python from the golden vectors) → observe the refusal diagnostic in Shift+D, no state change. Commit — `feat(wsjtx-udp): inbound dispatch with fail-closed source gating; HaltTx honored; full audit trail`

---

### Task 7: Reply(4) → remote QSO initiation (arm-gated)

**Files:**
- Modify: `pancetta/src/coordinator/wsjtx_udp/mod.rs` (Reply handler)
- Modify: `pancetta/src/coordinator/mod.rs:863` area (arm seeding)

**Interfaces:**
- Consumes: the decode ring (Task 4); `QsoMessage::StartQso { callsign, frequency, dx_parity, remote_origin }` (message_bus.rs:568 area; handled at qso.rs:2172 with parity admission + self-call refusal); `TxPolicy::from_u8(..).allows_initiation()` (tx_policy.rs:46); the `remote_tx_arm: Arc<Mutex<pancetta_agent::arm::ArmState>>` seeding site (mod.rs:863, seeded from `[network.station_agent].remote_tx_enabled`) — **option A of the design spec: `[network.wsjtx_udp].allow_tx_initiation = true` ALSO seeds this arm, with a loud audit log line**. The station-agent's `tx_kind_to_qso_message` (station_agent/mod.rs:526-560) is the model for stamping `remote_origin: true`.
- Produces: `fn reply_to_call(reply: &ReplyMsg, ring: &VecDeque<...>) -> Option<CallIntent>` (pure, tested) where `CallIntent { callsign: String, frequency_hz: u64, is_cq: bool }`; the handler that turns an allowed CQ/QRZ intent into `StartQso { remote_origin: true }`.

- [ ] **Step 1: Failing tests for matching + CQ detection** —

```rust
    #[test]
    fn reply_matches_only_retained_decodes_and_extracts_cq_caller() {
        let mut ring = VecDeque::new();
        ring.push_back(stored("CQ W1AW FN31", 45_296_000, 1302));
        ring.push_back(stored("K1ABC W9XYZ -07", 45_296_000, 800));
        // Exact echo of a retained CQ → intent on W1AW, is_cq.
        let r = reply("CQ W1AW FN31", 45_296_000, 1302);
        let intent = reply_to_call(&r, &ring).unwrap();
        assert_eq!((intent.callsign.as_str(), intent.is_cq), ("W1AW", true));
        // Non-CQ decode matches but is flagged !is_cq (never arms TX).
        let r = reply("K1ABC W9XYZ -07", 45_296_000, 800);
        assert!(!reply_to_call(&r, &ring).unwrap().is_cq);
        // Unknown decode → None (silently dropped per protocol notes §5).
        assert!(reply_to_call(&reply("CQ NOBODY AA00", 1, 1), &ring).is_none());
        // CQ with directional prefix still extracts the caller.
        ring.push_back(stored("CQ DX ZL1XYZ RF80", 45_297_000, 400));
        let r = reply("CQ DX ZL1XYZ RF80", 45_297_000, 400);
        assert_eq!(reply_to_call(&r, &ring).unwrap().callsign, "ZL1XYZ");
    }
```

Match key: `(message, time_ms, delta_freq)` equality. CQ/QRZ detection: first token `CQ` or `QRZ`; caller = the token after any directional/DX modifier (reuse the existing CQ-parsing in `pancetta_ft8::Ft8Message` if it exposes `from_callsign` — check `message.rs`; prefer the library parse over a hand-rolled split, and adjust the test to construct via the library type if so). `frequency_hz` = current dial + `delta_freq` (RF absolute, matching what `TuiCommand::CallStation` sends). Run → FAIL; implement; PASS.

- [ ] **Step 2: The gated handler** — on `InMsg::Reply` (already past Task 6's gates):

```rust
    // Remote QSO initiation — the design spec's five ANDed fail-closed gates.
    // Gates 1-3 (master switch + source filter) were enforced in dispatch.
    let Some(intent) = reply_to_call(&reply, &self.ring) else {
        emit_diag(Warn, format!("Reply from {src} matched no retained decode — ignored")).await;
        return;
    };
    if !cfg.allow_tx_initiation || !intent.is_cq {
        // Non-CQ replies are targeting-only upstream too (protocol notes §5);
        // v1 treats both cases as refusal-with-audit rather than partial action.
        emit_diag(Warn, format!(
            "Reply({}) refused: allow_tx_initiation={}, is_cq={}",
            intent.callsign, cfg.allow_tx_initiation, intent.is_cq
        )).await;
        return;
    }
    let policy = TxPolicy::from_u8(tx_policy.load(Ordering::Relaxed));
    if !policy.allows_initiation() {
        emit_diag(Warn, format!("Reply({}) refused: TX policy {policy:?}", intent.callsign)).await;
        return;
    }
    // remote_origin: true ⇒ every frame is TxOrigin::Remote ⇒ arm-gated
    // fail-closed in the TX worker + parity-admitted + dup-checked in the
    // QSO engine. Never TxOrigin::Local (repo invariant).
    let msg = ComponentMessage::new(
        ComponentId::WsjtxUdp,
        ComponentId::Qso,
        MessageType::Qso(QsoMessage::StartQso {
            callsign: intent.callsign.clone(),
            frequency: intent.frequency_hz,
            dx_parity: intent.dx_parity,
            remote_origin: true,
        }),
        Instant::now(),
    );
    if message_bus.send_message(msg).await.is_ok() {
        emit_diag(Info, format!("GridTracker/UDP Reply → calling {}", intent.callsign)).await;
    }
```

(Adapt the exact `QsoMessage`/`MessageType` wrapping to the real variant shapes at message_bus.rs — the StartQso construction at tui_relay.rs:1010-1027 is the byte-for-byte model, with `remote_origin: true` instead of `false`.)

- [ ] **Step 3: Arm seeding (option A)** — at the mod.rs:863 seeding site, extend the seed condition: arm iff `network.station_agent.remote_tx_enabled || network.wsjtx_udp.allow_tx_initiation`, and when the wsjtx flag contributes, log `warn!("remote-TX arm seeded by [network.wsjtx_udp].allow_tx_initiation — UDP Reply may initiate QSOs")` plus the same line as a startup DiagnosticEvent. Add a test next to the existing arm-seeding tests (grep `remote_tx_enabled` in mod.rs tests) asserting: wsjtx flag alone ⇒ armed; both flags off ⇒ not armed.

- [ ] **Step 4: End-to-end gate verification** — extend the Task 6 manual check: with `allow_tx_initiation=false`, send a valid Reply echoing a live decode → refusal diagnostic, no QSO. With it true + TxPolicy Disabled (`g` twice) → policy refusal. With everything open + mock rig → QSO appears in the QSO panel with remote origin. Then `cargo test -p pancetta --lib 2>&1 | tail -3` + full loopback suite.

- [ ] **Step 5: Commit** — `feat(wsjtx-udp): Reply(4) remote QSO initiation — five ANDed fail-closed gates, TxOrigin::Remote only`

---

### Task 8: Docs, dispensa question, operator drill

**Files:**
- Modify: `docs/CONFIG.md` (new `[network.wsjtx_udp]` section — full key table per Task 1), `docs/GUIDE.md` (new "how do I… use GridTracker with pancetta?" section incl. the cross-machine §6 recipe from the protocol notes), `docs/ARCHITECTURE.md` (one paragraph: the wsjtx_udp component + its taps), `docs/DECISIONS/remote-operation.md` (dated entry: protocol clean-room basis, option-A arm decision + rationale, v1 non-goals)
- Create: a dispensa question (sibling repo `../dispensa/questions/`, next Q-number): "pancetta adds a LAN UDP listener (WSJT-X protocol) that can initiate remote TX; consent = two config booleans + source allowlist; arm gate shared with station-agent (option A) — concur or counter?" per the contract-first methodology.

**Interfaces:** none new.

- [ ] **Step 1: File the dispensa question** (BLOCKS Task 7's merge, not its development): `git -C ../dispensa pull --rebase`, copy the question format from an existing `questions/Q-00xx`, reference the design spec path, push per dispensa's conventions.
- [ ] **Step 2: Write the doc updates** (CONFIG table = Task 1 fields verbatim; GUIDE section = same-host quickstart + machine-B GridTracker recipe: multicast group `224.0.0.73:2237`, GridTracker Settings→General port 2237 + Multicast on, firewall note, "double-click requires `accept_udp_requests` + `allow_tx_initiation` + your IP in `allowed_request_hosts`").
- [ ] **Step 3: Meatspace handoff** — add to the operator's at-rig list: run the acceptance drill from the design spec §Acceptance items 2-4 (same-host GridTracker, cross-host decode flow, double-click call, HaltTx button, hostile-replay negative check with the master switch off).
- [ ] **Step 4: Final gate + commit + PR** —

```bash
cargo fmt --all -- --check
cargo clippy --workspace --features transmit 2>&1 | tail -3
cargo test --workspace --features transmit 2>&1 | tail -5
```

Commit `docs(wsjtx-udp): CONFIG/GUIDE/ARCHITECTURE/DECISIONS coverage`; push; PR titled "WSJT-X UDP emit + consume (GridTracker interop, arm-gated remote initiation)" referencing both specs.
