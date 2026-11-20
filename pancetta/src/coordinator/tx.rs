//! FT8 transmitter component.
//!
//! Owns the encode → modulate → slot-aligned audio output → PTT key/unkey
//! sequence. Runs on the message-bus channel for `MessageType::TransmitRequest`
//! arriving from the QSO state machine, the autonomous operator, or the
//! TUI command-forwarding loop.
//!
//! The `PttGuard` RAII helper ensures the radio is keyed back to RX even
//! if the transmitter task is cancelled mid-transmission — without it a
//! panic in the audio output path would leave the rig stuck on TX.

use anyhow::Result;
use pancetta_ft8::{Ft8Encoder, Ft8Modulator};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, error, info, span, warn, Level};

use crate::message_bus::{
    AckState, ComponentId, ComponentMessage, DeliveryAck, MessageBus, MessageType,
    PlaybackStartAck, PlaybackStartState, PlaybackStartWaiter, SendIfOutcome,
};

use super::tx_log::{TxFrameLog, TxFrameRecord, TX_LOG_ACK_WAIT};
use super::tx_outcome::{self, AutonomousOutcome, TxFrame};

mod worker;
#[cfg(test)]
pub(crate) use worker::TxInbox;
use worker::*;
pub use worker::{resolve_required_parity, schedule_tx, TxSchedule};
pub(crate) use worker::{tx_attempts_count, tx_defers_count, TxRequeue};
mod gate;
use gate::*;
pub use gate::{
    remote_tx_permitted, remote_tx_permitted_for, tx_band_plan_mute_reason, tx_rf_for_gate,
    tx_signal_span_hz,
};
pub(crate) use gate::{tx_hard_mute_reason, tx_mute_causes, TxMuteCause};
mod ptt;
#[cfg(test)]
pub(crate) use ptt::KeyDownProbe;
use ptt::*;
pub(crate) use ptt::{send_ptt_command, PendingPttOff, SAFETY_SEND_RETRY_DELAYS};
mod bundle;
use bundle::*;
#[cfg(test)]
pub(crate) use bundle::{
    coalesce_backlog_into, effective_max_concurrent_qsos, live_fox_admitted_count,
    supersede_max_concurrent_qsos,
};
pub use bundle::{
    coalesce_transmit_requests, coalesce_transmit_requests_with_fox_priority, CoalesceEntry,
    CoalesceOutcome,
};
mod supersede;
use supersede::*;
mod report;
use report::*;
pub(crate) use report::{emit_diagnostic, emit_diagnostic_full};

mod drive_calibration;
#[cfg(test)]
#[path = "tx/tests/drive_calibration_tests.rs"]
mod drive_calibration_tests;
mod fox_multiplex;

#[cfg(test)]
#[path = "tx/tests/fox_multiplex_tests.rs"]
mod fox_multiplex_tests;

#[cfg(test)]
#[path = "tx/tests/on_ptt_wait_tests.rs"]
mod on_ptt_wait_tests;

/// Outcome of `interruptible_sleep_or_supersede`.
///
/// Deliberately does NOT derive `PartialEq`/`Eq`: `Superseded` carries the
/// full `MessageType`, and `MessageType` itself does not (and should not,
/// without a much wider cross-crate change — see
/// docs/superpowers/sdd/task-5-report.md) derive `PartialEq`. Tests that
/// need to assert on a `Superseded` payload pattern-match and compare
/// individual fields instead of using `assert_eq!` on the whole enum.
// `Superseded(MessageType)` is intentionally not boxed: this keeps the
// public shape exactly as specified (a later task's re-key consumer wants
// the full `MessageType`, including `MultiTransmitRequest`'s `items` list —
// see task-5-report.md), at the cost of a size-difference lint between
// variants. Silenced rather than changed.
#[cfg(test)]
#[path = "tx/tests/on_sleep_outcome_tests.rs"]
mod on_sleep_outcome_tests;
#[cfg(test)]
#[path = "tx/tests/tx_inbox_tests.rs"]
mod tx_inbox_tests;

