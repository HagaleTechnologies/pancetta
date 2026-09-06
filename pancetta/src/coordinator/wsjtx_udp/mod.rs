//! WSJT-X UDP companion-protocol support (GridTracker/JTAlert interop).
//!
//! See `docs/superpowers/specs/2026-07-13-wsjtx-udp-protocol-notes.md` for the
//! clean-room byte-level protocol reference this module implements against.
//!
//! Lifecycle mirrors `start_pskreporter_component`
//! (`pancetta/src/coordinator/psk_reporter.rs`): `enabled = false` (default)
//! ⇒ no socket is ever bound and a bus-drain task keeps the channel open so
//! the decoder fan-out never backs up; `enabled = true` ⇒ open the socket,
//! emit Heartbeat every 15s and change-driven Status, fan in FT8 decodes
//! (additive tap in `ft8.rs`) as Decode(2) into a 500-entry retention ring,
//! emit Clear(3) on band change, answer Replay(7) by re-emitting the ring
//! with `New=false`, and emit Close(6) on shutdown. `--replay` forces the
//! drain-only shape regardless of config (see
//! [`super::ApplicationCoordinator::replay_mode`]).

pub mod codec;

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Timelike, Utc};
use tokio::time::{interval, sleep};
use tracing::{debug, info, warn};

use codec::{InMsg, OutMsg};
use pancetta_config::WsjtxUdpConfig;
use pancetta_core::DiagnosticLevel;
use pancetta_ft8::Ft8Message;
use pancetta_tui::app::{AUDIO_PASSBAND_MAX_HZ, AUDIO_PASSBAND_MIN_HZ};

use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType, QsoMessage};

/// Heartbeat cadence (protocol notes §1: every 15s, starting immediately at
/// boot — `tokio::time::interval`'s first tick fires immediately).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// Status sampling cadence (protocol notes §5: sample on a 1s timer, emit
/// only on change).
const STATUS_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// A decode within this many ms of "now" counts as `Decoding = true`.
const DECODING_WINDOW_MS: u64 = 2_000;

/// Inbound-poll timeout: bounds how long one `recv_from` wait can delay
/// servicing the Heartbeat/Status timers and the shutdown flag. Small enough
/// that the component stays responsive; large enough to avoid a busy loop.
const INBOUND_POLL_TIMEOUT: Duration = Duration::from_millis(200);

/// `Heartbeat.Version` string pancetta advertises. JTAlert reads this field
/// (⚑ protocol notes §7); a plausible WSJT-X-shaped version avoids
/// triggering any version-gated warning in companion apps.
const HEARTBEAT_VERSION: &str = "2.6.1";

/// Cap on the retained-decode ring used to answer Replay(7) (protocol notes
/// §5: "Retain decodes until you emit Clear") — matches the TUI's decode
/// channel bound so this never grows without limit across a long idle
/// session. Oldest entry evicted once a new decode would exceed this.
const DECODE_RING_CAPACITY: usize = 500;

