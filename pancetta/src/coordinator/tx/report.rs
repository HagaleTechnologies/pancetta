use super::*;

/// Observer-only RAII guard for the TUI's TX-active badge (Batch 93).
///
/// Constructed right after `PttGuard` when PTT is asserted; its `Drop`
/// sends `MessageType::TxStatus { active: false }` to the TUI relay.
/// Because every exit from a TX arm — normal completion, operator abort
/// (F8 / Shift+Q `continue`), or shutdown `break` — drops the guard,
/// the badge clears on abort paths too, not just clean completion.
///
/// Strictly observational: it never touches PTT, audio, or scheduling.
/// The corresponding `active: true` is sent explicitly via
/// `send_tx_status` at PTT assert (async context is available there;
/// `Drop` is not async, hence the spawned fire-and-forget task).
pub(super) struct TxStatusGuard {
    message_bus: MessageBus,
}

impl TxStatusGuard {
    pub(super) fn new(message_bus: MessageBus) -> Self {
        Self { message_bus }
    }
}

impl Drop for TxStatusGuard {
    fn drop(&mut self) {
        let bus = self.message_bus.clone();
        // rationale: intentional fire-and-forget detach — `spawn` runs the task
        // independently; the dropped JoinHandle is the canonical detach idiom.
        #[allow(clippy::let_underscore_future)]
        let _ = tokio::task::spawn(async move {
            let msg = ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Tui,
                MessageType::TxStatus { active: false },
                Instant::now(),
            );
            if let Err(e) = bus.send_message(msg).await {
                tracing::debug!("TxStatus(false) relay failed (no TUI?): {}", e);
            }
            // Also clear the richer NOW-SENDING / QUEUED view so every
            // exit path (complete / abort / shutdown) returns the TX
            // panel to idle, mirroring the boolean badge.
            let idle = ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Tui,
                MessageType::TxQueueStatus {
                    sending: None,
                    queued: Vec::new(),
                },
                Instant::now(),
            );
            if let Err(e) = bus.send_message(idle).await {
                tracing::debug!("TxQueueStatus(idle) relay failed (no TUI?): {}", e);
            }
        });
    }
}

/// Notify the TUI of TX activity. Best-effort: failure (e.g. headless,
/// no TUI channel) is logged at debug and never affects the TX path.
pub(super) async fn send_tx_status(message_bus: &MessageBus, active: bool) {
    let msg = ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Tui,
        MessageType::TxStatus { active },
        Instant::now(),
    );
    if let Err(e) = message_bus.send_message(msg).await {
        tracing::debug!("TxStatus({}) relay failed (no TUI?): {}", active, e);
    }
}

/// PAN-71: bounded playback confirmation before the existing TX-log wait.
/// Step 8 still times playback from enqueue, so this does not extend key-down.
pub(crate) const TX_PLAYBACK_START_WAIT: Duration = Duration::from_millis(100);

/// Use the audio thread's playback-start report, falling back to the enqueue
/// instant if confirmation is unavailable. Called only after authorized enqueue.
pub(super) async fn frame_log_instant(
    waiter: &mut PlaybackStartWaiter,
    enqueued_at: chrono::DateTime<chrono::Utc>,
    text: &str,
) -> chrono::DateTime<chrono::Utc> {
    match waiter.wait_for(TX_PLAYBACK_START_WAIT).await {
        PlaybackStartState::Started(at) => at,
        PlaybackStartState::Pending => {
            warn!("TX audio start not confirmed within {} ms for '{text}'; logging the frame at its enqueue time", TX_PLAYBACK_START_WAIT.as_millis());
            enqueued_at
        }
        PlaybackStartState::NotStarted => {
            warn!("TX audio for '{text}' was not played (queue error, flushed, or output stream replaced); logging the frame at its enqueue time");
            enqueued_at
        }
        PlaybackStartState::Dropped => {
            debug!("TX audio playback-start ack dropped for '{text}' (no audio thread); logging the frame at its enqueue time");
            enqueued_at
        }
    }
}