#[cfg(test)]
#[path = "tx/tests/interruptible_sleep_tests.rs"]
mod interruptible_sleep_tests;

#[cfg(test)]
#[path = "tx/tests/band_plan_gate_tests.rs"]
mod band_plan_gate_tests;

/// PAN-208: a `custom_bands` entry with two or more `ranges` leaves gaps
/// inside the band. The occupied signal `[floor(low), ceil(low + span)]`
/// must fit inside ONE allowed range; checking the low and high edges
/// independently let a frame whose base tone sat in one range and top
/// tone in the next key across the excluded gap (PR #446 Codex review).
#[cfg(test)]
#[path = "tx/tests/band_plan_gap_tests.rs"]
pub(crate) mod band_plan_gap_tests;

#[cfg(test)]
#[path = "tx/tests/effective_max_concurrent_qsos_tests.rs"]
mod effective_max_concurrent_qsos_tests;
#[cfg(test)]
#[path = "tx/tests/tx_hard_mute_reason_tests.rs"]
mod tx_hard_mute_reason_tests;

#[cfg(test)]
#[path = "tx/tests/fox_admitted_ceiling_tests.rs"]
mod fox_admitted_ceiling_tests;

impl SlotTimingContext {
    /// The mode-scaled late cap, [`tx_late_max_ms_effective`].
    fn late_cap_ms(&self) -> u64 {
        tx_late_max_ms_effective(
            self.active_protocol,
            self.tx_late_max_ms.load(Ordering::Acquire),
        )
    }

    /// The PTT engage instant: `target_slot` minus `ptt_lead_ms`.
    fn ptt_target(
        &self,
        target_slot: chrono::DateTime<chrono::Utc>,
    ) -> chrono::DateTime<chrono::Utc> {
        target_slot - chrono::Duration::milliseconds(self.ptt_lead_ms as i64)
    }
}

#[cfg(test)]
impl SlotTimingContext {
    /// Test-only FT8 context: the supersede tests' old explicit arguments,
    /// with the default 80 ms `ptt_lead_ms` (unused by the supersede path).
    fn ft8_for_test(
        tx_late_max_ms: u64,
        sample_rate: u32,
        slot_ns: i64,
        tx_self_parity: pancetta_config::station::TxSelfParity,
        request_received_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self {
            active_protocol: pancetta_ft8::Protocol::Ft8,
            tx_late_max_ms: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(tx_late_max_ms)),
            tx_self_parity,
            ptt_lead_ms: 80,
            sample_rate,
            slot_ns,
            request_received_at,
            utc_clock: pancetta_core::time::system_utc_clock(),
        }
    }
}

#[cfg(test)]
#[path = "tx/tests/slot_timing_context_tests.rs"]
mod slot_timing_context_tests;

#[cfg(test)]
#[path = "tx/tests/schedule_tx_tests.rs"]
mod schedule_tx_tests;

/// Regression coverage for the silent-TX-failure gap: an encode/modulate
/// error used to only log a WARN and send a TransmitComplete no consumer
/// reads, so the operator had zero visible signal — the QSO just sat until
/// the watchdog timed out, indistinguishable from the DX not answering.
/// `emit_tx_failure_diagnostic` is the shared helper wired into every
/// genuine TX-attempt-failure site in this file (deliberate policy/security
/// skips are NOT routed through it — see its doc comment). This pins its
/// exact DiagnosticEvent contract directly, independent of the full
/// encode/modulate/PTT pipeline the call sites sit inside.
#[cfg(test)]
#[path = "tx/tests/tx_failure_diagnostic_tests.rs"]
mod tx_failure_diagnostic_tests;

#[cfg(test)]
#[path = "tx/tests/disarm_interrupt_signal_tests.rs"]
mod disarm_interrupt_signal_tests;

#[cfg(test)]
#[path = "tx/tests/supersede_rekey_tests.rs"]
mod supersede_rekey_tests;
#[cfg(test)]
#[path = "tx/tests/tx_frame_logged_tests.rs"]
mod tx_frame_logged_tests;

