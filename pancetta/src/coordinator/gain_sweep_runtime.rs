//! Coordinator-owned receive-gain experiment. CAT operations never hold ptt_sync_gate.
use super::{gain_sweep::*, pipeline::*, ApplicationCoordinator};
use anyhow::{anyhow, Context, Result};
use chrono::{TimeZone, Utc};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::time::{sleep, sleep_until, timeout, Instant};

#[derive(Clone)]
pub(super) struct GainSweepOptions {
    seconds: u64,
    path: PathBuf,
}

impl ApplicationCoordinator {
    /// Configure an opt-in headless gain sweep before starting the station.
    pub fn configure_gain_sweep(&mut self, seconds: u64, path: PathBuf) -> Result<()> {
        anyhow::ensure!(
            (1..=3600).contains(&seconds),
            "Sweep duration must be 1..3600 seconds"
        );
        anyhow::ensure!(
            path.symlink_metadata().is_err(),
            "Sweep report already exists"
        );
        self.gain_sweep_options = Some(GainSweepOptions { seconds, path });
        Ok(())
    }

    pub(super) async fn start_gain_sweep(
        &self,
    ) -> Result<Option<tokio::task::JoinHandle<Result<()>>>> {
        let Some(options) = self.gain_sweep_options.clone() else {
            return Ok(None);
        };
        #[cfg(not(feature = "pancetta-hamlib"))]
        anyhow::bail!("Sweep unavailable: Hamlib is disabled");
        #[cfg(feature = "pancetta-hamlib")]
        {
            let rig = self
                .rig_handle
                .clone()
                .context("Sweep unavailable: no rig connection")?;
            let config = self.config.clone();
            let dial = self.operating_frequency_hz.clone();
            let protocol = self.active_protocol_mode.clone();
            let shutdown = self.shutdown_signal.clone();
            let capture = self.sweep_capture.clone();
            let bus = self.message_bus.clone();
            let ptt = self.ptt_active.clone();
            let sync = self.ptt_sync_gate.clone();
            let policy = self.tx_policy.clone();
            let generation = self.hamlib_generation.clone();
            let snapshot = self.gain_sweep_original.clone();
            Ok(Some(tokio::spawn(async move {
                struct EndSweep(Arc<AtomicBool>);
                impl Drop for EndSweep {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::Release);
                    }
                }
                let _end_sweep = EndSweep(shutdown.clone());
                let result = async {
                    let cfg = config.read().await;
                    anyhow::ensure!(cfg.rig.operating_mode()? == pancetta_config::OperatingMode::Ft8,
                        "Sweep unavailable: requires FT8");
                    let lead = super::resolve_window_lead_secs(&cfg.decoder);
                    drop(cfg);
                    println!("Reading rig gain settings...");
                    let setup = async {
                        let caps = timeout(Duration::from_secs(5), rig.receive_gain_capabilities()).await??;
                        let original = timeout(Duration::from_secs(5), rig.get_receive_gain()).await??;
                        let settings = candidates(&caps.preamp_db, &caps.attenuator_db).map_err(|e| anyhow!(e))?;
                        anyhow::Ok((original, settings))
                    }.await;
                    let (original, settings) = match setup {
                        Ok(value) => value,
                        Err(error) => {
                            println!("Sweep unavailable: the rig cannot read and set PREAMP and ATT.\nRecommendation: none");
                            return Err(error);
                        }
                    };
                    let original_gain = GainSetting { preamp_db: original.preamp_db, attenuator_db: original.attenuator_db };
                    let frequency = dial.load(Ordering::Acquire);
                    anyhow::ensure!(frequency != 0, "Sweep unavailable: dial frequency is unknown");
                    let band = pancetta_qso::utils::frequency_to_band(frequency as f64);
                    let mut report = Report::new(band, frequency, options.seconds, original_gain, settings.clone());
                    println!("Receive-gain sweep: {}, FT8, {} seconds", report.band, options.seconds);
                    println!("Original gain: PREAMP {} dB, ATT {} dB", original.preamp_db, original.attenuator_db);
                    println!("Waiting for a receive window...");
                    *snapshot.lock().unwrap_or_else(|e| e.into_inner()) = Some(original);
                    let started = Instant::now();
                    let end = started + Duration::from_secs(options.seconds);
                    let initial_generation = generation.load(Ordering::Acquire);
                    let initial_mode = protocol.load(Ordering::Acquire);
                    let mut schedule = Schedule::new(settings);
                    let mut epoch = 0;
                    let mut failure = None;
                    let mut current = original;
                    println!("Measuring receive windows. Transmission windows are skipped.");
                    while Instant::now() < end {
                        drain(&capture, &mut report);
                        if shutdown.load(Ordering::Acquire) { failure = Some("interrupted".into()); break; }
                        if dial.load(Ordering::Acquire) != frequency || protocol.load(Ordering::Acquire) != initial_mode
                            || generation.load(Ordering::Acquire) != initial_generation {
                            failure = Some("Dial, mode or Hamlib generation changed".into()); break;
                        }
                        let now = Utc::now();
                        let slot_secs = (now.timestamp_millis() + (lead * 1000.0) as i64).div_euclid(15000) * 15 + 15;
                        let slot = Utc.timestamp_opt(slot_secs, 0).single().context("Invalid source slot")?;
                        let boundary = slot - chrono::Duration::milliseconds((lead * 1000.0) as i64);
                        let wait = (boundary - Utc::now()).to_std().unwrap_or_default();
                        let wake = Instant::now() + wait;
                        while Instant::now() < wake.min(end) && !shutdown.load(Ordering::Acquire) {
                            sleep_until((Instant::now() + Duration::from_millis(50)).min(wake).min(end)).await;
                        }
                        if Instant::now() >= end || shutdown.load(Ordering::Acquire) { continue; }
                        let lateness = Utc::now().signed_duration_since(boundary).num_milliseconds();
                        if !(0..=50).contains(&lateness) { skip(&mut report, slot_secs, "late boundary or clock discontinuity"); continue; }
                        // Configured TX parity is reserved even with no queued frame.
                        // Auto can choose either parity, so both remain unavailable.
                        let cfg = config.read().await;
                        let blocked_parity = if pancetta_core::TxPolicy::from_u8(policy.load(Ordering::Acquire)).allows_any_tx() {
                            match cfg.station.tx_self_parity {
                                pancetta_config::station::TxSelfParity::Even => slot_secs.div_euclid(15).rem_euclid(2) == 0,
                                pancetta_config::station::TxSelfParity::Odd => slot_secs.div_euclid(15).rem_euclid(2) == 1,
                                _ => false,
                            }
                        } else { false };
                        drop(cfg);
                        if blocked_parity || bus.gain_tx_intents.load(Ordering::Acquire) != 0 { skip(&mut report, slot_secs, "reserved TX parity"); continue; }
                        if dial.load(Ordering::Acquire) != frequency || protocol.load(Ordering::Acquire) != initial_mode
                            || generation.load(Ordering::Acquire) != initial_generation {
                            failure = Some("Dial, mode or Hamlib generation changed".into()); break;
                        }
                        let admitted = {
                            let _guard = sync.lock().await;
                            !ptt.load(Ordering::Acquire) && bus.gain_tx_intents.load(Ordering::Acquire) == 0 && reserve(&bus.gain_tx_gate)
                        };
                        if !admitted { skip(&mut report, slot_secs, "PTT active or pending"); continue; }
                        let mut trial = schedule.clone();
                        let Some(gain) = trial.next(slot_secs) else {
                            schedule = trial;
                            bus.gain_tx_gate.store(0, Ordering::Release);
                            skip(&mut report, slot_secs, "parity balance quiet slot"); continue;
                        };
                        let budget = (slot - chrono::Duration::milliseconds(250) - Utc::now()).to_std().unwrap_or_default();
                        let deadline = Instant::now() + budget;
                        let transaction = async {
                            anyhow::ensure!(rig.get_ptt(pancetta_hamlib::Vfo::Current).await? == pancetta_hamlib::PttState::Off, "Rig is transmitting");
                            anyhow::ensure!(rig.get_receive_gain().await? == current, "Front-panel gain changed");
                            rig.set_receive_gain(pancetta_hamlib::ReceiveGain { preamp_db: gain.preamp_db, attenuator_db: gain.attenuator_db }, deadline).await
                        };
                        match tokio::time::timeout_at(deadline, transaction).await {
                            Ok(Ok(())) => {
                                current = pancetta_hamlib::ReceiveGain { preamp_db: gain.preamp_db, attenuator_db: gain.attenuator_db };
                                // Do not release TX exclusion until the 250 ms settling interval ends.
                                let settle = (slot - Utc::now()).to_std().unwrap_or_default();
                                sleep(settle).await;
                                epoch += 1;
                                let capture_admitted = capture.lock().unwrap_or_else(|e| e.into_inner()).admit(GainCaptureMetadata {
                                    generation: initial_generation, epoch, tx_epoch: bus.gain_tx_epoch.load(Ordering::Acquire), source_slot: slot, dial_hz: frequency, gain,
                                });
                                if !capture_admitted { skip(&mut report, slot_secs, "sweep admission backpressure"); }
                                schedule = trial;
                                bus.gain_tx_gate.store(0, Ordering::Release);
                            }
                            error => {
                                skip(&mut report, slot_secs, "gain transaction failed");
                                failure = Some(format!("Gain transaction failed: {error:?}"));
                                // A cancelled transport future must not retain a late reply.
                                let _ = rig.disconnect().await;
                                break;
                            }
                        }
                    }
                    if shutdown.load(Ordering::Acquire) { failure = Some("interrupted".into()); }
                    // Retain the original pair in this coordinator-owned task until cleanup
                    // has finished. This task is joined before Hamlib teardown, never aborted.
                    if failure.is_none() && rig.get_receive_gain().await.ok() != Some(current) {
                        failure = Some("Gain drift or missing final gain readback".into());
                        capture.lock().unwrap_or_else(|e| e.into_inner()).invalidate_all("unconfirmed final gain");
                    }
                    println!("Restoring original gain settings...");
                    let restored = restore(&*rig, original, &bus.gain_tx_gate, lead).await;
                    // A reported terminal restoration failure also ends cleanup
                    // ownership. Keep the TX inhibit, but do not restart a fresh
                    // 30-second deadline during ordinary shutdown.
                    *snapshot.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    if restored {
                        println!("Original gain settings restored.");
                    }
                    else { println!("Sweep failed: original gain settings could not be restored.\nCheck rig gain settings before continuing."); }
                    let drain_end = Instant::now() + Duration::from_secs(15);
                    while Instant::now() < drain_end && !shutdown.load(Ordering::Acquire) {
                        drain(&capture, &mut report);
                        sleep(Duration::from_millis(50)).await;
                    }
                    capture.lock().unwrap_or_else(|e| e.into_inner()).invalidate_all("unfinished at decode drain deadline");
                    drain(&capture, &mut report);
                    if dial.load(Ordering::Acquire) != frequency || protocol.load(Ordering::Acquire) != initial_mode
                        || generation.load(Ordering::Acquire) != initial_generation {
                        failure = Some("Dial, mode or Hamlib generation changed".into());
                    }
                    if shutdown.load(Ordering::Acquire) { failure = Some("interrupted".into()); }
                    report.finish(restored, failure.clone());
                    print_report(&report);
                    save_report(&options.path, &report).context("Sweep failed: could not save report.")?;
                    println!("Report saved: {}", options.path.display());
                    anyhow::ensure!(restored, "Original gain restoration failed");
                    if let Some(reason) = failure { anyhow::bail!(reason); }
                    Ok(())
                }.await;
                shutdown.store(true, Ordering::Release);
                result
            })))
        }
    }
}

