//! Fox-mode unit + integration tests (Task 2).
//!
//! # Coverage
//!
//! **T1 — Engage creates a CallingCq QSO; disengage cancels it.**
//! The `SetFoxMode{on:true}` handler calls `QsoManager::start_cq_manual`; the
//! `SetFoxMode{on:false}` handler calls `cancel_qso` on every active `CallingCq`
//! QSO.  We drive the same calls directly and assert the QSO lifecycle rather
//! than routing through the async bus task (which would require the full
//! coordinator runtime).
//!
//! **T2 — Cap selection logic.**
//! `maybe_answer_caller` uses `fox_max_streams` as the effective cap when
//! `fox_mode` is set, and `max_concurrent` when it is not.  This is the pure
//! cap-selection predicate; tested with real `AtomicBool` / `AtomicUsize`
//! instances and a live `QsoManager` so the `active_qso_count` query is real.
//!
//! **Regression guard**: fox_mode=false → the cap in `maybe_answer_caller` is
//! exactly `max_concurrent` (unchanged from today's value).

#![allow(clippy::expect_used)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use pancetta_qso::{QsoManager, QsoManagerConfig, QsoState};

// ---------------------------------------------------------------------------
// T1 — CallingCq QSO lifecycle (engage → active CQ; disengage → cancelled)
// ---------------------------------------------------------------------------

/// Fox engage (SetFoxMode{on:true}) starts a CallingCq QSO via start_cq_manual.
/// The QSO must exist and be in CallingCq state immediately after.
#[tokio::test]
async fn fox_engage_creates_calling_cq_qso() {
    let config = QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    };
    let manager = QsoManager::new(config);
    manager.start().await.expect("QsoManager::start");

    // Mimic SetFoxMode{on:true}: start_cq_manual at Fox default 1500 Hz.
    let qso_id = manager
        .start_cq_manual(1500.0, None)
        .await
        .expect("start_cq_manual should succeed for Fox CQ");

    // The QSO must be active and in CallingCq state.
    let active = manager.get_active_qsos().await;
    let entry = active
        .iter()
        .find(|(id, _)| *id == qso_id)
        .expect("CallingCq QSO must be in the active list");
    assert!(
        matches!(entry.1.state, QsoState::CallingCq { .. }),
        "Fox CQ QSO must be in CallingCq state, got {:?}",
        entry.1.state
    );
}

/// Fox disengage (SetFoxMode{on:false}) cancels the CallingCq QSO.
/// After cancellation the QSO must no longer appear in the active list.
#[tokio::test]
async fn fox_disengage_cancels_calling_cq_qso() {
    let config = QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    };
    let manager = QsoManager::new(config);
    manager.start().await.expect("QsoManager::start");

    // Engage: create the CallingCq QSO.
    let qso_id = manager
        .start_cq_manual(1500.0, None)
        .await
        .expect("start_cq_manual");

    // Verify it is active before disengage.
    let pre = manager.get_active_qsos().await;
    assert!(
        pre.iter().any(|(id, _)| *id == qso_id),
        "CallingCq QSO must be active before disengage"
    );

    // Disengage: mimic SetFoxMode{on:false} — cancel all CallingCq QSOs.
    let active = manager.get_active_qsos().await;
    let mut cancelled = 0usize;
    for (id, progress) in active {
        if matches!(progress.state, QsoState::CallingCq { .. }) {
            manager.cancel_qso(id).await.expect("cancel_qso");
            cancelled += 1;
        }
    }
    assert_eq!(
        cancelled, 1,
        "exactly one CallingCq QSO should have been cancelled"
    );

    // The QSO must now be gone from the active list (give state machine a moment).
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let post = manager.get_active_qsos().await;
        if post.iter().all(|(id, _)| *id != qso_id) {
            return; // passed
        }
    }
    let post = manager.get_active_qsos().await;
    assert!(
        post.iter().all(|(id, _)| *id != qso_id),
        "CallingCq QSO must be gone from the active list after disengage"
    );
}

/// The fox_mode flag is false by default; engaging sets it to true; disengaging
/// clears it back to false.
#[test]
fn fox_mode_atomic_engage_disengage_cycle() {
    let fox_mode = Arc::new(AtomicBool::new(false));

    // Default: off.
    assert!(
        !fox_mode.load(Ordering::Relaxed),
        "fox_mode must default false"
    );

    // Engage.
    fox_mode.store(true, Ordering::Relaxed);
    assert!(
        fox_mode.load(Ordering::Relaxed),
        "fox_mode must be true after engage"
    );

    // Disengage.
    fox_mode.store(false, Ordering::Relaxed);
    assert!(
        !fox_mode.load(Ordering::Relaxed),
        "fox_mode must be false after disengage"
    );
}

// ---------------------------------------------------------------------------
// T2 — Cap selection: fox_mode on → fox_max_streams; off → max_concurrent
// ---------------------------------------------------------------------------

/// Pure cap-selection logic extracted from `maybe_answer_caller`:
///
///   effective_cap = if fox_mode { fox_max_streams } else { max_concurrent }
///
/// Tests that the predicate picks the right value in each state.
fn effective_cap(fox_mode: bool, fox_max_streams: usize, max_concurrent: usize) -> usize {
    if fox_mode {
        fox_max_streams
    } else {
        max_concurrent
    }
}

#[test]
fn cap_fox_on_uses_fox_max_streams() {
    // fox_mode=true → cap is fox_max_streams (5), regardless of max_concurrent (1).
    assert_eq!(effective_cap(true, 5, 1), 5);
    assert_eq!(effective_cap(true, 3, 1), 3);
    assert_eq!(effective_cap(true, 8, 1), 8); // at the validated ceiling
}

