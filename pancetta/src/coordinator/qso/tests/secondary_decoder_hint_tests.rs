use super::secondary_decoder_hint_freq_for;
use chrono::{Duration, Utc};
use pancetta_qso::states::PreSwitchOffset;
use pancetta_qso::{GridSquares, QsoMetadata, QsoProgress, QsoState, SignalReports};

/// FT8's default slot (15s) -> a 30s grace, matching
/// `pancetta-qso`'s never-injected default.
const FT8_SLOT_NS: i64 = 15_000_000_000;

fn calling_cq_progress(pre_switch_offset: Option<PreSwitchOffset>) -> QsoProgress {
    let start = Utc::now() - Duration::seconds(5);
    QsoProgress {
        state: QsoState::CallingCq {
            frequency: 1900.0,
            started_at: start,
            call_count: 1,
        },
        state_history: Vec::new(),
        messages: Vec::new(),
        metadata: QsoMetadata {
            qso_id: pancetta_qso::QsoId::new_v4(),
            our_callsign: "K5ARH".to_string(),
            their_callsign: None,
            frequency: 1900.0,
            completed_rf_frequency_hz: None,
            mode: "FT8".to_string(),
            first_answer_at: None,
            start_time: start,
            end_time: None,
            reports: SignalReports::default(),
            grids: GridSquares::default(),
            their_state: None,
            contest_info: None,
            tags: std::collections::HashMap::new(),
            adif_field_types: std::collections::HashMap::new(),
            notes: None,
            tx_parity: None,
            initiated_by: Default::default(),
            role: Default::default(),
            call_count: 1,
            first_call_at: None,
            last_call_at: None,
            progressed_this_cycle: false,
            stall_cycles: 1,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset,
            hound: false,
            partner_freq: None,
            pending_freq_drift: None,
            hound_qsyed: false,
            fox: false,
            remote_origin: false,
            remote_client_key_id: None,
            tx_parity_provisional: false,
        },
    }
}

#[test]
fn no_pre_switch_offset_yields_no_secondary() {
    let progress = calling_cq_progress(None);
    assert_eq!(
        secondary_decoder_hint_freq_for(&progress, FT8_SLOT_NS),
        None
    );
}

#[test]
fn a_fresh_relocation_still_inside_grace_yields_the_vacated_offset() {
    let left_at = Utc::now();
    let progress = calling_cq_progress(Some(PreSwitchOffset {
        offset_hz: 1500.0,
        left_at,
        operator_forced: true,
    }));
    let expected_expiry =
        left_at + pancetta_qso::qso_manager::pre_switch_offset_grace_from_slot_ns(FT8_SLOT_NS);
    assert_eq!(
        secondary_decoder_hint_freq_for(&progress, FT8_SLOT_NS),
        Some((1500.0, expected_expiry)),
        "PAN-72 round-9: the secondary now carries its expires_at \
         alongside the frequency, so a later READ can re-evaluate \
         freshness without needing another write-time event"
    );
}

/// PAN-72 round-8 redesign (fix 2) test-plan item: the secondary clears
/// once `left_at + grace` has passed — a bounded window, not a
/// permanent second decode band.
#[test]
fn a_relocation_past_its_grace_yields_no_secondary() {
    let progress = calling_cq_progress(Some(PreSwitchOffset {
        offset_hz: 1500.0,
        left_at: Utc::now() - Duration::seconds(31),
        operator_forced: true,
    }));
    assert_eq!(
        secondary_decoder_hint_freq_for(&progress, FT8_SLOT_NS),
        None
    );
}

/// The grace window scales with the active protocol slot (Fix 1),
/// so a fast FT2 slot (3.2s -> 6.4s grace) must lapse sooner than the
/// FT8 default.
#[test]
fn the_grace_scales_with_the_injected_slot_ns() {
    const FT2_SLOT_NS: i64 = 3_200_000_000; // -> 6.4s grace
    let progress = calling_cq_progress(Some(PreSwitchOffset {
        offset_hz: 1500.0,
        left_at: Utc::now() - Duration::seconds(10),
        operator_forced: false,
    }));
    assert_eq!(
        secondary_decoder_hint_freq_for(&progress, FT2_SLOT_NS),
        None,
        "10s ago must already be outside FT2's 6.4s grace"
    );
    assert_eq!(
        secondary_decoder_hint_freq_for(&progress, FT8_SLOT_NS).map(|(freq, _)| freq),
        Some(1500.0),
        "the same 10s-old relocation is still inside FT8's 30s grace"
    );
}

/// Scope guard: an established QSO (not `CallingCq`) never gains a
/// secondary hint here, even with a live `pre_switch_offset` — it
/// already has a stable hint via `partner_freq` (the DX transmits
/// there regardless of our TX side), so touching that path is
/// explicitly out of scope for this fix.
#[test]
fn an_established_qso_never_gets_a_secondary_hint() {
    let mut progress = calling_cq_progress(Some(PreSwitchOffset {
        offset_hz: 1500.0,
        left_at: Utc::now(),
        operator_forced: true,
    }));
    progress.state = QsoState::RespondingToCq {
        target_callsign: "K9ZZ".to_string(),
        frequency: 1900.0,
        started_at: Utc::now(),
    };
    assert_eq!(
        secondary_decoder_hint_freq_for(&progress, FT8_SLOT_NS),
        None
    );
}
