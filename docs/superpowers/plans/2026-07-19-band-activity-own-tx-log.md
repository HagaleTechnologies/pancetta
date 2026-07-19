# Band Activity Own-TX Logging (#172) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Interleave our own transmitted frames into the Band Activity panel alongside decoded RX,
so the operator sees the full back-and-forth of a QSO (and CQ calls) in one chronological list
instead of only the RX half.

**Architecture:** A new `MessageType::TxFrameLogged` bus event fires from
`pancetta/src/coordinator/tx.rs::send_tx_queue_status` every time a frame is actually keyed
(`sending: Some(item)`), timestamped at key-time. `tui_relay.rs` relays it 1:1 to a new
`TuiMessage::TxFrameLogged`. `tui_runner.rs` routes it to a new `App::add_tx_frame`, which builds
a `DecodedMessageView` (reusing the existing struct, with a new `is_own_tx: bool` field) and
pushes it into the same `decoded_messages` deque RX rows already live in — same cap, same prune
logic, same `is_directed_at_us`-pinned ordering `App::displayed_messages()` already implements, no
changes needed to that ordering function itself. `band_activity.rs` marks `is_own_tx` rows with a
`»` text marker and a `TX` SNR-column override.

**Tech Stack:** Rust workspace, `pancetta` (coordinator) + `pancetta-tui` crates.

## Global Constraints