fn reserve(gate: &AtomicU64) -> bool {
    gate.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

#[cfg(feature = "pancetta-hamlib")]
pub(super) async fn restore(
    rig: &dyn pancetta_hamlib::RigControl,
    original: pancetta_hamlib::ReceiveGain,
    gate: &AtomicU64,
    lead: f64,
) -> bool {
    let end = Instant::now() + Duration::from_secs(30);
    // Inhibit new key-downs, including when a prior pending key-down has not
    // unkeyed yet. Physical PTT-off is checked before every restoration write.
    gate.fetch_or(1, Ordering::AcqRel);
    let restored = tokio::time::timeout_at(end, async {
        loop {
            let _ = rig.connect().await;
            let now = Utc::now();
            let slot_secs =
                (now.timestamp_millis() + (lead * 1000.0) as i64).div_euclid(15000) * 15 + 15;
            let slot = Utc
                .timestamp_opt(slot_secs, 0)
                .single()
                .context("Invalid restoration boundary")?;
            let boundary = slot - chrono::Duration::milliseconds((lead * 1000.0) as i64);
            sleep((boundary - Utc::now()).to_std().unwrap_or_default()).await;
            if Utc::now()
                .signed_duration_since(boundary)
                .num_milliseconds()
                > 50
            {
                continue;
            }
            let deadline = Instant::now()
                + (slot - chrono::Duration::milliseconds(250) - Utc::now())
                    .to_std()
                    .unwrap_or_default();
            let attempt = tokio::time::timeout_at(deadline, async {
                anyhow::ensure!(
                    rig.get_ptt(pancetta_hamlib::Vfo::Current).await?
                        == pancetta_hamlib::PttState::Off,
                    "PTT still on"
                );
                rig.set_receive_gain(original, deadline.min(end)).await
            })
            .await;
            if matches!(attempt, Ok(Ok(()))) {
                sleep((slot - Utc::now()).to_std().unwrap_or_default()).await;
                return anyhow::Ok(());
            }
            let _ = rig.disconnect().await;
        }
    })
    .await;
    if matches!(restored, Ok(Ok(()))) {
        gate.store(0, Ordering::Release);
        true
    } else {
        false
    }
}

fn skip(report: &mut Report, slot: i64, reason: &str) {
    report.skipped.push(SkippedWindow {
        slot,
        reason: reason.into(),
    });
}
fn drain(capture: &std::sync::Mutex<GainCaptureRegistry>, report: &mut Report) {
    let completed = capture
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take_completed();
    for completion in completed {
        let meta = completion.metadata;
        if let Some(reason) = completion.invalid_reason {
            skip(report, meta.source_slot.timestamp(), &reason);
            continue;
        }
        let Some(noise) = completion.noise_dbfs_per_hz else {
            skip(report, meta.source_slot.timestamp(), "missing noise");
            continue;
        };
        report.observations.push(Observation {
            slot: meta.source_slot.timestamp(),
            generation: meta.generation,
            gain_epoch: meta.epoch,
            gain: meta.gain,
            noise_dbfs_per_hz: noise,
            stations: completion
                .messages
                .iter()
                .filter_map(|m| {
                    m.message.from_callsign.as_ref().map(|call| StationSnr {
                        callsign: call.clone(),
                        snr_db: m.snr_db as f64,
                    })
                })
                .collect(),
        });
    }
}
fn print_report(report: &Report) {
    println!("Setting                 Windows  Noise dBFS/Hz  Matched SNR dB  Unique stations");
    for s in &report.settings {
        let number = |n: Option<f64>| n.map(|v| format!("{v:.1}")).unwrap_or_else(|| "n/a".into());
        println!(
            "PREAMP {} dB, ATT {} dB  {}  {}  {}  {}",
            s.gain.preamp_db,
            s.gain.attenuator_db,
            s.valid_windows,
            number(s.noise_dbfs_per_hz),
            number(s.matched_snr_db),
            s.unique_stations
        );
    }
    if let Some(gain) = report.recommendation {
        println!(
            "Recommendation: PREAMP {} dB, ATT {} dB on {}",
            gain.preamp_db, gain.attenuator_db, report.band
        );
    } else {
        println!("Recommendation: none");
    }
    for reason in &report.reasons {
        if report.status == "inconclusive" {
            println!("Inconclusive: {reason}.");
        } else {
            println!("Reason: {reason}.");
        }
    }
}
fn save_report(path: &std::path::Path, report: &Report) -> Result<()> {
    use std::io::Write;
    // create_new guarantees a concurrent writer cannot be overwritten. The
    // temporary inode and hard-link publication make the destination atomic.
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(report)?)?;
        file.sync_all()?;
        std::fs::hard_link(&temporary, path)?;
        anyhow::Ok(())
    })();
    let _ = std::fs::remove_file(temporary);
    result
}

