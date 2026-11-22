//! Tests for `has_active_or_recent_qso_with` — the completion-aware dedup
//! gate used by `maybe_answer_caller` to suppress post-completion duplicate
//! QSOs (fix for the ZL1UHD-style four-73 bug).
use super::*;
use jiff::SignedDuration;
use pancetta_core::time::UtcTimestamp;
use std::collections::{HashMap, VecDeque};
use uuid::Uuid;

fn manager() -> QsoManager {
    QsoManager::new(QsoManagerConfig {
        our_callsign: "W1ABC".into(),
        our_grid: Some("FN42".into()),
        timeouts: TimeoutConfig::default(),
        contest_mode: None,
        contest_scoring: None,
        auto_sequence: AutoSequenceConfig::default(),
        duplicate_checking: DuplicateCheckConfig::default(),
        hound: HoundRegions::default(),
        active_mode: "FT8".to_string(),
    })
}

/// Minimal `QsoMetadata` for a QSO with `their_callsign`.
fn meta(their_callsign: &str) -> QsoMetadata {
    let now = UtcTimestamp::now();
    QsoMetadata {
        qso_id: Uuid::new_v4(),
        our_callsign: "W1ABC".into(),
        their_callsign: Some(their_callsign.into()),
        frequency: 1500.0,
        completed_rf_frequency_hz: None,
        mode: "FT8".into(),
        first_answer_at: None,
        start_time: now,
        end_time: None,
        reports: SignalReports::default(),
        grids: GridSquares::default(),
        their_state: None,
        contest_info: None,
        tags: HashMap::new(),
        adif_field_types: HashMap::new(),
        notes: None,
        tx_parity: None,
        initiated_by: CallInitiation::Auto,
        role: QsoRole::Caller,
        call_count: 1,
        first_call_at: Some(now),
        last_call_at: Some(now),
        progressed_this_cycle: false,
        stall_cycles: 0,
        last_known_good_offset_hz: None,
        advance_generation: 0,
        pre_switch_offset: None,
        hound: false,
        partner_freq: None,
        pending_freq_drift: None,
        hound_qsyed: false,
        fox: false,
        remote_origin: false,
        remote_client_key_id: None,
        tx_parity_provisional: false,
    }
}

/// Insert a `QsoProgress` with the given state directly into the manager's
/// map (mirrors the pattern used by other test modules).
async fn insert(manager: &QsoManager, state: QsoState, their_call: &str) -> Uuid {
    let id = Uuid::new_v4();
    let progress = QsoProgress {
        state,
        state_history: vec![],
        messages: vec![],
        metadata: meta(their_call),
    };
    manager.qsos.write().await.insert(id, progress);
    id
}

/// PAN-25 round 2 (Codex P2): `has_active_or_recent_qso_with` — the
/// `maybe_answer_caller` preflight's own gate — was still callsign-only,
/// unaffected by PAN-25's earlier band-scoping fixes to the OTHER two
/// suppression checks. A station worked on 20m must not suppress a
/// direct caller reply on 40m here either.
#[tokio::test]
async fn has_active_or_recent_qso_with_is_band_scoped() {
    let m = manager();
    let mut progress = QsoProgress {
        state: QsoState::Completed {
            their_callsign: "ZL1UHD".into(),
            their_report: -12,
            our_report: -7,
            frequency: 1500.0,
            grid_square: None,
            completed_at: UtcTimestamp::now() - SignedDuration::from_secs(5),
            duration_seconds: 60,
        },
        state_history: vec![],
        messages: vec![],
        metadata: meta("ZL1UHD"),
    };
    // Explicit 20m completion band, bypassing real dial machinery for a
    // direct, focused test of the band comparison itself.
    progress.metadata.completed_rf_frequency_hz = Some(14_074_000.0);
    let id = Uuid::new_v4();
    m.qsos.write().await.insert(id, progress);

    assert!(
        m.has_active_or_recent_qso_with(
            "ZL1UHD",
            14_074_000.0,
            std::time::Duration::from_secs(120)
        )
        .await,
        "a same-band (20m) direct call must still be suppressed"
    );
    assert!(
        !m.has_active_or_recent_qso_with(
            "ZL1UHD",
            7_074_000.0,
            std::time::Duration::from_secs(120)
        )
        .await,
        "a different-band (40m) direct call must NOT be suppressed by a 20m completion"
    );
}

