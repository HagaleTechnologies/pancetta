//! PSKReporter component.
//!
//! Forwards locally-decoded FT8 and FT4 messages to the global PSKReporter
//! spotting database, contributing to the world-wide propagation map
//! and earning reciprocal visibility for spot lookups. Submits in
//! batches; rate-limited to PSKReporter's policy. Each report carries the
//! station's live operating mode (PAN-285); FT2 decodes are not uploaded.
//!
//! Opt-in: uploads run when `[network.psk_reporter].enabled = true`
//! (`PskReporterConfig::default()` is `enabled: false`, so an operator
//! has to turn this on deliberately — no credentials required once they
//! do, just outbound UDP datagrams to report.pskreporter.info:4739).
//! Suppressed outright under `--replay`, because replayed decodes are not
//! live receptions.

use anyhow::Result;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, span, warn, Level};

use crate::message_bus::{ComponentId, MessageType};

impl super::ApplicationCoordinator {
    /// Start PSKReporter upload component
    ///
    /// Receives decoded FT8 messages, batches them, and uploads to PSKReporter
    /// at the configured interval (default: 5 minutes).
    pub(crate) async fn start_pskreporter_component(&mut self) -> Result<()> {
        // `--replay` decodes are historical off-air captures, but every spot
        // built from them is stamped `SystemTime::now()`. Uploading those
        // would inject fabricated "live" reception reports into the public
        // pskreporter.info propagation dataset other operators rely on, so a
        // replay session takes the uploads-disabled path unconditionally --
        // regardless of what the operator's config says.
        // (`PskReporterConfig::default()` is `enabled: false`, so this only
        // ever bites an operator who deliberately turned uploads on: exactly
        // the station whose reports are trusted.)
        let replay_mode = self.replay_mode();

        let config = self.config.read().await;
        if replay_mode || !config.network.psk_reporter.enabled {
            if replay_mode {
                info!("Replay mode: PSKReporter uploads suppressed -- replayed decodes are not live spots");
            } else {
                info!("PSKReporter upload disabled in configuration");
            }
            drop(config);
            // The decoder fans every decoded message out to PskReporter via the
            // message bus unconditionally. If we created the channel without a
            // reader, it would fill within a few cycles and emit a continuous
            // "Channel full" warning flood (10k+ warnings/session observed in
            // the 2026-05-30 live capture). Spawn a noop drain task so the
            // channel stays open but messages are silently discarded.
            let (_drain_tx, drain_rx) = self
                .message_bus
                .get_or_create_channel(ComponentId::PskReporter)
                .await?;
            let shutdown = self.shutdown_signal.clone();
            let drain_handle = tokio::spawn(async move {
                while !shutdown.load(Ordering::Acquire) {
                    loop {
                        match drain_rx.try_recv() {
                            Ok(_) => {}
                            Err(crossbeam_channel::TryRecvError::Empty) => break,
                            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                                debug!("PSKReporter drain channel disconnected");
                                return Ok(());
                            }
                        }
                    }
                    sleep(Duration::from_millis(100)).await;
                }
                Ok(())
            });
            self.named_task_handles
                .push((ComponentId::PskReporter, drain_handle));
            return Ok(());
        }

        let upload_config = psk_upload_config(&config);
        let upload_interval = upload_config.upload_interval_secs;
        drop(config);

        info!(
            "Starting PSKReporter upload component (interval: {}s)",
            upload_interval
        );

        let (_psk_tx, psk_rx) = self
            .message_bus
            .get_or_create_channel(ComponentId::PskReporter)
            .await?;

        let psk_operating_freq = self.operating_frequency_hz.clone();
        let psk_mode = self.active_protocol_mode();
        let psk_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                use pancetta_dx::pskreporter::PskReporterUploader;

                let mut uploader = PskReporterUploader::new(upload_config);
                let mut upload_timer = interval(Duration::from_secs(upload_interval));
                // Cap evictions already warned about (PAN-285).
                let mut drops_logged: u64 = 0;
                // The FT2 skip is logged once per task start (PAN-285).
                let mut ft2_skip_logged = false;

