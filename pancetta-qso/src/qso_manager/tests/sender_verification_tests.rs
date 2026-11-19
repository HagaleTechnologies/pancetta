use super::*;
use chrono::Utc;

fn manager_with_call(our: &str) -> QsoManager {
    let config = QsoManagerConfig {
        our_callsign: our.into(),
        our_grid: Some("FN42".into()),
        timeouts: TimeoutConfig::default(),
        contest_mode: None,
        contest_scoring: None,
        auto_sequence: AutoSequenceConfig::default(),
        duplicate_checking: DuplicateCheckConfig::default(),
        hound: HoundRegions::default(),
        active_mode: "FT8".to_string(),
    };
    QsoManager::new(config)
}

/// Minimal `QsoMetadata` for unit tests: `partner_freq = None` (normal QSO,
/// no Hound split). All tests that call `is_message_relevant` directly must
/// pass a metadata; this helper ensures the regression cases use the
/// canonical "no partner_freq" path, matching pre-Hound behavior.
fn normal_metadata(our: &str, their_freq: f64) -> QsoMetadata {
    let now = Utc::now();
    QsoMetadata {
        qso_id: uuid::Uuid::new_v4(),
        our_callsign: our.into(),
        their_callsign: None,
        frequency: their_freq,
        completed_rf_frequency_hz: None,
        mode: "FT8".into(),
        first_answer_at: None,
        start_time: now,
        end_time: None,
        reports: SignalReports::default(),
        grids: GridSquares::default(),
        their_state: None,
        contest_info: None,
        tags: std::collections::HashMap::new(),
        adif_field_types: std::collections::HashMap::new(),
        notes: None,
        tx_parity: None,
        initiated_by: CallInitiation::default(),
        role: QsoRole::default(),
        call_count: 0,
        first_call_at: None,
        last_call_at: None,
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

#[tokio::test]
async fn spoofed_signal_report_does_not_advance_state() {
    let manager = manager_with_call("K5ARH");
    let mut events = manager.subscribe();
    let qso_id = Uuid::new_v4();
    let state = QsoState::RespondingToCq {
        target_callsign: "K9ZZ".into(),
        frequency: 1500.0,
        started_at: Utc::now(),
    };
    // Attacker sends a properly-addressed report from a DIFFERENT call.
    let spoof = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "NF4KE".into(),
        report: -12,
    };
    let new_state = manager
        .determine_state_transition(qso_id, &state, &spoof, None, CallInitiation::Auto, false)
        .await
        .unwrap();
    // State must NOT advance.
    assert!(matches!(new_state, QsoState::RespondingToCq { .. }));
    match events.try_recv().expect("security rejection event") {
        QsoEvent::MessageRejected {
            qso_id: got,
            reason,
            from_callsign,
            to_callsign,
        } => {
            assert_eq!(got, qso_id);
            assert_eq!(reason, RejectionReason::SenderNotPartner);
            assert_eq!(from_callsign.as_deref(), Some("NF4KE"));
            assert_eq!(to_callsign.as_deref(), Some("K5ARH"));
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[tokio::test]
async fn legitimate_signal_report_advances_state() {
    let manager = manager_with_call("K5ARH");
    let state = QsoState::RespondingToCq {
        target_callsign: "K9ZZ".into(),
        frequency: 1500.0,
        started_at: Utc::now(),
    };
    let legit = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "K9ZZ".into(),
        report: -12,
    };
    let new_state = manager
        .determine_state_transition(
            Uuid::new_v4(),
            &state,
            &legit,
            None,
            CallInitiation::Auto,
            false,
        )
        .await
        .unwrap();
    assert!(
        matches!(new_state, QsoState::SendingReport { .. }),
        "expected SendingReport, got {:?}",
        new_state
    );
}

#[test]
fn is_message_relevant_rejects_spoofed_sender() {
    let manager = manager_with_call("K5ARH");
    let state = QsoState::RespondingToCq {
        target_callsign: "K9ZZ".into(),
        frequency: 1500.0,
        started_at: Utc::now(),
    };
    let spoof = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "NF4KE".into(),
        report: -12,
    };
    // partner_freq = None → regression path (normal QSO behavior unchanged).
    let md = normal_metadata("K5ARH", 1500.0);
    assert!(!manager.is_message_relevant(&state, &md, &spoof, 1500.0, false));
}

#[test]
fn classify_relevance_reports_only_entangled_security_traffic() {
    let manager = manager_with_call("K5ARH");
    let state = QsoState::RespondingToCq {
        target_callsign: "K9ZZ".into(),
        frequency: 1500.0,
        started_at: Utc::now(),
    };
    let metadata = normal_metadata("K5ARH", 1500.0);
    let impostor = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "NF4KE".into(),
        report: -12,
    };
    let verdict = manager.classify_relevance(&state, &metadata, &impostor, 1500.0, false);
    assert!(!verdict.relevant);
    assert_eq!(verdict.reason, Some(RejectionReason::SenderNotPartner));

    for (from_station, to_station, frequency, description) in [
        ("K9ZZ", "W1AW", 1500.0, "partner working a third party"),
        ("NF4KE", "W1AW", 1500.0, "unrelated third-party traffic"),
        ("NF4KE", "K5ARH", 2400.0, "off-frequency impostor traffic"),
    ] {
        let ordinary_traffic = MessageType::SignalReport {
            to_station: to_station.into(),
            from_station: from_station.into(),
            report: -12,
        };
        let verdict =
            manager.classify_relevance(&state, &metadata, &ordinary_traffic, frequency, false);
        assert!(!verdict.relevant, "{description}");
        assert_eq!(verdict.reason, None, "{description} must stay silent");
    }
}

#[tokio::test]
async fn message_rejected_event_is_deliverable_to_subscribers() {
    let manager = manager_with_call("K5ARH");
    let mut events = manager.subscribe();
    let qso_id = Uuid::new_v4();
    manager
        .emit_event(QsoEvent::MessageRejected {
            qso_id,
            reason: RejectionReason::SenderNotPartner,
            from_callsign: Some("BOGUS9".into()),
            to_callsign: Some("K5ARH".into()),
        })
        .await;
    assert!(matches!(
        events.try_recv(),
        Ok(QsoEvent::MessageRejected { qso_id: got, .. }) if got == qso_id
    ));
}

#[test]
fn is_message_relevant_accepts_legitimate_sender() {
    let manager = manager_with_call("K5ARH");
    let state = QsoState::RespondingToCq {
        target_callsign: "K9ZZ".into(),
        frequency: 1500.0,
        started_at: Utc::now(),
    };
    let legit = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "K9ZZ".into(),
        report: -12,
    };
    // partner_freq = None → regression path (normal QSO behavior unchanged).
    let md = normal_metadata("K5ARH", 1500.0);
    assert!(manager.is_message_relevant(&state, &md, &legit, 1500.0, false));
}

