# pancetta-protocol crate — Implementation Plan (Remote-op Sub-plan A)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Create a new `pancetta-protocol` crate holding the versioned, serde-serializable wire types for the future remote API (Panino client) — the v1 surface only (DX Hunter + QSO progress view; call/answer/CQ/frequency control).

**Architecture:** A standalone leaf crate that depends ONLY on `pancetta-core` (for the already-serde `SlotParity`/`ResponseStep`/`TxPolicy`) + `serde`/`serde_json`/`chrono`. It defines FRESH wire DTOs and command/event enums — it does NOT import or modify `pancetta-tui` or the bus types. Conversions from internal types live later in the gateway (Sub-plan B), not here. This makes the TUI-untouched invariant trivially true.

**Tech Stack:** Rust, serde (tagged enums), chrono (serde feature), optional `ts-rs` for TypeScript export (Panino).

**Spec:** `docs/superpowers/specs/2026-06-26-remote-operation-design.md` (§5.2).

**Invariant (from spec §3):** this sub-plan adds a new crate and touches NOTHING in `pancetta-tui` or the running pipeline. The only edit to existing files is adding the crate to the workspace `members`.

**Execution note:** building competes for CPU with the live decoder; run these builds in a window where the station is paused (or with operator OK).

---

## File structure

| File | Responsibility |
|------|----------------|
| `pancetta-protocol/Cargo.toml` | crate manifest (serde, chrono, pancetta-core) |
| `pancetta-protocol/src/lib.rs` | crate root, `PROTOCOL_VERSION`, module decls, re-exports |
| `pancetta-protocol/src/command.rs` | `ClientCommand` (client→server) |
| `pancetta-protocol/src/event.rs` | `ServerEvent` (server→client) |
| `pancetta-protocol/src/dto.rs` | `DxRow`, `QsoProgress`, `PendingCall`, `DecodedView` |
| `pancetta-protocol/src/session.rs` | `Hello`, `Welcome`, `StateSnapshot`, `ClientFrame`, `ServerFrame` |
| `Cargo.toml` (root) | add `"pancetta-protocol"` to `[workspace] members` + `default-members` |

---

## Task 1: Create the crate skeleton + wire it into the workspace

**Files:**
- Create: `pancetta-protocol/Cargo.toml`
- Create: `pancetta-protocol/src/lib.rs`
- Modify: `Cargo.toml` (root) — `members` + `default-members`

- [ ] **Step 1: Create `pancetta-protocol/Cargo.toml`**