                while !shutdown.load(Ordering::Acquire) {
                    // Drain incoming decoded messages
                    loop {
                        match psk_rx.try_recv() {
                            Ok(message) => {
                                if let MessageType::DecodedMessage(ref decoded_msg) =
                                    message.message_type
                                {
                                    let timestamp = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs()
                                        as i64;

                                    // PAN-285: label each report with the
                                    // station's live mode, not a literal.
                                    let mode = pancetta_config::OperatingMode::from_u8(
                                        psk_mode.load(Ordering::Relaxed),
                                    );
                                    match reception_report(
                                        decoded_msg,
                                        psk_operating_freq.load(Ordering::Relaxed),
                                        mode,
                                        timestamp,
                                    ) {
                                        Some(report) => uploader.add_report(report),
                                        None if decoded_msg.message.from_callsign.is_some()
                                            && !ft2_skip_logged =>
                                        {
                                            info!("PSKReporter: FT2 decodes are not uploaded (PSKReporter has no FT2 mode name)");
                                            ft2_skip_logged = true;
                                        }
                                        None => {}
                                    }
                                }
                            }
                            Err(crossbeam_channel::TryRecvError::Empty) => break,
                            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                                info!("PSKReporter channel disconnected");
                                return Ok(());
                            }
                        }
                    }

                    // Check if it's time to upload
                    tokio::select! {
                        _ = upload_timer.tick() => {
                            if let Some(n) = new_drops(uploader.dropped_count(), &mut drops_logged) {
                                warn!(
                                    "PSKReporter: upload queue full at {} reports; dropped the {} oldest since the last upload attempt",
                                    uploader.max_pending_reports(),
                                    n
                                );
                            }
                            if uploader.pending_count() > 0 {
                                match uploader.flush().await {
                                    Ok(count) => {
                                        info!("PSKReporter: sent {} spots (delivery unconfirmed)", count);
                                    }
                                    Err(e) => {
                                        warn!(
                                            "PSKReporter upload failed: {}; {} reports still queued (cap {})",
                                            e,
                                            uploader.pending_count(),
                                            uploader.max_pending_reports()
                                        );
                                    }
                                }
                            }
                        }
                        _ = sleep(Duration::from_millis(100)) => {
                            // Short sleep to avoid busy-looping
                        }
                    }
                }

                // Flush remaining on shutdown
                if uploader.pending_count() > 0 {
                    if let Err(e) = uploader.flush().await {
                        warn!("PSKReporter final flush failed: {}", e);
                    }
                }

                info!("PSKReporter component stopped");
                Ok(())
            })
        };

        self.named_task_handles
            .push((ComponentId::PskReporter, psk_handle));
        info!("PSKReporter component started");
        Ok(())
    }
}

/// The uploader settings for this station (PAN-285: includes the queue cap).
fn psk_upload_config(
    config: &pancetta_config::Config,
) -> pancetta_dx::pskreporter::PskReporterUploadConfig {
    let psk = &config.network.psk_reporter;
    pancetta_dx::pskreporter::PskReporterUploadConfig {
        reporter_callsign: config.station.callsign.clone(),
        reporter_grid: config.station.grid_square.clone(),
        antenna: psk.reporter_info.antenna_info.clone().unwrap_or_default(),
        software: format!(
            "{}/{}",
            psk.reporter_info.software_name, psk.reporter_info.software_version
        ),
        upload_interval_secs: psk.upload_interval_seconds,
        max_pending_reports: usize::try_from(psk.max_pending_reports).unwrap_or(usize::MAX),
        ..Default::default()
    }
}

/// How many reports were dropped since the last warning, or `None` if none
/// were. Moves `logged` up to `dropped_total` (PAN-285).
fn new_drops(dropped_total: u64, logged: &mut u64) -> Option<u64> {
    let new = dropped_total.saturating_sub(*logged);
    *logged = dropped_total;
    (new > 0).then_some(new)
}