#[test]
fn is_message_relevant_tight_gate_for_initial_ambiguous_match() {
    // The tight 15 Hz gate still governs INITIAL / ambiguous matching —
    // a state with no known contra callsign (CallingCq). This preserves
    // the security-review C-1 tightening for the case where we have not
    // yet locked onto a partner. (B15 only widens the gate once a QSO is
    // ESTABLISHED; see is_message_relevant_established_qso_allows_drift.)
    let manager = manager_with_call("K5ARH");
    let state = QsoState::CallingCq {
        frequency: 1500.0,
        started_at: Utc::now(),
        call_count: 1,
    };
    // A bare report answering our CQ (A4 routing shape), addressed to us.
    let legit = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "K9ZZ".into(),
        report: -12,
    };
    // partner_freq = None → regression path (normal QSO behavior unchanged).
    let md = normal_metadata("K5ARH", 1500.0);
    // 16 Hz off, no partner latched yet → rejected (tight gate).
    assert!(!manager.is_message_relevant(&state, &md, &legit, 1516.0, false));
    // 14 Hz off → accepted.
    assert!(manager.is_message_relevant(&state, &md, &legit, 1514.0, false));
}

#[test]
fn is_message_relevant_established_qso_allows_drift() {
    // B15: once a QSO is ESTABLISHED (contra callsign known, here a
    // RespondingToCq partner answering us with from+to+state all matching),
    // callsign+state continuity wins over the tight 15 Hz window — a DX that
    // drifted beyond 15 Hz is still routed (up to the 100 Hz established
    // bound) instead of being dropped, so an actively-answering partner can
    // complete the contact. The old 50 Hz tolerance is gone for the initial
    // case, but the established case now intentionally accepts up to 100 Hz.
    let manager = manager_with_call("K5ARH");
    let state = QsoState::RespondingToCq {
        target_callsign: "K9ZZ".into(),
        frequency: 1500.0,
        started_at: Utc::now(),
    };
    let legit = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "K9ZZ".into(),
        report: -12,
    };
    // partner_freq = None → regression path (normal QSO behavior unchanged).
    let md = normal_metadata("K5ARH", 1500.0);
    // 16 Hz, 45 Hz, 100 Hz drift from an established partner → accepted.
    assert!(manager.is_message_relevant(&state, &md, &legit, 1516.0, false));
    assert!(manager.is_message_relevant(&state, &md, &legit, 1545.0, false));
    assert!(manager.is_message_relevant(&state, &md, &legit, 1600.0, false));
    // Beyond the 100 Hz established bound → rejected (still bounded).
    assert!(!manager.is_message_relevant(&state, &md, &legit, 1601.0, false));
}

// ── Hound partner_freq routing ───────────────────────────────────────────

/// Hound QSO: the QSO's TX offset (`frequency`) is low (600 Hz, where Hounds
/// call), but the Fox replies on a different, higher offset (`partner_freq =
/// Some(1800.0)`). A frame arriving at the Fox's frequency IS relevant; a
/// frame arriving at the Hound's TX offset but far from the Fox's RX offset
/// is NOT relevant.
#[test]
fn is_message_relevant_hound_keys_on_partner_freq() {
    let manager = manager_with_call("K5ARH");
    // Hound state: our TX offset is low (600 Hz); Fox's RX offset is 1800 Hz.
    let state = QsoState::RespondingToCq {
        target_callsign: "KH8B".into(),
        frequency: 600.0, // our TX offset (Hound calls low)
        started_at: Utc::now(),
    };
    let fox_report = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "KH8B".into(),
        report: -10,
    };
    // Hound metadata: partner_freq = Some(1800.0) (Fox's RX offset).
    let mut md = normal_metadata("K5ARH", 600.0);
    md.partner_freq = Some(1800.0);

    // Frame at the Fox's offset (1800 Hz) — within ESTABLISHED tolerance.
    // The gate must match against partner_freq (1800), NOT our TX freq (600).
    assert!(
        manager.is_message_relevant(&state, &md, &fox_report, 1800.0, false),
        "Hound: frame at Fox's offset must be relevant"
    );
    // Frame at 1810 Hz — still within 100 Hz ESTABLISHED bound of 1800.
    assert!(
        manager.is_message_relevant(&state, &md, &fox_report, 1810.0, false),
        "Hound: frame 10 Hz from Fox's offset must be relevant (within established tolerance)"
    );
    // Frame at OUR TX offset (600 Hz) but far from the Fox's RX offset —
    // must be rejected because 600 Hz is not close to partner_freq 1800 Hz.
    assert!(
        !manager.is_message_relevant(&state, &md, &fox_report, 600.0, false),
        "Hound: frame at our TX offset (far from Fox) must NOT be relevant"
    );
    // Frame beyond even the ESTABLISHED bound from partner_freq — rejected.
    assert!(
        !manager.is_message_relevant(&state, &md, &fox_report, 1901.0, false),
        "Hound: frame >100 Hz from Fox's offset must NOT be relevant"
    );
}

/// Regression: with `partner_freq = None` (every normal QSO), the gate
/// falls back to `state.frequency()` exactly as before — byte-identical
/// behavior. A frame at the QSO's latched frequency is relevant; one far
/// away is not.
#[test]
fn is_message_relevant_partner_freq_none_falls_back_to_state_freq() {
    let manager = manager_with_call("K5ARH");
    let state = QsoState::RespondingToCq {
        target_callsign: "K9ZZ".into(),
        frequency: 1500.0,
        started_at: Utc::now(),
    };
    let legit = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "K9ZZ".into(),
        report: -12,
    };
    // partner_freq = None — normal QSO regression path.
    let md = normal_metadata("K5ARH", 1500.0);
    // Frame at the QSO frequency → relevant (unchanged from pre-Hound).
    assert!(
        manager.is_message_relevant(&state, &md, &legit, 1500.0, false),
        "regression: frame at state.frequency must be relevant when partner_freq=None"
    );
    // Frame far from QSO frequency → not relevant (unchanged).
    assert!(
        !manager.is_message_relevant(&state, &md, &legit, 2000.0, false),
        "regression: frame far from state.frequency must NOT be relevant when partner_freq=None"
    );
}