#[cfg(all(test, feature = "pancetta-hamlib"))]
mod tests {
    use super::*;
    use pancetta_hamlib::{MockRig, ReceiveGain, ReceiveGainCapabilities, RigControl};

    async fn coordinator(path: PathBuf) -> (ApplicationCoordinator, Arc<MockRig>) {
        let rig = Arc::new(MockRig::new(pancetta_hamlib::MockRigConfig {
            receive_gain: Some((
                ReceiveGainCapabilities {
                    preamp_db: vec![0, 10, 20],
                    attenuator_db: vec![0, 6],
                },
                ReceiveGain {
                    preamp_db: 0,
                    attenuator_db: 0,
                },
            )),
            ..Default::default()
        }));
        rig.connect().await.unwrap();
        let mut coordinator = ApplicationCoordinator::new(
            pancetta_config::Config::default(),
            None,
            true,
            true,
            false,
            9090,
            None,
            None,
            None,
            1500.0,
            Arc::new(AtomicBool::new(false)),
            vec![],
            path.with_extension("config.toml"),
            false,
        )
        .await
        .unwrap();
        coordinator
            .operating_frequency_hz
            .store(14_074_000, Ordering::Release);
        coordinator.rig_handle = Some(rig.clone());
        coordinator.configure_gain_sweep(1, path).unwrap();
        (coordinator, rig)
    }