#[cfg(test)]
#[path = "tx/tests/coalesce_tests.rs"]
mod coalesce_tests;
#[cfg(test)]
#[path = "tx/tests/protocol_tx_tests.rs"]
mod protocol_tx_tests;

/// Unit tests for the remote-TX arm gate helper (`remote_tx_permitted`).
///
/// These lock the safety-critical fail direction: a fresh (unarmed) `ArmState`
/// denies, a fully-armed+consented state permits, and a **poisoned** arm lock
/// **fails CLOSED** (denies) — the opposite of `tx_qso_is_live`'s fail-open.
#[cfg(test)]
#[path = "tx/tests/remote_arm_gate_tests.rs"]
mod remote_arm_gate_tests;

/// Unit tests for PAN-91's client-identity-bound gate (`remote_tx_permitted_for`).
///
/// These lock the fix's actual invariant: a frame bound to one client must
/// key TX only while THAT client is armed — not merely while *some* client
/// is armed, which is exactly the gap that let a different peer's arm
/// authorize a QSO it never requested.
#[cfg(test)]
#[path = "tx/tests/remote_arm_gate_identity_tests.rs"]
mod remote_arm_gate_identity_tests;

#[cfg(test)]
#[path = "tx/tests/arm_snapshot_attribution_tests.rs"]
mod arm_snapshot_attribution_tests;

#[cfg(test)]
#[path = "tx/tests/tx_counter_tests.rs"]
mod tx_counter_tests;

#[cfg(test)]
#[path = "tx/tests/classifier_tests.rs"]
mod classifier_tests;

/// PAN-115: the two Gherkin scenarios end to end, through the real TX worker
/// (`start_transmitter_component`) over a real `MessageBus`: a request made
/// while the rig CAT link is lost, or while the audio output is dead, is
/// refused at the key-time gate with no `SetPtt{true}` ever reaching Hamlib.
#[cfg(test)]
#[path = "tx/tests/worker_gate_tests.rs"]
pub(super) mod worker_gate_tests;

#[cfg(test)]
#[path = "tx/tests/report_transmit_complete_tests.rs"]
mod report_transmit_complete_tests;
/// PAN-118: the band-plan Gherkin end to end, through the real TX worker
/// (`start_transmitter_component`) over a real `MessageBus`, reusing the
/// PAN-115 harness: a transmission whose absolute TX RF (effective
/// split-aware dial + absolute audio offset) falls outside the configured
/// amateur band plan is refused before `SetPtt{true}` ever reaches Hamlib.
/// The dial-0 "not yet known" sentinel (mock/rig-disabled dev modes,
/// `--test-tx`, replay seed) skips the check, keeping those modes
/// byte-identical.
#[cfg(test)]
#[path = "tx/tests/worker_band_plan_tests.rs"]
mod worker_band_plan_tests;

/// PAN-202: key-down reaches Hamlib's queue while another component's
/// channel registration holds the bus table, through the real TX worker over
/// a real `MessageBus`, at every key-down site (single TX, bundle, tune), plus
/// the shared helper the TUI's manual PTT uses.
#[cfg(test)]
#[path = "tx/tests/key_down_worker_tests.rs"]
mod key_down_worker_tests;

/// PAN-136: a multi-TX bundle whose item is re-bound during the pre-PTT wait
/// (Step 4b-pivot swaps the item's binding with its payload) is gated under
/// the RE-FOLDED binding, through the real TX worker over a real `MessageBus`.
/// Before PAN-136 the bundle kept its coalesce-time binding: a `Local` bundle
/// keyed a frame now bound to an unarmed remote client, and client A's bundle
/// aired client B's frame under A's gate.
#[cfg(test)]
#[path = "tx/tests/bundle_binding_worker_tests.rs"]
mod bundle_binding_worker_tests;