impl super::ApplicationCoordinator {
    /// Start the WSJT-X UDP companion-protocol component.
    ///
    /// Lifecycle mirrors `start_pskreporter_component` exactly:
    /// `enabled = false` (default) — or any `--replay` run ⇒ no socket is
    /// ever bound; a bus-drain task keeps the `ComponentId::WsjtxUdp`
    /// channel open so the decoder
    /// fan-out never backs up against a channel with no reader. `enabled =
    /// true` ⇒ snapshot config, open the socket, emit an immediate Status
    /// (GridTracker instance-validity rule — protocol notes §5: an instance
    /// is valid only after its first Status), then loop: draining the bus
    /// (mapping each decode to Decode(2), retaining it in a 500-entry ring,
    /// and emitting it), a 15s Heartbeat (+ Status right after each one)
    /// and a 1s change-driven Status sample (both also sample the dial
    /// frequency for Clear-on-band-change), and polling for inbound
    /// datagrams (answering Replay(7) by re-emitting the ring with
    /// `New=false` then a Status). Emits Close(6) on loop exit.
    pub(crate) async fn start_wsjtx_udp_component(&mut self) -> Result<()> {
        // `--replay` decodes are historical off-air captures, but every
        // Decode(2) datagram built from them is stamped with the current
        // slot's wall clock. Broadcasting those would feed replayed traffic
        // to GridTracker/JTAlert (and anything else listening) as live
        // reception, which those apps log and upload onward. Same class as
        // the PSKReporter and cqdx.io gates -- see
        // `ApplicationCoordinator::replay_mode`. Take the existing drain-only
        // path so no socket is ever bound.
        let replay_mode = self.replay_mode();

        let config = self.config.read().await;
        if replay_mode || !config.network.wsjtx_udp.enabled {
            if replay_mode {
                info!("Replay mode: WSJT-X UDP companion protocol suppressed -- replayed decodes are not live spots");
            } else {
                info!("WSJT-X UDP companion protocol disabled in configuration");
            }
            drop(config);
            self.spawn_wsjtx_drain_task().await?;
            return Ok(());
        }

        let cfg = config.network.wsjtx_udp.clone();
        let station_ids = StationIds {
            de_call: config.station.callsign.clone(),
            de_grid: config.station.grid_square.clone(),
        };
        // Stamped into QSOLogged's TxPower field the same way qso.rs stamps
        // it into every rendered ADIF record's TX_PWR.
        let station_power_watts = config.station.power_watts;
        drop(config);

        let socket = match open_socket(&cfg) {
            Ok(s) => s,
            Err(e) => {
                // Fail-safe, not fail-closed-app: a bad destination/interface
                // shouldn't crash the whole station. Fall back to the same
                // drain-only shape as the disabled path so the channel still
                // has a reader.
                warn!("WSJT-X UDP: failed to open socket ({e}); component disabled for this run");
                self.spawn_wsjtx_drain_task().await?;
                return Ok(());
            }
        };

        let dest: SocketAddr = cfg
            .destination
            .parse()
            .context("invalid wsjtx_udp destination")?;

        info!(
            "Starting WSJT-X UDP companion component (destination: {}, id: \"{}\")",
            cfg.destination, cfg.instance_id
        );

        // Design spec Option A audit: when `allow_tx_initiation` is on, the
        // shared remote-TX arm was seeded by THIS channel (see the constructor
        // + station-agent seeding sites). Surface it as a startup diagnostic so
        // the operator sees, in the TUI, that a GridTracker/UDP Reply can now
        // initiate an (arm-gated) on-air QSO — matching the warn! logged at
        // seed time.
        if cfg.allow_tx_initiation {
            emit_diagnostic(
                &self.message_bus,
                DiagnosticLevel::Warn,
                "remote-TX arm seeded by [network.wsjtx_udp].allow_tx_initiation \
                 — UDP Reply may initiate QSOs"
                    .to_string(),
            )
            .await;
        }

        let (_wsjtx_tx, wsjtx_rx) = self
            .message_bus
            .create_channel(ComponentId::WsjtxUdp)
            .await?;

        let state = WsjtxState {
            operating_frequency_hz: self.operating_frequency_hz.clone(),
            active_protocol_mode: self.active_protocol_mode.clone(),
            tx_offset_hold_hz: self.tx_offset_hold_hz.clone(),
            tx_policy: self.tx_policy.clone(),
            ptt_active: self.ptt_active.clone(),
            last_decode_timestamp: self.last_decode_timestamp.clone(),
        };

        let instance_id = cfg.instance_id.clone();
        let shutdown = self.shutdown_signal.clone();
        // Task 6 (inbound dispatch): the audit trail (`emit_diagnostic`) and
        // HaltTx's immediate-stop effect both need these — cloned here, the
        // same pattern as every other `Arc`/`MessageBus` handoff into this
        // spawned task.
        let message_bus = self.message_bus.clone();
        let abort_current_tx = self.abort_current_tx.clone();

        // Task 5 (QSOLogged/LoggedADIF): `qso.rs` populates this field
        // synchronously while constructing `QsoManager`, before
        // `start_qso_component` returns (see the `wsjtx_qso_events_rx` doc
        // comment on `ApplicationCoordinator`). Taken here (not earlier) so
        // the disabled/socket-open-failure early returns above leave it
        // untouched.
        let qso_events_handoff = self.wsjtx_qso_events_rx.take();

        let wsjtx_handle = tokio::spawn(async move {
            // GridTracker instance-validity rule (protocol notes §5): the
            // FIRST Status this session emits must precede any Decode —
            // send it synchronously here, before the loop (and therefore
            // before any future Decode fan-in drained from `wsjtx_rx`) ever
            // gets a chance to run.
            let initial_snapshot = state.sample();
            let initial_status = build_status(&initial_snapshot, &station_ids);
            let mut last_status = initial_status.encode(&instance_id);
            if let Err(e) = socket.send_to(&last_status, dest).await {
                warn!("WSJT-X UDP: initial Status send failed: {e}");
            }

            // Task 5: `qso_events_handoff` is already the resolved
            // `broadcast::Receiver` (populated synchronously by
            // `start_qso_component`, before this task was ever spawned) — no
            // channel or timeout to resolve. `None` only when this coordinator
            // run never wired one up (e.g. a unit test constructing
            // `ApplicationCoordinator` without going through
            // `start_qso_component`); QSOLogged/LoggedADIF simply stay
            // disabled for the run in that case — every other message type is
            // unaffected.
            let mut qso_events = qso_events_handoff;
            // Renders each completed QSO's ADIF record the identical way the
            // source-of-truth writer and the per-QSO upload subscriber do
            // (`qso.rs`'s `start_adif_subscriber` / `start_qso_upload_subscriber`)
            // — no second ADIF-formatting implementation.
            let adif_processor =
                pancetta_qso::AdifProcessor::new().with_station_power_watts(station_power_watts);

            let mut heartbeat_timer = interval(HEARTBEAT_INTERVAL);
            let mut status_timer = interval(STATUS_SAMPLE_INTERVAL);
            let mut inbound_buf = [0u8; 2048];

            // Retention ring for Replay(7) support, seeded empty. Band
            // pre-seeded from the initial snapshot so the first sample never
            // spuriously fires a Clear (there's nothing to invalidate yet).
            let mut decode_ring: VecDeque<(OutMsg, StoredKey)> = VecDeque::new();
            let mut last_band = Some(band_of(initial_snapshot.dial_frequency_hz));

            'outer: while !shutdown.load(Ordering::Acquire) {
                // (c) Drain the bus channel: fan-in from the FT8 decoder
                // tap (ft8.rs, gated on `wsjtx_enabled`). Each decode is
                // mapped, retained in the ring, and emitted as Decode(2).
                loop {
                    match wsjtx_rx.try_recv() {
                        Ok(message) => {
                            if let MessageType::DecodedMessage(decoded) = message.message_type {
                                if is_stale_band_decode(decoded.captured_dial_hz, last_band) {
                                    debug!(
                                        "WSJT-X UDP: dropping decode captured on a prior \
                                         band (band switch happened mid-decode)"
                                    );
                                    continue;
                                }
                                let mode = pancetta_config::OperatingMode::from_u8(
                                    state.active_protocol_mode.load(Ordering::Relaxed),
                                );
                                if let Some(out) = decode_to_msg(&decoded, mode) {
                                    if let Some(key) = stored_key(&out, decoded.slot_parity) {
                                        push_decode_ring(
                                            &mut decode_ring,
                                            (out.clone(), key),
                                            DECODE_RING_CAPACITY,
                                        );
                                    }
                                    if let Err(e) =
                                        socket.send_to(&out.encode(&instance_id), dest).await
                                    {
                                        warn!("WSJT-X UDP: decode send failed: {e}");
                                    }
                                }
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => break,
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            debug!("WSJT-X UDP channel disconnected");
                            break 'outer;
                        }
                    }
                }

                tokio::select! {
                    // (a) 15s Heartbeat, followed by a Status (protocol
                    // notes §5: "emit ... after each Heartbeat"). Fires
                    // immediately on the very first tick (interval's
                    // documented behavior), matching "start immediately at
                    // boot".
                    _ = heartbeat_timer.tick() => {
                        let hb = OutMsg::Heartbeat {
                            max_schema: 3,
                            version: HEARTBEAT_VERSION.to_string(),
                            revision: String::new(),
                        };
                        if let Err(e) = socket.send_to(&hb.encode(&instance_id), dest).await {
                            warn!("WSJT-X UDP: heartbeat send failed: {e}");
                        }
                        let snapshot = state.sample();
                        if let Some(band) = band_changed(snapshot.dial_frequency_hz, last_band) {
                            last_band = Some(band);
                            decode_ring.clear();
                            if let Err(e) = socket.send_to(&OutMsg::Clear.encode(&instance_id), dest).await {
                                warn!("WSJT-X UDP: Clear send failed: {e}");
                            }
                        }
                        let encoded = build_status(&snapshot, &station_ids).encode(&instance_id);
                        if let Err(e) = socket.send_to(&encoded, dest).await {
                            warn!("WSJT-X UDP: post-heartbeat status send failed: {e}");
                        }
                        last_status = encoded;
                    }
                    // (b) 1s Status sample, emitted only when the encoded
                    // bytes changed since the last send (protocol notes §5).
                    // Also the band-change sampler: a dial-frequency move
                    // into a new MHz bucket invalidates the retained ring.
                    _ = status_timer.tick() => {
                        let snapshot = state.sample();
                        if let Some(band) = band_changed(snapshot.dial_frequency_hz, last_band) {
                            last_band = Some(band);
                            decode_ring.clear();
                            if let Err(e) = socket.send_to(&OutMsg::Clear.encode(&instance_id), dest).await {
                                warn!("WSJT-X UDP: Clear send failed: {e}");
                            }
                        }
                        let encoded = build_status(&snapshot, &station_ids).encode(&instance_id);
                        if encoded != last_status {
                            if let Err(e) = socket.send_to(&encoded, dest).await {
                                warn!("WSJT-X UDP: status send failed: {e}");
                            }
                            last_status = encoded;
                        }
                    }
                    // (d) Inbound poll: decode, gate, dispatch. Bad-magic
                    // datagrams are dropped silently (`InMsg::decode` ->
                    // `None`); a mismatched `id` is debug-logged and ignored
                    // (protocol notes §5 "Id targeting" — this instance isn't
                    // who the request was addressed to); everything else
                    // passes through the fail-closed source gate
                    // (`request_allowed`) before dispatch, with a Warn
                    // diagnostic on refusal. `HaltTx`, `Replay`, and `Reply`
                    // (remote QSO initiation — the five-gate model) are all
                    // honored here; a Reply that fails any of gates 2/4/5 is
                    // refused-with-audit, never partially acted on.
                    // Every honored AND refused request gets a retained
                    // `DiagnosticEvent` (design spec's audit-trail
                    // requirement). The timeout throttles this branch so the
                    // loop never busy-spins.
                    recv = tokio::time::timeout(INBOUND_POLL_TIMEOUT, socket.recv_from(&mut inbound_buf)) => {
                        if let Ok(Ok((n, from))) = recv {
                            match InMsg::decode(&inbound_buf[..n]) {
                                Some((id, _msg)) if id != instance_id => {
                                    debug!(
                                        "WSJT-X UDP: {} inbound bytes from {} addressed to id \
                                         {:?} (this instance is {:?}), ignored",
                                        n, from, id, instance_id
                                    );
                                }
                                Some((_id, msg)) if !request_allowed(&cfg, from.ip(), &dest) => {
                                    emit_diagnostic(
                                        &message_bus,
                                        DiagnosticLevel::Warn,
                                        format!(
                                            "refused request from {}: {}",
                                            from,
                                            msg_type_label(&msg)
                                        ),
                                    )
                                    .await;
                                }
                                Some((_id, InMsg::HaltTx { auto_tx_only })) => {
                                    // Drop-stale-TX / emergency-stop posture:
                                    // the abort flag is set unconditionally
                                    // and immediately — the TX worker's
                                    // `interruptible_sleep` wakes within
                                    // ~50ms regardless of `AutoTxOnly`
                                    // (protocol notes §4 IN HaltTx: "false =
                                    // stop TX immediately; true = only
                                    // disable auto-transmit" — the in-flight
                                    // transmission itself is stopped either
                                    // way; `AutoTxOnly=true` only skips
                                    // clearing the QSO/CQ *sources* below).
                                    abort_current_tx.store(true, Ordering::Release);
                                    if !auto_tx_only {
                                        let cancel_all = ComponentMessage::new(
                                            ComponentId::WsjtxUdp,
                                            ComponentId::Qso,
                                            MessageType::QsoMessage(QsoMessage::CancelAllQsos),
                                            Instant::now(),
                                        );
                                        if let Err(e) = message_bus.send_message(cancel_all).await {
                                            warn!(
                                                "WSJT-X UDP: HaltTx: failed to send \
                                                 CancelAllQsos: {e}"
                                            );
                                        }
                                        let stop_cq = ComponentMessage::new(
                                            ComponentId::WsjtxUdp,
                                            ComponentId::Qso,
                                            MessageType::QsoMessage(QsoMessage::StopCq),
                                            Instant::now(),
                                        );
                                        if let Err(e) = message_bus.send_message(stop_cq).await {
                                            warn!(
                                                "WSJT-X UDP: HaltTx: failed to send StopCq: {e}"
                                            );
                                        }
                                    }
                                    emit_diagnostic(
                                        &message_bus,
                                        DiagnosticLevel::Info,
                                        format!(
                                            "HaltTx from {from} honored (AutoTxOnly={auto_tx_only})"
                                        ),
                                    )
                                    .await;
                                }
                                Some((_id, InMsg::Replay)) => {
                                    for (out, _key) in &decode_ring {
                                        if let Some(replay) = as_replay(out) {
                                            if let Err(e) = socket
                                                .send_to(&replay.encode(&instance_id), dest)
                                                .await
                                            {
                                                warn!("WSJT-X UDP: replay decode send failed: {e}");
                                            }
                                        }
                                    }
                                    let encoded = build_status(&state.sample(), &station_ids).encode(&instance_id);
                                    if let Err(e) = socket.send_to(&encoded, dest).await {
                                        warn!("WSJT-X UDP: post-replay status send failed: {e}");
                                    }
                                    last_status = encoded;
                                    emit_diagnostic(
                                        &message_bus,
                                        DiagnosticLevel::Info,
                                        format!("Replay from {from} honored"),
                                    )
                                    .await;
                                }
                                Some((_id, reply @ InMsg::Reply { .. })) => {
                                    // Remote QSO initiation — the design spec's
                                    // five ANDed fail-closed gates (numbered per
                                    // the design spec's "Remote QSO initiation"
                                    // section, not the order checks run in):
                                    // gate 1 (`accept_udp_requests`) and gate 3
                                    // (source filter) were already enforced by
                                    // the `!request_allowed` refusal arm ABOVE,
                                    // so only allowed, id-matched Replies reach
                                    // here. Gate 2 (`allow_tx_initiation`) and
                                    // gate 4 (`TxPolicy::allows_initiation()`)
                                    // are applied below; gate 5 is the TX
                                    // worker's `remote_tx_permitted` fail-closed
                                    // arm check, applied downstream of the
                                    // `StartQso` this handler sends. The
                                    // retained-decode match (`reply_to_call`,
                                    // "does it echo a decode we emitted?") is
                                    // an additional hard precondition the spec
                                    // doesn't number — it must pass regardless
                                    // of gate state. ANY failure ⇒
                                    // refuse-with-audit, no side effect.
                                    match reply_to_call(&reply, &decode_ring) {
                                        None => {
                                            emit_diagnostic(
                                                &message_bus,
                                                DiagnosticLevel::Warn,
                                                format!(
                                                    "Reply from {from} matched no retained \
                                                     decode — ignored"
                                                ),
                                            )
                                            .await;
                                        }
                                        Some(intent) if !cfg.allow_tx_initiation || !intent.is_cq => {
                                            // Non-CQ replies are targeting-only
                                            // upstream too (protocol notes §5);
                                            // v1 treats both !consent and
                                            // !is_cq as refusal-with-audit
                                            // rather than partial action.
                                            emit_diagnostic(
                                                &message_bus,
                                                DiagnosticLevel::Warn,
                                                format!(
                                                    "Reply({}) from {from} refused: \
                                                     allow_tx_initiation={}, is_cq={}",
                                                    intent.callsign,
                                                    cfg.allow_tx_initiation,
                                                    intent.is_cq
                                                ),
                                            )
                                            .await;
                                        }
                                        Some(intent) => {
                                            let policy = pancetta_core::TxPolicy::from_u8(
                                                state.tx_policy.load(Ordering::Acquire),
                                            );
                                            if !policy.allows_initiation() {
                                                emit_diagnostic(
                                                    &message_bus,
                                                    DiagnosticLevel::Warn,
                                                    format!(
                                                        "Reply({}) from {from} refused: \
                                                         TX policy {policy:?}",
                                                        intent.callsign
                                                    ),
                                                )
                                                .await;
                                            } else {
                                                // All five gates passed.
                                                // remote_origin: true ⇒ every
                                                // frame is TxOrigin::Remote ⇒
                                                // arm-gated fail-closed in the
                                                // TX worker + parity-admitted +
                                                // dup-checked in the QSO engine.
                                                // Never TxOrigin::Local (repo
                                                // invariant).
                                                let msg = ComponentMessage::new(
                                                    ComponentId::WsjtxUdp,
                                                    ComponentId::Qso,
                                                    MessageType::QsoMessage(
                                                        QsoMessage::StartQso {
                                                            callsign: intent.callsign.clone(),
                                                            frequency: intent.frequency_hz,
                                                            dx_parity: intent.dx_parity,
                                                            remote_origin: true,
                                                        },
                                                    ),
                                                    Instant::now(),
                                                );
                                                if let Err(e) =
                                                    message_bus.send_message(msg).await
                                                {
                                                    warn!(
                                                        "WSJT-X UDP: Reply StartQso send \
                                                         failed: {e}"
                                                    );
                                                } else {
                                                    emit_diagnostic(
                                                        &message_bus,
                                                        DiagnosticLevel::Info,
                                                        format!(
                                                            "GridTracker/UDP Reply → calling \
                                                             {} (remote-origin, arm-gated)",
                                                            intent.callsign
                                                        ),
                                                    )
                                                    .await;
                                                }
                                            }
                                        }
                                    }
                                }
                                Some((_id, msg)) => {
                                    debug!(
                                        "WSJT-X UDP: {} inbound bytes from {} ({}), no action",
                                        n,
                                        from,
                                        msg_type_label(&msg)
                                    );
                                }
                                None => {
                                    debug!(
                                        "WSJT-X UDP: {} inbound bytes from {} (undecodable, ignored)",
                                        n, from
                                    );
                                }
                            }
                        }
                    }
                    // (e) Task 5: QSOLogged(5) + LoggedADIF(12) on QSO
                    // completion. `qso_events` is `None` whenever the
                    // handoff never resolved (disabled at this coordinator
                    // level, timed out, or the sender was dropped); the
                    // `pending()` branch below then simply never fires,
                    // leaving this arm permanently idle without busy-looping
                    // the select.
                    qso_ev = async {
                        match qso_events.as_mut() {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        match qso_ev {
                            Ok(pancetta_qso::QsoEvent::QsoCompleted { metadata, .. }) => {
                                let adif_qso = adif_processor
                                    .qso_to_adif(&metadata, metadata.contest_info.as_ref());
                                match adif_processor.generate_record(&adif_qso) {
                                    Ok(record) => {
                                        let (qso_logged, logged_adif) =
                                            qso_to_msgs(&metadata, station_power_watts, record);
                                        if let Err(e) = socket
                                            .send_to(&qso_logged.encode(&instance_id), dest)
                                            .await
                                        {
                                            warn!("WSJT-X UDP: QSOLogged send failed: {e}");
                                        }
                                        if let Err(e) = socket
                                            .send_to(&logged_adif.encode(&instance_id), dest)
                                            .await
                                        {
                                            warn!("WSJT-X UDP: LoggedADIF send failed: {e}");
                                        }
                                    }
                                    Err(e) => {
                                        warn!(
                                            "WSJT-X UDP: skipping QSOLogged/LoggedADIF for QSO {}: \
                                             ADIF render failed: {e}",
                                            metadata.qso_id,
                                        );
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!("WSJT-X UDP: QSO-event stream lagged by {n} events");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                debug!("WSJT-X UDP: QSO-event stream closed");
                                qso_events = None;
                            }
                        }
                    }
                }
            }

            let close = OutMsg::Close.encode(&instance_id);
            if let Err(e) = socket.send_to(&close, dest).await {
                warn!("WSJT-X UDP: Close send failed: {e}");
            }
            info!("WSJT-X UDP component stopped");
            Ok(())
        });

        self.named_task_handles
            .push((ComponentId::WsjtxUdp, wsjtx_handle));
        info!("WSJT-X UDP component started");
        Ok(())
    }

    /// Spawn the disabled-path drain task: creates the `ComponentId::WsjtxUdp`
    /// channel with no real reader beyond a discard loop, so a sender (the
    /// decoder fan-out, once wired) never fills the channel and floods
    /// "Channel full" warnings — same rationale as
    /// `start_pskreporter_component`'s disabled path (`psk_reporter.rs`).
    async fn spawn_wsjtx_drain_task(&mut self) -> Result<()> {
        let (_drain_tx, drain_rx) = self
            .message_bus
            .create_channel(ComponentId::WsjtxUdp)
            .await?;
        let shutdown = self.shutdown_signal.clone();
        let drain_handle = tokio::spawn(async move {
            while !shutdown.load(Ordering::Acquire) {
                loop {
                    match drain_rx.try_recv() {
                        Ok(_) => {}
                        Err(crossbeam_channel::TryRecvError::Empty) => break,
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            debug!("WSJT-X UDP drain channel disconnected");
                            return Ok(());
                        }
                    }
                }
                sleep(Duration::from_millis(100)).await;
            }
            Ok(())
        });
        self.named_task_handles
            .push((ComponentId::WsjtxUdp, drain_handle));
        Ok(())
    }
}

/// Shared atomics the component task samples into a [`StatusSnapshot`] on
/// every Heartbeat and every 1s Status tick. Cloned `Arc`s captured by the
/// spawned task at component-start time — the same pattern as every other
/// coordinator component (`psk_reporter.rs`'s `psk_operating_freq`, etc.).
struct WsjtxState {
    operating_frequency_hz: Arc<AtomicU64>,
    active_protocol_mode: Arc<AtomicU8>,
    tx_offset_hold_hz: Arc<AtomicU64>,
    tx_policy: Arc<AtomicU8>,
    ptt_active: Arc<AtomicBool>,
    last_decode_timestamp: Arc<AtomicU64>,
}

impl WsjtxState {
    /// Sample every atomic into a plain [`StatusSnapshot`] `build_status`
    /// can map onto the wire message. Kept as a thin, mostly-mechanical
    /// translation — the actual field-mapping logic (and its tests) lives
    /// in `build_status`.
    fn sample(&self) -> StatusSnapshot {
        let mode = pancetta_config::OperatingMode::from_u8(
            self.active_protocol_mode.load(Ordering::Relaxed),
        );
        let policy = pancetta_core::TxPolicy::from_u8(self.tx_policy.load(Ordering::Relaxed));
        let last_decode_ms = self.last_decode_timestamp.load(Ordering::Relaxed);
        let now_ms = super::now_epoch_ms();
        let decoding =
            last_decode_ms != 0 && now_ms.saturating_sub(last_decode_ms) <= DECODING_WINDOW_MS;

        StatusSnapshot {
            dial_frequency_hz: self.operating_frequency_hz.load(Ordering::Relaxed),
            mode: super::mode_str(mode),
            tx_df_hz: self.tx_offset_hold_hz.load(Ordering::Relaxed) as u32,
            tx_enabled: policy.allows_any_tx(),
            transmitting: self.ptt_active.load(Ordering::Relaxed),
            decoding,
        }
    }
}

/// Plain snapshot of the atomics sampled into a Status message. Kept
/// separate from the atomics themselves so [`build_status`] — the mapping
/// from sampled values to wire fields — is a pure, easily-tested function
/// with no `Arc`/`Ordering` involved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StatusSnapshot {
    /// `operating_frequency_hz` (mod.rs) — the rig dial frequency in Hz.
    pub(crate) dial_frequency_hz: u64,
    /// `mode_str(OperatingMode::from_u8(active_protocol_mode))`.
    pub(crate) mode: &'static str,
    /// `tx_offset_hold_hz` — the operator-held TX audio offset in Hz.
    pub(crate) tx_df_hz: u32,
    /// `TxPolicy::from_u8(tx_policy).allows_any_tx()`.
    pub(crate) tx_enabled: bool,
    /// `ptt_active`.
    pub(crate) transmitting: bool,
    /// `true` when `last_decode_timestamp` is within the last 2s.
    pub(crate) decoding: bool,
}

/// Station identity fields sampled once from config at component start,
/// mirroring the PSKReporter pattern (`psk_reporter.rs`: callsign/grid
/// snapshotted before the config read-lock is dropped).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StationIds {
    /// `config.station.callsign`.
    pub(crate) de_call: String,
    /// `config.station.grid_square`.
    pub(crate) de_grid: String,
}