/// 2026-07-26 incident regression (v2, two-strike confirm): a single off-latch
/// SignalReport must NOT advance the QSO — it only notes a pending drift
/// candidate. This must be byte-identical to today's existing (unmodified) drop
/// behavior; `is_message_relevant`/`determine_state_transition` are untouched by
/// this fix, so this test is really proving the new pre-pass doesn't change
/// first-sighting behavior at all.
#[tokio::test]
async fn single_off_frequency_sighting_does_not_advance_or_relatch() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
        .await
        .unwrap();

    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "K5ARH".into(),
                from_station: "LU7LRP".into(),
                report: -11,
            },
            "K5ARH LU7LRP -11".into(),
            937.5,
            Some(-19.0),
        )
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::RespondingToCq { .. }),
        "a single off-frequency sighting must not advance the QSO; got {:?}",
        progress.state
    );
    assert!(
        matches!(progress.metadata.pending_freq_drift, Some((f, _)) if f == 937.5),
        "the first sighting must be noted as a pending drift candidate"
    );
}

/// PAN-142 (Codex P1 on PR #371): noting a NEW pending drift candidate
/// must emit a (same-state) `StateChanged` so the coordinator's TUI
/// snapshot picks up `pending_freq_drift` — this mechanism's own
/// state-machine state is deliberately untouched, so nothing else would
/// ever trigger that snapshot while a candidate is pending, and the
/// operator-visible drift indicator built for exactly this moment would
/// never actually render.
#[tokio::test]
async fn noting_a_new_drift_candidate_emits_state_changed() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
        .await
        .unwrap();
    let mut events = manager.subscribe();

    let report = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "LU7LRP".into(),
        report: -11,
    };
    manager
        .maybe_confirm_frequency_drift_at(&report, 937.5, Utc::now())
        .await;

    assert!(
        matches!(
            manager.get_qso(qso_id).await.unwrap().metadata.pending_freq_drift,
            Some((f, _)) if f == 937.5
        ),
        "premise: this call must actually note a new candidate"
    );
    let mut saw_state_changed_for_qso = false;
    while let Ok(ev) = events.try_recv() {
        if matches!(ev, QsoEvent::StateChanged { qso_id: id, .. } if id == qso_id) {
            saw_state_changed_for_qso = true;
        }
    }
    assert!(
        saw_state_changed_for_qso,
        "a new drift candidate must emit StateChanged so the TUI snapshot refreshes"
    );
}

/// PAN-142 (Codex P1 on PR #371): a pending candidate cleared by an
/// in-tolerance sighting (the DX came back within range before the
/// second confirming sighting) must ALSO emit, for the same reason —
/// otherwise the drift indicator sticks on the TUI after the QSO engine
/// has already dropped the candidate. Calls `maybe_confirm_frequency_drift_at`
/// directly (like the duplicate-delivery test above) rather than through
/// `process_message`, so a real state advance from the ordinary
/// relevance-gate path can't also emit and mask a missing fix here.
#[tokio::test]
async fn clearing_a_drift_candidate_without_confirming_emits_state_changed() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
        .await
        .unwrap();
    let report = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "LU7LRP".into(),
        report: -11,
    };
    manager
        .maybe_confirm_frequency_drift_at(&report, 937.5, Utc::now())
        .await;
    assert!(
        manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .pending_freq_drift
            .is_some(),
        "premise: a candidate must be pending before it can be cleared"
    );

    let mut events = manager.subscribe();
    // Back within tolerance of the QSO's OWN frequency (1500.0) — the
    // early "in-tolerance, clear the candidate" branch, not a confirm.
    manager
        .maybe_confirm_frequency_drift_at(&report, 1500.0, Utc::now())
        .await;

    assert!(
        manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .pending_freq_drift
            .is_none(),
        "an in-tolerance sighting must clear the pending candidate"
    );
    let mut saw_state_changed_for_qso = false;
    while let Ok(ev) = events.try_recv() {
        if matches!(ev, QsoEvent::StateChanged { qso_id: id, .. } if id == qso_id) {
            saw_state_changed_for_qso = true;
        }
    }
    assert!(
        saw_state_changed_for_qso,
        "clearing a pending candidate must emit StateChanged so the TUI snapshot refreshes"
    );
}

/// The confirming second sighting at the SAME new frequency relatches and lets the
/// QSO advance normally through the completely-unmodified existing pipeline —
/// exactly the real 2026-07-26 LU7LRP timeline (two SignalReport decodes at
/// 937.5 Hz, ~30s apart).
#[tokio::test]
async fn second_matching_sighting_confirms_and_relatches() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
        .await
        .unwrap();

    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "K5ARH".into(),
                from_station: "LU7LRP".into(),
                report: -11,
            },
            "K5ARH LU7LRP -11".into(),
            937.5,
            Some(-19.0),
        )
        .await
        .unwrap();
    // Confirmation now requires a real >=5s gap from the ORIGINAL candidate
    // timestamp (final-review Critical fix), so the confirming sighting must
    // land after a genuine delay, not back-to-back with the first.
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "K5ARH".into(),
                from_station: "LU7LRP".into(),
                report: -11,
            },
            "K5ARH LU7LRP -11".into(),
            937.5,
            Some(-17.0),
        )
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::SendingReport { .. }),
        "the confirmed second sighting must advance the QSO; got {:?}",
        progress.state
    );
    assert_eq!(
        progress.metadata.frequency, 937.5,
        "metadata.frequency must relatch to the confirmed frequency"
    );
    assert_eq!(
        progress.metadata.pending_freq_drift, None,
        "pending_freq_drift must clear once confirmed"
    );
    if let QsoState::SendingReport { frequency, .. } = progress.state {
        assert_eq!(
            frequency, 937.5,
            "the state's own embedded frequency field must also relatch \
             (is_message_relevant reads this field, not metadata.frequency)"
        );
    }
}