- `cargo fmt` must be run for real (not `--check` alone) before every commit.
- Additive only: `MessageType::TxQueueStatus`/`TxItem` (the existing "now sending" transient
  snapshot `qso_status.rs`'s "Now:" line reads) are completely untouched — `TxFrameLogged` is a
  second, independent event fired alongside it, not a replacement.
- Log ALL keyed TX, including CQ calls — no `qso_id`-based filtering (per the approved design
  scope decision).
- Own-TX rows get `is_directed_at_us: true` unconditionally — this is the approved ordering
  decision that pins them into the same top tier as the RX-directed half of the same exchange,
  with zero changes to `App::displayed_messages()`'s existing two-tier ordering.
- Not color-only: the `»`/`TX` text markers carry the meaning themselves, independent of color.
- `DecodedMessageView.frequency` holds **dial MHz** (matching every RX construction site, e.g.
  `14.074`), NOT an audio offset — do not confuse this with `TxItem.freq_hz`, which is an audio
  offset in Hz (e.g. `1500.0`) and belongs in `delta_freq` instead, matching how RX rows already
  use `delta_freq` for audio-frequency-offset display (the `DF` column).

---

## File Structure

- Modify `pancetta-tui/src/app.rs`: `DecodedMessageView` gains `is_own_tx: bool`; new
  `App::add_tx_frame` method; 2 test-fixture construction sites updated.
- Modify `pancetta-tui/src/tui_runner.rs` (Task 1 only): 2 test-fixture construction sites updated
  (all 7 sites across the crate are updated together in Task 1, so the whole crate compiles from
  that task onward).
- Modify `pancetta-tui/src/ui/band_activity.rs` (Task 1): 1 test-fixture construction site updated;
  (Task 5): `»`/`TX` marker rendering for `is_own_tx` rows.
- Modify `pancetta-tui/src/ui/station_card.rs` (Task 1): 1 test-fixture construction site updated.
- Modify `pancetta/src/message_bus.rs` (Task 2): new `MessageType::TxFrameLogged` variant.
- Modify `pancetta/src/coordinator/tx.rs` (Task 2): `send_tx_queue_status` emits `TxFrameLogged`
  alongside the existing `TxQueueStatus`; new test module.
- Modify `pancetta-tui/src/tui_runner.rs` (Task 3): new `TuiMessage::TxFrameLogged` variant +
  handler.
- Modify `pancetta/src/coordinator/tui_relay.rs` (Task 4): new match arm relaying
  `MessageType::TxFrameLogged` → `TuiMessage::TxFrameLogged`.

---

### Task 1: `DecodedMessageView.is_own_tx` + `App::add_tx_frame`

Independent of the bus wiring — testable by calling `add_tx_frame` directly.

**Files:**
- Modify: `pancetta-tui/src/app.rs:112-172` (`DecodedMessageView` struct)
- Modify: `pancetta-tui/src/app.rs:3944-3963` (`fixture_view` test helper, `mod tests`)
- Modify: `pancetta-tui/src/app.rs:3848-3863` (`calculate_dx_priority_fallback_uses_tiered_scorer_when_no_precomputed_score`)
- Modify: `pancetta-tui/src/ui/band_activity.rs:465-482` (`fixture_message` test helper)
- Modify: `pancetta-tui/src/ui/station_card.rs:170-188` (`fixture_view` test helper)
- Modify: `pancetta-tui/src/tui_runner.rs:4044-4063` (`mode_update_clears_live_decode_lists` fixture)
- Modify: `pancetta-tui/src/tui_runner.rs:4314-4332` (G8KHF RR73 fixture)
- Test: `pancetta-tui/src/app.rs` (`mod tests`, bottom of file)

**Interfaces:**
- Produces: `DecodedMessageView.is_own_tx: bool` and `App::add_tx_frame(&mut self, text: String,
  freq_hz: f64, timestamp: chrono::DateTime<chrono::Utc>)` — consumed by Task 3 (tui_runner.rs
  handler) and Task 5 (band_activity.rs renderer).

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `pancetta-tui/src/app.rs`:

```rust
#[tokio::test]
async fn add_tx_frame_produces_own_tx_row_with_correct_fields() {
    let mut config = crate::config::Config::default();
    config.station.call_sign = "K5ARH".to_string();
    config.station.default_frequency = 14.074;
    let mut app = App::new(config, None).await.unwrap();
    let ts = chrono::Utc::now();

    app.add_tx_frame("K5ARH JA1ABC RR73".to_string(), 1500.0, ts);

    assert_eq!(app.decoded_messages.len(), 1);
    let row = &app.decoded_messages[0];
    assert!(row.is_own_tx);
    assert_eq!(row.message, "K5ARH JA1ABC RR73");
    assert_eq!(row.call_sign, None);
    assert_eq!(row.delta_freq, 1500.0);
    assert_eq!(row.frequency, 14.074);
    assert!(row.is_directed_at_us);
    assert_eq!(row.timestamp, ts);
}

#[tokio::test]
async fn add_tx_frame_respects_the_1000_row_cap() {
    let mut app = App::new(crate::config::Config::default(), None)
        .await
        .unwrap();
    for i in 0..1005 {
        app.add_tx_frame(format!("frame {i}"), 1500.0, chrono::Utc::now());
    }
    assert_eq!(app.decoded_messages.len(), 1000);
    // Oldest evicted first (pop_front), newest retained.
    assert_eq!(app.decoded_messages.back().unwrap().message, "frame 1004");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-tui --lib add_tx_frame`
Expected: FAIL — `add_tx_frame` is not defined (compile error).

- [ ] **Step 3: Add the `is_own_tx` field**

In `pancetta-tui/src/app.rs`, in the `DecodedMessageView` struct (lines 112-172), add after the
existing `priority_score` field (line 171, just before the closing `}` at line 172):

```rust
    #[serde(default)]
    pub priority_score: Option<u32>,
    /// `true` when this row is a frame WE transmitted, not a decoded RX
    /// message (#172). `call_sign` is always `None` for these rows — the
    /// full exchange text is already in `message`. Test-default false.
    #[serde(default)]
    pub is_own_tx: bool,
}
```

- [ ] **Step 4: Update every existing `DecodedMessageView` struct-literal construction site**

`is_own_tx` has `#[serde(default)]` (for deserialization only) but Rust struct literals still
require every field explicitly — none of these sites use `..Default::default()`. Add
`is_own_tx: false,` right after `priority_score` in each:

`pancetta-tui/src/app.rs:3944-3963` (`fixture_view`):
```rust
            band_needed: false,
            priority_score: None,
            is_own_tx: false,
        }
    }
```

`pancetta-tui/src/app.rs:3848-3864` (`calculate_dx_priority_fallback_uses_tiered_scorer_when_no_precomputed_score`):
```rust
            band_needed: false,
            priority_score: None,
            is_own_tx: false,
        };
```

`pancetta-tui/src/ui/band_activity.rs:465-482` (`fixture_message`):
```rust
            band_needed: false,
            priority_score: None,
            is_own_tx: false,
        }
    }
```

`pancetta-tui/src/ui/station_card.rs:170-188` (`fixture_view`):
```rust
            band_needed: false,
            priority_score: None,
            is_own_tx: false,
        }
    }
```

`pancetta-tui/src/tui_runner.rs:4044-4063` (`mode_update_clears_live_decode_lists`):
```rust
            band_needed: false,
            priority_score: None,
            is_own_tx: false,
        };
```

`pancetta-tui/src/tui_runner.rs:4314-4332` (G8KHF RR73 fixture, nested inside
`push_back(crate::app::DecodedMessageView { ... })` — one indent level deeper than the site above):
```rust
                    band_needed: false,
                    priority_score: None,
                    is_own_tx: false,
                });
```

> All 7 construction sites in the crate are edited in this one step. This is required for the
> crate to compile at all — Rust struct literals need every field present, so leaving any site
> unedited breaks `cargo build -p pancetta-tui` entirely, not just that one test. Task 3 (which
> also touches `tui_runner.rs`) does NOT re-touch these two sites — they're already done here.

- [ ] **Step 5: Implement `App::add_tx_frame`**

Add near `App::push_qso_history` (`pancetta-tui/src/app.rs:2306-2320`):

```rust
    /// Append a keyed TX frame to Band Activity's history (#172) — reuses
    /// the same `decoded_messages` deque, cap, and prune logic RX rows
    /// already go through, so Band Activity shows a chronologically
    /// interleaved view of everything we heard AND everything we sent.
    /// `is_directed_at_us: true` unconditionally pins TX rows into the same
    /// top tier `App::displayed_messages()` already reserves for RX frames
    /// addressed to us — a QSO's full back-and-forth reads as one block.
    pub fn add_tx_frame(
        &mut self,
        text: String,
        freq_hz: f64,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) {
        self.decoded_messages.push_back(DecodedMessageView {
            timestamp,
            frequency: self.station_info.operating_frequency,
            mode: self.station_info.mode.clone(),
            snr: 0,
            delta_time: 0.0,
            delta_freq: freq_hz as f32,
            call_sign: None,
            grid_square: None,
            message: text,
            distance: None,
            bearing: None,
            slot_parity: None,
            is_directed_at_us: true,
            worked_before: false,
            needed: false,
            atno: false,
            band_needed: false,
            priority_score: None,
            is_own_tx: true,
        });
        // Same highlight-preservation bump `add_decoded_message` does
        // (app.rs:1598-1600) — a push_back makes a new row-0, so a
        // manually-scrolled highlight must shift by one to keep pointing
        // at the same logical row.
        if self.band_activity_scroll > 0 {
            self.band_activity_scroll += 1;
        }
        while self.decoded_messages.len() > 1000 {
            self.decoded_messages.pop_front();
        }
    }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p pancetta-tui --lib add_tx_frame`
Expected: PASS (2 tests)

- [ ] **Step 7: Run the full pancetta-tui test suite to check for regressions from the fixture edits**

Run: `cargo test -p pancetta-tui --lib`
Expected: PASS, 0 failures. Since Step 4 updated all 7 construction sites in one pass (including
the 2 in `tui_runner.rs`), the whole crate compiles and this task is fully self-contained — no
dependency on Task 3 to compile or pass.

- [ ] **Step 8: Commit**

```bash
cargo fmt
git add pancetta-tui/src/app.rs pancetta-tui/src/tui_runner.rs pancetta-tui/src/ui/band_activity.rs pancetta-tui/src/ui/station_card.rs
git commit -m "feat(tui): add DecodedMessageView.is_own_tx + App::add_tx_frame (#172)"
```

---

### Task 2: `MessageType::TxFrameLogged` bus event

Independent of Task 1 — testable via the message bus directly.

**Files:**
- Modify: `pancetta/src/message_bus.rs` (near `TxQueueStatus`, lines 245-250)
- Modify: `pancetta/src/coordinator/tx.rs:310-324` (`send_tx_queue_status`)
- Test: `pancetta/src/coordinator/tx.rs` (new `#[cfg(test)] mod` in this file, alongside
  `tx_failure_diagnostic_tests` at line 3943)

**Interfaces:**
- Produces: `MessageType::TxFrameLogged { text: String, freq_hz: f64, qso_id: Option<String>,
  timestamp: chrono::DateTime<chrono::Utc> }` — consumed by Task 4 (`tui_relay.rs`).

- [ ] **Step 1: Write the failing tests**

Add a new test module to `pancetta/src/coordinator/tx.rs`, alongside the existing
`tx_failure_diagnostic_tests` module (near line 3943):

```rust
#[cfg(test)]
mod tx_frame_logged_tests {
    use super::*;
    use crate::message_bus::{ComponentId, MessageBus, MessageType};

    #[tokio::test]
    async fn keying_a_frame_emits_tx_frame_logged_before_tx_queue_status() {
        let bus = MessageBus::new(16).unwrap();
        let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();

        send_tx_queue_status(
            &bus,
            Some(crate::message_bus::TxItem {
                text: "K5ARH JA1ABC -12".to_string(),
                freq_hz: 1500.0,
                qso_id: Some("qso-1".to_string()),
                deferred: false,
            }),
            Vec::new(),
        )
        .await;

        let msg = receiver
            .try_recv()
            .expect("a TxFrameLogged message should have been sent");
        match msg.message_type {
            MessageType::TxFrameLogged {
                text,
                freq_hz,
                qso_id,
                ..
            } => {
                assert_eq!(text, "K5ARH JA1ABC -12");
                assert_eq!(freq_hz, 1500.0);
                assert_eq!(qso_id.as_deref(), Some("qso-1"));
            }
            other => panic!("expected TxFrameLogged, got {other:?}"),
        }

        let msg2 = receiver.try_recv().expect("TxQueueStatus should follow");
        assert!(matches!(
            msg2.message_type,
            MessageType::TxQueueStatus { .. }
        ));
    }

    #[tokio::test]
    async fn cq_frame_qso_id_is_none() {
        let bus = MessageBus::new(16).unwrap();
        let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();

        send_tx_queue_status(
            &bus,
            Some(crate::message_bus::TxItem {
                text: "CQ K5ARH EM10".to_string(),
                freq_hz: 1200.0,
                qso_id: None,
                deferred: false,
            }),
            Vec::new(),
        )
        .await;

        let msg = receiver.try_recv().expect("TxFrameLogged should send for CQ too");
        match msg.message_type {
            MessageType::TxFrameLogged { qso_id, .. } => assert_eq!(qso_id, None),
            other => panic!("expected TxFrameLogged, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn idle_clear_does_not_emit_tx_frame_logged() {
        let bus = MessageBus::new(16).unwrap();
        let (_sender, receiver) = bus.create_channel(ComponentId::Tui).await.unwrap();

        send_tx_queue_status(&bus, None, Vec::new()).await;

        let msg = receiver.try_recv().expect("TxQueueStatus should still send");
        assert!(matches!(
            msg.message_type,
            MessageType::TxQueueStatus { sending: None, .. }
        ));
        assert!(
            receiver.try_recv().is_err(),
            "no TxFrameLogged when sending is None"
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta --lib tx_frame_logged_tests`
Expected: FAIL — `MessageType::TxFrameLogged` does not exist (compile error).

- [ ] **Step 3: Add the `MessageType::TxFrameLogged` variant**

In `pancetta/src/message_bus.rs`, immediately after the `TxQueueStatus` variant (after its closing
`},` around line 250, before `TxPolicyStatus`):

```rust
    TxQueueStatus {
        /// What is being transmitted right now (keyed). `None` = idle.
        sending: Option<TxItem>,
        /// Items dequeued and scheduled but not yet on the air.
        queued: Vec<TxItem>,
    },

    /// Pushed once per keyed TX frame (#172) — Band Activity's own-TX
    /// history. Emitted from `send_tx_queue_status` alongside (not instead
    /// of) `TxQueueStatus`, whenever `sending` is `Some`; additive, no
    /// change to the existing NOW-SENDING/QUEUED snapshot. `qso_id: None`
    /// means a CQ/manual frame, matching `TxItem`'s existing convention —
    /// every keyed frame is logged regardless of origin (#172 scope: all
    /// TX, not just QSO-related).
    TxFrameLogged {
        text: String,
        freq_hz: f64,
        qso_id: Option<String>,
        timestamp: chrono::DateTime<chrono::Utc>,
    },

    /// TX-policy state echo for the TUI banner. Sent by the coordinator's
```

> Keep the existing `TxPolicyStatus` doc comment and variant exactly as-is below this insertion —
> only add the new variant between `TxQueueStatus` and `TxPolicyStatus`.

- [ ] **Step 4: Emit it from `send_tx_queue_status`**

In `pancetta/src/coordinator/tx.rs`, replace the `send_tx_queue_status` function (lines 310-324):

```rust
/// Push a richer TX-queue snapshot (NOW-SENDING + QUEUED) to the TUI.
/// Best-effort, observation-only: never touches PTT/audio/scheduling.
async fn send_tx_queue_status(
    message_bus: &MessageBus,
    sending: Option<crate::message_bus::TxItem>,
    queued: Vec<crate::message_bus::TxItem>,
) {
    // #172: log every actually-keyed frame for Band Activity's own-TX
    // history, before the existing NOW-SENDING snapshot below. Idle-clear
    // calls (sending: None) emit nothing here — only real key events.
    if let Some(item) = sending.as_ref() {
        let log_msg = ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Tui,
            MessageType::TxFrameLogged {
                text: item.text.clone(),
                freq_hz: item.freq_hz,
                qso_id: item.qso_id.clone(),
                timestamp: chrono::Utc::now(),
            },
            Instant::now(),
        );
        if let Err(e) = message_bus.send_message(log_msg).await {
            tracing::debug!("TxFrameLogged relay failed (no TUI?): {}", e);
        }
    }

    let msg = ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Tui,
        MessageType::TxQueueStatus { sending, queued },
        Instant::now(),
    );
    if let Err(e) = message_bus.send_message(msg).await {
        tracing::debug!("TxQueueStatus relay failed (no TUI?): {}", e);
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p pancetta --lib tx_frame_logged_tests`
Expected: PASS (3 tests)

- [ ] **Step 6: Run the full tx.rs test module to check for regressions**

Run: `cargo test -p pancetta --lib coordinator::tx::`
Expected: PASS, 0 failures — this exercises all 22 existing `send_tx_queue_status(...)` call sites
indirectly; none of their behavior changes (the new emission is additive-only), but this confirms
nothing upstream broke.

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add pancetta/src/message_bus.rs pancetta/src/coordinator/tx.rs
git commit -m "feat(tx): emit MessageType::TxFrameLogged on every keyed frame (#172)"
```

---

### Task 3: `TuiMessage::TxFrameLogged` + `tui_runner.rs` handler

Depends on Task 1 (`App::add_tx_frame`).

**Files:**
- Modify: `pancetta-tui/src/tui_runner.rs:160-168` (`TuiMessage` enum, near `QsoHistoryEntry`)
- Modify: `pancetta-tui/src/tui_runner.rs:747-754` (message handler, near the `QsoHistoryEntry`
  arm)
- Test: `pancetta-tui/src/tui_runner.rs` (`mod key_tests`, using the existing `make_runner()`
  helper at line 2250)

> Note: `tui_runner.rs`'s two `DecodedMessageView` fixture sites (`mode_update_clears_live_decode_lists`
> and the G8KHF RR73 test) were already updated in Task 1, Step 4 — this task only adds the
> `TuiMessage` variant and handler, no further fixture edits needed here.

**Interfaces:**
- Consumes: `App::add_tx_frame(&mut self, text: String, freq_hz: f64, timestamp:
  chrono::DateTime<chrono::Utc>)` (Task 1).
- Produces: `TuiMessage::TxFrameLogged { text: String, freq_hz: f64, qso_id: Option<String>,
  timestamp: chrono::DateTime<chrono::Utc> }` — consumed by Task 4 (`tui_relay.rs`).

- [ ] **Step 1: Write the failing test**

Add to `mod key_tests` in `pancetta-tui/src/tui_runner.rs`, near `tx_queue_update_populates_view`
(line 3181):

```rust
#[tokio::test]
async fn tx_frame_logged_appends_to_band_activity() {
    let (mut r, _cmd_rx, app) = make_runner().await;
    let ts = chrono::Utc::now();
    r.handle_message(TuiMessage::TxFrameLogged {
        text: "K5ARH JA1ABC RR73".to_string(),
        freq_hz: 1500.0,
        qso_id: Some("qso-1".to_string()),
        timestamp: ts,
    })
    .await
    .unwrap();
    let app = app.read().await;
    assert_eq!(app.decoded_messages.len(), 1);
    let row = app.decoded_messages.back().unwrap();
    assert!(row.is_own_tx);
    assert_eq!(row.message, "K5ARH JA1ABC RR73");
    assert_eq!(row.timestamp, ts);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p pancetta-tui --lib tx_frame_logged_appends_to_band_activity`
Expected: FAIL — `TuiMessage::TxFrameLogged` does not exist (compile error).

- [ ] **Step 3: Add the `TuiMessage::TxFrameLogged` variant and the handler**

In `pancetta-tui/src/tui_runner.rs`, add after the `QsoHistoryEntry` variant (lines 160-168):

```rust
    /// Pushed once per QSO reaching a terminal state — #165's last-10-QSOs
    /// history panel.
    QsoHistoryEntry {
        call_sign: String,
        band: String,
        success: bool,
        reason: Option<String>,
        completed_at: chrono::DateTime<chrono::Utc>,
    },
    /// Pushed once per keyed TX frame (#172) — Band Activity's own-TX
    /// history. Forwarded by the coordinator relay from the TX worker's
    /// `TxFrameLogged`, fired alongside (not instead of) `TxQueueUpdate`.
    TxFrameLogged {
        text: String,
        freq_hz: f64,
        qso_id: Option<String>,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
```

Add the handler after the `QsoHistoryEntry` arm (lines 747-754):

```rust
            TuiMessage::QsoHistoryEntry {
                call_sign,
                success,
                completed_at,
                ..
            } => {
                app.push_qso_history(call_sign, success, completed_at);
            }
            TuiMessage::TxFrameLogged {
                text,
                freq_hz,
                timestamp,
                ..
            } => {
                app.add_tx_frame(text, freq_hz, timestamp);
            }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p pancetta-tui --lib tx_frame_logged_appends_to_band_activity`
Expected: PASS

- [ ] **Step 5: Run the full pancetta-tui test suite to check for regressions**

Run: `cargo test -p pancetta-tui --lib`
Expected: PASS, 0 failures.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add pancetta-tui/src/tui_runner.rs
git commit -m "feat(tui): add TuiMessage::TxFrameLogged + relay to App::add_tx_frame (#172)"
```

---

### Task 4: `tui_relay.rs` wiring

Depends on Task 2 (`MessageType::TxFrameLogged`) and Task 3 (`TuiMessage::TxFrameLogged`).

**Files:**
- Modify: `pancetta/src/coordinator/tui_relay.rs:346-368` (near the `TxQueueStatus` match arm)

**Interfaces:**
- Consumes: `MessageType::TxFrameLogged` (Task 2), `TuiMessage::TxFrameLogged` (Task 3).

- [ ] **Step 1: Add the relay match arm**

In `pancetta/src/coordinator/tui_relay.rs`, immediately after the `TxQueueStatus` arm (after its
closing `}` around line 368, before `TxPolicyStatus`):

```rust
                        MessageType::TxQueueStatus {
                            ref sending,
                            ref queued,
                        } => {
                            // Richer NOW-SENDING / QUEUED view. Re-shape the
                            // coordinator's TxItem into the TUI's local
                            // TxQueueItem (decoupled so the TUI doesn't link
                            // the main crate).
                            let map = |it: &crate::message_bus::TxItem| {
                                pancetta_tui::app::TxQueueItem {
                                    text: it.text.clone(),
                                    freq_hz: it.freq_hz,
                                    qso_id: it.qso_id.clone(),
                                    deferred: it.deferred,
                                }
                            };
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::TxQueueUpdate {
                                    sending: sending.as_ref().map(map),
                                    queued: queued.iter().map(map).collect(),
                                },
                            );
                        }
                        MessageType::TxFrameLogged {
                            text,
                            freq_hz,
                            qso_id,
                            timestamp,
                        } => {
                            // #172: pass-through relay for Band Activity's
                            // own-TX history, same shape as QsoHistoryEntry.
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::TxFrameLogged {
                                    text,
                                    freq_hz,
                                    qso_id,
                                    timestamp,
                                },
                            );
                        }
                        MessageType::TxPolicyStatus { policy } => {
```

> This is a direct field-for-field pass-through, structurally identical to the existing
> `QsoHistoryEntry` arm (`tui_relay.rs:441-457`). Per that arm's precedent, this file has no
> existing test coverage of the live relay-loop match arms themselves (the one test module in this
> file, `tui_relay_tests`, only tests small pure helper functions called *from inside* other arms —
> see the comment at `tui_relay.rs:2418-2423` explaining why: "the full async loop isn't unit-
> testable without a live bus"). Task 2's `tx.rs` test proves `TxFrameLogged` is emitted correctly;
> Task 3's `tui_runner.rs` test proves the TUI side handles it correctly once received — this arm
> is the untested-by-precedent seam between them, exactly like `QsoHistoryEntry`'s already is. Do
> not invent new relay-loop test infrastructure here; that would be new scope beyond what this
> plan covers.

- [ ] **Step 2: Full workspace build check (this file has no isolated test to run)**

Run: `cargo build -p pancetta`
Expected: clean build, no errors.

- [ ] **Step 3: Commit**

```bash
cargo fmt
git add pancetta/src/coordinator/tui_relay.rs
git commit -m "feat(relay): wire TxFrameLogged through to the TUI (#172)"
```

---

### Task 5: Band Activity rendering — `»`/`TX` marker for own-TX rows

Depends on Task 1 (`DecodedMessageView.is_own_tx`).

**Files:**
- Modify: `pancetta-tui/src/ui/band_activity.rs:177-330` (`create_message_row`)
- Test: `pancetta-tui/src/ui/band_activity.rs` (`mod tests`, bottom of file)

**Interfaces:**
- Consumes: `DecodedMessageView.is_own_tx: bool` (Task 1).

- [ ] **Step 1: Write the failing test**

Add a `row_text` helper (generalizing the existing row-1-only `header_row_text`) and a new test to
the `mod tests` block in `pancetta-tui/src/ui/band_activity.rs`, near `header_row_text` and
`render_shows_all_columns_at_width_120`:

```rust
    fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    fn header_row_text(buf: &ratatui::buffer::Buffer) -> String {
        row_text(buf, 1)
    }

    #[tokio::test]
    async fn own_tx_row_renders_marker_and_tx_snr() {
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        let mut tx_row = fixture_message("K5ARH");
        tx_row.is_own_tx = true;
        tx_row.is_directed_at_us = true;
        tx_row.call_sign = None;
        tx_row.message = "K5ARH JA1ABC RR73".to_string();
        app.decoded_messages.push_back(tx_row);

        let backend = ratatui::backend::TestBackend::new(120, 20);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| {
            let area = f.area();
            render_band_activity(f, area, &app).unwrap();
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        // Row 0 = block border/title, row 1 = header, row 2 = first data row.
        let data_row = row_text(&buf, 2);
        assert!(data_row.contains("» TX"), "marker missing: {data_row:?}");
        assert!(data_row.contains("RR73"), "message text missing: {data_row:?}");
    }

    #[tokio::test]
    async fn rx_row_unaffected_by_own_tx_marker() {
        let mut app = crate::app::App::new(crate::config::Config::default(), None)
            .await
            .unwrap();
        app.decoded_messages.push_back(fixture_message("K1ABC"));

        let backend = ratatui::backend::TestBackend::new(120, 20);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| {
            let area = f.area();
            render_band_activity(f, area, &app).unwrap();
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let data_row = row_text(&buf, 2);
        assert!(!data_row.contains("» TX"), "unexpected TX marker: {data_row:?}");
        assert!(data_row.contains("K1ABC"), "callsign missing: {data_row:?}");
    }
```

> `header_row_text` already exists at its own definition site — this step REPLACES that existing
> definition with the two-line version shown above (`row_text` + `header_row_text` calling it), it
> does not add a second `header_row_text`. Every existing call site of `header_row_text(&buf)`
> keeps working unchanged since the signature is identical.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pancetta-tui --lib own_tx_row_renders_marker_and_tx_snr rx_row_unaffected_by_own_tx_marker`
Expected: `own_tx_row_renders_marker_and_tx_snr` FAILs (no marker rendered yet);
`rx_row_unaffected_by_own_tx_marker` may already pass vacuously — that's fine, it guards the
"no marker on RX rows" behavior going forward.

- [ ] **Step 3: Render the marker**

In `pancetta-tui/src/ui/band_activity.rs`, in `create_message_row` (lines 177-330):

Replace the `call_str` computation (lines 205-210):

```rust
    // Lead the call column with "→" for directed-at-us decodes so even
    // colorblind / monochrome terminals can spot them at a glance, "● "
    // for an engaged (active-QSO) station, or "» TX" for a frame WE
    // transmitted (#172) — call_sign is always None for these rows, the
    // full exchange text is in the Msg column.
    let call_str = if msg.is_own_tx {
        "» TX".to_string()
    } else {
        match msg.call_sign.as_deref() {
            Some(c) if msg.is_directed_at_us => format!("→ {}", c),
            Some(c) if is_engaged => format!("● {}", c),
            Some(c) => c.to_string(),
            None => "---".to_string(),
        }
    };
```

Replace the `snr_str` computation (line 193):

```rust
    // #172: own-TX rows have no meaningful RX SNR (snr is a 0 sentinel);
    // render "TX" instead so it's never mistaken for a real 0 dB decode —
    // same not-color-only precedent as dx_hunter.rs::format_dx_snr's "---".
    let snr_str = if msg.is_own_tx {
        "TX".to_string()
    } else {
        format!("{:+}", msg.snr)
    };
```

Add an `own_tx_style` and route `snr_style`, `call_style`, and `msg_style` through it (the style
block starting at line 239):

```rust
    // #172: distinct style for own-TX rows so they read as ours at a
    // glance, checked before the existing is_directed_at_us styling
    // (own-TX rows are always is_directed_at_us: true too, but the "»"/
    // "TX" text markers already carry the meaning independent of color —
    // this is additional, not the only, signal).
    let own_tx_style = Style::default()
        .fg(ratatui::style::Color::Red)
        .add_modifier(Modifier::BOLD);

    let snr_style = if msg.is_own_tx {
        own_tx_style
    } else if msg.is_directed_at_us {
        directed_style
    } else {
        Style::default().fg(get_snr_color(msg.snr, &app.theme))
    };
```

The `call_style` block (lines 245-262) — add the `is_own_tx` check as the first arm:

```rust
    let call_style = if msg.is_own_tx {
        own_tx_style
    } else if msg.is_directed_at_us {
        directed_style
    } else if is_globally_focused {
        Style::default()
            .fg(app.theme.selected_color())
            .add_modifier(Modifier::BOLD)
    } else if msg.worked_before {
        // Already in the log on this band — dim the callsign the same
        // way the DX hunter panel does, so the operator's eye skips
        // stations the autonomous scorer would also dup-penalize.
        Style::default().fg(app.theme.muted_color())
    } else if msg.call_sign.is_some() {
        Style::default()
            .fg(app.theme.success_color())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.muted_color())
    };
```

The `msg_style` block (lines 273-281) — add the `is_own_tx` check as the first arm:

```rust
    let msg_style = if msg.is_own_tx {
        own_tx_style
    } else if msg.is_directed_at_us {
        directed_style
    } else if msg.message.contains("CQ") {
        Style::default()
            .fg(app.theme.warning_color())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.foreground_color())
    };
```

> Leave the `is_engaged` computation (line 199-200), `neutral_style`, and `muted_style` blocks
> unchanged — `is_engaged` already short-circuits to `false` for own-TX rows since `call_sign` is
> `None`, and `neutral_style`/`muted_style` already route through `directed_style` (which own-TX
> rows also satisfy via `is_directed_at_us: true`), which is an acceptable, non-jarring look for
> the Time/DT/DF/Grid/Dist columns on a TX row.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pancetta-tui --lib own_tx_row_renders_marker_and_tx_snr rx_row_unaffected_by_own_tx_marker`
Expected: PASS (2 tests)

- [ ] **Step 5: Run the full band_activity.rs test module to check for regressions**

Run: `cargo test -p pancetta-tui --lib ui::band_activity::`
Expected: PASS, 0 failures.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add pancetta-tui/src/ui/band_activity.rs
git commit -m "feat(tui): render » TX marker for own-TX rows in Band Activity (#172)"
```

---

### Task 6: Full workspace verification + PR

**Files:** none (verification only)

- [ ] **Step 1: Run the full pancetta-tui test suite**

Run: `cargo test -p pancetta-tui --lib`
Expected: PASS, 0 failures.

- [ ] **Step 2: Run the full pancetta (coordinator) test suite**

Run: `cargo test -p pancetta --lib`
Expected: PASS, 0 failures.

- [ ] **Step 3: Run the loopback integration test**

Run: `cargo test -p pancetta --test loopback_qso`
Expected: PASS — confirms the new `send_tx_queue_status` emission doesn't disturb the end-to-end
encode→modulate→decode QSO flow.

- [ ] **Step 4: Run clippy across both touched crates**

Run: `cargo clippy -p pancetta -p pancetta-tui --all-targets -- -D warnings`
Expected: clean, no warnings.

- [ ] **Step 5: Run fmt check**

Run: `cargo fmt --check`
Expected: clean. If not, run plain `cargo fmt` and re-check (a non-empty `--check` diff means
unformatted code — fix it for real, don't treat the check itself as the fix).

- [ ] **Step 6: Full workspace build sanity check**

Run: `cargo build --workspace --exclude pancetta-research`
Expected: clean build, no errors.

- [ ] **Step 7: Open the PR**

```bash
git push -u origin <branch-name>
gh pr create --title "feat(tui,tx): log own TX in Band Activity (#172)" --body "$(cat <<'EOF'
## Summary
- New `MessageType::TxFrameLogged` bus event fires from `send_tx_queue_status` on every actually-
  keyed frame (text, audio freq, qso_id, key-time timestamp) — additive, alongside the existing
  `TxQueueStatus` NOW-SENDING snapshot, not a replacement.
- Relayed 1:1 to a new `TuiMessage::TxFrameLogged`, handled by a new `App::add_tx_frame`, which
  appends a `DecodedMessageView` (new `is_own_tx: bool` field) into the same `decoded_messages`
  deque RX rows already live in — same 1000-row cap/prune, same ordering function.
- Own-TX rows get `is_directed_at_us: true` unconditionally, pinning them into the same top tier
  as the RX-directed half of the same exchange — a QSO's full back-and-forth now reads as one
  chronologically interleaved block in Band Activity.
- Logs ALL keyed TX, including CQ calls, not just QSO-related frames.
- Band Activity renders a `» TX` marker (Call column) and `TX` (SNR column, replacing the
  meaningless snr:0 sentinel) for own-TX rows — text markers, not color-only.

## Test plan
- [x] `cargo test -p pancetta-tui --lib` — all passing
- [x] `cargo test -p pancetta --lib` — all passing
- [x] `cargo test -p pancetta --test loopback_qso` — all passing
- [x] `cargo fmt --check` clean
- [x] `cargo clippy -p pancetta -p pancetta-tui --all-targets -- -D warnings` clean
- [ ] On-air: complete a QSO and confirm both our TX and the DX's RX frames appear interleaved,
  chronologically, in Band Activity.

Closes #172
EOF
)"
```