    #[test]
    fn gain_sweep_atomic_reservation_and_pending_tx_exclude_each_other() {
        let gate = AtomicU64::new(2);
        assert!(!reserve(&gate));
        gate.store(0, Ordering::Release);
        assert!(reserve(&gate));
        assert!(!reserve(&gate));
    }

    #[tokio::test]
    async fn gain_sweep_ptt_on_refused_during_transition_and_off_remains_available() {
        use crate::message_bus::{ComponentId, MessageBus, SendIfOutcome};
        let bus = MessageBus::new(8).unwrap();
        let (_, rx) = bus.create_channel(ComponentId::Hamlib).await.unwrap();
        assert!(reserve(&bus.gain_tx_gate));
        assert_eq!(
            super::super::tx::send_ptt_command(&bus, ComponentId::Tui, true, || true),
            SendIfOutcome::Denied
        );
        assert!(rx.is_empty());
        assert_eq!(
            super::super::tx::send_ptt_command(&bus, ComponentId::Tui, false, || true),
            SendIfOutcome::Enqueued
        );
        assert_eq!(bus.gain_tx_gate.load(Ordering::Acquire), 1);
        bus.gain_tx_gate.store(0, Ordering::Release);
        assert_eq!(
            super::super::tx::send_ptt_command(&bus, ComponentId::Tui, true, || true),
            SendIfOutcome::Enqueued
        );
        assert!(!reserve(&bus.gain_tx_gate));
        // Unkey enqueue alone cannot authorize an experiment. The rig owner
        // retires the reservation only after the physical unkey succeeds.
        super::super::tx::send_ptt_command(&bus, ComponentId::Tui, false, || true);
        assert!(!reserve(&bus.gain_tx_gate));
        bus.gain_tx_gate.fetch_and(!2, Ordering::AcqRel);
        assert!(reserve(&bus.gain_tx_gate));
    }