/// A different second frequency does NOT confirm — the candidate simply resets to
/// the newest off-latch sighting instead, and the QSO stays stuck (matching
/// today's behavior). This is the direct proof this mechanism can't be tricked by
/// two DIFFERENT spoofed frequencies in a row either.
#[tokio::test]
async fn different_second_frequency_does_not_confirm() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
        .await
        .unwrap();

    for freq in [937.5, 1100.0] {
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "LU7LRP".into(),
                    report: -11,
                },
                "K5ARH LU7LRP -11".into(),
                freq,
                Some(-19.0),
            )
            .await
            .unwrap();
    }

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::RespondingToCq { .. }),
        "a differing second sighting must not confirm/advance; got {:?}",
        progress.state
    );
    assert!(
        matches!(progress.metadata.pending_freq_drift, Some((f, _)) if f == 1100.0),
        "the candidate must reset to the newest off-latch sighting"
    );
}

/// A normal in-tolerance message arriving after a pending candidate clears it —
/// no spurious relatch or advance from a stale candidate once the drift resolves
/// itself (or was noise).
#[tokio::test]
async fn in_tolerance_message_clears_a_stale_pending_candidate() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
        .await
        .unwrap();

    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "K5ARH".into(),
                from_station: "LU7LRP".into(),
                report: -11,
            },
            "K5ARH LU7LRP -11".into(),
            937.5,
            Some(-19.0),
        )
        .await
        .unwrap();
    assert!(matches!(
        manager
            .get_qso(qso_id)
            .await
            .unwrap()
            .metadata
            .pending_freq_drift,
        Some((f, _)) if f == 937.5
    ));

    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "K5ARH".into(),
                from_station: "LU7LRP".into(),
                report: -9,
            },
            "K5ARH LU7LRP -09".into(),
            1550.0,
            Some(-12.0),
        )
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::SendingReport { .. }),
        "the in-tolerance message must advance the QSO normally; got {:?}",
        progress.state
    );
    assert_eq!(
        progress.metadata.pending_freq_drift, None,
        "the stale candidate must clear once a normal in-tolerance message arrives"
    );
}

/// A passband-violating decode must not overwrite a legitimate pending candidate —
/// it's likely a garbage decode, not a real drift signal, and shouldn't reset real
/// tracking. The genuine confirming sighting must still work afterward.
#[tokio::test]
async fn out_of_passband_decode_does_not_reset_pending_candidate() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
        .await
        .unwrap();

    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "K5ARH".into(),
                from_station: "LU7LRP".into(),
                report: -11,
            },
            "K5ARH LU7LRP -11".into(),
            937.5,
            Some(-19.0),
        )
        .await
        .unwrap();

    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "K5ARH".into(),
                from_station: "LU7LRP".into(),
                report: -11,
            },
            "K5ARH LU7LRP -11".into(),
            -50.0,
            Some(-25.0),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            manager
                .get_qso(qso_id)
                .await
                .unwrap()
                .metadata
                .pending_freq_drift,
            Some((f, _)) if f == 937.5
        ),
        "an out-of-passband decode must not overwrite a legitimate pending candidate"
    );

    // Confirmation now requires a real >=5s gap from the ORIGINAL candidate
    // timestamp (final-review Critical fix), so the confirming sighting must
    // land after a genuine delay, not back-to-back with the earlier sightings.
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "K5ARH".into(),
                from_station: "LU7LRP".into(),
                report: -11,
            },
            "K5ARH LU7LRP -11".into(),
            937.5,
            Some(-17.0),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            manager.get_qso(qso_id).await.unwrap().state,
            QsoState::SendingReport { .. }
        ),
        "the real confirming sighting must still relatch and advance after the noise"
    );
}

/// Final-review Critical finding: the hb-091 scoped fast-path can forward two decode
/// copies of the SAME physical transmission to the QSO component milliseconds apart
/// (dedup previously relied on is_message_relevant, which this pre-pass runs before).
/// Two same-instant deliveries of one transmission must NOT confirm a relatch -- only a
/// sighting that's genuinely >=5s after the ORIGINAL candidate may confirm.
#[tokio::test]
async fn duplicate_delivery_of_same_transmission_does_not_confirm() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
        .await
        .unwrap();

    let report = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "LU7LRP".into(),
        report: -11,
    };
    let t0 = Utc::now();
    // Two decode-pipeline copies of the SAME transmission, milliseconds apart --
    // mirrors the hb-091 scoped fast-path + standard pipeline both forwarding the
    // same window's content to the QSO component.
    manager
        .maybe_confirm_frequency_drift_at(&report, 937.5, t0)
        .await;
    manager
        .maybe_confirm_frequency_drift_at(&report, 937.5, t0 + chrono::Duration::milliseconds(50))
        .await;

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::RespondingToCq { .. }),
        "two same-instant deliveries of ONE transmission must not confirm/relatch"
    );
    assert!(
        matches!(progress.metadata.pending_freq_drift, Some((f, t)) if f == 937.5 && t == t0),
        "the pending candidate must keep its ORIGINAL timestamp, not be pushed forward \
         by the duplicate delivery -- otherwise repeated fast deliveries could delay \
         confirmation indefinitely"
    );

    // A genuinely later sighting (>=5s after the ORIGINAL t0, not the duplicate) confirms.
    manager
        .maybe_confirm_frequency_drift_at(&report, 937.5, t0 + chrono::Duration::seconds(6))
        .await;
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        progress.metadata.frequency, 937.5,
        "a sighting >=5s after the original candidate must confirm and relatch"
    );
    assert_eq!(progress.metadata.pending_freq_drift, None);
}