/// Map a [`StatusSnapshot`] + [`StationIds`] onto the wire `Status` message
/// (protocol notes §4 OUT table — all 21 fields). This is intentionally a
/// pure function: no atomics, no I/O, no coordinator state — so field
/// mapping is exhaustively unit-tested without a socket or running
/// coordinator.
///
/// Fields this task has no live source for yet (DXCall/Report/DXGrid —
/// tracked from an active QSO/decode in later tasks; TxMessage — the text
/// currently being transmitted) are emitted as unset (empty string; the
/// codec always writes the explicit `0x00000000` empty form, never the
/// `0xFFFFFFFF` null sentinel — both decode to `""` per protocol notes §3).
pub(crate) fn build_status(snapshot: &StatusSnapshot, ids: &StationIds) -> OutMsg {
    OutMsg::Status {
        dial_frequency: snapshot.dial_frequency_hz,
        mode: snapshot.mode.to_string(),
        dx_call: String::new(),
        report: String::new(),
        tx_mode: snapshot.mode.to_string(),
        tx_enabled: snapshot.tx_enabled,
        transmitting: snapshot.transmitting,
        decoding: snapshot.decoding,
        rx_df: 0,
        tx_df: snapshot.tx_df_hz,
        de_call: ids.de_call.clone(),
        de_grid: ids.de_grid.clone(),
        dx_grid: String::new(),
        tx_watchdog: false,
        sub_mode: String::new(),
        fast_mode: false,
        special_operation_mode: 0,
        frequency_tolerance: u32::MAX,
        tr_period: u32::MAX,
        configuration_name: "Default".to_string(),
        tx_message: String::new(),
    }
}

/// Minimal decode-identity triple used to match an echoed `Reply(4)`
/// against a retained decode (protocol notes §4 IN Reply: "Message + Time +
/// DeltaFrequency uniquely identify a decode in practice"). Built and
/// retained alongside each ring entry; [`reply_to_call`] matches an inbound
/// Reply against it — an additional hard precondition of the remote-
/// initiation model, not one of its five numbered gates (see the design
/// spec's "Remote QSO initiation" section for the canonical numbering).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredKey {
    /// Same value as the paired `OutMsg::Decode`'s `time_ms`.
    pub(crate) time_ms: u32,
    /// Same value as the paired `OutMsg::Decode`'s `delta_freq`.
    pub(crate) delta_freq: u32,
    /// Same value as the paired `OutMsg::Decode`'s `message`.
    pub(crate) message: String,
    /// The decoder's own authoritative slot parity for this decode
    /// (`DecodedMessage::slot_parity`, set in `ft8.rs` via
    /// `slot_parity_for_receipt` — which subtracts `decode_phase` to recover
    /// the true slot start before taking parity). Carried verbatim from the
    /// `DecodedMessage` this entry was built from; [`reply_to_call`] reads it
    /// straight off a matched entry rather than re-deriving parity from
    /// `time_ms` (which is stamped at decode-completion time, already past
    /// the slot boundary, and would floor into the wrong 15s slot under
    /// decode latency).
    pub(crate) slot_parity: Option<pancetta_core::slot::SlotParity>,
}

/// Map a live FT8 decode onto the wire `Decode` message (protocol notes §4
/// OUT table). Pure — no atomics, no I/O, `mode` passed in explicitly — so
/// field mapping is exhaustively unit-tested without a running component.
///
/// v1 emits `Decode` only when the station's active mode is FT8: `"~"` is
/// the only byte-verified mode glyph (protocol notes §8 flag 7 — FT4's
/// glyph is unconfirmed from non-GPL sources). Returns `None` for any other
/// active mode rather than guessing an unverified glyph; this must not
/// perturb decode behavior outside this fan-out (`mode=FT8` paths stay
/// byte-identical — the gate only decides whether a *wsjtx* message is
/// produced).
pub(crate) fn decode_to_msg(
    d: &pancetta_ft8::DecodedMessage,
    mode: pancetta_config::OperatingMode,
) -> Option<OutMsg> {
    if mode != pancetta_config::OperatingMode::Ft8 {
        return None;
    }
    Some(OutMsg::Decode {
        new: true,
        time_ms: time_ms_since_midnight_utc(d.timestamp),
        snr: d.snr_db.round() as i32,
        delta_time: d.time_offset,
        delta_freq: d.frequency_offset as u32,
        mode: "~".to_string(),
        message: d.text.clone(),
        low_confidence: false,
        off_air: false,
    })
}

/// `QTime` wire value for a decode: milliseconds since midnight UTC of
/// `t` (protocol notes §3 QTime / §4 Decode `Time`).
fn time_ms_since_midnight_utc(t: SystemTime) -> u32 {
    let datetime: DateTime<Utc> = t.into();
    let time = datetime.time();
    time.num_seconds_from_midnight() * 1_000 + time.nanosecond() / 1_000_000
}

/// Extract the `StoredKey` for a just-built `Decode` message. `None` for
/// any other `OutMsg` variant (defensive — only ever called right after
/// [`decode_to_msg`] returns `Some`).
///
/// `slot_parity` is the source `DecodedMessage`'s own authoritative
/// `slot_parity` field — passed in verbatim, never re-derived from `out`'s
/// `time_ms`, so the retained key carries the same parity the decoder itself
/// computed (see [`StoredKey::slot_parity`]).
fn stored_key(
    out: &OutMsg,
    slot_parity: Option<pancetta_core::slot::SlotParity>,
) -> Option<StoredKey> {
    match out {
        OutMsg::Decode {
            time_ms,
            delta_freq,
            message,
            ..
        } => Some(StoredKey {
            time_ms: *time_ms,
            delta_freq: *delta_freq,
            message: message.clone(),
            slot_parity,
        }),
        _ => None,
    }
}

/// Push a new decode into the retention ring, evicting the oldest entry(s)
/// once the ring exceeds `cap`. Pure — the ring and cap are passed in
/// explicitly — so eviction behavior is unit-tested without a running
/// component/socket.
fn push_decode_ring(
    ring: &mut VecDeque<(OutMsg, StoredKey)>,
    entry: (OutMsg, StoredKey),
    cap: usize,
) {
    ring.push_back(entry);
    while ring.len() > cap {
        ring.pop_front();
    }
}

/// Integer-MHz-bucket approximation of "band", sufficient for detecting
/// the kind of frequency change that invalidates a band-activity window
/// (brief: "integer MHz bucket is sufficient" — a full band-plan lookup
/// isn't needed here).
fn band_of(dial_hz: u64) -> u64 {
    dial_hz / 1_000_000
}

