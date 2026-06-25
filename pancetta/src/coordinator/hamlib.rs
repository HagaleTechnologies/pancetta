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

    /// SECURITY (I-10 / I-11): validate the `station.interface.port` device
    /// spec before handing it to rigctld's `-r` argument. Accepts only shapes
    /// that look like a real serial device or a `host:port` network rig.
    /// Linux serial: `/dev/ttyUSB<N>`, `/dev/ttyACM<N>`, `/dev/ttyS<N>`.
    /// macOS serial: `/dev/cu.*`, `/dev/tty.*` (dev machine uses
    /// `/dev/cu.usbserial-*`). Windows serial: `COM<N>`. Network rig:
    /// `host:port`, where `port` parses as a `u16` in `1..=65535` (I-11
    /// port-range check). Everything else (bare `/dev/tty`, `/dev/null`,
    /// malformed/out-of-range network ports, arbitrary paths) is rejected.
    pub(crate) fn device_path_looks_safe(port_field: &str) -> bool {
        // Linux serial: /dev/ttyUSB<N>, /dev/ttyACM<N>, /dev/ttyS<N> with a
        // trailing all-digit index.
        let linux_serial = |prefix: &str| {
            port_field
                .strip_prefix(prefix)
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        };
        if linux_serial("/dev/ttyUSB") || linux_serial("/dev/ttyACM") || linux_serial("/dev/ttyS") {
            return true;
        }
        // macOS callout/tty devices: /dev/cu.* and /dev/tty.* (require a
        // non-empty suffix after the dot so a bare "/dev/tty" is rejected).
        if let Some(suffix) = port_field
            .strip_prefix("/dev/cu.")
            .or_else(|| port_field.strip_prefix("/dev/tty."))
        {
            return !suffix.is_empty();
        }
        // Windows serial: COM<N>.
        if let Some(n) = port_field.strip_prefix("COM") {
            return !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
        }
        // Network rig: host:port. Port must be a valid u16 (1..=65535).
        // rsplit so IPv6-ish hosts still parse on the final ':' segment;
        // host non-emptiness is required, but host *content* stays a warn
        // (see RIGCTLD_HOST handling) for remote-rig operability.
        if let Some((host, port)) = port_field.rsplit_once(':') {
            if host.is_empty() {
                return false;
            }
            return matches!(port.parse::<u16>(), Ok(p) if p >= 1);
        }
        false
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

        // SECURITY (I-11): rigctld talks to the radio over an unauthenticated
        // TCP socket. The default 127.0.0.1 keeps it loopback-only; if the
        // user explicitly sets RIGCTLD_HOST to a non-loopback address, anyone
        // who can reach that port can drive the rig (key TX, change frequency,
        // etc.). We deliberately keep this a *warning*, not a hard reject:
        // some operators legitimately run rigctld on a separate machine
        // (remote rig) and a hard block would break them. (Port-range
        // validation for any `host:port` device spec is enforced below.)
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
            // SECURITY (I-10): rig_config.interface.port is interpolated into
            // the rigctld -r argument and identifies the serial device the
            // daemon will open. Args are passed as a vec (no shell), so
            // command-injection isn't a risk, but a hostile/typo'd config
            // could still ask rigctld to open an unrelated path. Restrict to
            // the shapes that look like a real serial / network rig spec
            // (see `device_path_looks_safe`):
            //   - /dev/ttyUSB<N> / ttyACM<N> / ttyS<N>   (Linux USB-serial)
            //   - /dev/cu.* and /dev/tty.*               (macOS — dev machine)
            //   - COM<N>                                 (Windows)
            //   - host:port                              (rigctld network rig)
            let port_field = &rig_config.interface.port;
            if !port_field.is_empty() && !Self::device_path_looks_safe(port_field) {
                warn!(
                    "Refusing to spawn rigctld with suspicious port path \
                     '{}'. Expected /dev/ttyUSB<N>|ttyACM<N>|ttyS<N>, \
                     /dev/cu.*, /dev/tty.*, COM<N>, or host:port (valid \
                     1-65535 port) — adjust station.interface.port in config.",
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
        let ptt_active = self.ptt_active.clone();
        let rig_conn_state = self.rig_conn_state.clone();
        // C9 dedup anchor (most recent pancetta-initiated SetFrequency) — the
        // poll loop reads it to tell an operator dial move (tear down) from a
        // pancetta-commanded change (already torn down by the TUI / autonomous
        // site; must NOT double-fire).
        let last_freq_command = self.last_freq_command.clone();

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
                let ptt_active_poll = ptt_active.clone();
                let rig_conn_state_poll = rig_conn_state.clone();
                // C9 dial-poll teardown plumbing.
                let last_freq_command_poll = last_freq_command.clone();
                let bus_for_polling = message_bus.clone();
                let mut spawned_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

                spawned_handles.push(tokio::spawn(async move {
                    let mut poll_interval = interval(Duration::from_millis(500));
                    let mut consecutive_failures: u32 = 0;
                    const CRASH_WARN_THRESHOLD: u32 = 10; // 5 seconds of failures
                    // C9 dial-poll band-change detection: the frequency this
                    // poll loop last *accepted* as the current dial. Seeded to
                    // the rig's already-read startup frequency so the first poll
                    // doesn't false-fire (`is_band_change(0, _)` is also false,
                    // belt and braces). Updated only when the loop accepts a new
                    // reading, so a teardown fires at most once per dial move.
                    let mut last_seen_freq: u64 = op_freq_for_polling.load(Ordering::Relaxed);
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

                                    // C9 — operator turned the rig's dial. We
                                    // learn of the new dial freq by polling (not
                                    // via the TUI), so this is a band change
                                    // pancetta did not initiate. If it's a real
                                    // band change (not a tiny fine-tune wobble)
                                    // AND not attributable to a freq pancetta
                                    // itself just commanded (the TUI / autonomous
                                    // site already fired the teardown, and the
                                    // rig may still be settling), fire the same
                                    // BandChanged teardown. `last_seen_freq` is
                                    // only advanced once we've decided, so a
                                    // single dial move tears down at most once.
                                    if super::is_band_change(last_seen_freq, freq) {
                                        let cmd = last_freq_command_poll
                                            .lock()
                                            .ok()
                                            .and_then(|g| *g);
                                        let attributable =
                                            super::band_change_attributable_to_command(
                                                freq,
                                                cmd,
                                                Instant::now(),
                                            );
                                        if attributable {
                                            // pancetta commanded this (or the rig
                                            // is still slewing to it) — accept the
                                            // reading without a second teardown.
                                            last_seen_freq = freq;
                                        } else {
                                            info!(
                                                target: "operator.override",
                                                "Rig dial band change {} Hz -> {} Hz (operator) — tearing down active QSOs",
                                                last_seen_freq, freq
                                            );
                                            let teardown = ComponentMessage::new(
                                                ComponentId::Hamlib,
                                                ComponentId::Qso,
                                                MessageType::QsoMessage(
                                                    crate::message_bus::QsoMessage::BandChanged {
                                                        previous_hz: last_seen_freq,
                                                        new_hz: freq,
                                                    },
                                                ),
                                                Instant::now(),
                                            );
                                            if let Err(e) =
                                                bus_for_polling.send_message(teardown).await
                                            {
                                                warn!(
                                                    "Rig dial band change: failed to send teardown: {}",
                                                    e
                                                );
                                            }
                                            last_seen_freq = freq;
                                        }
                                    } else if freq != last_seen_freq {
                                        // Same-band fine-tune / wobble: track the
                                        // new reading but don't tear anything down.
                                        last_seen_freq = freq;
                                    }

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

                                    // SWR — only meaningful while keyed (needs
                                    // forward power). Poll every tick during TX so
                                    // the status bar tracks across the ~12.6s
                                    // burst; skipped entirely on RX. Best-effort,
                                    // like the S-meter read.
                                    if ptt_active_poll.load(Ordering::Acquire) {
                                        if let Ok(swr) = rig_for_polling.get_swr().await {
                                            let swr_msg = ComponentMessage::new(
                                                ComponentId::Hamlib,
                                                ComponentId::Tui,
                                                MessageType::RigControl(
                                                    crate::message_bus::RigControlMessage::SwrResponse {
                                                        swr,
                                                    },
                                                ),
                                                Instant::now(),
                                            );
                                            let _ = message_bus.send_message(swr_msg).await;
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
                                        match rig_poll
                                            .set_ptt(pancetta_hamlib::Vfo::Current, ptt)
                                            .await
                                        {
                                            Ok(()) => info!(
                                                target: "tx.ptt",
                                                "rig set_ptt {} OK",
                                                if *state { "ON" } else { "OFF" }
                                            ),
                                            Err(e) => error!("Failed to set PTT: {}", e),
                                        }
                                    }
                                    crate::message_bus::RigControlMessage::SetSplit {
                                        enabled,
                                        tx_frequency,
                                    } => {
                                        if *enabled {
                                            if let Err(e) =
                                                rig_poll.set_split_freq(*tx_frequency).await
                                            {
                                                warn!(target: "rig.split", "set_split_freq failed: {}", e);
                                            }
                                            if let Err(e) = rig_poll
                                                .set_split(true, pancetta_hamlib::Vfo::B)
                                                .await
                                            {
                                                warn!(target: "rig.split", "set_split(on) failed: {}", e);
                                            } else {
                                                info!(target: "rig.split", "split ON, TX {} Hz", tx_frequency);
                                            }
                                        } else if let Err(e) =
                                            rig_poll.set_split(false, pancetta_hamlib::Vfo::A).await
                                        {
                                            warn!(target: "rig.split", "set_split(off) failed: {}", e);
                                        } else {
                                            info!(target: "rig.split", "split OFF");
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

#[cfg(test)]
mod device_path_tests {
    use super::*;

    #[test]
    fn accepts_real_serial_and_network_shapes() {
        let ok = [
            // Linux serial
            "/dev/ttyUSB0",
            "/dev/ttyUSB10",
            "/dev/ttyACM0",
            "/dev/ttyS0",
            // macOS (dev machine: /dev/cu.usbserial-*)
            "/dev/cu.usbserial-1410",
            "/dev/tty.usbserial-1410",
            // Windows
            "COM3",
            "COM12",
            // network rig
            "127.0.0.1:4532",
            "192.168.1.50:4532",
            "myrig.local:65535",
            "myrig.local:1",
        ];
        for p in ok {
            assert!(
                super::super::ApplicationCoordinator::device_path_looks_safe(p),
                "expected {p:?} to be accepted"
            );
        }
    }

    #[test]
    fn rejects_bogus_paths_and_bad_ports() {
        let bad = [
            // bare/loose serial roots that the old starts_with("/dev/tty") let through
            "/dev/tty",
            "/dev/ttyZZZ",
            "/dev/null",
            "/dev/cu.",
            "/dev/tty.",
            "/etc/passwd",
            "COM",
            "COMx",
            // network: bad / out-of-range / missing ports
            "myrig.local:0",
            "myrig.local:70000",
            "myrig.local:abc",
            "myrig.local:",
            ":4532",
            // arbitrary
            "rm -rf /",
            "hello",
        ];
        for p in bad {
            assert!(
                !super::super::ApplicationCoordinator::device_path_looks_safe(p),
                "expected {p:?} to be rejected"
            );
        }
    }
}