/// Log one actually-keyed frame for Band Activity's own-TX history (#172).
/// The caller supplies the playback-start instant from `frame_log_instant`,
/// or its enqueue-time fallback, also used by the durable TX-log record.
pub(super) async fn log_tx_frame(
    message_bus: &MessageBus,
    text: String,
    freq_hz: f64,
    qso_id: Option<String>,
    timestamp: chrono::DateTime<chrono::Utc>,
) {
    let log_msg = ComponentMessage::new(
        ComponentId::Ft8Transmitter,
        ComponentId::Tui,
        MessageType::TxFrameLogged {
            text,
            freq_hz,
            qso_id,
            timestamp,
        },
        Instant::now(),
    );
    if let Err(e) = message_bus.send_message(log_msg).await {
        tracing::debug!("TxFrameLogged relay failed (no TUI?): {}", e);
    }
}

/// PAN-122: the effective (split-aware) TX dial for a TX-log record; 0 = unknown.
pub(super) fn tx_log_dial_hz(
    operating: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    split: &std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> u64 {
    pancetta_qso::qso_manager::effective_tx_dial(
        operating.load(Ordering::Relaxed),
        split.load(Ordering::Relaxed),
    )
}

/// Push a richer TX-queue snapshot (NOW-SENDING + QUEUED) to the TUI.
/// Best-effort, observation-only: never touches PTT/audio/scheduling.
///
/// Does NOT emit `TxFrameLogged`: that fires separately at Step 7 after
/// playback confirmation or its bounded fallback (see [`log_tx_frame`]). This
/// function's "NOW-SENDING" snapshot legitimately reflects Step 5 (PTT
/// asserted now); Band Activity's history needs the later, more accurate
/// timestamp instead.
pub(super) async fn send_tx_queue_status(
    message_bus: &MessageBus,
    sending: Option<crate::message_bus::TxItem>,
    queued: Vec<crate::message_bus::TxItem>,
) {
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

/// Report every TX outcome to Autonomous, preserving its self-CQ correlation.
/// PAN-75 also mirrors failed QSO frames to the QSO engine, which reverses
/// their keep-call accounting. Covers cap truncation, F8, supersede, denial,
/// hard mute, PAN-141 hold, PAN-114 clock-jump drops, and encode/PTT failure
/// through the same path.
/// Shutdown branches deliberately send nothing: the QSO loop stops on the
/// same flag and its in-memory accounting is discarded at exit.
/// `attempt_id` is the frame's own ledger attempt (PAN-183), carried by both
/// halves (PAN-279). Delegates to [`super::super::tx_outcome`], which sends both
/// halves on the durable tier (PAN-185); always `Ok(())`.
pub(super) async fn report_transmit_complete(
    message_bus: &MessageBus,
    success: bool,
    message_text: String,
    duration_ms: u64,
    qso_id: Option<String>,
    attempt_id: Option<pancetta_qso::TxAttemptId>,
) -> Result<()> {
    let frame = TxFrame {
        qso_id,
        message_text,
        attempt_id,
    };
    if success {
        tx_outcome::settle_aired_frame(message_bus, frame, duration_ms, None).await;
    } else {
        settle_unaired(message_bus, frame, None, AutonomousOutcome::Report).await;
    }
    Ok(())
}

/// [`tx_outcome::settle_unaired_frame`] from the TX worker.
pub(super) async fn settle_unaired(
    message_bus: &MessageBus,
    frame: TxFrame,
    replaced: Option<pancetta_qso::UnsentAttemptPayload>,
    autonomous: AutonomousOutcome,
) {
    tx_outcome::settle_unaired_frame(
        message_bus,
        ComponentId::Ft8Transmitter,
        frame,
        replaced,
        autonomous,
    )
    .await;
}

/// Report a frame after PTT is released — one whose audio played out
/// (`success` from a Step-7 `Enqueued`), or one whose audio delivery
/// failed (`Dropped`/error: no samples were ever enqueued, so the frame
/// never aired and routes through the PAN-75 not-sent path). On success it
/// also tells the QSO engine the frame aired, with the offset and binding
/// it aired on, so a stall offset action held on that frame can commit
/// (PAN-182). Never called before or during key-down (PAN-202).
pub(super) async fn report_played_transmit_complete(
    message_bus: &MessageBus,
    success: bool,
    message_text: String,
    duration_ms: u64,
    qso_id: Option<String>,
    aired: pancetta_qso::UnsentAttemptPayload,
    attempt_id: Option<pancetta_qso::TxAttemptId>,
) -> Result<()> {
    let frame = TxFrame {
        qso_id,
        message_text,
        attempt_id,
    };
    if success {
        tx_outcome::settle_aired_frame(message_bus, frame, duration_ms, Some(aired)).await;
    } else {
        settle_unaired(message_bus, frame, None, AutonomousOutcome::Report).await;
    }
    Ok(())
}

/// PAN-223: tell the QSO engine which slot this frame was scheduled into.
/// Takes the `TxSchedule` itself so the reported slot cannot differ from
/// the one the worker waits for. Durable (PAN-185); its only await is the
/// channel table's read lock, and callers send it before Step 4's PTT wait.
pub(super) async fn report_frame_scheduled(
    message_bus: &MessageBus,
    qso_id: Option<&str>,
    attempt_id: Option<pancetta_qso::TxAttemptId>,
    schedule: &TxSchedule,
) {
    let (Some(qso_id), Some(attempt_id)) = (qso_id, attempt_id) else {
        return;
    };
    message_bus
        .send_message_durable(ComponentMessage::new(
            ComponentId::Ft8Transmitter,
            ComponentId::Qso,
            MessageType::QsoMessage(crate::message_bus::QsoMessage::TxFrameScheduled {
                qso_id: qso_id.to_string(),
                attempt_id,
                target_slot: schedule.target_slot,
            }),
            Instant::now(),
        ))
        .await;
}

/// The offset and binding a transmitted frame carried (PAN-182).
pub(super) fn aired_payload(
    frequency_hz: f64,
    origin: crate::message_bus::TxOrigin,
    remote_client_key_id: Option<String>,
) -> pancetta_qso::UnsentAttemptPayload {
    pancetta_qso::UnsentAttemptPayload {
        frequency_hz,
        remote_binding: Some((
            origin == crate::message_bus::TxOrigin::Remote,
            remote_client_key_id,
        )),
    }
}

/// A frame that never aired after Step 4c may have pivoted to it.
///
/// Pivot ownership rule (PAN-185): the pivot tombstone (`pivoted_once`)
/// decides which request reports the QSO's outcome to Autonomous, so a
/// tracked self-CQ settles exactly once.
/// - Tombstone kept (the pivoted frame keyed): the frame reports, and the
///   queued request whose payload it absorbed is dropped silently (Step
///   0-dup, the backlog pivot-duplicate pass, mid-TX `IncomingDuringTx::Drop`).
/// - Tombstone cleared (the pivoted frame was refused before air): the frame
///   sends Autonomous nothing. The request that produced the pivoted payload
///   is still queued (its intent was only pivotable once queued) and reports
///   its own outcome.
///
/// Either way the QSO engine rolls back the frame the pivot replaced
/// (`pivot_replaced`), which never aired as itself (PAN-75). `attempt_id` is
/// the unaired frame's own attempt, reported only when no pivot replaced it;
/// a pivot reports the replaced frame's (PAN-183).
pub(super) async fn report_unaired_frame(
    message_bus: &MessageBus,
    message_text: String,
    qso_id: Option<String>,
    attempt_id: Option<pancetta_qso::TxAttemptId>,
    pivot_replaced: Option<ReplacedFrame>,
) -> Result<()> {
    match pivot_replaced {
        None => {
            settle_unaired(
                message_bus,
                TxFrame {
                    qso_id,
                    message_text,
                    attempt_id,
                },
                None,
                AutonomousOutcome::Report,
            )
            .await;
        }
        Some(replaced) => report_pivot_duplicate_dropped(message_bus, qso_id, Some(replaced)).await,
    }
    Ok(())
}

/// Step 0-dup: this request's payload already aired through an earlier
/// request's pivot. Autonomous gets nothing: the pivoted frame keyed and
/// already reported (ownership rule: [`report_unaired_frame`], PAN-185). The
/// QSO engine keeps this request's attempt and instead rolls back the frame
/// the pivot replaced (`pivoted_from`: its text and payload), which never
/// aired as itself (PAN-75). The payload tells the two apart when the pivot
/// changed only the offset or the remote binding.
pub(super) async fn report_pivot_duplicate_dropped(
    message_bus: &MessageBus,
    qso_id: Option<String>,
    pivoted_from: Option<ReplacedFrame>,
) {
    if let (Some(qso_id), Some((pivoted_from_text, replaced, replaced_attempt_id))) =
        (qso_id, pivoted_from)
    {
        settle_unaired(
            message_bus,
            TxFrame {
                qso_id: Some(qso_id),
                message_text: pivoted_from_text,
                attempt_id: replaced_attempt_id,
            },
            Some(replaced),
            AutonomousOutcome::Withheld,
        )
        .await;
    }
}

/// PAN-38 round 4 (Codex): report a failed `TransmitComplete` for a
/// single-item `TransmitRequest` that's being abandoned before it was ever
/// sent to the radio (operator F8 abort, shutdown-adjacent abandonment,
/// etc.). A tracked self-CQ's `pending_self_cq_qsos` entry (see
/// `coordinator/autonomous.rs`) is only ever cleared by a `TransmitComplete`
/// or an explicit `AutonomousCqDispatchFailed` -- an abort path that
/// silently `continue`s instead leaks that entry forever and never rolls
/// back the speculative streak/offset mutation the attempt made. No-op for
/// any other `MessageType` (Tune/Multi have their own completion paths).
pub(super) async fn emit_failed_transmit_complete_for_request(
    message_bus: &MessageBus,
    message_type: &MessageType,
) {
    if let MessageType::TransmitRequest {
        message_text,
        qso_id,
        attempt_id,
        ..
    } = message_type
    {
        settle_unaired(
            message_bus,
            TxFrame {
                qso_id: qso_id.clone(),
                message_text: message_text.clone(),
                attempt_id: *attempt_id,
            },
            None,
            AutonomousOutcome::Report,
        )
        .await;
    }
}

/// Surface a genuine TX-attempt failure (encode/modulate error, invalid
/// frequency, etc.) as a RETAINED diagnostic so the operator can see "TX
/// failed: <reason>" instead of a QSO silently sitting until the watchdog
/// times out — indistinguishable, from the operator's view, from the DX
/// simply not answering. Deliberate policy/security skips (TxPolicy
/// Disabled, stale-QSO drops, poisoned-lock fail-closed) are NOT routed
/// through this — those already have their own operator-visible signal
/// (TX-policy status, the emergency-stop toggle) and aren't the "invisible
/// failure" gap this closes. Best-effort: never blocks or fails the TX path.
pub(super) async fn emit_tx_failure_diagnostic(
    message_bus: &MessageBus,
    qso_id: Option<&str>,
    message_text: &str,
    reason: &str,
) {
    emit_diagnostic(
        message_bus,
        "tx.encode",
        pancetta_core::DiagnosticLevel::Warn,
        format!("TX failed for '{}': {}", message_text, reason),
        qso_id,
    )
    .await;
}

/// Send a fully attributed `DiagnosticEvent` to the TUI's retained Diagnostics
/// overlay. Best-effort: diagnostics never block or fail the caller's path.
pub(crate) async fn emit_diagnostic_full(
    message_bus: &MessageBus,
    source: ComponentId,
    target: &'static str,
    level: pancetta_core::DiagnosticLevel,
    text: String,
    qso_id: Option<&str>,
    callsign: Option<&str>,
) {
    let msg = ComponentMessage::new(
        source,
        ComponentId::Tui,
        MessageType::DiagnosticEvent {
            target,
            level,
            text,
            qso_id: qso_id.map(|s| s.to_string()),
            callsign: callsign.map(|s| s.to_string()),
        },
        Instant::now(),
    );
    if let Err(e) = message_bus.send_message(msg).await {
        tracing::debug!("DiagnosticEvent({}) relay failed (no TUI?): {}", target, e);
    }
}

/// Send a `DiagnosticEvent` to the TUI's retained Diagnostics overlay
/// (observability-diagnostics-plan.md Layer 1, "Emission"). `target` reuses
/// the same vocabulary already used by the co-located `tracing` call at each
/// site (e.g. `"tx.policy"`) — this is a SEPARATE field from that macro's own
/// `target:` (the tracing one gates file-log visibility via `EnvFilter`; this
/// one is just a TUI-side label used for filtering the Diagnostics panel).
/// Best-effort: never blocks or fails the TX path.
pub(crate) async fn emit_diagnostic(
    message_bus: &MessageBus,
    target: &'static str,
    level: pancetta_core::DiagnosticLevel,
    text: String,
    qso_id: Option<&str>,
) {
    emit_diagnostic_full(
        message_bus,
        ComponentId::Ft8Transmitter,
        target,
        level,
        text,
        qso_id,
        None,
    )
    .await;
}

/// PAN-92: shared diagnostic/audit/relay for a Remote transmission stopped
/// by an arm re-check — mirrors the Step 0a pickup-time drop's dispensa
/// Q-0051 Phase A/B/C signals (TUI diagnostic, `AuditKind::TxDenied`, and a
/// client-visible `error` event) so the interruption is exactly as visible
/// as a pickup-time one, whether it happens mid-flight
/// ([`SleepOutcome::AbortedByDisarm`]) or at the last-instant pre-PTT
/// recheck (round-2 review: the arm can go stale in the gap between
/// Step 4b-arm and the actual `SetPtt` send).
///
/// `diagnostic_verb` and `denial_reason` are caller-supplied rather than
/// built in here, since the wording differs by call site ("interrupted" for
/// a transmission already on the air vs. "denied" for one PTT hasn't
/// asserted for yet — reusing "disarmed mid-transmission" verbatim for the
/// latter would be factually wrong).
///
/// PAN-91 review follow-up: for an in-flight interruption, the CALLER must
/// send PTT off and disarm its `PttGuard` BEFORE calling this — every step
/// here awaits (diagnostic, synchronous audit-log I/O, relay), and delaying
/// PTT release behind that work leaves a disarmed remote transmission keyed
/// longer than necessary. Not applicable to a pre-PTT denial (PTT was never
/// asserted).
#[allow(clippy::too_many_arguments)]
pub(super) async fn emit_disarm_interrupt_signals(
    message_bus: &MessageBus,
    audit_log: &pancetta_agent::audit::AuditLog,
    display_feed_enabled: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    // Round-12 review (Codex P2): the CALLER captures this at the denial
    // check itself — before this function's own `emit_diagnostic` await —
    // rather than this function re-locking the live `ArmState` after that
    // await. If the arm is absent/expired at the denial point and a
    // client then arms (or takes control) while `emit_diagnostic` is
    // pending, re-locking here would attribute the durable `TxDenied`
    // record to that LATER arm state, not the one that actually rejected
    // the frame. Mirrors `ArmSnapshot::operator_callsign`'s identical
    // reasoning for the coalescer's own (already detached) denial path.
    operator_callsign: Option<String>,
    diagnostic_verb: &str,
    denial_reason: String,
    qso_id: Option<&str>,
) {
    emit_diagnostic(
        message_bus,
        "agent.tx",
        pancetta_core::DiagnosticLevel::Warn,
        format!("Remote TX {diagnostic_verb} ({denial_reason})"),
        qso_id,
    )
    .await;
    audit_log.record(&pancetta_agent::audit::AuditEvent {
        ts_unix_ms: chrono::Utc::now().timestamp_millis(),
        kind: pancetta_agent::audit::AuditKind::TxDenied,
        operator_callsign,
        detail: denial_reason.clone(),
    });
    super::super::remote_gateway::relay_to_gateway(
        message_bus,
        display_feed_enabled,
        ComponentId::Ft8Transmitter,
        MessageType::TxDenied {
            reason: denial_reason,
            qso_id: qso_id.map(str::to_string),
        },
    )
    .await;
}

impl super::super::ApplicationCoordinator {
    /// PAN-122: the coordinator's durable local TX log, created on first use
    /// and reused on every later call — the supervisor's restart of the TX
    /// worker re-runs `start_transmitter_component`, and the file must never
    /// have two writers. Under `--replay` the log is disabled (no file).
    pub(crate) fn ensure_tx_frame_log(&mut self) -> std::sync::Arc<TxFrameLog> {
        if let Some(log) = &self.tx_frame_log {
            return log.clone();
        }
        let log = if self.replay_mode() {
            info!("Replay mode: TX log disabled; replayed frames are not recorded");
            TxFrameLog::disabled()
        } else {
            let path = super::super::tx_log::tx_log_path(self.pancetta_home_override.as_deref());
            info!(
                "TX log: appending every transmitted frame to {}",
                path.display()
            );
            TxFrameLog::open(path)
        };
        let log = std::sync::Arc::new(log);
        self.tx_frame_log = Some(log.clone());
        log
    }
}

/// Publish a completed FT burst's selected drive and queue session persistence.
pub(super) async fn finish_auto_drive_burst(
    bus: &MessageBus,
    drive: &std::sync::Arc<super::super::tx_drive::TxDrive>,
    snapshot: super::super::tx_drive::DriveSnapshot,
    samples: &[pancetta_core::tx_meter::TxMeterReading],
    context: super::super::tx_drive::DriveContext,
    session_record: &std::sync::Arc<std::sync::Mutex<super::super::session_state::SessionRecord>>,
    session_path: &Option<std::path::PathBuf>,
    operating_frequency_hz: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    split_tx_frequency_hz: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    rig_generation: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    ft_mode: &std::sync::Arc<std::sync::atomic::AtomicU8>,
) {
    use super::super::tx_drive::DriveDecision;
    let live_band = pancetta_core::Band::from_frequency(tx_log_dial_hz(
        operating_frequency_hz,
        split_tx_frequency_hz,
    ));
    let still_same_context = snapshot.band == live_band
        && rig_generation.load(Ordering::Acquire) == context.rig_generation
        && ft_mode.load(Ordering::Acquire) == context.ft_mode;
    let decision = if still_same_context {
        drive.complete(snapshot, samples, context)
    } else {
        DriveDecision::Stale
    };
    if decision == DriveDecision::Stale
        && snapshot.mode == pancetta_core::tx_drive_mode::TxDriveMode::Auto
    {
        if let Some(limit) = drive.warn_only_limit() {
            let mut previous = None;
            for sample in samples {
                match sample
                    .alc
                    .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                {
                    Some(alc) if alc > limit => {
                        if let Some(earlier) = previous {
                            super::super::tx_meter::act(
                                super::super::tx_meter::AlcAction::ReportOverdrive {
                                    readings: [earlier, alc],
                                    max_alc: limit,
                                },
                                bus,
                            )
                            .await;
                            break;
                        }
                        previous = Some(alc);
                    }
                    Some(_) => previous = None,
                    None => (),
                }
            }
        }
    }
    let selected = drive.snapshot_for_band(snapshot.band);
    if selected.band == live_band && selected != snapshot {
        let _ = bus
            .send_message(ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Tui,
                MessageType::RigControl(crate::message_bus::RigControlMessage::TxDriveState {
                    db: selected.db,
                    ceiling_db: drive.ceiling_db(),
                    mode: selected.mode,
                    available: selected.available,
                }),
                Instant::now(),
            ))
            .await;
    }
    let alert = match decision {
        DriveDecision::Overdrive(db) => Some(format!("ALC high: drive {} dB", format_drive_db(db))),
        DriveDecision::Floor => Some("ALC high: lower the rig's USB/DATA input level".to_string()),
        _ => None,
    };
    if let Some(alert) = alert {
        let _ = bus
            .send_message(ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Tui,
                MessageType::Error {
                    component_id: ComponentId::Ft8Transmitter,
                    error_message: alert.clone(),
                    error_code: None,
                },
                Instant::now(),
            ))
            .await;
        let _ = bus
            .send_message(ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Tui,
                MessageType::DiagnosticEvent {
                    target: "tx.drive",
                    level: pancetta_core::DiagnosticLevel::Warn,
                    text: alert,
                    qso_id: None,
                    callsign: None,
                },
                Instant::now(),
            ))
            .await;
    }
    if drive.needs_persist() {
        let record = session_record.clone();
        let path = session_path.clone();
        let drive = drive.clone();
        let bus = bus.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                super::super::session_state::persist_tx_drive_current(&record, &path, &drive)
            })
            .await;
            if let Ok(Err(error)) = result {
                tracing::warn!("Could not save TX drive: {error}");
                let notice =
                    "TX drive changed for this session only. Could not save TX drive.".to_string();
                let _ = bus
                    .send_message(ComponentMessage::new(
                        ComponentId::Ft8Transmitter,
                        ComponentId::Tui,
                        MessageType::Error {
                            component_id: ComponentId::Ft8Transmitter,
                            error_message: notice.clone(),
                            error_code: None,
                        },
                        Instant::now(),
                    ))
                    .await;
                let _ = bus
                    .send_message(ComponentMessage::new(
                        ComponentId::Ft8Transmitter,
                        ComponentId::Tui,
                        MessageType::DiagnosticEvent {
                            target: "tx.drive",
                            level: pancetta_core::DiagnosticLevel::Warn,
                            text: format!("{notice} {error}"),
                            qso_id: None,
                            callsign: None,
                        },
                        Instant::now(),
                    ))
                    .await;
            }
        });
    }
}

fn format_drive_db(db: f32) -> String {
    let value = if db.fract() == 0.0 {
        format!("{db:.0}")
    } else {
        format!("{db:.1}")
    };
    value.replace('-', "−")
}