/// Boundary case for the 5s confirm gap: a sighting just under the gate must NOT
/// confirm, and must leave the pending candidate's ORIGINAL timestamp untouched
/// (proving the non-reset-on-duplicate rule holds right at the boundary, where a
/// future refactor is most likely to break it).
#[tokio::test]
async fn sighting_just_under_the_confirm_gap_does_not_confirm() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
        .await
        .unwrap();

    let report = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "LU7LRP".into(),
        report: -11,
    };
    let t0 = Utc::now();
    manager
        .maybe_confirm_frequency_drift_at(&report, 937.5, t0)
        .await;
    manager
        .maybe_confirm_frequency_drift_at(&report, 937.5, t0 + chrono::Duration::milliseconds(4900))
        .await;

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::RespondingToCq { .. }),
        "a sighting 4.9s after the original must not confirm/relatch"
    );
    assert!(
        matches!(progress.metadata.pending_freq_drift, Some((f, t)) if f == 937.5 && t == t0),
        "the candidate must still carry the ORIGINAL timestamp, unchanged"
    );
}

/// Closes the boundary from the other side: a sighting at EXACTLY 5.0s (the
/// inclusive edge of `DRIFT_CONFIRM_MIN_GAP`) must confirm. Combined with
/// `sighting_just_under_the_confirm_gap_does_not_confirm` (4.9s, must NOT confirm),
/// this pins the gate to `>=`, not `>` -- either an accidental `>` or a shift of
/// the constant itself (e.g. 5s -> 6s) would flip one of these two tests.
#[tokio::test]
async fn sighting_at_exactly_the_confirm_gap_does_confirm() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
        .await
        .unwrap();

    let report = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "LU7LRP".into(),
        report: -11,
    };
    let t0 = Utc::now();
    manager
        .maybe_confirm_frequency_drift_at(&report, 937.5, t0)
        .await;
    manager
        .maybe_confirm_frequency_drift_at(&report, 937.5, t0 + chrono::Duration::seconds(5))
        .await;

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        progress.metadata.frequency, 937.5,
        "a sighting at EXACTLY the 5s gap must confirm and relatch (inclusive bound)"
    );
    assert_eq!(progress.metadata.pending_freq_drift, None);
}

/// Final-review Important finding: the drift-candidate eligibility bound must be
/// the wide RX-plausibility range (200-2900 Hz), NOT this file's narrower
/// TX_OFFSET_MIN_HZ/MAX_HZ (300-2700 Hz, which govern autonomous fresh-offset
/// picking, not where a real DX might legitimately be heard). Pins BOTH edges so a
/// future refactor that silently narrows this back to TX_OFFSET_* is caught —
/// exactly the un-caught regression that produced this test. A DX replying from
/// near either edge of the real passband must still be able to confirm and
/// relatch.
#[tokio::test]
async fn drift_candidate_confirms_near_the_passband_edges() {
    let manager = manager_with_call("K5ARH");

    // Lower edge: 250 Hz is inside DRIFT_CANDIDATE_MIN_HZ..=MAX_HZ (200-2900) but
    // outside TX_OFFSET_MIN_HZ..=MAX_HZ (300-2700) -- if the eligibility check ever
    // regresses back to the TX bounds, this confirm silently stops happening.
    let qso_low = manager
        .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
        .await
        .unwrap();
    let report_low = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "LU7LRP".into(),
        report: -11,
    };
    let t0 = Utc::now();
    manager
        .maybe_confirm_frequency_drift_at(&report_low, 250.0, t0)
        .await;
    manager
        .maybe_confirm_frequency_drift_at(&report_low, 250.0, t0 + chrono::Duration::seconds(6))
        .await;
    let progress = manager.get_qso(qso_low).await.unwrap();
    assert_eq!(
        progress.metadata.frequency, 250.0,
        "a DX at 250 Hz (inside the RX-plausibility bound, outside TX_OFFSET_*) must \
         still be able to confirm and relatch"
    );

    // Upper edge: 2850 Hz, same reasoning.
    let qso_high = manager
        .respond_to_cq_manual("VK9XYZ".into(), 1500.0, None)
        .await
        .unwrap();
    let report_high = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "VK9XYZ".into(),
        report: -9,
    };
    let t1 = Utc::now();
    manager
        .maybe_confirm_frequency_drift_at(&report_high, 2850.0, t1)
        .await;
    manager
        .maybe_confirm_frequency_drift_at(&report_high, 2850.0, t1 + chrono::Duration::seconds(6))
        .await;
    let progress = manager.get_qso(qso_high).await.unwrap();
    assert_eq!(
        progress.metadata.frequency, 2850.0,
        "a DX at 2850 Hz (inside the RX-plausibility bound, outside TX_OFFSET_*) must \
         still be able to confirm and relatch"
    );
}

/// Pins the OUTER upper bound of DRIFT_CANDIDATE_MAX_HZ (2900 Hz) -- a decode past
/// it is implausible/garbage and must never become (or confirm) a candidate.
#[tokio::test]
async fn drift_candidate_rejects_outside_the_rx_plausibility_bound() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_manual("LU7LRP".into(), 1500.0, None)
        .await
        .unwrap();
    let report = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "LU7LRP".into(),
        report: -11,
    };
    let t0 = Utc::now();
    manager
        .maybe_confirm_frequency_drift_at(&report, 2950.0, t0)
        .await;
    manager
        .maybe_confirm_frequency_drift_at(&report, 2950.0, t0 + chrono::Duration::seconds(6))
        .await;

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::RespondingToCq { .. }),
        "a decode past the RX-plausibility bound must never confirm/relatch"
    );
    assert_eq!(
        progress.metadata.pending_freq_drift, None,
        "an implausible decode must never even become a pending candidate"
    );
}

/// PAN-12 / issue #245: an ordinary split-TX QSO created by the TX ceiling
/// clamp relatches where it hears the DX while preserving our TX offset.
#[tokio::test]
async fn clamped_split_tx_qso_relatches_partner_freq_on_confirmed_drift() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_with(
            "K6L".into(),
            2700.0,
            Some(pancetta_core::slot::SlotParity::Even),
            CallInitiation::Manual,
            Some(2931.0),
            false,
            None,
        )
        .await
        .unwrap();

    for (frequency, snr) in [(855.0, -19.0), (856.0, -17.0)] {
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "K6L".into(),
                    report: -11,
                },
                "K5ARH K6L -11".into(),
                frequency,
                Some(snr),
            )
            .await
            .unwrap();

        if frequency == 855.0 {
            let progress = manager.get_qso(qso_id).await.unwrap();
            assert!(
                matches!(progress.metadata.pending_freq_drift, Some((f, _)) if f == 855.0),
                "a clamped split-TX QSO must note a drift candidate"
            );
            assert_eq!(progress.metadata.partner_freq, Some(2931.0));
            tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        }
    }

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(progress.metadata.partner_freq, Some(856.0));
    assert_eq!(progress.metadata.frequency, 2700.0);
    assert_eq!(progress.metadata.pending_freq_drift, None);
    assert!(matches!(progress.state, QsoState::SendingReport { .. }));
    if let QsoState::SendingReport { frequency, .. } = progress.state {
        assert_eq!(frequency, 2700.0);
    }
}