```toml
[package]
name = "pancetta-protocol"
version.workspace = true
edition.workspace = true
publish = false

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
chrono = { workspace = true }
pancetta-core = { path = "../pancetta-core" }

[dev-dependencies]
serde_json = { workspace = true }
```
(If `version.workspace`/`edition.workspace` aren't how sibling crates declare it, copy the exact style from `pancetta-core/Cargo.toml`. `serde`/`serde_json`/`chrono` are `[workspace.dependencies]` per the workspace; `chrono` already has the `serde` feature workspace-wide.)

- [ ] **Step 2: Create `pancetta-protocol/src/lib.rs`**

```rust
//! Versioned, serde-serializable wire protocol for the pancetta remote API
//! (consumed by the Panino client). Fresh DTOs — decoupled from TUI/bus
//! internals; conversions live in the coordinator's remote gateway.
#![forbid(unsafe_code)]

/// Wire protocol version. Bump on any incompatible change; clients negotiate
/// it in the `Hello`/`Welcome` handshake.
pub const PROTOCOL_VERSION: u32 = 1;

pub mod command;
pub mod dto;
pub mod event;
pub mod session;

pub use command::ClientCommand;
pub use dto::{DecodedView, DxRow, PendingCall, QsoProgress};
pub use event::ServerEvent;
pub use session::{ClientFrame, Hello, ServerFrame, StateSnapshot, Welcome};
```

- [ ] **Step 3: Add the crate to the workspace** — in root `Cargo.toml`, add `"pancetta-protocol"` to the `[workspace] members` array and to `default-members` (alphabetical placement near `pancetta-qso`/`pancetta-research`). Leave `pancetta-research` exclusion intact.

- [ ] **Step 4: Verify it builds** — Run: `cargo build -p pancetta-protocol`. Expected: fails (modules `command`/`dto`/`event`/`session` not yet created). That's fine — Task 2 onward create them. (If you prefer a green checkpoint here, temporarily stub the four modules as empty files, then build clean, then commit.)

- [ ] **Step 5: Commit**

```bash
cargo fmt -p pancetta-protocol
git add pancetta-protocol/Cargo.toml pancetta-protocol/src/lib.rs Cargo.toml
git commit -m "feat(protocol): scaffold pancetta-protocol crate + workspace member"
```

---

## Task 2: DTOs (`dto.rs`) — the view payloads

**Files:**
- Create: `pancetta-protocol/src/dto.rs`
- Test: in-file `#[cfg(test)]`

These mirror the v1-relevant fields of the internal types (`DxStation`, `ActiveQsoSnapshotItem`, `PendingCallSnapshotItem`, `DecodedMessageView`) but are the protocol's OWN definitions. Timestamps use `chrono::DateTime<Utc>` (serde-friendly). `SlotParity` comes from `pancetta-core` (already serde).

- [ ] **Step 1: Write `dto.rs`**

```rust
//! View payloads sent server→client.
use chrono::{DateTime, Utc};
use pancetta_core::slot::SlotParity;
use serde::{Deserialize, Serialize};

/// A DX-Hunter row (a spotted/decoded station).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DxRow {
    pub call_sign: String,
    pub grid_square: Option<String>,
    pub frequency_hz: f64,
    pub mode: String,
    pub snr: i32,
    pub distance_km: Option<f64>,
    pub bearing: Option<f64>,
    pub worked_before: bool,
    pub needed: bool,
    pub atno: bool,
    pub priority: u32,
    pub entity_name: Option<String>,
    pub rarity_tier: Option<String>,
    pub audio_offset_hz: Option<u64>,
    pub slot_parity: Option<SlotParity>,
    pub last_seen: DateTime<Utc>,
    /// "local" | "network" | "both" (string for forward-compat).
    pub source: String,
}

/// QSO progress (the exchange ladder + last messages).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QsoProgress {
    pub qso_id: String,
    pub their_callsign: String,
    pub state: String,
    pub frequency_hz: f64,
    pub tx_parity: Option<SlotParity>,
    pub ladder_labels: Vec<String>,
    pub ladder_ours: Vec<bool>,
    pub ladder_index: usize,
    pub now_line: String,
    pub next_line: String,
    pub last_tx_text: Option<String>,
    pub last_rx_text: Option<String>,
    pub report_sent: Option<i32>,
    pub report_received: Option<i32>,
    pub dx_last_activity: Option<String>,
    pub started_at: DateTime<Utc>,
}

/// A manual call parked in the cross-parity queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingCall {
    pub callsign: String,
    pub dx_parity: Option<SlotParity>,
    pub waited_secs: u64,
}

/// A single decoded FT8 frame (the live decode feed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecodedView {
    pub timestamp: DateTime<Utc>,
    pub frequency_hz: f64,
    pub snr: i32,
    pub delta_time: f32,
    pub delta_freq: f32,
    pub call_sign: Option<String>,
    pub grid_square: Option<String>,
    pub message: String,
    pub slot_parity: Option<SlotParity>,
    pub is_directed_at_us: bool,
    pub worked_before: bool,
    pub needed: bool,
    pub atno: bool,
    pub priority_score: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dxrow_roundtrips() {
        let r = DxRow {
            call_sign: "D2UY".into(), grid_square: Some("JI64".into()),
            frequency_hz: 14_074_000.0, mode: "FT8".into(), snr: -11,
            distance_km: None, bearing: None, worked_before: false,
            needed: true, atno: true, priority: 720, entity_name: Some("Angola".into()),
            rarity_tier: Some("rare".into()), audio_offset_hz: Some(1934),
            slot_parity: Some(SlotParity::Even), last_seen: Utc::now(), source: "local".into(),
        };
        let j = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<DxRow>(&j).unwrap(), r);
    }

    #[test]
    fn qsoprogress_and_pending_and_decoded_roundtrip() {
        let q = QsoProgress {
            qso_id: "abc".into(), their_callsign: "ZL3IO".into(), state: "WaitingForReport".into(),
            frequency_hz: 14_075_500.0, tx_parity: Some(SlotParity::Odd),
            ladder_labels: vec!["Grid".into(), "Rpt".into()], ladder_ours: vec![true, false],
            ladder_index: 1, now_line: "TX: ZL3IO K5ARH R-09".into(), next_line: "RR73".into(),
            last_tx_text: Some("ZL3IO K5ARH R-09".into()), last_rx_text: Some("K5ARH ZL3IO -12".into()),
            report_sent: Some(-9), report_received: Some(-12), dx_last_activity: Some("\u{2192} us -12".into()),
            started_at: Utc::now(),
        };
        let j = serde_json::to_string(&q).unwrap();
        assert_eq!(serde_json::from_str::<QsoProgress>(&j).unwrap(), q);

        let p = PendingCall { callsign: "VK9XX".into(), dx_parity: Some(SlotParity::Even), waited_secs: 45 };
        assert_eq!(serde_json::from_str::<PendingCall>(&serde_json::to_string(&p).unwrap()).unwrap(), p);

        let d = DecodedView {
            timestamp: Utc::now(), frequency_hz: 14_075_931.0, snr: -8, delta_time: 0.2, delta_freq: 0.0,
            call_sign: Some("D2UY".into()), grid_square: Some("JI64".into()), message: "CQ D2UY JI64".into(),
            slot_parity: Some(SlotParity::Even), is_directed_at_us: false, worked_before: false,
            needed: true, atno: true, priority_score: Some(720),
        };
        assert_eq!(serde_json::from_str::<DecodedView>(&serde_json::to_string(&d).unwrap()).unwrap(), d);
    }
}
```

- [ ] **Step 2: Run tests** — `cargo test -p pancetta-protocol dto::` → PASS. (If `lib.rs` still references not-yet-created `command`/`event`/`session`, temporarily comment those `mod`/`pub use` lines, or do Tasks 2-4 before first build. Recommended: create all four module files (Tasks 2-4) before building, then build once.)

- [ ] **Step 3: Commit**

```bash
cargo fmt -p pancetta-protocol
git add pancetta-protocol/src/dto.rs
git commit -m "feat(protocol): v1 view DTOs (DxRow/QsoProgress/PendingCall/DecodedView) + roundtrip tests"
```

---

## Task 3: `ClientCommand` (`command.rs`)

**Files:**
- Create: `pancetta-protocol/src/command.rs`

- [ ] **Step 1: Write `command.rs`**

```rust
//! Commands sent client→server. v1 control surface (call/answer/CQ/frequency)
//! plus session/control primitives (enforced server-side in a later sub-plan).
use pancetta_core::{slot::SlotParity, ResponseStep};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ClientCommand {
    /// Set the RX dial (Hz). `vfo` 0 = A.
    SetFrequency { vfo: u8, frequency_hz: u64 },
    /// Enable/disable split; `tx_frequency_hz` ignored when disabled.
    SetSplit { enabled: bool, tx_frequency_hz: u64 },
    /// Call a station selected from the DX Hunter list.
    CallStation { callsign: String, frequency_hz: u64, dx_parity: Option<SlotParity> },
    /// Answer a station calling us, opening at `step`.
    AnswerCaller {
        callsign: String,
        frequency_hz: u64,
        dx_parity: Option<SlotParity>,
        step: ResponseStep,
        snr: Option<f32>,
    },
    /// Start a manual CQ at the given audio offset (Hz).
    StartCq { frequency_offset_hz: f64 },
    /// Stop an unanswered manual CQ.
    StopCq,
    /// Request the control role (one control operator at a time).
    TakeControl,
    /// Release the control role.
    ReleaseControl,
    /// Arm/disarm transmit for this control session (must be armed before any TX).
    SetTransmitArmed { armed: bool },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_roundtrips_tagged() {
        let cmds = vec![
            ClientCommand::SetFrequency { vfo: 0, frequency_hz: 14_074_000 },
            ClientCommand::CallStation { callsign: "D2UY".into(), frequency_hz: 14_074_000, dx_parity: Some(SlotParity::Even) },
            ClientCommand::AnswerCaller { callsign: "ZL3IO".into(), frequency_hz: 14_075_500, dx_parity: Some(SlotParity::Odd), step: ResponseStep::Report, snr: Some(-12.0) },
            ClientCommand::StartCq { frequency_offset_hz: 1500.0 },
            ClientCommand::SetTransmitArmed { armed: true },
        ];
        for c in cmds {
            let j = serde_json::to_string(&c).unwrap();
            assert_eq!(serde_json::from_str::<ClientCommand>(&j).unwrap(), c, "json was {j}");
        }
    }
}
```
(Confirm `ResponseStep`'s variant name for the plain report rung — the test uses `ResponseStep::Report`; check `pancetta-core/src/response_step.rs` for the actual variant names and adjust the test value if different. The enum derives serde already.)

- [ ] **Step 2: Run** — `cargo test -p pancetta-protocol command::` → PASS.
- [ ] **Step 3: Commit**

```bash
cargo fmt -p pancetta-protocol && git add pancetta-protocol/src/command.rs
git commit -m "feat(protocol): ClientCommand v1 surface + tagged-roundtrip test"
```

---

## Task 4: `ServerEvent` (`event.rs`) + session/handshake (`session.rs`)

**Files:**
- Create: `pancetta-protocol/src/event.rs`
- Create: `pancetta-protocol/src/session.rs`

- [ ] **Step 1: Write `event.rs`**

```rust
//! Events sent server→client.
use crate::dto::{DecodedView, DxRow, PendingCall, QsoProgress};
use pancetta_core::{slot::SlotParity, TxPolicy};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ServerEvent {
    Decoded(DecodedView),
    DxHunter { rows: Vec<DxRow> },
    ActiveQsos { qsos: Vec<QsoProgress>, pending: Vec<PendingCall> },
    Frequency { vfo: u8, frequency_hz: u64 },
    Split { tx_hz: u64 },
    SignalStrength { db_over_s9: i32 },
    TxStatus { active: bool },
    TxPolicy { policy: TxPolicy },
    /// Control/arm state for the receiving session.
    ControlState { control_held_by_me: bool, transmit_armed: bool },
    Status { component: String, status: String },
    Error { component: String, message: String },
}

// Silence unused import if SlotParity ends up only used via DTOs; keep for
// forward events that may carry parity directly.
#[allow(unused_imports)]
use SlotParity as _KeepSlotParity;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::DecodedView;
    use chrono::Utc;
    #[test]
    fn event_roundtrips() {
        let e = ServerEvent::TxPolicy { policy: TxPolicy::Full };
        assert_eq!(serde_json::from_str::<ServerEvent>(&serde_json::to_string(&e).unwrap()).unwrap(), e);
        let d = ServerEvent::Decoded(DecodedView {
            timestamp: Utc::now(), frequency_hz: 14_075_931.0, snr: -8, delta_time: 0.2, delta_freq: 0.0,
            call_sign: Some("D2UY".into()), grid_square: None, message: "CQ D2UY JI64".into(),
            slot_parity: None, is_directed_at_us: false, worked_before: false, needed: true, atno: true,
            priority_score: Some(720),
        });
        assert_eq!(serde_json::from_str::<ServerEvent>(&serde_json::to_string(&d).unwrap()).unwrap(), d);
    }
}
```
(If the `SlotParity` keep-alive import causes a warning/clippy issue, simply drop it — only import `TxPolicy` and the DTOs. Verify against `cargo clippy`.)

- [ ] **Step 2: Write `session.rs`**

```rust
//! Handshake + full-state snapshot + top-level wire frames.
use crate::command::ClientCommand;
use crate::dto::{DecodedView, DxRow, PendingCall, QsoProgress};
use crate::event::ServerEvent;
use pancetta_core::TxPolicy;
use serde::{Deserialize, Serialize};

/// Client's opening frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub client_name: String,
    pub client_version: String,
}

/// Server's reply: negotiated version + full current state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol_version: u32,
    pub server_version: String,
    pub snapshot: StateSnapshot,
}

/// Full current state sent once on connect, before live deltas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub frequency_hz: u64,
    pub split_tx_hz: u64,
    pub tx_policy: TxPolicy,
    pub dx_hunter: Vec<DxRow>,
    pub active_qsos: Vec<QsoProgress>,
    pub pending_calls: Vec<PendingCall>,
    pub recent_decodes: Vec<DecodedView>,
}

/// Top-level frame client→server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ClientFrame {
    Hello(Hello),
    Command(ClientCommand),
}

/// Top-level frame server→client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ServerFrame {
    Welcome(Welcome),
    Event(ServerEvent),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROTOCOL_VERSION;
    #[test]
    fn frames_roundtrip() {
        let hello = ClientFrame::Hello(Hello {
            protocol_version: PROTOCOL_VERSION, client_name: "Panino".into(), client_version: "0.1.0".into(),
        });
        assert_eq!(serde_json::from_str::<ClientFrame>(&serde_json::to_string(&hello).unwrap()).unwrap(), hello);

        let snap = StateSnapshot {
            frequency_hz: 14_074_000, split_tx_hz: 0, tx_policy: TxPolicy::Full,
            dx_hunter: vec![], active_qsos: vec![], pending_calls: vec![], recent_decodes: vec![],
        };
        let welcome = ServerFrame::Welcome(Welcome {
            protocol_version: PROTOCOL_VERSION, server_version: "0.9.5".into(), snapshot: snap,
        });
        assert_eq!(serde_json::from_str::<ServerFrame>(&serde_json::to_string(&welcome).unwrap()).unwrap(), welcome);
    }
}
```

- [ ] **Step 3: Build + test the whole crate** — `cargo build -p pancetta-protocol` → clean; `cargo test -p pancetta-protocol` → all pass.
- [ ] **Step 4: Commit**

```bash
cargo fmt -p pancetta-protocol
git add pancetta-protocol/src/event.rs pancetta-protocol/src/session.rs
git commit -m "feat(protocol): ServerEvent + handshake/StateSnapshot/frames + roundtrip tests"
```

---

## Task 5: Verify the TUI/workspace invariant + finalize

**Files:** none (verification only)

- [ ] **Step 1: Confirm nothing else changed** — `git diff --stat main -- pancetta-tui` should be EMPTY (the TUI is untouched). `git status` should show only the new crate + the root `Cargo.toml` member edit.
- [ ] **Step 2: Workspace builds** — `cargo build --workspace` → clean (the new crate compiles; nothing else changed). (Heavy build — run in a station-paused window.)
- [ ] **Step 3: clippy clean** — `cargo clippy -p pancetta-protocol --all-targets -- -D warnings` → no warnings (drop the keep-alive import if it warns).
- [ ] **Step 4: No commit needed** (verification). If clippy required dropping the unused import, commit that one-line fix.

---

## Task 6 (OPTIONAL): `ts-rs` TypeScript export for Panino

Only do this if it stays clean; otherwise defer to the Panino handoff. Adds `ts-rs` as a dep, `#[derive(TS)]` + `#[ts(export)]` on the public types, and a test that emits `.ts` files under `pancetta-protocol/bindings/`. Gives the Panino peer session ready-made TypeScript types. If `ts-rs` complicates the build or chrono/SlotParity need `ts` impls, SKIP and note it — the serde JSON contract is the source of truth regardless.

---

## Self-review notes (author)

- **Spec coverage (§5.2):** versioned (`PROTOCOL_VERSION` + handshake) ✓; serde wire types ✓; commands (v1 subset: SetFrequency/SetSplit/CallStation/AnswerCaller/StartCq/StopCq + control/arm) ✓; events (Decoded/DxHunter/ActiveQsos/Frequency/Split/SignalStrength/TxStatus/TxPolicy/ControlState) ✓; `StateSnapshot`-on-connect ✓; ts-rs (optional task) ✓.
- **v1 scope (spec §10.1):** DX Hunter (`DxRow`/`ServerEvent::DxHunter`) ✓; QSO progress (`QsoProgress`/`ActiveQsos`) ✓; control = call-from-DX-Hunter (`CallStation`), answer caller (`AnswerCaller`), call CQ (`StartCq`), frequency select (`SetFrequency`) ✓.
- **TUI invariant:** no pancetta-tui edits; new crate only. ✓ (Task 5 asserts the empty TUI diff.)
- **Decoupling:** protocol depends only on pancetta-core + serde/chrono; conversions deferred to the gateway (Sub-plan B). ✓
- **Naming consistency:** `ClientCommand`/`ServerEvent`/`ClientFrame`/`ServerFrame`/`StateSnapshot`/`DxRow`/`QsoProgress`/`PendingCall`/`DecodedView` used consistently across tasks.
- **Verify-at-build:** `ResponseStep` variant name (Task 3) and the `SlotParity` keep-alive import (Task 4) flagged to confirm against real source.

## What's next (not this plan)
- **Sub-plan B:** `remote_gateway` component (WS server + handshake + snapshot + event fan-out, read-only) — builds bus→protocol conversions (`ActiveQsoSnapshotItem`→`QsoProgress`, decode→`DecodedView`, DX-Hunter→`DxRow`).
- **Sub-plan C:** control + station-paired auth + transmit-arm + fail-safe + arbitration + wiring the v1 `ClientCommand`s to the bus.
- Then the **Panino handoff doc** for the peer session.