    #[tokio::test]
    async fn gain_sweep_shutdown_unkeys_before_restoring_gain() {
        use crate::message_bus::{ComponentId, MessageType, RigControlMessage};
        use pancetta_hamlib::{PttState, Vfo};
        for join_sweep in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (coordinator, rig) = coordinator(dir.path().join("shutdown.json")).await;
            // Exercise the bus fallback without contacting a real rigctld endpoint.
            coordinator.config.write().await.rig.interface.enabled = false;
            *coordinator.gain_sweep_original.lock().unwrap() =
                Some(rig.get_receive_gain().await.unwrap());
            let original = rig.get_receive_gain().await.unwrap();
            rig.set_receive_gain(
                ReceiveGain {
                    preamp_db: 10,
                    attenuator_db: 0,
                },
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap();
            rig.set_ptt(Vfo::Current, PttState::On).await.unwrap();
            let (_, rx) = coordinator
                .message_bus
                .create_channel(ComponentId::Hamlib)
                .await
                .unwrap();
            let (unkeyed_tx, unkeyed_rx) = tokio::sync::oneshot::channel();
            let (cleanup_tx, cleanup_rx) = tokio::sync::oneshot::channel();
            let worker_rig = rig.clone();
            let worker = tokio::spawn(async move {
                let message = loop {
                    if let Ok(message) = rx.try_recv() {
                        break message;
                    }
                    sleep(Duration::from_millis(10)).await;
                };
                assert!(matches!(
                    message.message_type,
                    MessageType::RigControl(RigControlMessage::SetPtt { state: false })
                ));
                worker_rig
                    .set_ptt(Vfo::Current, PttState::Off)
                    .await
                    .unwrap();
                unkeyed_tx.send(()).unwrap();
                let _ = cleanup_tx.send(());
            });
            // A sweep cleanup task cannot finish until physical PTT is released.
            // Its error must still allow the fallback restoration to run.
            let sweep_handle = join_sweep.then(|| {
                tokio::spawn(async move {
                    cleanup_rx.await.unwrap();
                    anyhow::bail!("sweep cleanup failed")
                })
            });
            let shutdown = tokio::spawn(async move {
                if join_sweep {
                    coordinator.shutdown_with_gain_sweep(sweep_handle).await
                } else {
                    coordinator.shutdown().await
                }
            });
            tokio::time::timeout(Duration::from_secs(2), unkeyed_rx)
                .await
                .expect("shutdown must unkey before waiting for gain restoration")
                .unwrap();
            let result = shutdown.await.unwrap();
            if join_sweep {
                assert_eq!(result.unwrap_err().to_string(), "sweep cleanup failed");
            } else {
                result.unwrap();
            }
            worker.await.unwrap();
            assert_eq!(rig.get_ptt(Vfo::Current).await.unwrap(), PttState::Off);
            assert_eq!(rig.get_receive_gain().await.unwrap(), original);
        }
    }