// PAN-287: a network-spot call opens at an offset derived from the spot's
// RF (or the allocator's pick), which can be stale. These pin the existing
// two-sighting recovery (C-1 / PAN-15) that spot calls now rely on, so a
// later tolerance change cannot silently break them.

/// Tx=Rx spot call at a stale offset: the DX answers at 1210 Hz. The first
/// sighting (planted 30 s earlier) is only a candidate; the second confirms
/// the drift, relatches TX to 1210 Hz, and the QSO advances.
#[tokio::test]
async fn spot_call_at_a_stale_offset_recovers_on_the_second_reply() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_manual("ZL1ABC".into(), 1500.0, None)
        .await
        .unwrap();
    let report = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "ZL1ABC".into(),
        report: -11,
    };
    manager
        .maybe_confirm_frequency_drift_at(
            &report,
            1210.0,
            Utc::now() - chrono::Duration::seconds(30),
        )
        .await;
    manager
        .process_message(report, "K5ARH ZL1ABC -11".into(), 1210.0, Some(-11.0))
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        !matches!(progress.state, QsoState::RespondingToCq { .. }),
        "the confirmed second reply must advance the QSO, got {:?}",
        progress.state
    );
    assert_eq!(progress.metadata.frequency, 1210.0);
}

/// TX-offset-only spot call (allocator pick 1000 Hz, placeholder
/// `partner_freq = Some(1000)`): two DX replies at 1210 Hz relatch only
/// `partner_freq`; our TX stays at 1000 Hz and the QSO advances.
#[tokio::test]
async fn tx_offset_only_call_relatches_partner_and_keeps_tx() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_with(
            "ZL1ABC".into(),
            1000.0,
            None,
            CallInitiation::Manual,
            Some(1000.0),
            false,
            None,
        )
        .await
        .unwrap();
    let report = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "ZL1ABC".into(),
        report: -11,
    };
    manager
        .maybe_confirm_frequency_drift_at(
            &report,
            1210.0,
            Utc::now() - chrono::Duration::seconds(30),
        )
        .await;
    manager
        .process_message(report, "K5ARH ZL1ABC -11".into(), 1210.0, Some(-11.0))
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(progress.metadata.partner_freq, Some(1210.0));
    assert_eq!(progress.metadata.frequency, 1000.0);
    assert!(
        !matches!(progress.state, QsoState::RespondingToCq { .. }),
        "the confirmed reply must advance the QSO, got {:?}",
        progress.state
    );
}

/// One off-frequency frame (a spoof, or a stray decode) never advances a
/// TX-offset-only spot call, and never moves where we listen.
#[tokio::test]
async fn tx_offset_only_call_ignores_one_off_frequency_frame() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_with(
            "ZL1ABC".into(),
            1000.0,
            None,
            CallInitiation::Manual,
            Some(1000.0),
            false,
            None,
        )
        .await
        .unwrap();
    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "K5ARH".into(),
                from_station: "ZL1ABC".into(),
                report: -11,
            },
            "K5ARH ZL1ABC -11".into(),
            2400.0,
            Some(-11.0),
        )
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        matches!(progress.state, QsoState::RespondingToCq { .. }),
        "a single off-frequency frame must not advance, got {:?}",
        progress.state
    );
    assert_eq!(progress.metadata.partner_freq, Some(1000.0));
    assert_eq!(progress.metadata.frequency, 1000.0);
}

/// A split-TX relatch must announce itself on the event bus.
///
/// The coordinator refreshes its scoped/AP decoder hint
/// (`active_qso_freq_hz`) only inside the `QsoEvent::StateChanged` arm,
/// where it re-reads `metadata.partner_freq`. A relatch that mutated
/// `partner_freq` silently therefore left the decoder centred on the
/// obsolete partner offset — while the relevance gate had already moved to
/// the new one — until some unrelated transition happened to fire. Two
/// repeated `SignalReport` frames leave the QSO in `SendingReport`, so
/// there is no such transition to piggyback on.
#[tokio::test]
async fn split_tx_relatch_emits_state_change_for_decoder_refresh() {
    use tokio::sync::broadcast::error::TryRecvError;

    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_with(
            "K6L".into(),
            2700.0,
            Some(pancetta_core::slot::SlotParity::Even),
            CallInitiation::Manual,
            Some(2931.0),
            false,
            None,
        )
        .await
        .unwrap();
    let report = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "K6L".into(),
        report: -11,
    };

    // Subscribe AFTER establishment so only relatch-driven events are seen.
    let mut events = manager.subscribe();
    let t0 = Utc::now();
    manager
        .maybe_confirm_frequency_drift_at(&report, 855.0, t0)
        .await;
    manager
        .maybe_confirm_frequency_drift_at(&report, 856.0, t0 + chrono::Duration::seconds(6))
        .await;

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(progress.metadata.partner_freq, Some(856.0));

    let mut saw_state_change = false;
    loop {
        match events.try_recv() {
            Ok(QsoEvent::StateChanged {
                qso_id: changed, ..
            }) => {
                if changed == qso_id {
                    saw_state_change = true;
                }
            }
            Ok(_) => {}
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
            Err(TryRecvError::Lagged(_)) => continue,
        }
    }
    assert!(
        saw_state_change,
        "a confirmed split-TX relatch must emit StateChanged so the coordinator \
         re-reads partner_freq into its decoder hint"
    );
}