/// Decide whether the sampled dial frequency has moved to a new band
/// bucket relative to `last_band`. Returns `Some(new_band)` when a
/// Clear-on-band-change should fire (and `last_band` should be updated to
/// it), `None` when the band is unchanged. Pure — no I/O — so band-change
/// detection is unit-tested without a socket.
fn band_changed(dial_hz: u64, last_band: Option<u64>) -> Option<u64> {
    let band = band_of(dial_hz);
    if last_band == Some(band) {
        None
    } else {
        Some(band)
    }
}

/// PAN-67 review round 5 (folded into PAN-69's broader stale-band-rejection
/// scope): true when a decode's own captured band differs from the band
/// this WSJT-X relay currently believes it's on (`last_band`, kept in sync
/// by the `band_changed` sampler above).
///
/// Decoding is real CPU work with a wall-clock gap after window-close, so a
/// band switch mid-decode can otherwise land an old-band decode in the
/// ring right after `Clear` already invalidated it for the new band —
/// GridTracker/JTAlert would then interpret its offset against the NEW
/// Status dial, and with `allow_tx_initiation` on, a Reply could match that
/// retained entry and send `StartQso` on the wrong band.
///
/// Never treats an unknown value as stale: `captured_dial_hz` of `None` or
/// `0` (dsp.rs's "not yet established" sentinel) and `current_band` of
/// `None` (no rig sample yet) both fall through to "not stale" — there's
/// nothing concrete to compare against, and a live decode should never be
/// silently dropped on a false positive.
fn is_stale_band_decode(captured_dial_hz: Option<u64>, current_band: Option<u64>) -> bool {
    match (captured_dial_hz, current_band) {
        (Some(hz), Some(band)) if hz != 0 => band_of(hz) != band,
        _ => false,
    }
}

/// Rebuild a retained `Decode` for Replay(7): every field verbatim except
/// `New`, forced to `false` (protocol notes §4 IN Replay: "re-emitting
/// every decode still in the band-activity window as Decode(2) with
/// New=false"). `None` for any non-`Decode` `OutMsg` (defensive — the ring
/// only ever holds `Decode` entries).
fn as_replay(out: &OutMsg) -> Option<OutMsg> {
    match out.clone() {
        OutMsg::Decode {
            time_ms,
            snr,
            delta_time,
            delta_freq,
            mode,
            message,
            low_confidence,
            off_air,
            ..
        } => Some(OutMsg::Decode {
            new: false,
            time_ms,
            snr,
            delta_time,
            delta_freq,
            mode,
            message,
            low_confidence,
            off_air,
        }),
        _ => None,
    }
}

/// A remote QSO-initiation intent derived from a `Reply(4)` that matched a
/// retained decode. Pure result of [`reply_to_call`] — no atomics, no I/O — so
/// the match + CQ-extraction logic is exhaustively unit-tested without a
/// running component. The handler turns an allowed CQ/QRZ intent into
/// `QsoMessage::StartQso { remote_origin: true }` (gates 2 and 4 checked in
/// the handler first; gate 5, the TX worker's arm check, applies downstream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallIntent {
    /// The DX station we would call — the CQ/QRZ caller for a CQ, or the
    /// message sender otherwise (only CQ/QRZ intents are ever acted on).
    pub(crate) callsign: String,
    /// TX **audio offset** in Hz within the FT8 passband, clamped to
    /// `[AUDIO_PASSBAND_MIN_HZ, AUDIO_PASSBAND_MAX_HZ]` exactly like the TUI
    /// CallStation path (`app.rs::get_selected_station`). `StartQso.frequency`
    /// is an audio offset, NOT an absolute RF frequency — comfortably within
    /// the modulator's real `MAX_FREQUENCY_DEVIATION` (3100 Hz).
    pub(crate) frequency_hz: u64,
    /// `true` iff the echoed decode is a CQ or QRZ. Only CQ/QRZ intents are
    /// TX-eligible; every other match is refused-with-audit (checked
    /// alongside gate 2, `allow_tx_initiation`, in the handler).
    pub(crate) is_cq: bool,
    /// The decoder's own authoritative slot parity for the matched decode
    /// (`StoredKey::slot_parity`, carried through unchanged — never
    /// re-derived from `time_ms`), matching CallStation's
    /// `station.slot_parity`. The QSO engine TXes on `opposite(dx_parity)` for
    /// this pounce (`desired_tx_parity = dx_parity.map(opposite)` in qso.rs).
    pub(crate) dx_parity: Option<pancetta_core::slot::SlotParity>,
}

/// Matches an inbound `Reply(4)` against the retained decode ring and, on a
/// hit, extracts the CQ/QRZ caller. This is the remote-initiation model's
/// unnumbered hard precondition (see [`CallIntent`] and the design spec's
/// "Remote QSO initiation" section for the five numbered gates) — it must
/// pass regardless of gate state. Pure — ring + reply in, `Option<CallIntent>`
/// out, no atomics/I/O — so it is exhaustively unit-tested without a socket
/// or running coordinator.
///
/// Returns `None` (⇒ the handler hard-returns, no side effect) when:
/// - `reply` is not an `InMsg::Reply` (defensive — the caller only ever
///   passes a Reply), or
/// - no retained decode matches the echoed `(message, time_ms, delta_freq)`
///   triple (protocol notes §4/§5: "Message + Time + DeltaFrequency uniquely
///   identify a decode"; an unmatched Reply is silently dropped), or
/// - the matched decode carries no extractable caller callsign.
///
/// A matched **non-CQ** decode still yields `Some` (with `is_cq = false`) so
/// the handler can refuse-with-audit rather than silently drop — the TX
/// eligibility decision (gate 2, alongside `allow_tx_initiation`) belongs to
/// the handler, not this matcher.
pub(crate) fn reply_to_call(
    reply: &InMsg,
    ring: &VecDeque<(OutMsg, StoredKey)>,
) -> Option<CallIntent> {
    let InMsg::Reply {
        time_ms,
        delta_freq,
        message,
        ..
    } = reply
    else {
        return None;
    };
    // Exact-triple match against a still-retained decode (the unnumbered hard
    // precondition, not one of the five gates). The ring is cleared on band
    // change, so a match also proves same-band relevance.
    let (_out, key) = ring.iter().find(|(_, k)| {
        k.time_ms == *time_ms && k.delta_freq == *delta_freq && &k.message == message
    })?;
    let (callsign, is_cq) = parse_cq_caller(&key.message)?;
    Some(CallIntent {
        // Reply there: the audio offset where the DX was decoded, clamped into
        // the FT8 passband identically to `app.rs::get_selected_station`.
        frequency_hz: (key.delta_freq as u64).clamp(AUDIO_PASSBAND_MIN_HZ, AUDIO_PASSBAND_MAX_HZ),
        // The decoder's own authoritative parity, carried verbatim off the
        // matched `StoredKey` — never re-derived from `time_ms` here. See
        // `StoredKey::slot_parity` for why re-derivation is unsafe (decode
        // latency can floor a raw timestamp into the wrong 15s slot).
        dx_parity: key.slot_parity,
        callsign,
        is_cq,
    })
}

/// Extract the callsign we would call from a decode's text, plus whether it is
/// a CQ/QRZ (the only TX-eligible forms). Reuses `pancetta_ft8::Ft8Message`'s
/// text parser (`from_text`) for callsign extraction — including its CQ
/// directional/DX-modifier handling (`CQ DX <call>`) — rather than a
/// hand-rolled split. `is_cq` is keyed off the first token (`CQ`/`QRZ`);
/// `QRZ` is normalized to `CQ` so the shared parser extracts the caller after
/// any modifier identically. Returns `None` when no caller can be extracted.
fn parse_cq_caller(message: &str) -> Option<(String, bool)> {
    let first = message.split_whitespace().next()?;
    let is_cq = first == "CQ" || first == "QRZ";
    // Normalize a leading QRZ to CQ so `from_text`'s CQ-modifier handling runs.
    let parsed = if first == "QRZ" {
        Ft8Message::from_text(&normalize_qrz(message))
    } else {
        Ft8Message::from_text(message)
    };
    let callsign = parsed.from_callsign?;
    Some((callsign, is_cq))
}

/// Rewrite a leading `QRZ` token to `CQ`, preserving the rest of the message
/// verbatim, so [`parse_cq_caller`] can hand it to the shared CQ parser.
fn normalize_qrz(message: &str) -> String {
    let rest = message
        .split_whitespace()
        .skip(1)
        .collect::<Vec<_>>()
        .join(" ");
    if rest.is_empty() {
        "CQ".to_string()
    } else {
        format!("CQ {rest}")
    }
}

/// Option A of the design spec's arm-gate decision: the shared
/// `remote_tx_arm` local-consent bit is seeded true when EITHER channel's
/// operator consent is on — the station agent's `remote_tx_enabled` OR the
/// WSJT-X-UDP `allow_tx_initiation`. The arm is the operator's
/// channel-independent "I consent to remote TX" last line; each channel still
/// carries its own upstream consent (this returns only the *arm* seed, not a
/// bypass) and source filtering. Pure, so the seeding rule is unit-tested
/// directly. Used at BOTH `set_local_consent` sites (coordinator constructor
/// and `start_station_agent_component`) so the later seed never clobbers the
/// wsjtx contribution.
pub(crate) fn remote_tx_arm_consent(
    station_agent_enabled: bool,
    wsjtx_allow_tx_initiation: bool,
) -> bool {
    station_agent_enabled || wsjtx_allow_tx_initiation
}

/// Julian Day Number for a UTC instant's calendar date (protocol notes §3
/// `QDateTime`: `qint64` JDN + ms-since-midnight + timespec). Pure
/// calendar-date math, independent of time-of-day — `2020-10-30` at any
/// time of day maps to the same JDN `2459153` (protocol notes §3 golden
/// example).
///
/// Implementation: `chrono::NaiveDate::num_days_from_ce()` counts days since
/// `0001-01-01` (day 1, proleptic Gregorian) — the same epoch/convention as
/// the civil "days since the epoch" ordinal used by every standard
/// Gregorian→JDN conversion. `1721425` is the fixed JDN-vs-days-from-CE
/// offset (equivalently: JDN of the Unix epoch `1970-01-01` is the
/// well-known `2440588`, and `NaiveDate(1970-01-01).num_days_from_ce()` is
/// `719163`; `2440588 - 719163 = 1721425`).
fn unix_to_jdn(t: DateTime<Utc>) -> u64 {
    const JDN_MINUS_DAYS_FROM_CE: i64 = 1_721_425;
    (i64::from(t.date_naive().num_days_from_ce()) + JDN_MINUS_DAYS_FROM_CE) as u64
}

/// Map a UTC instant onto the wire `QDateTime` pair (JDN, ms-since-midnight
/// UTC). Reuses [`unix_to_jdn`] for the date half and
/// [`time_ms_since_midnight_utc`] (via the standard `DateTime<Utc> ->
/// SystemTime` conversion) for the time-of-day half, so there is exactly one
/// implementation of each half shared with the `Decode` mapping.
fn qdatetime_of(t: DateTime<Utc>) -> codec::QDateTimeUtc {
    (unix_to_jdn(t), time_ms_since_midnight_utc(t.into()))
}

/// dB signal report as WSJT-X's `ReportSent`/`ReportReceived` fields expect
/// it: signed, zero-padded to 2 digits (matches
/// `AdifProcessor::signal_report_to_rst`'s `{:+03}` — protocol notes §4
/// Status gives the worked example `"-15"`). `None` (report not yet
/// exchanged) maps to the empty string, the same "no live source yet"
/// convention `build_status` uses for other not-yet-known fields.
fn format_signal_report(r: Option<pancetta_qso::SignalReport>) -> String {
    r.map(|v| format!("{v:+03}")).unwrap_or_default()
}