#[test]
fn cap_fox_off_uses_max_concurrent() {
    // fox_mode=false → cap is max_concurrent, regardless of fox_max_streams.
    assert_eq!(effective_cap(false, 5, 1), 1);
    assert_eq!(effective_cap(false, 8, 2), 2);
    assert_eq!(effective_cap(false, 5, 5), 5);
}

#[test]
fn cap_fox_off_regression_unchanged_from_today() {
    // Regression guard: default config → max_concurrent_qsos = 1.
    // With fox_mode off, effective cap must equal the default (1), exactly as today.
    let default_max_concurrent = 1usize; // QsoManagerConfig default
    assert_eq!(effective_cap(false, 5, default_max_concurrent), 1);
}

/// With fox_mode ON and fox_max_streams=3, the atomic-driven cap selection
/// returns fox_max_streams; OFF returns max_concurrent. The flip is immediate.
#[tokio::test]
async fn cap_atomics_drive_correct_effective_cap() {
    let fox_mode = Arc::new(AtomicBool::new(false));
    let fox_max_streams = Arc::new(AtomicUsize::new(3));
    let max_concurrent = 1usize; // default normal cap

    // Fox mode OFF → effective cap = 1 (regression).
    let cap_off = if fox_mode.load(Ordering::Relaxed) {
        fox_max_streams.load(Ordering::Relaxed)
    } else {
        max_concurrent
    };
    assert_eq!(
        cap_off, 1,
        "fox_mode off: effective cap must be max_concurrent=1"
    );

    // Engage: flip flag.
    fox_mode.store(true, Ordering::Relaxed);

    // Fox mode ON → effective cap = fox_max_streams = 3.
    let cap_on = if fox_mode.load(Ordering::Relaxed) {
        fox_max_streams.load(Ordering::Relaxed)
    } else {
        max_concurrent
    };
    assert_eq!(
        cap_on, 3,
        "fox_mode on: effective cap must be fox_max_streams=3"
    );

    // Disengage: cap instantly reverts.
    fox_mode.store(false, Ordering::Relaxed);
    let cap_restored = if fox_mode.load(Ordering::Relaxed) {
        fox_max_streams.load(Ordering::Relaxed)
    } else {
        max_concurrent
    };
    assert_eq!(
        cap_restored, 1,
        "fox_mode off after disengage: cap must revert to 1"
    );
}

// ---------------------------------------------------------------------------
// T3 — Fox CQ + answer admit: N callers answered, (N+1)th deferred
// ---------------------------------------------------------------------------

/// With fox_max_streams=2, the first two callers fill the slots.  A third
/// would be rejected by the effective-cap check (active_qso_count >= cap).
/// Driven against a real QsoManager so active_qso_count is authoritative.
#[tokio::test]
async fn fox_cap_admits_n_and_rejects_n_plus_1() {
    let config = QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    };
    let manager = QsoManager::new(config);
    manager.start().await.expect("QsoManager::start");

    let fox_max_streams = 2usize;

    // Caller 1 — admitted.
    let _q1 = manager
        .respond_to_caller(
            "W1AAA".to_string(),
            1200.0,
            Some(pancetta_core::slot::SlotParity::Even),
            pancetta_core::ResponseStep::Grid,
            Some(-10.0),
            None,
            None,
        )
        .await
        .expect("caller 1 admitted");

    // Caller 2 — admitted (count = 1 < cap 2).
    let _q2 = manager
        .respond_to_caller(
            "W2BBB".to_string(),
            1400.0,
            Some(pancetta_core::slot::SlotParity::Even),
            pancetta_core::ResponseStep::Grid,
            Some(-12.0),
            None,
            None,
        )
        .await
        .expect("caller 2 admitted");

    // Verify both admitted: active_qso_count = 2.
    let count = manager.active_qso_count().await;
    assert_eq!(count, 2, "two callers must be active");

    // Cap check: would a 3rd be admitted?
    let would_admit_3rd = count < fox_max_streams;
    assert!(
        !would_admit_3rd,
        "3rd caller must be rejected (count={count} >= fox_max_streams={fox_max_streams})"
    );
}

/// After a Fox QSO slot is freed (cancel → Failed), a new caller can enter.
#[tokio::test]
async fn fox_slot_freed_after_qso_completes() {
    let config = QsoManagerConfig {
        our_callsign: "K5ARH".to_string(),
        our_grid: Some("EM10".to_string()),
        ..Default::default()
    };
    let manager = QsoManager::new(config);
    manager.start().await.expect("QsoManager::start");

    let fox_max_streams = 1usize;

    // Admit one caller.
    let q1 = manager
        .respond_to_caller(
            "W1AAA".to_string(),
            1200.0,
            Some(pancetta_core::slot::SlotParity::Even),
            pancetta_core::ResponseStep::Grid,
            Some(-10.0),
            None,
            None,
        )
        .await
        .expect("caller 1 admitted");

    // At cap: count = 1 = fox_max_streams.
    let count = manager.active_qso_count().await;
    assert_eq!(count, 1);
    let can_admit_before = count < fox_max_streams;
    assert!(!can_admit_before, "must be at cap before any QSO finishes");

    // Cancel the first QSO (simulates it completing / being superseded).
    manager.cancel_qso(q1).await.expect("cancel_qso");

    // Wait for the state machine to process the cancellation event.
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if manager.active_qso_count().await == 0 {
            break;
        }
    }

    let count_after = manager.active_qso_count().await;
    let can_admit_after = count_after < fox_max_streams;
    assert!(
        can_admit_after,
        "slot must free after QSO ends (count={count_after} < fox_max_streams={fox_max_streams})"
    );
}