/// Once split-TX drift relatches, the relevance gate follows the new
/// partner location and rejects the obsolete one.
#[tokio::test]
async fn relatched_partner_freq_routes_subsequent_frames() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_with(
            "K6L".into(),
            2700.0,
            Some(pancetta_core::slot::SlotParity::Even),
            CallInitiation::Manual,
            Some(2931.0),
            false,
            None,
        )
        .await
        .unwrap();
    let report = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "K6L".into(),
        report: -11,
    };
    let t0 = Utc::now();
    manager
        .maybe_confirm_frequency_drift_at(&report, 855.0, t0)
        .await;
    manager
        .maybe_confirm_frequency_drift_at(&report, 856.0, t0 + chrono::Duration::seconds(6))
        .await;

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(manager.is_message_relevant(
        &progress.state,
        &progress.metadata,
        &report,
        856.0,
        false
    ));
    assert!(!manager.is_message_relevant(
        &progress.state,
        &progress.metadata,
        &report,
        2931.0,
        false
    ));
}

/// A healthy held-offset QSO measures drift from partner_freq, not our TX.
#[tokio::test]
async fn held_offset_qso_with_dx_on_partner_freq_never_drifts() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_with(
            "VK4DX".into(),
            1500.0,
            Some(pancetta_core::slot::SlotParity::Even),
            CallInitiation::Manual,
            Some(700.0),
            false,
            None,
        )
        .await
        .unwrap();
    let report = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "VK4DX".into(),
        report: -11,
    };
    let t0 = Utc::now();
    manager
        .maybe_confirm_frequency_drift_at(&report, 700.0, t0)
        .await;
    manager
        .maybe_confirm_frequency_drift_at(&report, 700.0, t0 + chrono::Duration::seconds(6))
        .await;

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(progress.metadata.pending_freq_drift, None);
    assert_eq!(progress.metadata.partner_freq, Some(700.0));
    assert_eq!(progress.metadata.frequency, 1500.0);
}