    #[tokio::test]
    async fn gain_sweep_coordinator_completion_restores_before_saved_report() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.json");
        let (coordinator, rig) = coordinator(path.clone()).await;
        // Feed one source-window completion through the same production mailbox
        // consumed by the coordinator. It represents a valid zero-decode window.
        let meta = GainCaptureMetadata {
            generation: 0,
            epoch: 1,
            tx_epoch: 0,
            source_slot: Utc.timestamp_opt(30, 0).single().unwrap(),
            dial_hz: 14_074_000,
            gain: GainSetting {
                preamp_db: 0,
                attenuator_db: 0,
            },
        };
        {
            let mut mailbox = coordinator.sweep_capture.lock().unwrap();
            assert!(mailbox.admit(meta.clone()));
            mailbox.complete(GainCaptureCompletion {
                metadata: meta,
                noise_dbfs_per_hz: Some(-93.0),
                messages: vec![],
                invalid_reason: None,
            });
        }
        coordinator
            .start_gain_sweep()
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        let report: Report = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(report.restoration, "confirmed");
        assert!(report.recommendation.is_none());
        assert_eq!(report.observations.len(), 1);
        assert!(report.observations[0].stations.is_empty());
        assert_eq!(
            rig.get_receive_gain().await.unwrap(),
            ReceiveGain {
                preamp_db: 0,
                attenuator_db: 0
            }
        );
        assert!(coordinator.gain_sweep_original.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn gain_sweep_coordinator_cancellation_restores_and_never_recommends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cancelled.json");
        let (coordinator, rig) = coordinator(path.clone()).await;
        coordinator.shutdown_signal.store(true, Ordering::Release);
        assert!(coordinator
            .start_gain_sweep()
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap()
            .is_err());
        let report: Report = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(report.status, "interrupted");
        assert!(report.recommendation.is_none());
        assert_eq!(report.restoration, "confirmed");
        assert_eq!(rig.get_receive_gain().await.unwrap().preamp_db, 0);
    }

