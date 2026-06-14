use anyhow::Result;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{debug, error, info, span, warn, Level};

use crate::message_bus::{ComponentId, ComponentMessage, MessageType};

/// Rig connection state surfaced to the TUI as a station-panel badge.
///
/// The coordinator stores this in an [`std::sync::atomic::AtomicU8`] (see
/// [`ApplicationCoordinator::rig_conn_state`](super::ApplicationCoordinator))
/// written by the hamlib connect/poll loop. Round-trips via
/// [`RigConnState::as_u8`] / [`RigConnState::from_u8`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum RigConnState {
    /// No connection attempted yet, or rig control disabled (mock rig).
    #[default]
    NotConnected,
    /// Connected to rigctld and last poll succeeded.
    Connected,
    /// Was connected but recent polls are failing (rigctld may have crashed).
    PollingFailed,
}

impl RigConnState {
    /// Stable `u8` encoding for atomic storage (fixed mapping).
    pub(crate) fn as_u8(self) -> u8 {
        match self {
            RigConnState::NotConnected => 0,
            RigConnState::Connected => 1,
            RigConnState::PollingFailed => 2,
        }
    }

    /// Decode from the stable `u8` encoding; unknown values map to the safe
    /// default ([`RigConnState::NotConnected`]).
    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            1 => RigConnState::Connected,
            2 => RigConnState::PollingFailed,
            _ => RigConnState::NotConnected,
        }
    }
}

impl super::ApplicationCoordinator {
    /// Map rig model name to hamlib model number.
    /// See: https://github.com/Hamlib/Hamlib/wiki/Supported-Radios
    #[cfg(feature = "pancetta-hamlib")]
    pub(crate) fn hamlib_model_id(model: &str) -> Option<u32> {
        match model.to_lowercase().replace(['-', ' '], "").as_str() {
            "ftdx10" => Some(1042),
            "ftdx101d" | "ftdx101mp" => Some(1040),
            "ft991" | "ft991a" => Some(1036),
            "ft710" => Some(1046),
            "ft891" => Some(1038),
            "ft857" | "ft857d" => Some(1022),
            "ft817" | "ft817nd" => Some(1020),
            "ic7300" => Some(3073),
            "ic7610" => Some(3078),
            "ic7851" => Some(3075),
            "ic705" => Some(3085),
            "ic9700" => Some(3081),
            "ts890" | "ts890s" => Some(2029),
            "ts590" | "ts590s" | "ts590sg" => Some(2026),
            _ => None,
        }
    }

    #[cfg(feature = "pancetta-hamlib")]
    pub(crate) async fn start_hamlib_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_hamlib");
        let _enter = span.enter();

        info!("Starting Hamlib component");

        let (_hamlib_tx, hamlib_rx) = self.message_bus.create_channel(ComponentId::Hamlib).await?;
        let message_bus = self.message_bus.clone();

        // Read rig config before spawning
        let rig_config = {
            let config = self.config.read().await;
            config.rig.clone()
        };

        // Use mock rig only if explicitly requested via env var
        let use_mock = std::env::var("PANCETTA_MOCK_RIG")
            .map(|v| v.to_lowercase() == "true" || v == "1")
            .unwrap_or(false);
        let rig_enabled = rig_config.interface.enabled && !use_mock;