/// Genuine Hound/Fox retains its protocol-specific drift carve-out.
#[tokio::test]
async fn genuine_hound_qso_is_still_skipped_by_drift_confirm() {
    let manager = manager_with_call("K5ARH");
    let mut events = manager.subscribe();
    let qso_id = manager
        .engage_hound(
            "D2UY",
            600.0,
            Some("JI64"),
            Some(pancetta_core::slot::SlotParity::Even),
        )
        .await
        .unwrap();
    while events.try_recv().is_ok() {}

    // PAN-15 item 4: no real-time sleep between these two off-Fox frames.
    // The Hound skip in `maybe_confirm_frequency_drift_at` returns before
    // any drift candidate is ever recorded (`pending_freq_drift` stays
    // `None` throughout), so the >=5s two-strike confirmation gap can't
    // influence the outcome here — a sleep between the frames was
    // provably dead time.
    for snr in [-19.0, -17.0] {
        manager
            .process_message(
                MessageType::SignalReport {
                    to_station: "K5ARH".into(),
                    from_station: "D2UY".into(),
                    report: -12,
                },
                "K5ARH D2UY -12".into(),
                // PAN-259: outside the 300-900 Hz Fox envelope, so the
                // Hound relevance exception does not apply either.
                1800.0,
                Some(snr),
            )
            .await
            .unwrap();
    }

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(progress.metadata.pending_freq_drift, None);
    assert_eq!(progress.metadata.partner_freq, Some(600.0));
    assert!(!progress.metadata.hound_qsyed);
    assert!(matches!(progress.state, QsoState::RespondingToCq { .. }));
    assert!(
        events.try_recv().is_err(),
        "off-Fox frames must not advance, QSY, or emit a TX event"
    );

    manager
        .process_message(
            MessageType::SignalReport {
                to_station: "K5ARH".into(),
                from_station: "D2UY".into(),
                report: -12,
            },
            "K5ARH D2UY -12".into(),
            600.0,
            Some(-15.0),
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(progress.metadata.hound_qsyed);
    assert_eq!(progress.metadata.partner_freq, Some(600.0));
    assert_eq!(progress.metadata.frequency, 600.0);
    assert!(matches!(progress.state, QsoState::SendingReport { .. }));
}

/// PAN-15 item 2 regression: `engage_hound` calls `respond_to_cq_with`
/// (which sets `partner_freq` but leaves `hound=false`) and only stamps
/// `hound=true` in a SEPARATE later write-lock acquisition
/// (`stamp_hound_flag`). A decode landing in that window sees
/// `hound=false` + `partner_freq=Some` and is eligible to record a drift
/// candidate — this reproduces that window directly (bypassing
/// `engage_hound`'s single atomic call, exactly as a concurrent decode
/// would) and proves `stamp_hound_flag` clears the candidate rather than
/// leaving it stuck forever (the Hound skip in
/// `maybe_confirm_frequency_drift_at` `continue`s before ever reaching
/// the in-tolerance reset once `hound=true`, so an uncleared candidate
/// would otherwise persist in serialized metadata for the QSO's life).
#[tokio::test]
async fn stamp_hound_flag_clears_pending_freq_drift_from_the_construction_window() {
    let manager = manager_with_call("K5ARH");

    // Mirrors exactly what `engage_hound` does internally before it stamps
    // `hound=true`: a Manual QSO with `partner_freq` set atomically at
    // construction, `hound` still at its default `false`.
    let qso_id = manager
        .respond_to_cq_with(
            "D2UY".into(),
            700.0,
            Some(pancetta_core::slot::SlotParity::Even),
            CallInitiation::Manual,
            Some(1800.0), // Fox's RX offset
            false,
            None,
        )
        .await
        .unwrap();
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(!progress.metadata.hound, "sanity: hound not yet stamped");

    // Simulate a decode landing in the window between construction and
    // the hound stamp: still `hound=false`, so this is NOT skipped and
    // records a drift candidate.
    manager
        .maybe_confirm_frequency_drift(
            &MessageType::SignalReport {
                to_station: "K5ARH".into(),
                from_station: "D2UY".into(),
                report: -12,
            },
            1950.0, // 150 Hz from the Fox's 1800 Hz — beyond ESTABLISHED_FREQ_TOLERANCE_HZ
        )
        .await;
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(
        progress.metadata.pending_freq_drift.is_some(),
        "sanity: the construction window must be able to record a drift candidate"
    );

    // The fix: stamping the hound flag must clear it atomically.
    let mut events = manager.subscribe();
    manager.stamp_hound_flag(qso_id).await;
    let progress = manager.get_qso(qso_id).await.unwrap();
    assert!(progress.metadata.hound, "hound flag must now be stamped");
    assert_eq!(
        progress.metadata.pending_freq_drift, None,
        "stamp_hound_flag must clear any drift candidate accumulated in the window, \
         otherwise it can never be cleared again for this Hound QSO's life"
    );
    // PAN-142 (Codex P2 on PR #371, round 5): clearing it here must ALSO
    // emit, same as the ordinary in-tolerance clear in
    // `maybe_confirm_frequency_drift_at` — otherwise a TUI snapshot that
    // already captured the pending candidate has nothing to ever refresh
    // it, since genuine Hound QSOs are skipped by that mechanism from
    // here on (no other event would ever clear a stale indicator).
    let mut saw_state_changed_for_qso = false;
    while let Ok(ev) = events.try_recv() {
        if matches!(ev, QsoEvent::StateChanged { qso_id: id, .. } if id == qso_id) {
            saw_state_changed_for_qso = true;
        }
    }
    assert!(
        saw_state_changed_for_qso,
        "clearing the candidate via the hound stamp must emit StateChanged so the TUI \
         snapshot refreshes"
    );
}

/// PAN-15 item 3: `tx_separation_warning` boundary behavior — the pure
/// predicate behind the "relatched partner_freq too close to our own TX
/// offset" warn.
#[test]
fn tx_separation_warning_flags_separations_below_min_tx_separation() {
    assert_eq!(
        QsoManager::tx_separation_warning(2680.0, 2700.0),
        Some(20.0)
    );
    assert_eq!(
        QsoManager::tx_separation_warning(2626.0, 2700.0),
        Some(74.0),
        "1 Hz inside the MIN_TX_SEPARATION_HZ boundary must still warn"
    );
}

#[test]
fn tx_separation_warning_allows_adequate_separations() {
    assert_eq!(
        QsoManager::tx_separation_warning(2625.0, 2700.0),
        None,
        "exactly MIN_TX_SEPARATION_HZ apart is adequate separation"
    );
    assert_eq!(QsoManager::tx_separation_warning(2600.0, 2700.0), None);
}

/// PAN-15 item 3 (behavioral): a confirmed relatch that drifts the
/// partner's RX offset to within `MIN_TX_SEPARATION_HZ` of our own TX
/// offset still relatches (the ticket's minimum bar is a warn, not a
/// block) — this proves the warn path doesn't alter the relatch outcome
/// or panic.
#[tokio::test]
async fn relatch_within_min_tx_separation_of_our_own_tx_offset_still_relatches() {
    let manager = manager_with_call("K5ARH");
    let qso_id = manager
        .respond_to_cq_with(
            "K6L".into(),
            2700.0,
            Some(pancetta_core::slot::SlotParity::Even),
            CallInitiation::Manual,
            Some(2931.0),
            false,
            None,
        )
        .await
        .unwrap();

    // K6L drifts to 2680 Hz — only 20 Hz from our own 2700 Hz TX offset,
    // well inside MIN_TX_SEPARATION_HZ (75 Hz). Two sightings >=5s apart
    // are required to confirm-and-relatch.
    let report = MessageType::SignalReport {
        to_station: "K5ARH".into(),
        from_station: "K6L".into(),
        report: -11,
    };
    manager
        .process_message(report.clone(), "K5ARH K6L -11".into(), 2680.0, Some(-19.0))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    manager
        .process_message(report, "K5ARH K6L -11".into(), 2680.0, Some(-17.0))
        .await
        .unwrap();

    let progress = manager.get_qso(qso_id).await.unwrap();
    assert_eq!(
        progress.metadata.partner_freq,
        Some(2680.0),
        "the relatch still occurs even when it lands within MIN_TX_SEPARATION_HZ of our TX \
         offset — the ticket's minimum bar is a warn, not a block"
    );
    assert_eq!(
        progress.metadata.frequency, 2700.0,
        "our TX offset itself is never touched by a partner_freq relatch"
    );
}

#[tokio::test]
async fn spoofed_report_ack_does_not_advance_to_completion() {
    let manager = manager_with_call("K5ARH");
    let state = QsoState::SendingReport {
        their_callsign: "K9ZZ".into(),
        their_report: Some(-15),
        our_report: -10,
        frequency: 1500.0,
        started_at: Utc::now(),
    };
    let spoof = MessageType::ReportAck {
        to_station: "K5ARH".into(),
        from_station: "NF4KE".into(),
        report: -10,
    };
    let new_state = manager
        .determine_state_transition(
            Uuid::new_v4(),
            &state,
            &spoof,
            None,
            CallInitiation::Auto,
            false,
        )
        .await
        .unwrap();
    assert!(matches!(new_state, QsoState::SendingReport { .. }));
}

#[tokio::test]
async fn spoofed_final_confirmation_does_not_complete_qso() {
    let manager = manager_with_call("K5ARH");
    let state = QsoState::WaitingForConfirmation {
        their_callsign: "K9ZZ".into(),
        their_report: -15,
        our_report: -10,
        frequency: 1500.0,
        grid_square: None,
        started_at: Utc::now(),
    };
    let spoof = MessageType::FinalConfirmation {
        to_station: "K5ARH".into(),
        from_station: "NF4KE".into(),
    };
    let new_state = manager
        .determine_state_transition(
            Uuid::new_v4(),
            &state,
            &spoof,
            None,
            CallInitiation::Auto,
            false,
        )
        .await
        .unwrap();
    assert!(matches!(new_state, QsoState::WaitingForConfirmation { .. }));
}

#[tokio::test]
async fn legitimate_final_confirmation_completes_qso() {
    let manager = manager_with_call("K5ARH");
    let state = QsoState::WaitingForConfirmation {
        their_callsign: "K9ZZ".into(),
        their_report: -15,
        our_report: -10,
        frequency: 1500.0,
        grid_square: None,
        started_at: Utc::now(),
    };
    let legit = MessageType::FinalConfirmation {
        to_station: "K5ARH".into(),
        from_station: "K9ZZ".into(),
    };
    let new_state = manager
        .determine_state_transition(
            Uuid::new_v4(),
            &state,
            &legit,
            None,
            CallInitiation::Auto,
            false,
        )
        .await
        .unwrap();
    assert!(matches!(new_state, QsoState::Completed { .. }));
}