/// PAN-208: the gap-straddling Gherkin end to end, through the real TX
/// worker with a two-range `custom_bands` entry for 20m in its config. A
/// frame whose base tone sits in the first range and top tone in the second
/// is refused before `SetPtt{true}` reaches Hamlib; a frame wholly inside
/// one range still keys.
#[cfg(test)]
#[path = "tx/tests/worker_band_plan_gap_tests.rs"]
mod worker_band_plan_gap_tests;

#[cfg(test)]
#[path = "tx/tests/disarm_flush_tests.rs"]
mod disarm_flush_tests;
#[cfg(test)]
#[path = "tx/tests/ptt_release_tests.rs"]
mod ptt_release_tests;
#[cfg(test)]
#[path = "tx/tests/worker_disarm_tests.rs"]
mod worker_disarm_tests;
/// PAN-273: the real TX worker unkeys through `PttGuard::release`, so a
/// PTT-off dropped on a momentarily full Hamlib channel is retried, and a
/// delivered one stands the guard down (no duplicate unkey from its `Drop`).
#[cfg(test)]
#[path = "tx/tests/worker_unkey_tests.rs"]
mod worker_unkey_tests;

/// PAN-122: every frame whose audio the real TX worker enqueues gets one
/// durable line in `<home>/.pancetta/tx-log.jsonl` — headless, with the TUI
/// channel full, single or bundled — and nothing else does.
#[cfg(test)]
#[path = "tx/tests/tx_log_worker_tests.rs"]
mod tx_log_worker_tests;

/// PAN-146: the DX-parity gate, through the real TX worker, scopes its a7
/// evidence to the live dial's band. A DX kept fresh on 20 m Odd holds an
/// Odd call on 20 m, and does not hold the same call after the operator
/// moved to 15 m.
#[cfg(test)]
#[path = "tx/tests/dx_parity_context_worker_tests.rs"]
mod dx_parity_context_worker_tests;

#[cfg(test)]
#[path = "tx/tests/audio_delivery_tests.rs"]
mod audio_delivery_tests;

/// PAN-278: a PTT-on that `send_ptt_command` returns `Dropped` while Hamlib's
/// route is registered (here: Hamlib's channel is full) never keyed the rig.
/// The worker must unwind like a pre-PTT refusal, play no audio, and settle
/// the frame as not sent. Without a route (`--replay`) the drop is by
/// design and the frame still plays.
///
/// Hamlib's backlog is read only after the completion: draining it earlier
/// (as `await_completion` does) un-saturates the channel and lets the
/// key-down through, which hides the defect.
#[cfg(test)]
#[path = "tx/tests/dropped_ptt_on_tests.rs"]
mod dropped_ptt_on_tests;

/// PAN-223: `report_frame_scheduled` carries the worker's own
/// `TxSchedule::target_slot` to the QSO engine, durably, and only for frames
/// that name both a QSO and a ledger attempt.
#[cfg(test)]
#[path = "tx/tests/frame_scheduled_report_tests.rs"]
mod frame_scheduled_report_tests;

/// PAN-223: the real TX worker reports the slot `schedule_tx` picked from its
/// own dequeue instant. Each test ends at the report and never waits for the
/// slot itself.
#[cfg(test)]
#[path = "tx/tests/scheduled_slot_worker_tests.rs"]
mod scheduled_slot_worker_tests;

/// PAN-183: each frame's keep-call ledger attempt rides its request through
/// coalescing, pivots and supersedes, and comes back on `TxFrameNotSent`, so
/// the QSO engine rolls back exactly the attempt that failed.
#[cfg(test)]
#[path = "tx/tests/attempt_identity_tests.rs"]
mod attempt_identity_tests;
#[cfg(test)]
#[path = "tx/tests/playback_start_tests.rs"]
mod playback_start_tests;

#[cfg(test)]
#[path = "tx/tests/receive_overload_ptt_admission_tests.rs"]
mod receive_overload_ptt_admission_tests;

/// PAN-279: every unaired frame settles through `tx_outcome`, driven
/// through the real TX worker.
#[cfg(test)]
#[path = "tx/tests/settlement_worker_tests.rs"]
mod settlement_worker_tests;

#[cfg(test)]
mod abort_transcripts;