        // Spawn rigctld as a managed child process if rig is enabled
        // and no external rigctld is already running
        let rigctld_port: u16 = std::env::var("RIGCTLD_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(4532);
        let rigctld_host =
            std::env::var("RIGCTLD_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());

        // SECURITY: rigctld talks to the radio over an unauthenticated TCP
        // socket. The default 127.0.0.1 keeps it loopback-only; if the user
        // explicitly sets RIGCTLD_HOST to a non-loopback address, anyone who
        // can reach that port can drive the rig (key TX, change frequency,
        // etc.). We don't outright block it because some operators have a
        // legitimate reason to expose rigctld on a private network, but we
        // do log a prominent warning so it's not silent.
        if rigctld_host != "127.0.0.1" && rigctld_host != "localhost" && rigctld_host != "::1" {
            warn!(
                "RIGCTLD_HOST is set to a non-loopback address ({}). The \
                 rigctld TCP port is unauthenticated; anyone who can reach \
                 it can drive the radio. Use a firewall or revert to \
                 127.0.0.1 if you didn't intend this.",
                rigctld_host
            );
        }

        if rig_enabled {
            // SECURITY: rig_config.interface.port is interpolated into the
            // rigctld -r argument and identifies the serial device the
            // daemon will open. Args are passed as a vec (no shell), so
            // command-injection isn't a risk, but a hostile config could
            // still ask rigctld to open an unrelated path. Restrict to the
            // shapes that look like a real serial / network rig spec:
            //   - /dev/tty*          (Linux/macOS USB-serial)
            //   - /dev/cu.*          (macOS callout devices)
            //   - COM<N>             (Windows)
            //   - host:port          (rigctld's network rig syntax)
            let port_field = &rig_config.interface.port;
            let looks_safe = port_field.starts_with("/dev/tty")
                || port_field.starts_with("/dev/cu.")
                || port_field.starts_with("COM")
                || port_field.contains(':');
            if !looks_safe && !port_field.is_empty() {
                warn!(
                    "Refusing to spawn rigctld with suspicious port path \
                     '{}'. Expected /dev/tty*, /dev/cu.*, COM<N>, or \
                     host:port — adjust station.interface.port in config.",
                    port_field
                );
                return Ok(());
            }

            // Check if rigctld is already running
            let already_running =
                tokio::net::TcpStream::connect(format!("{}:{}", rigctld_host, rigctld_port))
                    .await
                    .is_ok();

            if already_running {
                info!(
                    "rigctld already running on {}:{}",
                    rigctld_host, rigctld_port
                );
            } else if let Some(model_id) = Self::hamlib_model_id(&rig_config.model) {
                // rigctld knows the correct serial parameters (stop bits, parity,
                // flow control) for each rig model -- we only need to specify
                // model, port, and baud rate.
                info!(
                    "Spawning rigctld: model={} (hamlib {}), port={}, baud={}",
                    rig_config.model,
                    model_id,
                    rig_config.interface.port,
                    rig_config.interface.baud_rate
                );

                match std::process::Command::new("rigctld")
                    .args([
                        "-m",
                        &model_id.to_string(),
                        "-r",
                        &rig_config.interface.port,
                        "-s",
                        &rig_config.interface.baud_rate.to_string(),
                        "-t",
                        &rigctld_port.to_string(),
                        "-T",
                        &rigctld_host,
                    ])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                {
                    Ok(child) => {
                        info!("rigctld spawned (PID {})", child.id());
                        self.rigctld_process = Some(child);
                        // Give rigctld time to bind the port and open the serial device
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to spawn rigctld: {}. Install hamlib: brew install hamlib",
                            e
                        );
                    }
                }
            } else {
                warn!(
                    "Unknown rig model '{}' -- cannot determine hamlib ID. \
                     Set RIGCTLD_HOST/RIGCTLD_PORT to use an external rigctld.",
                    rig_config.model
                );
            }
        }

        let operating_frequency_hz = self.operating_frequency_hz.clone();
        let rig_conn_state = self.rig_conn_state.clone();

        let hamlib_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                let rig: Box<dyn pancetta_hamlib::RigControl + Send + Sync> = if !rig_enabled {
                    info!("Rig control disabled, using mock rig");
                    Box::new(pancetta_hamlib::MockRig::default())
                } else {
                    info!("Connecting to rigctld at {}:{}", rigctld_host, rigctld_port);

                    let config = pancetta_hamlib::RigctldConfig {
                        host: rigctld_host,
                        port: rigctld_port,
                        ..Default::default()
                    };
                    Box::new(pancetta_hamlib::RigctldClient::new(config))
                };

                match rig.connect().await {
                    Ok(_) => {
                        info!("Rig connected successfully");
                        // Only flag a *real* CAT link as Connected — a mock rig
                        // (rig control disabled) stays NotConnected so the TUI
                        // badge never claims a radio is attached when none is.
                        if rig_enabled {
                            rig_conn_state
                                .store(RigConnState::Connected.as_u8(), Ordering::Relaxed);
                        }
                        // Read the rig's current frequency immediately so we start
                        // on whatever band the radio is already tuned to, rather
                        // than assuming 20m.
                        match rig.get_frequency(pancetta_hamlib::Vfo::Current).await {
                            Ok(freq) => {
                                operating_frequency_hz.store(freq, Ordering::Relaxed);
                                info!(
                                    "Rig initial frequency: {} Hz ({:.3} MHz)",
                                    freq,
                                    freq as f64 / 1_000_000.0
                                );
                            }
                            Err(e) => {
                                warn!("Could not read initial rig frequency: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to connect to rig: {}. Continuing without.", e);
                        rig_conn_state.store(RigConnState::NotConnected.as_u8(), Ordering::Relaxed);
                    }
                }

                // Polling task
                let rig_poll = Arc::new(rig);
                let rig_for_polling = Arc::clone(&rig_poll);
                let shutdown_for_polling = shutdown.clone();
                let op_freq_for_polling = operating_frequency_hz.clone();
                let rig_conn_state_poll = rig_conn_state.clone();
                let mut spawned_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

                spawned_handles.push(tokio::spawn(async move {
                    let mut poll_interval = interval(Duration::from_millis(500));
                    let mut consecutive_failures: u32 = 0;
                    const CRASH_WARN_THRESHOLD: u32 = 10; // 5 seconds of failures
                    // S-meter poll: every 4th frequency tick (one
                    // STRENGTH read per 2s). Modest on purpose — each
                    // read is a rigctld round-trip on the same serial
                    // CAT link the TX path uses, and the TUI only
                    // renders it for situational awareness.
                    const S_METER_EVERY_N_TICKS: u32 = 4;
                    let mut tick_count: u32 = 0;

                    while !shutdown_for_polling.load(Ordering::Acquire) {
                        poll_interval.tick().await;
                        tick_count = tick_count.wrapping_add(1);

                        let poll_ok = if let Ok(status) = rig_for_polling.get_status().await {
                            if status.connection_state
                                == pancetta_hamlib::ConnectionState::Connected
                            {
                                if let Ok(freq) = rig_for_polling
                                    .get_frequency(pancetta_hamlib::Vfo::Current)
                                    .await
                                {
                                    // Update shared operating frequency for spot reporters
                                    op_freq_for_polling.store(freq, Ordering::Relaxed);
                                    let message = ComponentMessage::new(
                                        ComponentId::Hamlib,
                                        ComponentId::Tui,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::FrequencyResponse {
                                                vfo: 0,
                                                frequency: freq,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    let _ = message_bus.send_message(message).await;

                                    // Batch 95: real rig S-meter for the
                                    // TUI. Best-effort — a failed read
                                    // (rig busy, no STRENGTH support)
                                    // skips the update rather than
                                    // counting as a poll failure; the
                                    // TUI shows the reading as stale
                                    // after 10s of silence.
                                    if tick_count.is_multiple_of(S_METER_EVERY_N_TICKS) {
                                        if let Ok(db) = rig_for_polling.get_s_meter().await {
                                            let s_msg = ComponentMessage::new(
                                                ComponentId::Hamlib,
                                                ComponentId::Tui,
                                                MessageType::RigControl(
                                                    crate::message_bus::RigControlMessage::SignalStrengthResponse {
                                                        db_over_s9: db,
                                                    },
                                                ),
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(s_msg).await;
                                        }
                                    }
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        } else {
                            false
                        };

                        if poll_ok {
                            // Recovered (or steady) — reflect Connected for a
                            // real rig so a transient blip clears the badge.
                            if consecutive_failures > 0 && rig_enabled {
                                rig_conn_state_poll.store(
                                    RigConnState::Connected.as_u8(),
                                    Ordering::Relaxed,
                                );
                            }
                            consecutive_failures = 0;
                        } else {
                            consecutive_failures += 1;
                            if consecutive_failures == CRASH_WARN_THRESHOLD {
                                warn!(
                                    "Rig polling has failed {} consecutive times -- rigctld may have crashed. \
                                     Check rigctld process and restart Pancetta if needed.",
                                    consecutive_failures
                                );
                                // Surface the degraded state to the TUI badge.
                                if rig_enabled {
                                    rig_conn_state_poll.store(
                                        RigConnState::PollingFailed.as_u8(),
                                        Ordering::Relaxed,
                                    );
                                }
                            }
                        }
                    }
                }));

                // PTT safety watchdog: force PTT off if a transmission runs
                // longer than expected. FT8 transmissions are 12.64s within a
                // 15s slot, so 14s is a safe ceiling — long enough for any
                // legitimate FT8 TX, short enough to never bleed into the
                // next slot. Catches stuck/crashed pipelines.
                const PTT_SAFETY_TIMEOUT_SECS: u64 = 14;
                let ptt_on_since: Arc<RwLock<Option<Instant>>> = Arc::new(RwLock::new(None));

                // Spawn the PTT watchdog as a background task
                let rig_for_watchdog = Arc::clone(&rig_poll);
                let ptt_watchdog_tracker = ptt_on_since.clone();
                let shutdown_for_watchdog = shutdown.clone();
                spawned_handles.push(tokio::spawn(async move {
                    let mut watchdog_interval = interval(Duration::from_secs(1));
                    loop {
                        watchdog_interval.tick().await;
                        if shutdown_for_watchdog.load(Ordering::Acquire) {
                            break;
                        }

                        let ptt_time = {
                            let guard = ptt_watchdog_tracker.read().await;
                            *guard
                        };

                        if let Some(on_since) = ptt_time {
                            if on_since.elapsed() > Duration::from_secs(PTT_SAFETY_TIMEOUT_SECS) {
                                error!(
                                    "PTT SAFETY WATCHDOG: PTT has been on for >{} seconds -- forcing OFF",
                                    PTT_SAFETY_TIMEOUT_SECS
                                );
                                match rig_for_watchdog
                                    .set_ptt(
                                        pancetta_hamlib::Vfo::Current,
                                        pancetta_hamlib::PttState::Off,
                                    )
                                    .await
                                {
                                    Ok(_) => {
                                        warn!("PTT SAFETY WATCHDOG: PTT forced off successfully");
                                        // Only clear timer on success -- retry on next tick if it fails
                                        let mut guard = ptt_watchdog_tracker.write().await;
                                        *guard = None;
                                    }
                                    Err(e) => {
                                        error!(
                                            "PTT SAFETY WATCHDOG: failed to force PTT off: {} -- will retry in 1s",
                                            e
                                        );
                                    }
                                }
                            }
                        }
                    }
                }));

                // Process messages
                while !shutdown.load(Ordering::Acquire) {
                    match hamlib_rx.try_recv() {
                        Ok(message) => {
                            if let MessageType::RigControl(ref rig_msg) = message.message_type {
                                match rig_msg {
                                    crate::message_bus::RigControlMessage::SetFrequency {
                                        vfo,
                                        frequency,
                                    } => {
                                        let vfo_enum = if *vfo == 0 {
                                            pancetta_hamlib::Vfo::A
                                        } else {
                                            pancetta_hamlib::Vfo::B
                                        };
                                        if let Err(e) =
                                            rig_poll.set_frequency(vfo_enum, *frequency).await
                                        {
                                            error!("Failed to set frequency: {}", e);
                                        }
                                    }
                                    crate::message_bus::RigControlMessage::SetPtt { state } => {
                                        // Update PTT watchdog tracker
                                        {
                                            let mut guard = ptt_on_since.write().await;
                                            if *state {
                                                // PTT going on -- record the time
                                                if guard.is_none() {
                                                    *guard = Some(Instant::now());
                                                    debug!("PTT watchdog: PTT ON, timer started");
                                                }
                                            } else {
                                                // PTT going off -- clear the timer
                                                *guard = None;
                                                debug!("PTT watchdog: PTT OFF, timer cleared");
                                            }
                                        }

                                        let ptt = if *state {
                                            pancetta_hamlib::PttState::On
                                        } else {
                                            pancetta_hamlib::PttState::Off
                                        };
                                        if let Err(e) = rig_poll
                                            .set_ptt(pancetta_hamlib::Vfo::Current, ptt)
                                            .await
                                        {
                                            error!("Failed to set PTT: {}", e);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                    }
                }

                // Cancel spawned polling/watchdog tasks on shutdown
                for handle in spawned_handles {
                    handle.abort();
                }

                info!("Hamlib component stopped");
                Ok(())
            })
        };

        self.named_task_handles
            .push((ComponentId::Hamlib, hamlib_handle));
        info!("Hamlib component started");
        Ok(())
    }
}