/// PSKReporter's `mode` for decodes made while the station runs `mode`
/// (PAN-285). Goes through `protocol_from_mode`, so a build without the `ft2`
/// feature, which decodes FT8 when configured for FT2, says "FT8". `None` for
/// FT2: PSKReporter wants an ADIF MODE or SUBMODE, and FT2 has none.
fn psk_mode_label(mode: pancetta_config::OperatingMode) -> Option<&'static str> {
    match super::protocol_from_mode(mode) {
        pancetta_ft8::Protocol::Ft8 => Some("FT8"),
        pancetta_ft8::Protocol::Ft4 => Some("FT4"),
        #[cfg(feature = "ft2")]
        pancetta_ft8::Protocol::Ft2 => None,
    }
}

/// The reception report for one decode, or `None` when the decode has no
/// sender callsign or the mode has no PSKReporter label (PAN-285).
fn reception_report(
    decoded_msg: &pancetta_ft8::DecodedMessage,
    live_dial_hz: u64,
    mode: pancetta_config::OperatingMode,
    now_unix: i64,
) -> Option<pancetta_dx::pskreporter::ReceptionReport> {
    let callsign = decoded_msg.message.from_callsign.as_ref()?;
    let label = psk_mode_label(mode)?;
    Some(pancetta_dx::pskreporter::ReceptionReport {
        tx_callsign: callsign.clone(),
        frequency: spot_frequency_hz(decoded_msg, live_dial_hz),
        snr: Some(decoded_msg.snr_db as i32),
        mode: label.to_string(),
        tx_grid: decoded_msg.message.grid_square.clone(),
        timestamp: now_unix,
    })
}

/// The absolute frequency (Hz) to report a reception at: the dial frequency
/// the decode's own audio window was captured on
/// (`DecodedMessage::captured_dial_hz`), plus the decode's intra-window
/// audio tone offset.
///
/// PAN-67 review round 2: falls back to `live_dial_hz` only when the
/// message never got stamped (a path that predates the live decode
/// pipeline — remote-gateway relay, WSJT-X UDP import, test scaffolding).
/// For a live decode, using the live atomic directly here would reintroduce
/// the same race this PR fixed for the TUI: decoding is real CPU work, so
/// a band switch mid-decode could publish an old-band reception under the
/// operator's new band.
///
/// PAN-67 review round 3: `Some(0)` means "not yet established" (see
/// `dsp.rs`'s `last_known_dial_hz`/`cur_dial_hz`, 0 = no rig / pre-first-read),
/// never a real 0 Hz dial — treated the same as `None`, matching
/// `tui_relay.rs`'s `decode_view_dial_mhz` zero-sentinel handling.
fn spot_frequency_hz(decoded_msg: &pancetta_ft8::DecodedMessage, live_dial_hz: u64) -> u64 {
    let dial_hz = match decoded_msg.captured_dial_hz {
        Some(hz) if hz != 0 => hz,
        _ => live_dial_hz,
    };
    dial_hz + decoded_msg.frequency_offset as u64
}

#[cfg(test)]
mod psk_reporter_tests {
    use super::spot_frequency_hz;
    use pancetta_config::OperatingMode;
    use pancetta_dx::pskreporter::ReceptionReport;

    fn decoded_at(
        freq_offset_hz: f64,
        captured_dial_hz: Option<u64>,
    ) -> pancetta_ft8::DecodedMessage {
        let mut d = pancetta_ft8::DecodedMessage::new(
            pancetta_ft8::Ft8Message::default(),
            -10.0,
            0.5,
            freq_offset_hz,
            0.1,
        );
        d.captured_dial_hz = captured_dial_hz;
        d
    }

    /// PAN-67 review round 2: a decode from the old band's audio must
    /// report under the old band even if the live dial has already moved
    /// on to a new band by the time this spot gets published.
    #[test]
    fn spot_frequency_uses_the_captured_dial_not_the_live_one() {
        let old_band_decode = decoded_at(1500.0, Some(7_074_000));
        assert_eq!(spot_frequency_hz(&old_band_decode, 14_074_000), 7_075_500);
    }

    #[test]
    fn spot_frequency_falls_back_to_live_dial_when_uncaptured() {
        let unstamped_decode = decoded_at(1500.0, None);
        assert_eq!(spot_frequency_hz(&unstamped_decode, 14_074_000), 14_075_500);
    }