/// Map a completed QSO's metadata onto the `(QSOLogged, LoggedADIF)` pair
/// (protocol notes §4 OUT — types 5 and 12), pairing an already-rendered
/// ADIF record with pancetta's own header. Pure — no I/O, no socket — so
/// field mapping is exhaustively unit-tested without a running component;
/// `adif_record` is produced by the caller via the exact same
/// `AdifProcessor::qso_to_adif` → `generate_record` path the source-of-truth
/// writer and the per-QSO upload subscriber use (`qso.rs`), so there is no
/// second ADIF-formatting implementation.
///
/// Fields this task has no live source for (Comments, Name, ExchangeSent/
/// Received, ADIFPropagationMode) are emitted empty — protocol notes §4
/// marks Comments/ADIFPropagationMode as informational-only for consumers
/// and ExchangeSent/Received as contest-specific; `build_status` established
/// the same "no live source yet ⇒ empty string" convention for Status.
pub(crate) fn qso_to_msgs(
    m: &pancetta_qso::QsoMetadata,
    station_power_watts: u32,
    adif_record: String,
) -> (OutMsg, OutMsg) {
    let date_time_off = qdatetime_of(m.end_time.unwrap_or(m.start_time));
    let date_time_on = qdatetime_of(m.start_time);

    let qso_logged = OutMsg::QsoLogged {
        date_time_off,
        dx_call: m.their_callsign.clone().unwrap_or_default(),
        dx_grid: m.grids.theirs.clone().unwrap_or_default(),
        // brief: TxFrequency = `frequency as u64`. `metadata.frequency` is
        // already the actual RF TX frequency (dial + audio offset) — see
        // `QsoManager::set_dial_frequency_source` in qso.rs — so no further
        // dial+offset arithmetic belongs here.
        tx_frequency: m.frequency as u64,
        mode: m.mode.clone(),
        report_sent: format_signal_report(m.reports.sent),
        report_received: format_signal_report(m.reports.received),
        tx_power: station_power_watts.to_string(),
        comments: String::new(),
        name: String::new(),
        date_time_on,
        operator_call: m.our_callsign.clone(),
        my_call: m.our_callsign.clone(),
        my_grid: m.grids.ours.clone().unwrap_or_default(),
        exchange_sent: String::new(),
        exchange_received: String::new(),
        adif_propagation_mode: String::new(),
    };

    let logged_adif = OutMsg::LoggedAdif {
        adif: format!("<adif_ver:5>3.1.0\n<programid:8>pancetta\n<EOH>\n{adif_record}"),
    };

    (qso_logged, logged_adif)
}

/// Multicast socket options to apply when the destination is a multicast
/// group. `None` when `dest` is unicast — no socket options are needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MulticastOptions {
    /// `IP_MULTICAST_TTL` to set (GridTracker uses 3; config default 3).
    pub(crate) ttl: u32,
    /// `IP_MULTICAST_IF` to set, when `multicast_interface` is configured.
    /// `None` leaves the OS default outgoing interface (WSJT-X's classic
    /// loopback-only cross-host failure mode — protocol notes §1).
    pub(crate) interface: Option<Ipv4Addr>,
}

/// Detect whether `dest` is a multicast destination and, if so, derive the
/// socket options `open_socket` must apply (protocol notes §1/§6: TTL ≥ 1,
/// `IP_MULTICAST_IF` set to the LAN NIC for cross-host reach). Pure and
/// side-effect-free — no socket is touched — so this is unit-tested without
/// an actual send.
pub(crate) fn multicast_options(
    dest: &SocketAddr,
    cfg: &WsjtxUdpConfig,
) -> Option<MulticastOptions> {
    let SocketAddr::V4(v4) = dest else {
        // IPv6 multicast destinations are out of scope for v1 (design spec
        // non-goals: "IPv6"); treat as non-multicast (no socket options).
        return None;
    };
    if !v4.ip().is_multicast() {
        return None;
    }
    let interface = if cfg.multicast_interface.is_empty() {
        None
    } else {
        cfg.multicast_interface.parse::<Ipv4Addr>().ok()
    };
    Some(MulticastOptions {
        ttl: cfg.multicast_ttl,
        interface,
    })
}

/// Whether `dest` is a multicast destination, for gating purposes. Deliberately
/// broader than [`multicast_options`]'s IPv4-only scope (that function derives
/// *socket options*, which v1 only ever applies to IPv4 — design spec
/// non-goals: "IPv6"): here an IPv6 multicast destination must still be
/// treated as multicast so [`request_allowed`] routes it through the
/// allowlist check rather than falling through to the (never-true) unicast
/// host-equality branch. Fail-closed: when in doubt, this leans toward
/// "multicast" (the stricter, allowlist-gated path), never toward "unicast"
/// (the looser, single-peer path).
fn is_multicast_dest(dest: &SocketAddr) -> bool {
    match dest {
        SocketAddr::V4(v4) => v4.ip().is_multicast(),
        SocketAddr::V6(v6) => v6.ip().is_multicast(),
    }
}

/// Fail-closed source gate for every inbound WSJT-X UDP request
/// (Reply/HaltTx/Replay/...) — design spec's gates 1 ("master switch") and 3
/// ("source filtering"), ANDed. Pure and side-effect-free (no I/O, no
/// atomics) so it is exhaustively unit-tested in isolation; the inbound-poll
/// arm of `start_wsjtx_udp_component`'s select loop is the only caller and
/// checks this before dispatching anything.
///
/// - `accept_udp_requests == false` ⇒ always refuse (repo invariant: the
///   armed-TX gate — and every gate feeding it — fails CLOSED; this is gate
///   1, the master switch, and is checked first so every other branch below
///   is unreachable while it's off).
/// - `dest` unicast ⇒ allow only when `src` is exactly the configured
///   destination host (the only peer that could plausibly be the companion
///   app WSJT-X-role datagrams are being sent to).
/// - `dest` multicast ⇒ allow only when `src` is in `allowed_request_hosts`.
///   An empty allowlist refuses every request (never defaults to allowing
///   "everyone on the multicast group") — the operator must opt a host in
///   explicitly.
pub(crate) fn request_allowed(cfg: &WsjtxUdpConfig, src: IpAddr, dest: &SocketAddr) -> bool {
    if !cfg.accept_udp_requests {
        return false;
    }
    if is_multicast_dest(dest) {
        cfg.allowed_request_hosts
            .iter()
            .any(|host| host.parse::<IpAddr>().map(|ip| ip == src).unwrap_or(false))
    } else {
        src == dest.ip()
    }
}

/// Short label for an inbound message type, for diagnostic/debug text.
/// `Other` includes the raw numeric type id since there's no name for it.
fn msg_type_label(msg: &InMsg) -> String {
    match msg {
        InMsg::Heartbeat { .. } => "Heartbeat".to_string(),
        InMsg::Clear { .. } => "Clear".to_string(),
        InMsg::Reply { .. } => "Reply".to_string(),
        InMsg::Close => "Close".to_string(),
        InMsg::Replay => "Replay".to_string(),
        InMsg::HaltTx { .. } => "HaltTx".to_string(),
        InMsg::Other(t) => format!("Other({t})"),
    }
}

/// Send a `DiagnosticEvent` to the TUI's retained Diagnostics overlay
/// (Shift+D) with `target: "remote.wsjtx"` — the audit trail the design spec
/// requires for every honored *and* every refused inbound request. Mirrors
/// `tx.rs`'s `emit_diagnostic` (same message shape, same best-effort "never
/// blocks/fails the caller" contract); this component has its own copy
/// because it's the only one that needs `target: "remote.wsjtx"` and there's
/// no shared coordinator-wide helper to call instead.
async fn emit_diagnostic(message_bus: &MessageBus, level: DiagnosticLevel, text: String) {
    let msg = ComponentMessage::new(
        ComponentId::WsjtxUdp,
        ComponentId::Tui,
        MessageType::DiagnosticEvent {
            target: "remote.wsjtx",
            level,
            text,
            qso_id: None,
            callsign: None,
        },
        Instant::now(),
    );
    if let Err(e) = message_bus.send_message(msg).await {
        debug!("WSJT-X UDP: DiagnosticEvent relay failed (no TUI?): {e}");
    }
}

/// Open the WSJT-X-role UDP socket: bind an ephemeral local port, apply
/// multicast options when the configured destination is a multicast group,
/// and set non-blocking so the tokio-wrapped socket can be polled in the
/// component's select loop (protocol notes §1: one socket, both send and
/// `recv_from`, no `connect()` — multicast destinations aren't reply
/// sources).
pub(crate) fn open_socket(cfg: &WsjtxUdpConfig) -> Result<tokio::net::UdpSocket> {
    let dest: SocketAddr = cfg
        .destination
        .parse()
        .with_context(|| format!("invalid wsjtx_udp destination: {}", cfg.destination))?;

    // `set_multicast_if_v4` isn't exposed by `std::net::UdpSocket` — only by
    // socket2's `Socket` wrapper — so the socket is built with socket2 and
    // converted to `std::net::UdpSocket` (then to tokio's) once configured.
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .context("failed to create wsjtx_udp UDP socket")?;
    let bind_addr: SocketAddr = "0.0.0.0:0".parse().expect("valid bind literal");
    socket
        .bind(&bind_addr.into())
        .context("failed to bind wsjtx_udp UDP socket")?;

    if let Some(opts) = multicast_options(&dest, cfg) {
        socket
            .set_multicast_ttl_v4(opts.ttl)
            .context("failed to set wsjtx_udp multicast TTL")?;
        if let Some(iface) = opts.interface {
            socket
                .set_multicast_if_v4(&iface)
                .context("failed to set wsjtx_udp multicast interface")?;
        }
    }

    socket
        .set_nonblocking(true)
        .context("failed to set wsjtx_udp socket non-blocking")?;

    let std_socket: std::net::UdpSocket = socket.into();
    tokio::net::UdpSocket::from_std(std_socket).context("failed to wrap wsjtx_udp socket for tokio")
}

#[cfg(test)]
mod socket_tests {
    use super::*;

    #[test]
    fn multicast_destination_detected_and_options_derived() {
        let dest: SocketAddr = "224.0.0.73:2237".parse().unwrap();
        let cfg = WsjtxUdpConfig {
            multicast_interface: "192.168.1.5".to_string(),
            multicast_ttl: 3,
            ..WsjtxUdpConfig::default()
        };
        let opts = multicast_options(&dest, &cfg).expect("224.0.0.73 is multicast");
        assert_eq!(opts.ttl, 3);
        assert_eq!(opts.interface, Some(Ipv4Addr::new(192, 168, 1, 5)));
    }

    #[test]
    fn multicast_with_empty_interface_leaves_interface_unset() {
        let dest: SocketAddr = "239.255.0.1:2237".parse().unwrap();
        let cfg = WsjtxUdpConfig::default(); // multicast_interface = ""
        let opts = multicast_options(&dest, &cfg).expect("239.255.0.1 is multicast");
        assert_eq!(opts.interface, None);
        assert_eq!(opts.ttl, cfg.multicast_ttl);
    }

    #[test]
    fn unicast_destination_has_no_multicast_options() {
        let dest: SocketAddr = "127.0.0.1:2237".parse().unwrap();
        let cfg = WsjtxUdpConfig::default();
        assert_eq!(multicast_options(&dest, &cfg), None);

        let dest: SocketAddr = "192.168.1.20:2237".parse().unwrap();
        assert_eq!(multicast_options(&dest, &cfg), None);
    }

    #[tokio::test]
    async fn open_socket_binds_ephemeral_port_for_unicast_destination() {
        let cfg = WsjtxUdpConfig {
            destination: "127.0.0.1:2237".to_string(),
            ..WsjtxUdpConfig::default()
        };
        let socket = open_socket(&cfg).expect("unicast open_socket should succeed");
        // Ephemeral bind: local port is nonzero and not the destination port
        // (we never bind 2237 ourselves — protocol notes §1).
        assert_ne!(socket.local_addr().unwrap().port(), 0);
    }

