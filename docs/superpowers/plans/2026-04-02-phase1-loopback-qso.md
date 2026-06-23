# Phase 1: First Simulated QSO — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove the entire FT8 pipeline works by simulating a complete QSO (CQ → grid → report → RR73 → 73) between two virtual stations — no radio, no audio hardware, pure in-memory.

**Architecture:** A loopback integration test that exercises encoder → modulator → decoder → QSO state machine → message generation → encoder, forming a complete loop. Two simulated stations (Station A = "W1ABC", Station B = "K2DEF") exchange messages through audio buffers. The coordinator's QSO component is also wired to forward auto-sequence TX requests so the application itself can drive QSOs automatically.

**Tech Stack:** `pancetta-ft8` (encoder, modulator, decoder), `pancetta-qso` (QsoManager, MessageExchange, state machine), tokio (async test runtime)

**Spec:** `docs/superpowers/specs/2026-04-02-end-to-end-qso-design.md` — Phase 1

---

## File Structure

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `pancetta-ft8/src/decoder.rs:1130` | Fix log10(0) bug |
| Create | `pancetta/tests/loopback_qso.rs` | End-to-end loopback QSO integration test |
| Modify | `pancetta/src/coordinator.rs:1428-1534` | Wire QsoEvent::MessageToSend → TransmitRequest |

---