// ── within-window: recently completed → true ─────────────────────────────

#[tokio::test]
async fn recently_completed_within_window_returns_true() {
    let m = manager();
    // Completed 5 seconds ago — well within the 120 s window.
    let completed_state = QsoState::Completed {
        their_callsign: "ZL1UHD".into(),
        their_report: -12,
        our_report: -7,
        frequency: 1500.0,
        grid_square: None,
        completed_at: UtcTimestamp::now() - SignedDuration::from_secs(5),
        duration_seconds: 60,
    };
    insert(&m, completed_state, "ZL1UHD").await;

    assert!(
        m.has_active_or_recent_qso_with("ZL1UHD", 1500.0, std::time::Duration::from_secs(120))
            .await,
        "recently-completed QSO must block duplicate creation"
    );
}

// ── outside window: stale completed → false ───────────────────────────────

#[tokio::test]
async fn completed_outside_window_returns_false() {
    let m = manager();
    // Completed 200 seconds ago — outside the 120 s window.
    let completed_state = QsoState::Completed {
        their_callsign: "ZL1UHD".into(),
        their_report: -12,
        our_report: -7,
        frequency: 1500.0,
        grid_square: None,
        completed_at: UtcTimestamp::now() - SignedDuration::from_secs(200),
        duration_seconds: 60,
    };
    insert(&m, completed_state, "ZL1UHD").await;

    assert!(
        !m.has_active_or_recent_qso_with("ZL1UHD", 1500.0, std::time::Duration::from_secs(120))
            .await,
        "stale (>window) completed QSO must NOT block a new one"
    );
}

// ── active QSO → true ─────────────────────────────────────────────────────

#[tokio::test]
async fn active_qso_returns_true() {
    let m = manager();
    let active_state = QsoState::RespondingToCq {
        target_callsign: "ZL1UHD".into(),
        frequency: 1500.0,
        started_at: UtcTimestamp::now(),
    };
    insert(&m, active_state, "ZL1UHD").await;

    assert!(
        m.has_active_or_recent_qso_with("ZL1UHD", 1500.0, std::time::Duration::from_secs(120))
            .await,
        "active QSO must block duplicate"
    );
}

// ── different callsign → false ────────────────────────────────────────────

#[tokio::test]
async fn different_callsign_returns_false() {
    let m = manager();
    let completed_state = QsoState::Completed {
        their_callsign: "ZL1UHD".into(),
        their_report: -12,
        our_report: -7,
        frequency: 1500.0,
        grid_square: None,
        completed_at: UtcTimestamp::now() - SignedDuration::from_secs(5),
        duration_seconds: 60,
    };
    insert(&m, completed_state, "ZL1UHD").await;

    assert!(
        !m.has_active_or_recent_qso_with("K9ZZ", 1500.0, std::time::Duration::from_secs(120))
            .await,
        "QSO with a different callsign must not match"
    );
}

// ── no QSOs at all → false ────────────────────────────────────────────────

#[tokio::test]
async fn empty_manager_returns_false() {
    let m = manager();
    assert!(
        !m.has_active_or_recent_qso_with("ZL1UHD", 1500.0, std::time::Duration::from_secs(120))
            .await,
        "no QSOs → always false"
    );
}

// ── compound-call equivalence ─────────────────────────────────────────────

#[tokio::test]
async fn compound_call_matches_base() {
    let m = manager();
    // QSO was logged under the compound form EA8/G8BCG.
    let completed_state = QsoState::Completed {
        their_callsign: "EA8/G8BCG".into(),
        their_report: -12,
        our_report: -7,
        frequency: 1500.0,
        grid_square: None,
        completed_at: UtcTimestamp::now() - SignedDuration::from_secs(5),
        duration_seconds: 60,
    };
    insert(&m, completed_state, "EA8/G8BCG").await;

    // Query with the bare base call — must still match.
    assert!(
        m.has_active_or_recent_qso_with("G8BCG", 1500.0, std::time::Duration::from_secs(120))
            .await,
        "compound EA8/G8BCG QSO must match bare base G8BCG query"
    );
}