    #[tokio::test]
    async fn gain_sweep_coordinator_failed_restoration_is_not_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("failed.json");
        let (mut coordinator, mut rig) = coordinator(path.clone()).await;
        coordinator.rig_handle = None;
        Arc::get_mut(&mut rig)
            .unwrap()
            .update_config(pancetta_hamlib::MockRigConfig {
                failure_rate: 1.0,
                ..Default::default()
            });
        coordinator.rig_handle = Some(rig);
        coordinator.shutdown_signal.store(true, Ordering::Release);
        assert!(coordinator
            .start_gain_sweep()
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap()
            .is_err());
        let report: Report = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(report.status, "failed");
        assert_eq!(report.restoration, "failed");
        assert!(report.recommendation.is_none());
        assert_eq!(
            coordinator.message_bus.gain_tx_gate.load(Ordering::Acquire) & 1,
            1
        );
    }

    #[tokio::test]
    async fn gain_sweep_interruption_during_cleanup_suppresses_recommendation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("late-cancel.json");
        let (coordinator, rig) = coordinator(path.clone()).await;
        let task = coordinator.start_gain_sweep().await.unwrap().unwrap();
        while coordinator.gain_sweep_original.lock().unwrap().is_none() {
            sleep(Duration::from_millis(10)).await;
        }
        // Simulate a changed gain after the original snapshot, then interrupt
        // after the one-second experiment has entered restoration.
        rig.set_receive_gain(
            ReceiveGain {
                preamp_db: 10,
                attenuator_db: 0,
            },
            Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap();
        sleep(Duration::from_secs(2)).await;
        coordinator.shutdown_signal.store(true, Ordering::Release);
        assert!(task.await.unwrap().is_err());
        let report: Report = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(report.status, "interrupted");
        assert!(report.recommendation.is_none());
        assert_eq!(rig.get_receive_gain().await.unwrap().preamp_db, 0);
    }

    #[tokio::test]
    async fn gain_sweep_report_write_failure_still_restores() {
        let dir = tempfile::tempdir().unwrap();
        let (coordinator, _) = coordinator(dir.path().join("missing").join("report.json")).await;
        coordinator.shutdown_signal.store(true, Ordering::Release);
        let error = coordinator
            .start_gain_sweep()
            .await
            .unwrap()
            .unwrap()
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("could not save report"));
        assert!(coordinator.gain_sweep_original.lock().unwrap().is_none());
    }
}