### Task 1: Fix log10(0) Bug in Decoder

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs:1130`
- Test: `pancetta-ft8/tests/round_trip_tests.rs` (existing tests verify no regression)

- [ ] **Step 1: Write a failing test that triggers the bug**

Add to `pancetta-ft8/tests/round_trip_tests.rs`:

```rust
/// Verify decoder handles silence (all-zero audio) without producing -inf or NaN
#[test]
fn test_decode_silence_no_panic_or_inf() {
    let silence = vec![0.0f32; pancetta_ft8::WINDOW_SAMPLES];
    let config = pancetta_ft8::Ft8Config::default();
    let mut decoder = pancetta_ft8::Ft8Decoder::new(config).unwrap();
    let results = decoder.decode_window(&silence).unwrap_or_default();
    // Should decode nothing, but must not panic or produce -inf/NaN in waterfall
    assert!(results.is_empty(), "Silence should not decode any messages");
    if let Some(waterfall) = decoder.get_waterfall_data() {
        assert!(
            waterfall.min_power.is_finite(),
            "Waterfall min_power should be finite, got {}",
            waterfall.min_power
        );
        assert!(
            waterfall.max_power.is_finite(),
            "Waterfall max_power should be finite, got {}",
            waterfall.max_power
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta-ft8 test_decode_silence_no_panic_or_inf -- --nocapture`

Expected: FAIL — `waterfall.min_power` is `-inf` because `log10(0.0) = -inf`

- [ ] **Step 3: Fix the bug**

In `pancetta-ft8/src/decoder.rs`, find line 1130:

```rust
let power_db = 10.0 * power.log10();
```

Replace with:

```rust
let power_db = 10.0 * (power + f32::EPSILON).log10();
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pancetta-ft8 test_decode_silence_no_panic_or_inf -- --nocapture`

Expected: PASS — `min_power` and `max_power` are finite values

- [ ] **Step 5: Run all FT8 tests for regression**

Run: `cargo test --features transmit -p pancetta-ft8`

Expected: All ~200 tests pass

- [ ] **Step 6: Commit**

```bash
git add pancetta-ft8/src/decoder.rs pancetta-ft8/tests/round_trip_tests.rs
git commit -m "fix: prevent -inf in waterfall power when audio contains zeros

log10(0) produces -inf which propagates through waterfall min/max.
Add f32::EPSILON before log to clamp to a finite floor (~-150 dB)."
```

---

### Task 2: Create Loopback QSO Test — CQ Round-Trip

**Files:**
- Create: `pancetta/tests/loopback_qso.rs`

This task creates the test file and proves the first step: Station A sends CQ, Station B decodes it.

- [ ] **Step 1: Write the test file with helpers and first test**

Create `pancetta/tests/loopback_qso.rs`:

```rust
//! Loopback QSO integration test: two simulated stations complete a full FT8 QSO
//! via encode → modulate → decode → state machine → generate response → encode → ...
//!
//! No audio hardware, no coordinator, no async runtime for the core loop.
//! Tests the pure FT8 + QSO pipeline.

#![cfg(feature = "transmit")]

use pancetta_ft8::{
    DecodedMessage, Ft8Config, Ft8Decoder, Ft8Encoder, Ft8Modulator, NUM_SYMBOLS, WINDOW_SAMPLES,
};
use pancetta_qso::{
    utils, QsoManager, QsoManagerConfig, QsoEvent, AutoSequenceConfig,
    states::MessageType as QsoMessageType,
};

/// Station identity for the loopback test
struct Station {
    callsign: String,
    grid: String,
    encoder: Ft8Encoder,
    modulator: Ft8Modulator,
    decoder: Ft8Decoder,
}

impl Station {
    fn new(callsign: &str, grid: &str) -> Self {
        Self {
            callsign: callsign.to_string(),
            grid: grid.to_string(),
            encoder: Ft8Encoder::new(),
            modulator: Ft8Modulator::new_default().unwrap(),
            decoder: Ft8Decoder::new(Ft8Config::default()).unwrap(),
        }
    }

    /// Encode a message text into audio samples (padded to WINDOW_SAMPLES)
    fn encode_and_modulate(&mut self, text: &str, freq_offset: f64) -> Vec<f32> {
        let symbols = self
            .encoder
            .encode_message(text, None)
            .unwrap_or_else(|e| panic!("Failed to encode '{}': {}", text, e));
        let mut audio = self
            .modulator
            .modulate_symbols(&symbols, freq_offset)
            .unwrap();
        audio.resize(WINDOW_SAMPLES, 0.0);
        audio
    }

    /// Decode audio samples and return decoded messages
    fn decode(&mut self, audio: &[f32]) -> Vec<DecodedMessage> {
        self.decoder.decode_window(audio).unwrap_or_default()
    }
}

#[test]
fn test_loopback_cq_decode() {
    // Station A sends CQ
    let mut station_a = Station::new("W1ABC", "FN42");
    let cq_text = "CQ W1ABC FN42";
    let audio = station_a.encode_and_modulate(cq_text, 1000.0);

    // Station B decodes it
    let mut station_b = Station::new("K2DEF", "FM18");
    let decoded = station_b.decode(&audio);

    assert!(
        !decoded.is_empty(),
        "Station B should decode at least one message"
    );
    assert_eq!(
        decoded[0].text, cq_text,
        "Decoded message should match CQ text"
    );
}
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test -p pancetta --test loopback_qso test_loopback_cq_decode -- --nocapture`

Expected: PASS — this uses the same encode→modulate→decode path already proven in round_trip_tests.rs

- [ ] **Step 3: Commit**

```bash
git add pancetta/tests/loopback_qso.rs
git commit -m "test: add loopback QSO test harness with CQ round-trip

Introduces Station helper and proves CQ encode → modulate → decode
works across two simulated stations. Foundation for full QSO test."
```

---

### Task 3: Loopback QSO Test — Full CQ-to-73 Exchange

**Files:**
- Modify: `pancetta/tests/loopback_qso.rs`

This task adds the full QSO exchange test. Each step of the FT8 QSO protocol is exercised: CQ → grid response → signal report → R+report → RR73 → 73.

- [ ] **Step 1: Write the full loopback QSO test**

Add to `pancetta/tests/loopback_qso.rs`:

```rust
/// Helper: find a decoded message containing expected text (case-insensitive prefix match)
fn find_message<'a>(decoded: &'a [DecodedMessage], expected: &str) -> Option<&'a DecodedMessage> {
    let expected_upper = expected.to_uppercase();
    decoded.iter().find(|m| m.text.to_uppercase() == expected_upper)
}

#[test]
fn test_loopback_full_qso_cq_to_73() {
    let mut station_a = Station::new("W1ABC", "FN42");
    let mut station_b = Station::new("K2DEF", "FM18");
    let freq = 1000.0;

    // Step 1: Station A sends CQ
    let cq_text = "CQ W1ABC FN42";
    let audio = station_a.encode_and_modulate(cq_text, freq);
    let decoded = station_b.decode(&audio);
    assert!(
        find_message(&decoded, cq_text).is_some(),
        "Station B should decode CQ. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // Step 2: Station B responds with grid
    let grid_response = "W1ABC K2DEF FM18";
    let audio = station_b.encode_and_modulate(grid_response, freq);
    let decoded = station_a.decode(&audio);
    assert!(
        find_message(&decoded, grid_response).is_some(),
        "Station A should decode grid response. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // Step 3: Station A sends signal report
    let report = "K2DEF W1ABC -10";
    let audio = station_a.encode_and_modulate(report, freq);
    let decoded = station_b.decode(&audio);
    assert!(
        find_message(&decoded, report).is_some(),
        "Station B should decode signal report. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // Step 4: Station B sends R+report (acknowledges A's report, sends own)
    let r_report = "W1ABC K2DEF R-12";
    let audio = station_b.encode_and_modulate(r_report, freq);
    let decoded = station_a.decode(&audio);
    assert!(
        find_message(&decoded, r_report).is_some(),
        "Station A should decode R+report. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // Step 5: Station A sends RR73
    let rr73 = "K2DEF W1ABC RR73";
    let audio = station_a.encode_and_modulate(rr73, freq);
    let decoded = station_b.decode(&audio);
    assert!(
        find_message(&decoded, rr73).is_some(),
        "Station B should decode RR73. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );

    // Step 6: Station B sends 73
    let seventy_three = "W1ABC K2DEF 73";
    let audio = station_b.encode_and_modulate(seventy_three, freq);
    let decoded = station_a.decode(&audio);
    assert!(
        find_message(&decoded, seventy_three).is_some(),
        "Station A should decode 73. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
}
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test -p pancetta --test loopback_qso test_loopback_full_qso_cq_to_73 -- --nocapture`

Expected: PASS — each message encodes, modulates, and decodes correctly through the full exchange

- [ ] **Step 3: Commit**

```bash
git add pancetta/tests/loopback_qso.rs
git commit -m "test: add full CQ-to-73 loopback QSO exchange test

Exercises all 6 steps of a standard FT8 QSO through the pure
encode → modulate → decode pipeline between two simulated stations."
```

---

### Task 4: Loopback QSO Test — State Machine Driven Exchange

**Files:**
- Modify: `pancetta/tests/loopback_qso.rs`

This task adds a test where the QSO state machine drives the message generation, not hardcoded strings. Station A initiates a CQ, and the QsoManager + MessageExchange determine what to send at each step.

- [ ] **Step 1: Write the state-machine-driven loopback test**

Add to `pancetta/tests/loopback_qso.rs`:

```rust
/// Given a decoded FT8 message, parse it into a QSO MessageType for a given station
fn parse_for_station(text: &str, our_callsign: &str) -> Option<QsoMessageType> {
    utils::parse_ft8_message(text, our_callsign).ok()
}

/// Given a QSO MessageType, generate the FT8 message string for a given station
fn generate_for_station(msg_type: &QsoMessageType, our_callsign: &str) -> String {
    utils::generate_ft8_message(msg_type, our_callsign)
        .unwrap_or_else(|e| panic!("Failed to generate message for {:?}: {}", msg_type, e))
}

#[tokio::test]
async fn test_loopback_state_machine_driven_qso() {
    // Set up two QSO managers
    let config_a = QsoManagerConfig {
        our_callsign: "W1ABC".to_string(),
        our_grid: Some("FN42".to_string()),
        auto_sequence: AutoSequenceConfig {
            enabled: true,
            auto_respond_cq: true,
            auto_send_reports: true,
            auto_send_confirmations: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let config_b = QsoManagerConfig {
        our_callsign: "K2DEF".to_string(),
        our_grid: Some("FM18".to_string()),
        auto_sequence: AutoSequenceConfig {
            enabled: true,
            auto_respond_cq: true,
            auto_send_reports: true,
            auto_send_confirmations: true,
            ..Default::default()
        },
        ..Default::default()
    };

    let manager_a = QsoManager::new(config_a);
    let manager_b = QsoManager::new(config_b);
    manager_a.start().await.unwrap();
    manager_b.start().await.unwrap();

    let mut station_a = Station::new("W1ABC", "FN42");
    let mut station_b = Station::new("K2DEF", "FM18");
    let freq = 1000.0;

    // Subscribe to events from both managers
    let mut events_a = manager_a.subscribe();
    let mut events_b = manager_b.subscribe();

    // Step 1: Station A calls CQ
    let qso_id_a = manager_a.start_cq(freq).await.unwrap();

    // Collect the MessageToSend event
    let event = events_a.recv().await.unwrap();
    let cq_text = match event {
        QsoEvent::MessageToSend { message, .. } => {
            generate_for_station(&message, "W1ABC")
        }
        _ => panic!("Expected MessageToSend, got {:?}", event),
    };
    assert_eq!(cq_text, "CQ W1ABC FN42");

    // Transmit CQ over the air
    let audio = station_a.encode_and_modulate(&cq_text, freq);
    let decoded = station_b.decode(&audio);
    assert!(!decoded.is_empty(), "Station B should decode CQ");

    // Step 2: Station B sees CQ, starts QSO
    let cq_msg = parse_for_station(&decoded[0].text, "K2DEF").unwrap();
    if let QsoMessageType::Cq { callsign, .. } = &cq_msg {
        let qso_id_b = manager_b
            .respond_to_cq(callsign.clone(), freq)
            .await
            .unwrap();

        // Collect the grid response event
        let event = events_b.recv().await.unwrap();
        let response_text = match event {
            QsoEvent::MessageToSend { message, .. } => {
                generate_for_station(&message, "K2DEF")
            }
            _ => panic!("Expected MessageToSend for grid response, got {:?}", event),
        };

        // Transmit grid response
        let audio = station_b.encode_and_modulate(&response_text, freq);
        let decoded = station_a.decode(&audio);
        assert!(!decoded.is_empty(), "Station A should decode grid response");

        // Step 3: Feed decoded grid response to Station A's QSO manager
        let msg_type = parse_for_station(&decoded[0].text, "W1ABC").unwrap();
        manager_a
            .process_message(msg_type, decoded[0].text.clone(), freq, Some(decoded[0].snr_db))
            .await
            .unwrap();

        // Verify Station A transitions to WaitingForReport (or similar)
        let progress_a = manager_a.get_qso(qso_id_a).await.unwrap();
        assert!(
            matches!(progress_a.state, pancetta_qso::QsoState::WaitingForReport { .. }),
            "Station A should be in WaitingForReport, got {:?}",
            progress_a.state
        );
    } else {
        panic!("Expected CQ message type, got {:?}", cq_msg);
    }
}
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test -p pancetta --test loopback_qso test_loopback_state_machine_driven_qso -- --nocapture`

Expected: PASS — the QSO managers drive the exchange, state transitions verified

Note: If there are compilation errors due to type visibility or missing trait impls, fix them as needed. The QsoManager, QsoEvent, and related types are all `pub` in `pancetta-qso`.

- [ ] **Step 3: Commit**

```bash
git add pancetta/tests/loopback_qso.rs
git commit -m "test: add state-machine-driven loopback QSO test

QsoManager drives message generation and state transitions.
Verifies encode → modulate → decode → state machine integration."
```

---

### Task 5: Wire QSO Component Auto-Sequence Loop in Coordinator

**Files:**
- Modify: `pancetta/src/coordinator.rs:1428-1534`

The QSO component in the coordinator receives decoded messages and feeds them to `QsoManager.process_message()`. But when the QSO state machine decides to send a response (emitting `QsoEvent::MessageToSend`), nobody routes that event to the transmitter. This task closes that loop.

- [ ] **Step 1: Read the current QSO component code**

Read `pancetta/src/coordinator.rs` lines 1378-1538. Understand the tokio::spawn block and the message loop.

- [ ] **Step 2: Add QsoEvent subscriber and forward MessageToSend events**

In the QSO component's tokio::spawn block (after `qso_manager.start().await`), subscribe to QSO events and spawn a task to forward `MessageToSend` events as `TransmitRequest` messages.

Find this section (around line 1427):

```rust
info!(
    "QSO component ready (callsign={}, grid={:?})",
    our_callsign, our_grid
);
```

Add immediately after it (before the `while !shutdown.load` loop):

```rust
// Spawn a task to forward QSO auto-sequence TX requests to the transmitter
let mut qso_events = qso_manager.subscribe();
let tx_bus = message_bus.clone();
let tx_shutdown = shutdown.clone();
tokio::spawn(async move {
    while !tx_shutdown.load(Ordering::Acquire) {
        match qso_events.recv().await {
            Ok(pancetta_qso::QsoEvent::MessageToSend {
                qso_id,
                message,
                frequency,
            }) => {
                match pancetta_qso::utils::generate_ft8_message(
                    &message,
                    &our_callsign,
                ) {
                    Ok(text) => {
                        info!(
                            "QSO auto-sequence sending: '{}' on {:.1} Hz (qso={})",
                            text, frequency, qso_id
                        );
                        let tx_msg = ComponentMessage::new(
                            ComponentId::Qso,
                            ComponentId::Ft8Transmitter,
                            MessageType::TransmitRequest {
                                message_text: text,
                                frequency_offset: frequency,
                                qso_id: Some(qso_id.to_string()),
                            },
                            Instant::now(),
                        );
                        if let Err(e) = tx_bus.send_message(tx_msg).await {
                            warn!("Failed to send auto-sequence TX: {}", e);
                        }
                    }
                    Err(e) => {
                        warn!(
                            "Failed to generate FT8 message for QSO {}: {}",
                            qso_id, e
                        );
                    }
                }
            }
            Ok(_) => {} // Other events (StateChanged, etc.) — ignore for now
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!("QSO event subscriber lagged by {} events", n);
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                break;
            }
        }
    }
});
```

**Important:** The `our_callsign` variable is already in scope (captured by the outer closure at line 1370). You need to clone it before the new spawn:

Add before the `tokio::spawn` above:

```rust
let our_callsign_for_tx = our_callsign.clone();
```

And use `our_callsign_for_tx` inside the inner spawn instead of `our_callsign`.

- [ ] **Step 3: Verify compilation**

Run: `cargo check -p pancetta`

Expected: Compiles without errors

- [ ] **Step 4: Run workspace tests for regression**

Run: `cargo test --workspace --exclude pancetta-hamlib 2>&1 | tail -20`

Expected: All tests pass (hamlib excluded due to known tokio runtime conflict)

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/coordinator.rs
git commit -m "feat: wire QSO auto-sequence events to transmitter

Subscribe to QsoEvent::MessageToSend from QsoManager and forward
as TransmitRequest to the FT8 transmitter component. Closes the
auto-sequence loop: decoded message → state transition → TX."
```

---

### Task 6: Add Multi-Signal Decode Loopback Test

**Files:**
- Modify: `pancetta/tests/loopback_qso.rs`

This validates that the decoder can handle two simultaneous FT8 signals at different frequencies — a prerequisite for Phase 3's multi-stream TX. The round_trip_tests.rs already has a multi-message test, but this confirms it works in the loopback context.

- [ ] **Step 1: Write the multi-signal loopback test**

Add to `pancetta/tests/loopback_qso.rs`:

```rust
#[test]
fn test_loopback_two_simultaneous_signals() {
    let mut station_a = Station::new("W1ABC", "FN42");
    let mut station_b = Station::new("K2DEF", "FM18");
    let mut station_c = Station::new("N3GHI", "EM73");

    // Station A and Station B transmit simultaneously at different frequencies
    let msg_a = "CQ W1ABC FN42";
    let msg_b = "CQ K2DEF FM18";
    let audio_a = station_a.encode_and_modulate(msg_a, 800.0);
    let audio_b = station_b.encode_and_modulate(msg_b, 1400.0);

    // Sum the two signals (simulating two stations transmitting at once)
    let combined: Vec<f32> = audio_a
        .iter()
        .zip(audio_b.iter())
        .map(|(a, b)| a + b)
        .collect();

    // Station C decodes both
    let decoded = station_c.decode(&combined);

    let found_a = decoded.iter().any(|m| m.text == msg_a);
    let found_b = decoded.iter().any(|m| m.text == msg_b);

    assert!(
        found_a,
        "Should decode Station A's CQ. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
    assert!(
        found_b,
        "Should decode Station B's CQ. Got: {:?}",
        decoded.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
}
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test -p pancetta --test loopback_qso test_loopback_two_simultaneous_signals -- --nocapture`

Expected: PASS — decoder extracts both signals from the combined audio

- [ ] **Step 3: Commit**

```bash
git add pancetta/tests/loopback_qso.rs
git commit -m "test: verify two simultaneous FT8 signals decode in loopback

Sums two signals at 800 Hz and 1400 Hz offsets and confirms the
decoder extracts both. Foundation for Phase 3 multi-stream TX."
```

---

## Summary

| Task | What It Proves |
|------|---------------|
| Task 1 | Decoder handles edge cases (silence) without crashing |
| Task 2 | CQ encodes, modulates, and decodes between two stations |
| Task 3 | Full 6-step FT8 QSO exchange works through the audio pipeline |
| Task 4 | QSO state machine drives message generation correctly |
| Task 5 | Coordinator wires auto-sequence events to the transmitter |
| Task 6 | Two simultaneous signals decode correctly (multi-stream foundation) |

After all 6 tasks: the entire FT8 pipeline is proven end-to-end, both as a standalone test and wired through the coordinator.