    /// PAN-67 review round 3: a window that closed before the first CAT
    /// read stamps `captured_dial_hz` as `Some(0)`, not `None` — 0 is a
    /// real "not yet established" sentinel (see `dsp.rs`'s
    /// `last_known_dial_hz`/`cur_dial_hz`), not a legitimate 0 Hz dial.
    /// `unwrap_or` alone treats `Some(0)` as a real value and would
    /// publish the reception near the bare audio tone offset.
    #[test]
    fn spot_frequency_falls_back_to_live_dial_when_captured_is_zero() {
        let zero_captured_decode = decoded_at(1500.0, Some(0));
        assert_eq!(
            spot_frequency_hz(&zero_captured_decode, 14_074_000),
            14_075_500
        );
    }
    // ----- PAN-285: queue cap pass-through and drop warning. -----

    #[test]
    fn upload_config_carries_the_configured_cap_and_station_identity() {
        let mut cfg = pancetta_config::Config::default();
        cfg.station.callsign = "W1ABC".to_string();
        cfg.station.grid_square = "FN42".to_string();
        cfg.network.psk_reporter.max_pending_reports = 500;
        cfg.network.psk_reporter.reporter_info.antenna_info = Some("Dipole".to_string());
        let up = super::psk_upload_config(&cfg);
        assert_eq!(up.max_pending_reports, 500);
        assert_eq!(up.reporter_callsign, "W1ABC");
        assert_eq!(up.reporter_grid, "FN42");
        assert_eq!(up.antenna, "Dipole");
        assert_eq!(up.upload_interval_secs, 300);
        assert_eq!(
            up.software,
            format!(
                "{}/{}",
                cfg.network.psk_reporter.reporter_info.software_name,
                cfg.network.psk_reporter.reporter_info.software_version
            )
        );
    }

    #[test]
    fn new_drops_reports_each_increase_once() {
        let mut logged = 0;
        assert_eq!(super::new_drops(0, &mut logged), None);
        assert_eq!(super::new_drops(160, &mut logged), Some(160));
        assert_eq!(super::new_drops(160, &mut logged), None);
        assert_eq!(super::new_drops(200, &mut logged), Some(40));
    }

    // ----- PAN-285: report the station's actual mode. -----

    fn decoded_from(call: Option<&str>) -> pancetta_ft8::DecodedMessage {
        let mut d = decoded_at(1500.0, Some(14_074_000));
        d.message.from_callsign = call.map(str::to_string);
        d.message.grid_square = Some("FN42".to_string());
        d
    }

    #[test]
    fn ft4_station_reports_ft4() {
        let r = super::reception_report(
            &decoded_from(Some("K1ABC")),
            0,
            OperatingMode::Ft4,
            1_600_000_000,
        )
        .unwrap();
        assert_eq!(r.mode, "FT4");
    }

    /// D10: an FT8 station's report is exactly what the old literal built.
    #[test]
    fn ft8_station_report_is_unchanged() {
        let d = decoded_from(Some("K1ABC"));
        let r = super::reception_report(&d, 7_074_000, OperatingMode::Ft8, 1_600_000_000).unwrap();
        assert_eq!(
            r,
            ReceptionReport {
                tx_callsign: "K1ABC".to_string(),
                frequency: spot_frequency_hz(&d, 7_074_000),
                snr: Some(d.snr_db as i32),
                mode: "FT8".to_string(),
                tx_grid: Some("FN42".to_string()),
                timestamp: 1_600_000_000,
            }
        );
    }

    #[test]
    fn decode_without_a_sender_callsign_is_not_reported() {
        assert!(super::reception_report(&decoded_from(None), 0, OperatingMode::Ft4, 0).is_none());
    }

    #[cfg(not(feature = "ft2"))]
    #[test]
    fn ft2_config_without_the_ft2_feature_reports_the_ft8_it_decodes() {
        let r = super::reception_report(&decoded_from(Some("K1ABC")), 0, OperatingMode::Ft2, 0)
            .unwrap();
        assert_eq!(r.mode, "FT8");
    }

    #[cfg(feature = "ft2")]
    #[test]
    fn ft2_decodes_are_not_reported() {
        assert!(
            super::reception_report(&decoded_from(Some("K1ABC")), 0, OperatingMode::Ft2, 0)
                .is_none()
        );
    }
}