    #[tokio::test]
    async fn open_socket_applies_multicast_options_without_sending() {
        let cfg = WsjtxUdpConfig {
            destination: "224.0.0.73:2237".to_string(),
            multicast_ttl: 3,
            ..WsjtxUdpConfig::default()
        };
        // No actual datagram is sent — this only exercises socket-option
        // setup (set_multicast_ttl_v4 / set_nonblocking) on a real bound
        // socket, confirming it doesn't error for a multicast destination.
        let socket = open_socket(&cfg).expect("multicast open_socket should succeed");
        assert_ne!(socket.local_addr().unwrap().port(), 0);
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    #[test]
    fn request_gate_is_fail_closed() {
        let mut cfg = WsjtxUdpConfig::default();
        let dest: SocketAddr = "192.168.1.20:2237".parse().unwrap();
        let peer: IpAddr = "192.168.1.20".parse().unwrap();
        let stranger: IpAddr = "192.168.1.99".parse().unwrap();
        assert!(!request_allowed(&cfg, peer, &dest)); // master off
        cfg.accept_udp_requests = true;
        assert!(request_allowed(&cfg, peer, &dest)); // unicast: peer == dest host
        assert!(!request_allowed(&cfg, stranger, &dest));
        let mcast: SocketAddr = "224.0.0.73:2237".parse().unwrap();
        assert!(!request_allowed(&cfg, peer, &mcast)); // multicast + empty allowlist ⇒ refuse
        cfg.allowed_request_hosts = vec!["192.168.1.20".into()];
        assert!(request_allowed(&cfg, peer, &mcast));
    }

    #[test]
    fn unicast_stranger_is_refused_even_with_master_on() {
        let cfg = WsjtxUdpConfig {
            accept_udp_requests: true,
            ..Default::default()
        };
        let dest: SocketAddr = "192.168.1.20:2237".parse().unwrap();
        let stranger: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(!request_allowed(&cfg, stranger, &dest));
    }

    #[test]
    fn multicast_allowlist_only_admits_listed_hosts() {
        let mut cfg = WsjtxUdpConfig {
            accept_udp_requests: true,
            allowed_request_hosts: vec!["192.168.1.20".into()],
            ..WsjtxUdpConfig::default()
        };
        let mcast: SocketAddr = "224.0.0.73:2237".parse().unwrap();
        let listed: IpAddr = "192.168.1.20".parse().unwrap();
        let unlisted: IpAddr = "192.168.1.21".parse().unwrap();
        assert!(request_allowed(&cfg, listed, &mcast));
        assert!(!request_allowed(&cfg, unlisted, &mcast));
        // A malformed allowlist entry must not panic or accidentally match.
        cfg.allowed_request_hosts = vec!["not-an-ip".into()];
        assert!(!request_allowed(&cfg, listed, &mcast));
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use pancetta_core::TxPolicy;

    fn snapshot(tx_enabled: bool) -> StatusSnapshot {
        StatusSnapshot {
            dial_frequency_hz: 14_074_000,
            mode: "FT8",
            tx_df_hz: 1500,
            tx_enabled,
            transmitting: false,
            decoding: true,
        }
    }

    fn ids() -> StationIds {
        StationIds {
            de_call: "K5ARH".to_string(),
            de_grid: "EM12".to_string(),
        }
    }

    #[test]
    fn maps_dial_mode_txdf_de_call_de_grid() {
        let status = build_status(&snapshot(true), &ids());
        let OutMsg::Status {
            dial_frequency,
            mode,
            tx_df,
            de_call,
            de_grid,
            ..
        } = status
        else {
            panic!("expected OutMsg::Status");
        };
        assert_eq!(dial_frequency, 14_074_000);
        assert_eq!(mode, "FT8");
        assert_eq!(tx_df, 1500);
        assert_eq!(de_call, "K5ARH");
        assert_eq!(de_grid, "EM12");
    }

    #[test]
    fn tx_enabled_true_for_full_and_respond_only() {
        for policy in [TxPolicy::Full, TxPolicy::RespondOnly] {
            let s = StatusSnapshot {
                tx_enabled: policy.allows_any_tx(),
                ..snapshot(false)
            };
            let OutMsg::Status { tx_enabled, .. } = build_status(&s, &ids()) else {
                panic!("expected OutMsg::Status");
            };
            assert!(tx_enabled, "{policy:?} should map to TxEnabled=true");
        }
    }

    #[test]
    fn tx_enabled_false_for_disabled_policy() {
        let s = StatusSnapshot {
            tx_enabled: TxPolicy::Disabled.allows_any_tx(),
            ..snapshot(true)
        };
        let OutMsg::Status { tx_enabled, .. } = build_status(&s, &ids()) else {
            panic!("expected OutMsg::Status");
        };
        assert!(!tx_enabled);
    }

    #[test]
    fn unset_strings_are_null_i_e_empty() {
        let status = build_status(&snapshot(true), &ids());
        let OutMsg::Status {
            dx_call,
            report,
            dx_grid,
            sub_mode,
            tx_message,
            ..
        } = status
        else {
            panic!("expected OutMsg::Status");
        };
        assert_eq!(dx_call, "");
        assert_eq!(report, "");
        assert_eq!(dx_grid, "");
        assert_eq!(sub_mode, "");
        assert_eq!(tx_message, "");
    }

    #[test]
    fn frequency_tolerance_and_tr_period_are_sentinel_unset() {
        let status = build_status(&snapshot(true), &ids());
        let OutMsg::Status {
            frequency_tolerance,
            tr_period,
            ..
        } = status
        else {
            panic!("expected OutMsg::Status");
        };
        assert_eq!(frequency_tolerance, u32::MAX);
        assert_eq!(tr_period, u32::MAX);
    }

    #[test]
    fn special_operation_mode_is_always_zero() {
        let status = build_status(&snapshot(true), &ids());
        let OutMsg::Status {
            special_operation_mode,
            ..
        } = status
        else {
            panic!("expected OutMsg::Status");
        };
        assert_eq!(special_operation_mode, 0);
    }
}

#[cfg(test)]
mod decode_tests {
    use super::*;
    use chrono::TimeZone;
    use pancetta_config::OperatingMode;
    use pancetta_ft8::{DecodedMessage, Ft8Message};

    /// Build a `DecodedMessage` fixture with explicit `text`/`timestamp`
    /// overrides (the constructor derives `text` from an empty `Ft8Message`
    /// and stamps `timestamp = SystemTime::now()`, neither of which is
    /// deterministic enough for these tests).
    fn decoded(
        snr_db: f32,
        frequency_offset: f64,
        time_offset: f64,
        text: &str,
        timestamp: SystemTime,
    ) -> DecodedMessage {
        let mut d = DecodedMessage::new(
            Ft8Message::default(),
            snr_db,
            1.0,
            frequency_offset,
            time_offset,
        );
        d.text = text.to_string();
        d.timestamp = timestamp;
        d
    }

    fn known_timestamp() -> SystemTime {
        // 2026-07-15 12:34:56.789 UTC — mid-day, non-trivial ms remainder,
        // so both the seconds and sub-second parts of `time_ms_since_
        // midnight_utc` are exercised.
        Utc.with_ymd_and_hms(2026, 7, 15, 12, 34, 56)
            .unwrap()
            .checked_add_signed(chrono::Duration::milliseconds(789))
            .unwrap()
            .into()
    }

    #[test]
    fn ft8_mode_emits_decode_with_tilde_glyph_and_text() {
        let d = decoded(-5.0, 1302.0, 0.2, "CQ K1ABC FN42", known_timestamp());
        let out = decode_to_msg(&d, OperatingMode::Ft8).expect("FT8 mode must emit a Decode");
        let OutMsg::Decode {
            new,
            mode,
            message,
            delta_time,
            low_confidence,
            off_air,
            ..
        } = out
        else {
            panic!("expected OutMsg::Decode");
        };
        assert!(new, "live decodes must be New=true");
        assert_eq!(
            mode, "~",
            "protocol notes §8: only \"~\" is byte-verified for FT8"
        );
        assert_eq!(message, "CQ K1ABC FN42");
        assert_eq!(delta_time, 0.2);
        assert!(!low_confidence);
        assert!(!off_air);
    }

    #[test]
    fn non_ft8_active_mode_emits_nothing() {
        let d = decoded(-5.0, 1302.0, 0.2, "CQ K1ABC FN42", known_timestamp());
        // v1 only has a byte-verified glyph for FT8 (protocol notes §8 flag
        // 7) — FT4/FT2 must not guess one.
        assert!(decode_to_msg(&d, OperatingMode::Ft4).is_none());
        assert!(decode_to_msg(&d, OperatingMode::Ft2).is_none());
    }

    #[test]
    fn snr_rounds_to_nearest_i32() {
        let cases: [(f32, i32); 4] = [(-4.6, -5), (13.4, 13), (2.5, 3), (-2.5, -3)];
        for (snr_db, expected) in cases {
            let d = decoded(snr_db, 0.0, 0.0, "x", known_timestamp());
            let OutMsg::Decode { snr, .. } = decode_to_msg(&d, OperatingMode::Ft8).unwrap() else {
                panic!("expected OutMsg::Decode");
            };
            assert_eq!(snr, expected, "snr_db {snr_db} should round to {expected}");
        }
    }

    #[test]
    fn time_ms_is_milliseconds_since_utc_midnight() {
        let d = decoded(0.0, 0.0, 0.0, "x", known_timestamp());
        let OutMsg::Decode { time_ms, .. } = decode_to_msg(&d, OperatingMode::Ft8).unwrap() else {
            panic!("expected OutMsg::Decode");
        };
        // 12:34:56.789 UTC -> ((12*3600 + 34*60 + 56) * 1000) + 789
        assert_eq!(time_ms, 45_296_789);
    }

    #[test]
    fn delta_freq_truncates_frequency_offset_to_u32() {
        let d = decoded(0.0, 1302.9, 0.0, "x", known_timestamp());
        let OutMsg::Decode { delta_freq, .. } = decode_to_msg(&d, OperatingMode::Ft8).unwrap()
        else {
            panic!("expected OutMsg::Decode");
        };
        assert_eq!(delta_freq, 1302);
    }

    #[test]
    fn stored_key_matches_the_decode_it_was_built_from() {
        let mut d = decoded(-5.0, 1302.0, 0.2, "CQ K1ABC FN42", known_timestamp());
        d.slot_parity = Some(pancetta_core::slot::SlotParity::Odd);
        let out = decode_to_msg(&d, OperatingMode::Ft8).unwrap();
        let key = stored_key(&out, d.slot_parity).expect("Decode variant must yield a StoredKey");
        let OutMsg::Decode {
            time_ms,
            delta_freq,
            message,
            ..
        } = &out
        else {
            unreachable!()
        };
        assert_eq!(key.time_ms, *time_ms);
        assert_eq!(key.delta_freq, *delta_freq);
        assert_eq!(&key.message, message);
        // The decoder's own slot_parity is carried through verbatim, not
        // re-derived from time_ms.
        assert_eq!(key.slot_parity, d.slot_parity);
    }

    #[test]
    fn stored_key_carries_none_slot_parity_when_the_decoded_message_has_none() {
        let d = decoded(-5.0, 1302.0, 0.2, "CQ K1ABC FN42", known_timestamp());
        assert_eq!(d.slot_parity, None);
        let out = decode_to_msg(&d, OperatingMode::Ft8).unwrap();
        let key = stored_key(&out, d.slot_parity).expect("Decode variant must yield a StoredKey");
        assert_eq!(key.slot_parity, None);
    }

    #[test]
    fn stored_key_is_none_for_non_decode_messages() {
        assert!(stored_key(&OutMsg::Clear, None).is_none());
    }

    #[test]
    fn slot_parity_flows_unchanged_from_decoded_message_through_ring_to_reply_to_call() {
        // End-to-end: a DecodedMessage carrying the decoder's authoritative
        // slot_parity (Odd here, deliberately NOT matching what a raw
        // time_ms re-derivation would produce for this timestamp) is mapped
        // to a wire Decode, keyed into the retention ring, and a matching
        // Reply(4) must yield exactly that stored parity — never a
        // re-derivation from time_ms.
        let mut d = decoded(-5.0, 1302.0, 0.2, "CQ K1ABC FN42", known_timestamp());
        d.slot_parity = Some(pancetta_core::slot::SlotParity::Odd);
        let out = decode_to_msg(&d, OperatingMode::Ft8).unwrap();
        let key = stored_key(&out, d.slot_parity).expect("Decode variant must yield a StoredKey");
        let mut ring: VecDeque<(OutMsg, StoredKey)> = VecDeque::new();
        let (time_ms, delta_freq, message) = (key.time_ms, key.delta_freq, key.message.clone());
        push_decode_ring(&mut ring, (out, key), DECODE_RING_CAPACITY);

        let reply = InMsg::Reply {
            time_ms,
            snr: 0,
            delta_time: 0.0,
            delta_freq,
            mode: "~".to_string(),
            message,
            low_confidence: false,
            modifiers: 0,
        };
        let intent = reply_to_call(&reply, &ring).expect("Reply must match the retained decode");
        assert_eq!(intent.dx_parity, Some(pancetta_core::slot::SlotParity::Odd));
    }

    /// Same regression as `app.rs::get_selected_station` (2026-07-25 on-air
    /// bug: a station decoded at 2846.9 Hz got silently truncated to 2500 Hz,
    /// which then broke the QSO engine's frequency-relevance gate). This
    /// path shares the identical `.clamp(200, 2500)` call (per its own doc
    /// comment, "exactly like the TUI CallStation path") so it carries the
    /// same bug. The real modulator ceiling is
    /// `pancetta_ft8::modulator::MAX_FREQUENCY_DEVIATION` = 3100 Hz, not
    /// 2500 — the doc comments citing "MAX_FREQUENCY_DEVIATION = 2500 Hz"
    /// were simply wrong.
    #[test]
    fn reply_to_call_high_passband_frequency_not_truncated() {
        let mut ring: VecDeque<(OutMsg, StoredKey)> = VecDeque::new();
        let key = StoredKey {
            time_ms: 12_345,
            delta_freq: 2847,
            message: "CQ OM5NU JN98".to_string(),
            slot_parity: None,
        };
        push_decode_ring(
            &mut ring,
            (OutMsg::Clear, key.clone()),
            DECODE_RING_CAPACITY,
        );

        let reply = InMsg::Reply {
            time_ms: key.time_ms,
            snr: 0,
            delta_time: 0.0,
            delta_freq: key.delta_freq,
            mode: "~".to_string(),
            message: key.message.clone(),
            low_confidence: false,
            modifiers: 0,
        };
        let intent = reply_to_call(&reply, &ring).expect("Reply must match the retained decode");
        assert_eq!(
            intent.frequency_hz, 2847,
            "a real 2846.9 Hz (rounded 2847) decode must not be truncated down to 2500 Hz"
        );
    }

    #[test]
    fn push_decode_ring_evicts_oldest_past_capacity() {
        let mut ring: VecDeque<(OutMsg, StoredKey)> = VecDeque::new();
        for i in 0..5u32 {
            let entry = (
                OutMsg::Clear,
                StoredKey {
                    time_ms: i,
                    delta_freq: 0,
                    message: i.to_string(),
                    slot_parity: None,
                },
            );
            push_decode_ring(&mut ring, entry, 3);
        }
        assert_eq!(ring.len(), 3);
        let kept: Vec<u32> = ring.iter().map(|(_, k)| k.time_ms).collect();
        assert_eq!(
            kept,
            vec![2, 3, 4],
            "oldest entries evicted, insertion order preserved"
        );
    }

    #[test]
    fn push_decode_ring_caps_at_the_real_500_entry_capacity() {
        let mut ring: VecDeque<(OutMsg, StoredKey)> = VecDeque::new();
        for i in 0..(DECODE_RING_CAPACITY as u32 + 10) {
            let entry = (
                OutMsg::Clear,
                StoredKey {
                    time_ms: i,
                    delta_freq: 0,
                    message: String::new(),
                    slot_parity: None,
                },
            );
            push_decode_ring(&mut ring, entry, DECODE_RING_CAPACITY);
        }
        assert_eq!(ring.len(), DECODE_RING_CAPACITY);
    }

    #[test]
    fn band_changed_none_when_same_mhz_bucket() {
        // 14.074 MHz and 14.078 MHz are both bucket 14.
        assert_eq!(band_changed(14_078_000, Some(14)), None);
    }

    #[test]
    fn band_changed_some_when_mhz_bucket_differs() {
        // 14.074 MHz (bucket 14) -> 7.074 MHz (bucket 7).
        assert_eq!(band_changed(7_074_000, Some(14)), Some(7));
    }

    #[test]
    fn band_changed_some_on_first_sample_with_no_prior_band() {
        assert_eq!(band_changed(14_074_000, None), Some(14));
    }

    /// PAN-67 review round 5 (folded into PAN-69): a decode captured on the
    /// OLD band before a switch must not be treated as belonging to the
    /// current band just because it arrives after the switch.
    #[test]
    fn is_stale_band_decode_true_when_captured_band_differs_from_current() {
        assert!(is_stale_band_decode(Some(7_074_000), Some(14)));
    }

    #[test]
    fn is_stale_band_decode_false_when_captured_band_matches_current() {
        assert!(!is_stale_band_decode(Some(14_074_000), Some(14)));
    }

    #[test]
    fn is_stale_band_decode_false_when_captured_is_unknown() {
        assert!(!is_stale_band_decode(None, Some(14)));
        assert!(!is_stale_band_decode(Some(0), Some(14)));
    }

    #[test]
    fn is_stale_band_decode_false_when_current_band_is_unknown() {
        assert!(!is_stale_band_decode(Some(7_074_000), None));
    }

    #[test]
    fn as_replay_forces_new_false_and_keeps_other_fields() {
        let d = decoded(-5.0, 1302.0, 0.2, "CQ K1ABC FN42", known_timestamp());
        let live = decode_to_msg(&d, OperatingMode::Ft8).unwrap();
        let replay = as_replay(&live).expect("Decode variant must replay");
        let OutMsg::Decode {
            new: live_new,
            message: live_message,
            ..
        } = &live
        else {
            unreachable!()
        };
        let OutMsg::Decode {
            new: replay_new,
            message: replay_message,
            ..
        } = &replay
        else {
            unreachable!()
        };
        assert!(*live_new);
        assert!(!*replay_new, "Replay(7) responses must carry New=false");
        assert_eq!(live_message, replay_message);
    }

    #[test]
    fn as_replay_is_none_for_non_decode_messages() {
        assert!(as_replay(&OutMsg::Clear).is_none());
    }
}

#[cfg(test)]
mod qso_logged_tests {
    use super::*;
    use chrono::TimeZone;
    use pancetta_qso::{GridSquares, QsoMetadata, SignalReports};

    /// Fixture matching the pattern used elsewhere in the coordinator
    /// (`qso.rs`'s `metadata_with_call`): every `QsoMetadata` field named
    /// explicitly so new fields fail to compile here instead of silently
    /// defaulting.
    fn metadata() -> QsoMetadata {
        let start = Utc.with_ymd_and_hms(2020, 10, 30, 12, 34, 40).unwrap();
        let end = Utc
            .with_ymd_and_hms(2020, 10, 30, 12, 34, 56)
            .unwrap()
            .checked_add_signed(chrono::Duration::milliseconds(789))
            .unwrap();
        QsoMetadata {
            qso_id: pancetta_qso::QsoId::new_v4(),
            our_callsign: "K5ARH".to_string(),
            their_callsign: Some("K1ABC".to_string()),
            frequency: 14_074_200.4,
            completed_rf_frequency_hz: None,
            mode: "FT8".to_string(),
            start_time: start,
            end_time: Some(end),
            reports: SignalReports {
                sent: Some(-10),
                received: Some(-8),
            },
            grids: GridSquares {
                ours: Some("EM12".to_string()),
                theirs: Some("FN42".to_string()),
            },
            contest_info: None,
            tags: std::collections::HashMap::new(),
            notes: None,
            tx_parity: None,
            initiated_by: Default::default(),
            role: Default::default(),
            call_count: 0,
            first_call_at: None,
            last_call_at: None,
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            hound: false,
            partner_freq: None,
            pending_freq_drift: None,
            hound_qsyed: false,
            remote_origin: false,
            tx_parity_provisional: false,
        }
    }

    #[test]
    fn unix_to_jdn_matches_the_protocol_notes_golden_date() {
        // Protocol notes §3: "Example: JDN 2459153 = 2020-10-30".
        let d = Utc.with_ymd_and_hms(2020, 10, 30, 0, 0, 0).unwrap();
        assert_eq!(unix_to_jdn(d), 2_459_153);
    }

    #[test]
    fn unix_to_jdn_is_stable_across_times_of_day() {
        // The JDN is a pure calendar-date value; time-of-day must not shift it.
        let midnight = Utc.with_ymd_and_hms(2020, 10, 30, 0, 0, 0).unwrap();
        let noon = Utc.with_ymd_and_hms(2020, 10, 30, 12, 0, 0).unwrap();
        let just_before_midnight = Utc.with_ymd_and_hms(2020, 10, 30, 23, 59, 59).unwrap();
        assert_eq!(unix_to_jdn(midnight), 2_459_153);
        assert_eq!(unix_to_jdn(noon), 2_459_153);
        assert_eq!(unix_to_jdn(just_before_midnight), 2_459_153);
    }

    #[test]
    fn qso_to_msgs_maps_date_time_on_and_off() {
        let m = metadata();
        let (logged, _adif) = qso_to_msgs(&m, 100, String::new());
        let OutMsg::QsoLogged {
            date_time_on,
            date_time_off,
            ..
        } = logged
        else {
            panic!("expected OutMsg::QsoLogged");
        };
        // start_time = 2020-10-30 12:34:40 UTC.
        assert_eq!(date_time_on, (2_459_153, 45_280_000));
        // end_time = 2020-10-30 12:34:56.789 UTC.
        assert_eq!(date_time_off, (2_459_153, 45_296_789));
    }

    #[test]
    fn qso_to_msgs_falls_back_to_start_time_when_end_time_is_none() {
        let mut m = metadata();
        m.end_time = None;
        let (logged, _adif) = qso_to_msgs(&m, 100, String::new());
        let OutMsg::QsoLogged {
            date_time_on,
            date_time_off,
            ..
        } = logged
        else {
            panic!("expected OutMsg::QsoLogged");
        };
        assert_eq!(date_time_off, date_time_on);
    }

    #[test]
    fn qso_to_msgs_maps_frequency_calls_grids_mode() {
        let m = metadata();
        let (logged, _adif) = qso_to_msgs(&m, 100, String::new());
        let OutMsg::QsoLogged {
            dx_call,
            dx_grid,
            tx_frequency,
            mode,
            operator_call,
            my_call,
            my_grid,
            ..
        } = logged
        else {
            panic!("expected OutMsg::QsoLogged");
        };
        assert_eq!(dx_call, "K1ABC");
        assert_eq!(dx_grid, "FN42");
        // brief: TxFrequency = `frequency as u64` (metadata.frequency is
        // already the actual RF freq — dial + audio offset — per the
        // qso.rs `set_dial_frequency_source` wiring, no further math here).
        assert_eq!(tx_frequency, 14_074_200);
        assert_eq!(mode, "FT8");
        assert_eq!(operator_call, "K5ARH");
        assert_eq!(my_call, "K5ARH");
        assert_eq!(my_grid, "EM12");
    }

    #[test]
    fn qso_to_msgs_maps_reports_and_tx_power() {
        let m = metadata();
        let (logged, _adif) = qso_to_msgs(&m, 100, String::new());
        let OutMsg::QsoLogged {
            report_sent,
            report_received,
            tx_power,
            ..
        } = logged
        else {
            panic!("expected OutMsg::QsoLogged");
        };
        assert_eq!(report_sent, "-10");
        assert_eq!(report_received, "-08");
        assert_eq!(tx_power, "100");
    }

    #[test]
    fn qso_to_msgs_maps_missing_reports_and_grids_to_empty_strings() {
        let mut m = metadata();
        m.reports = SignalReports {
            sent: None,
            received: None,
        };
        m.grids = GridSquares {
            ours: None,
            theirs: None,
        };
        m.their_callsign = None;
        let (logged, _adif) = qso_to_msgs(&m, 0, String::new());
        let OutMsg::QsoLogged {
            dx_call,
            dx_grid,
            my_grid,
            report_sent,
            report_received,
            tx_power,
            ..
        } = logged
        else {
            panic!("expected OutMsg::QsoLogged");
        };
        assert_eq!(dx_call, "");
        assert_eq!(dx_grid, "");
        assert_eq!(my_grid, "");
        assert_eq!(report_sent, "");
        assert_eq!(report_received, "");
        assert_eq!(tx_power, "0");
    }

    #[test]
    fn qso_to_msgs_builds_the_logged_adif_fragment_around_the_given_record() {
        let m = metadata();
        let record = "<call:5>K1ABC<band:3>20M<mode:3>FT8<EOR>\n".to_string();
        let (_logged, adif) = qso_to_msgs(&m, 100, record.clone());
        let OutMsg::LoggedAdif { adif } = adif else {
            panic!("expected OutMsg::LoggedAdif");
        };
        assert_eq!(
            adif,
            format!("<adif_ver:5>3.1.0\n<programid:8>pancetta\n<EOH>\n{record}")
        );
    }
}

#[cfg(test)]
mod reply_tests {
    use super::*;
    use pancetta_core::slot::SlotParity;

    /// A retained decode `(OutMsg::Decode, StoredKey)` ring entry, keyed by
    /// `(message, time_ms, delta_freq)` — the exact triple `reply_to_call`
    /// matches against. `slot_parity` is stored verbatim (as the decoder
    /// would hand it to `stored_key`, via `DecodedMessage::slot_parity`) —
    /// most tests here don't care about parity and pass `None`; the parity
    /// tests pass an explicit, deliberately-not-time-derived value to prove
    /// `reply_to_call` reads it back unchanged rather than recomputing it.
    fn stored(
        message: &str,
        time_ms: u32,
        delta_freq: u32,
        slot_parity: Option<SlotParity>,
    ) -> (OutMsg, StoredKey) {
        let out = OutMsg::Decode {
            new: true,
            time_ms,
            snr: 0,
            delta_time: 0.0,
            delta_freq,
            mode: "~".to_string(),
            message: message.to_string(),
            low_confidence: false,
            off_air: false,
        };
        let key = stored_key(&out, slot_parity).expect("Decode yields a StoredKey");
        (out, key)
    }

    /// An inbound `Reply(4)` echoing a decode's `(message, time_ms,
    /// delta_freq)` — the "double-click" GridTracker sends back.
    fn reply(message: &str, time_ms: u32, delta_freq: u32) -> InMsg {
        InMsg::Reply {
            time_ms,
            snr: 0,
            delta_time: 0.0,
            delta_freq,
            mode: "~".to_string(),
            message: message.to_string(),
            low_confidence: false,
            modifiers: 0,
        }
    }

    #[test]
    fn reply_matches_only_retained_decodes_and_extracts_cq_caller() {
        let mut ring = VecDeque::new();
        ring.push_back(stored("CQ W1AW FN31", 45_296_000, 1302, None));
        ring.push_back(stored("K1ABC W9XYZ -07", 45_296_000, 800, None));
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
        ring.push_back(stored("CQ DX ZL1XYZ RF80", 45_297_000, 400, None));
        let r = reply("CQ DX ZL1XYZ RF80", 45_297_000, 400);
        assert_eq!(reply_to_call(&r, &ring).unwrap().callsign, "ZL1XYZ");
    }

    #[test]
    fn a_reply_that_differs_in_any_key_field_does_not_match() {
        let mut ring = VecDeque::new();
        ring.push_back(stored("CQ W1AW FN31", 45_296_000, 1302, None));
        // Same message + delta_freq, different time.
        assert!(reply_to_call(&reply("CQ W1AW FN31", 45_296_001, 1302), &ring).is_none());
        // Same message + time, different delta_freq.
        assert!(reply_to_call(&reply("CQ W1AW FN31", 45_296_000, 1303), &ring).is_none());
        // Same time + delta_freq, different message text.
        assert!(reply_to_call(&reply("CQ W1AW EM10", 45_296_000, 1302), &ring).is_none());
    }

    #[test]
    fn frequency_hz_is_the_audio_offset_clamped_to_the_ft8_passband() {
        let mut ring = VecDeque::new();
        // In-band offset passes through unchanged.
        ring.push_back(stored("CQ W1AW FN31", 10, 1302, None));
        assert_eq!(
            reply_to_call(&reply("CQ W1AW FN31", 10, 1302), &ring)
                .unwrap()
                .frequency_hz,
            1302
        );
        // Below the passband floor → clamped up to 200.
        ring.push_back(stored("CQ K1ABC FN42", 20, 50, None));
        assert_eq!(
            reply_to_call(&reply("CQ K1ABC FN42", 20, 50), &ring)
                .unwrap()
                .frequency_hz,
            200
        );
        // Above the passband ceiling → clamped down to 3000 (not 2500 — see
        // AUDIO_PASSBAND_MAX_HZ's doc comment: 2500 was narrower than the
        // real FT8 passband and silently truncated genuine high-frequency
        // decodes).
        ring.push_back(stored("CQ N0AX EN10", 30, 9000, None));
        assert_eq!(
            reply_to_call(&reply("CQ N0AX EN10", 30, 9000), &ring)
                .unwrap()
                .frequency_hz,
            3000
        );
    }

    #[test]
    fn dx_parity_is_the_stored_parity_carried_through_unchanged_not_rederived() {
        // 45_296_000 ms = 45296 s; 45296 / 15 = 3019 (an ODD slot index), so a
        // re-derivation from time_ms via SlotParity::of would yield Odd. Store
        // Even instead — deliberately the OPPOSITE of what re-derivation would
        // produce — and assert reply_to_call still returns Even: proof it
        // reads the authoritative stored value, not a recomputation.
        let mut ring = VecDeque::new();
        ring.push_back(stored(
            "CQ W1AW FN31",
            45_296_000,
            1302,
            Some(SlotParity::Even),
        ));
        assert_eq!(
            reply_to_call(&reply("CQ W1AW FN31", 45_296_000, 1302), &ring)
                .unwrap()
                .dx_parity,
            Some(SlotParity::Even)
        );
        // 30_000 ms = 30 s; 30 / 15 = 2 (an EVEN slot index) — store Odd,
        // again the opposite of what re-derivation would give.
        ring.push_back(stored("CQ K1ABC FN42", 30_000, 600, Some(SlotParity::Odd)));
        assert_eq!(
            reply_to_call(&reply("CQ K1ABC FN42", 30_000, 600), &ring)
                .unwrap()
                .dx_parity,
            Some(SlotParity::Odd)
        );
        // No stored parity (decoder hadn't set slot_parity) ⇒ None passed
        // through, never defaulted or re-derived.
        ring.push_back(stored("CQ N0AX EN10", 40_000, 700, None));
        assert_eq!(
            reply_to_call(&reply("CQ N0AX EN10", 40_000, 700), &ring)
                .unwrap()
                .dx_parity,
            None
        );
    }

    #[test]
    fn qrz_is_treated_as_cq_and_extracts_the_caller() {
        let mut ring = VecDeque::new();
        ring.push_back(stored("QRZ W1AW FN31", 45_296_000, 1302, None));
        let intent = reply_to_call(&reply("QRZ W1AW FN31", 45_296_000, 1302), &ring).unwrap();
        assert_eq!((intent.callsign.as_str(), intent.is_cq), ("W1AW", true));
        // QRZ with a directional prefix still extracts the caller.
        ring.push_back(stored("QRZ DX ZL1XYZ RF80", 45_297_000, 400, None));
        let intent = reply_to_call(&reply("QRZ DX ZL1XYZ RF80", 45_297_000, 400), &ring).unwrap();
        assert_eq!((intent.callsign.as_str(), intent.is_cq), ("ZL1XYZ", true));
    }

    #[test]
    fn non_reply_inmsg_yields_no_intent() {
        let ring = VecDeque::new();
        assert!(reply_to_call(&InMsg::Replay, &ring).is_none());
        assert!(reply_to_call(&InMsg::Close, &ring).is_none());
    }

    #[test]
    fn remote_tx_arm_consent_is_the_or_of_both_channels() {
        // Option A: the wsjtx flag alone seeds (arms) the shared consent.
        assert!(remote_tx_arm_consent(false, true));
        // Station-agent flag alone still seeds it (unchanged behavior).
        assert!(remote_tx_arm_consent(true, false));
        assert!(remote_tx_arm_consent(true, true));
        // Both off ⇒ not seeded (fail-closed default).
        assert!(!remote_tx_arm_consent(false, false));
    }
}

#[cfg(test)]
mod replay_gate_tests {
    use super::*;
    use crate::coordinator::ApplicationCoordinator;
    use pancetta_config::Config;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    /// A coordinator with the companion protocol *enabled* and pointed at a
    /// caller-owned loopback port, optionally in `--replay` mode.
    async fn coordinator_targeting(port: u16, replay: Option<PathBuf>) -> ApplicationCoordinator {
        let mut config = Config::default();
        config.network.wsjtx_udp.enabled = true;
        config.network.wsjtx_udp.destination = format!("127.0.0.1:{port}");
        ApplicationCoordinator::new(
            config,
            None,
            true,  // no_audio
            true,  // headless
            false, // metrics
            9090,
            None, // no WAV
            replay,
            None, // no test-tx
            1500.0,
            Arc::new(AtomicBool::new(false)),
            Vec::new(),                                             // no config warnings
            std::env::temp_dir().join("pancetta-test-config.toml"), // test-only config path
            true,                                                   // test-only: assume TOML
        )
        .await
        .expect("coordinator creation should succeed")
    }

    /// Waits for the startup Status datagram (protocol notes §5: the enabled
    /// path sends one synchronously before its loop). `Ok` ⇒ a datagram
    /// arrived, `Err` ⇒ nothing did within the window.
    async fn wait_for_datagram(listener: &tokio::net::UdpSocket) -> bool {
        let mut buf = [0u8; 2048];
        tokio::time::timeout(Duration::from_millis(1500), listener.recv_from(&mut buf))
            .await
            .is_ok()
    }

    /// Control: proves the assertion below can actually fail — with the same
    /// config and no `--replay`, the startup Status datagram does arrive.
    #[tokio::test]
    async fn enabled_component_emits_its_startup_status_when_not_replaying() {
        let listener = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut coordinator = coordinator_targeting(port, None).await;
        coordinator.start_wsjtx_udp_component().await.unwrap();
        assert!(
            wait_for_datagram(&listener).await,
            "a live run with wsjtx_udp enabled must emit its startup Status"
        );
    }

    /// `--replay` takes the drain-only path: no socket is bound, so nothing
    /// on the LAN (GridTracker, JTAlert, ...) is ever told that replayed
    /// historical traffic is a live reception.
    #[tokio::test]
    async fn replay_emits_no_datagram_even_with_the_component_enabled() {
        let listener = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut coordinator =
            coordinator_targeting(port, Some(PathBuf::from("/nonexistent/replay/dir"))).await;
        coordinator.start_wsjtx_udp_component().await.unwrap();
        assert!(
            !wait_for_datagram(&listener).await,
            "--replay must not broadcast replayed decodes to companion apps"
        );
    }
}
