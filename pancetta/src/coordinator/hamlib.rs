use anyhow::Result;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{debug, error, info, span, warn, Level};

/// Maximum time to wait for the rig's initial connect + frequency read before
/// proceeding with startup.  A QSO takes ≥ 15 s (one FT8 slot) to complete,
/// so anything under that is safe.  If the rig misses this window the poll
/// loop (every 500 ms) will catch up; only the very first seconds of operation
/// may stamp the wrong band.
const RIG_INITIAL_READ_TIMEOUT: Duration = Duration::from_secs(8);

pub(crate) struct HamlibChildren {
    pub(crate) poll: tokio::task::AbortHandle,
    pub(crate) watchdog: tokio::task::AbortHandle,
}

/// Whether a spawned Hamlib child task (poll loop or PTT watchdog)
/// terminated unexpectedly and the outer message-loop task should treat
/// that as a crash.
///
/// Both children exit cleanly (and expectedly) once `shutdown` flips. A
/// child can observe `shutdown` and return between the outer loop's own
/// `!shutdown.load(...)` guard and this check running -- without
/// re-checking `shutdown` here too, that race gets misclassified as a
/// crash: `teardown_hamlib()` runs (up to ~10s of PTT-off retries) and a
/// Hamlib restart gets dispatched while the process is shutting down
/// (PAN-19 MEDIUM #1).
///
/// Order matters here (PAN-19 round-1 review, Codex P2): evaluate
/// `is_finished()` FIRST and only THEN re-check `shutdown`, not the other
/// way around. Checking `shutdown` before `is_finished()` still leaves a
/// gap -- if `shutdown` flips true immediately after that read returns
/// `false`, a child can observe the flip, exit because of it, and have
/// `is_finished()` see that exit, but the bail decision would still be
/// gated on the STALE pre-flip `shutdown` read, so it fires anyway. By
/// checking `is_finished()` first (a child must actually be finished for
/// the `&&` to even reach the second operand) and re-reading `shutdown`
/// only at that point -- as close in time to the observed exit as
/// possible -- the second read is far more likely to already reflect a
/// flip that caused it.
pub(crate) fn child_task_crashed(
    shutdown: &std::sync::atomic::AtomicBool,
    spawned_handles: &[tokio::task::JoinHandle<()>],
) -> bool {
    spawned_handles.iter().any(|handle| handle.is_finished()) && !shutdown.load(Ordering::Acquire)
}

use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};

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
    pub(crate) async fn teardown_hamlib(&mut self) {
        for orphan in self.hamlib_orphans.drain(..) {
            orphan.abort();
        }

        let mut ptt_off = false;
        if let Some(rig) = self.rig_handle.as_ref() {
            for attempt in 1..=3 {
                match rig
                    .set_ptt(
                        pancetta_hamlib::Vfo::Current,
                        pancetta_hamlib::PttState::Off,
                    )
                    .await
                {
                    Ok(()) => {
                        ptt_off = true;
                        self.ptt_active.store(false, Ordering::Release);
                        break;
                    }
                    Err(error) => {
                        warn!("Hamlib teardown PTT-off attempt {attempt} failed: {error}");
                        // PAN-19 LOW: this loop runs on the coordinator's main
                        // supervision task (`run_main_loop`'s `select!` ->
                        // `check_task_handles` -> `handle_finished_task` ->
                        // `teardown_component` -> here), so all 3 attempts x
                        // up to 3s send + this 500ms sleep (up to ~10s worst
                        // case) stalls that task's own re-entry into
                        // `select!` -- a second, concurrent component
                        // failure sits undiscovered in `named_task_handles`
                        // until this returns. A full concurrency restructure
                        // (spawning this loop off the supervision task) risks
                        // the PTT-off-before-next-generation TX-safety
                        // ordering guarantee this loop exists to provide, so
                        // instead: skip only this inter-attempt sleep when
                        // another component's task has ALREADY finished
                        // (visible right now in `named_task_handles`) --
                        // that failure gets discovered and processed that
                        // much sooner, without touching PTT-off retry
                        // semantics or ordering at all.
                        let another_failure_pending = self
                            .named_task_handles
                            .iter()
                            .any(|(id, handle)| *id != ComponentId::Hamlib && handle.is_finished());
                        if !another_failure_pending {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                    }
                }
            }
        }

        if let Some(children) = self.hamlib_children.take() {
            children.poll.abort();
            if ptt_off {
                children.watchdog.abort();
            } else {
                self.hamlib_orphans.push(children.watchdog);
                super::tx::emit_diagnostic(
                    &self.message_bus,
                    "supervisor",
                    pancetta_core::DiagnosticLevel::Error,
                    "Hamlib teardown could not force PTT off; watchdog retained".to_string(),
                    None,
                )
                .await;
            }
        }

        if let Ok((sender, receiver)) = self
            .message_bus
            .get_or_create_channel(ComponentId::Hamlib)
            .await
        {
            let mut safe_messages = Vec::new();
            while let Ok(message) = receiver.try_recv() {
                // A stale key-up is the only command that is unsafe after a
                // restart. Preserve unkeys, frequency, mode, and split state
                // so teardown cannot silently desynchronize the rig.
                if !matches!(
                    message.message_type,
                    MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                        state: true
                    })
                ) {
                    safe_messages.push(message);
                }
            }
            for message in safe_messages {
                // PAN-19 LOW: `Sender::send` on a bounded crossbeam channel
                // BLOCKS the current thread if the channel is full. The
                // drain above just freed the same number of slots this
                // replays, so normally there's room -- but a concurrent
                // producer filling the channel in between (this whole
                // sequence has `.await` points in it) could still leave it
                // full by the time we get here, which would block this
                // async fn's underlying executor thread instead of
                // yielding. Use `try_send` and drop-with-a-warning on
                // `Full` instead of blocking.
                if let Err(e) = sender.try_send(message) {
                    warn!(
                        "Hamlib teardown: dropping a replayed message, channel still full: {}",
                        e
                    );
                }
            }
        }
    }

    /// Map rig model name to hamlib model number.
    /// See: https://github.com/Hamlib/Hamlib/wiki/Supported-Radios
    /// Public so `pancetta doctor` (in the bin crate) can pre-validate the
    /// configured model against the same table the spawner uses.
    #[cfg(feature = "pancetta-hamlib")]
    pub fn hamlib_model_id(model: &str) -> Option<u32> {
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

        if self
            .rigctld_process
            .as_mut()
            .is_some_and(|child| child.try_wait().ok().flatten().is_some())
        {
            self.rigctld_process.take();
        }

        let (_hamlib_tx, hamlib_rx) = self
            .message_bus
            .get_or_create_channel(ComponentId::Hamlib)
            .await?;
        let message_bus = self.message_bus.clone();
        let display_feed_enabled = self.display_feed_enabled.clone();

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
                report_rig_error(
                    &self.message_bus,
                    format!(
                        "Rig CAT disabled: suspicious [rig.interface].port '{port_field}' — \
                         run `pancetta setup` to pick a real serial port."
                    ),
                )
                .await;
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
                            "Failed to spawn rigctld: {}. Install hamlib: {}",
                            e,
                            hamlib_install_hint()
                        );
                        report_rig_error(
                            &self.message_bus,
                            format!(
                                "rigctld failed to start ({e}) — no CAT/PTT this session. \
                                 Fix: {}",
                                hamlib_install_hint()
                            ),
                        )
                        .await;
                    }
                }
            } else {
                warn!(
                    "Unknown rig model '{}' -- cannot determine hamlib ID. \
                     Set RIGCTLD_HOST/RIGCTLD_PORT to use an external rigctld.",
                    rig_config.model
                );
                report_rig_error(
                    &self.message_bus,
                    format!(
                        "Unknown rig model '{}' — cannot spawn rigctld (no CAT/PTT). \
                         Run `pancetta setup` to pick a supported model, or run an \
                         external rigctld and set RIGCTLD_HOST/RIGCTLD_PORT.",
                        rig_config.model
                    ),
                )
                .await;
            }
        }

        let rig: Arc<dyn pancetta_hamlib::RigControl + Send + Sync> = if !rig_enabled {
            info!("Rig control disabled, using mock rig");
            Arc::new(pancetta_hamlib::MockRig::default())
        } else {
            info!("Connecting to rigctld at {}:{}", rigctld_host, rigctld_port);
            Arc::new(pancetta_hamlib::RigctldClient::new(
                pancetta_hamlib::RigctldConfig {
                    host: rigctld_host.clone(),
                    port: rigctld_port,
                    ..Default::default()
                },
            ))
        };
        self.rig_handle = Some(rig.clone());

        let operating_frequency_hz = self.operating_frequency_hz.clone();
        let ptt_active = self.ptt_active.clone();
        let last_ptt_on_ms = self.last_ptt_on_ms.clone();
        let tx_restart_inhibit = self.tx_restart_inhibit.clone();
        let rig_conn_state = self.rig_conn_state.clone();
        // C9 dedup anchor (most recent pancetta-initiated SetFrequency) — the
        // poll loop reads it to tell an operator dial move (tear down) from a
        // pancetta-commanded change (already torn down by the TUI / autonomous
        // site; must NOT double-fire).
        let last_freq_command = self.last_freq_command.clone();

        // Oneshot used to gate startup: the spawned task signals here once the
        // initial connect + get_frequency have completed (or failed).  We await
        // this with a bounded timeout so the QSO pipeline doesn't go live while
        // the dial atomic is still 0, which would cause the first-slot QSO
        // completion to log the wrong band.
        let (initial_read_tx, initial_read_rx) = tokio::sync::oneshot::channel::<()>();
        let (children_tx, children_rx) = tokio::sync::oneshot::channel();

        let hamlib_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                // PAN-19 HIGH: the poll + PTT-watchdog child tasks are spawned
                // and their abort handles published via `children_tx` FIRST,
                // BEFORE the initial `rig.connect()` / PTT-off / frequency-read
                // sequence below -- deliberately, not incidentally. Spawning
                // them doesn't need a live connection: `rig_poll` is a cheap
                // `Arc::clone` of the same `Arc<dyn RigControl>` `rig` (no
                // network I/O to construct), and the poll loop already
                // tolerates a not-yet-connected rig fine (`get_status()`
                // returns `Disconnected` gracefully rather than erroring, so
                // the first tick or two just count as a `consecutive_failures`
                // poll miss, same as any other transient failure).
                //
                // Previously `children_tx.send` happened AFTER the connect
                // sequence, at essentially the same wall-clock time as
                // `initial_read_tx.send` (only a few non-blocking lines
                // later). Worst case that sequence budgets ~5s (rig connect)
                // + 3s (`set_ptt` retry) + 3s (`get_frequency` retry) ≈ 11s.
                // The caller first awaits `initial_read_rx` with an 8s
                // timeout (`RIG_INITIAL_READ_TIMEOUT`) and, on timeout, logs
                // a warning and continues (fine, working as intended) --
                // but it THEN awaits `children_rx` with a hardcoded 1s
                // timeout starting from t=8s. If T ≈ 11s, `children_tx`
                // fires outside that 8s-9s window, so the 1s wait times out
                // too -- and THAT timeout aborts the task and bails out of
                // `start_hamlib_component` entirely, failing startup (or
                // burning a restart attempt) on exactly the "rig is slow"
                // condition Hamlib restart exists to recover from. Spawning
                // the children up front means `children_tx` fires almost
                // immediately on task start, independent of how long
                // `rig.connect()`/`get_frequency()` take.
                // `message_bus` itself gets moved into the poll task below
                // (it's used there directly, not via a separately-named
                // clone); the connect sequence -- now running AFTER that
                // spawn -- needs its own clone to report a connect failure.
                let message_bus_for_connect = message_bus.clone();
                let rig_poll = Arc::clone(&rig);
                let rig_for_polling = Arc::clone(&rig_poll);
                let shutdown_for_polling = shutdown.clone();
                let op_freq_for_polling = operating_frequency_hz.clone();
                let ptt_active_poll = ptt_active.clone();
                let rig_conn_state_poll = rig_conn_state.clone();
                // C9 dial-poll teardown plumbing.
                let last_freq_command_poll = last_freq_command.clone();
                let bus_for_polling = message_bus.clone();
                let display_feed_enabled_poll = display_feed_enabled.clone();
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
                                    super::remote_gateway::relay_to_gateway(
                                        &message_bus,
                                        &display_feed_enabled_poll,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::FrequencyResponse {
                                                vfo: 0,
                                                frequency: freq,
                                            },
                                        ),
                                    )
                                    .await;

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
                                            super::remote_gateway::relay_to_gateway(
                                                &message_bus,
                                                &display_feed_enabled_poll,
                                                ComponentId::Hamlib,
                                                MessageType::RigControl(
                                                    crate::message_bus::RigControlMessage::SignalStrengthResponse {
                                                        db_over_s9: db,
                                                    },
                                                ),
                                            )
                                            .await;
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
                let initial_ptt_on = if ptt_active.load(Ordering::Acquire) {
                    let last_ms = super::now_epoch_ms()
                        .saturating_sub(last_ptt_on_ms.load(Ordering::Acquire));
                    let now = Instant::now();
                    Some(
                        now.checked_sub(Duration::from_millis(last_ms))
                            .unwrap_or(now),
                    )
                } else {
                    None
                };
                let ptt_on_since: Arc<RwLock<Option<Instant>>> =
                    Arc::new(RwLock::new(initial_ptt_on));

                // Spawn the PTT watchdog as a background task
                let rig_for_watchdog = Arc::clone(&rig_poll);
                let ptt_watchdog_tracker = ptt_on_since.clone();
                let shutdown_for_watchdog = shutdown.clone();
                let ptt_active_watchdog = ptt_active.clone();
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
                                        ptt_active_watchdog.store(false, Ordering::Release);
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

                let _ = children_tx.send(HamlibChildren {
                    poll: spawned_handles[0].abort_handle(),
                    watchdog: spawned_handles[1].abort_handle(),
                });

                // Initial connect + PTT-off + frequency-read sequence. Uses
                // `rig` directly (not `rig_poll`) -- both are clones of the
                // same underlying `Arc<dyn RigControl>`, so this is
                // equivalent, but keeps the polling task's own capture
                // (`rig_poll`/`rig_for_polling`) visibly separate from this
                // one-shot startup sequence.
                match rig.connect().await {
                    Ok(_) => {
                        info!("Rig connected successfully");
                        if let Err(e) = rig
                            .set_ptt(
                                pancetta_hamlib::Vfo::Current,
                                pancetta_hamlib::PttState::Off,
                            )
                            .await
                        {
                            warn!("Startup PTT-off failed: {e}");
                        }
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
                        if rig_enabled {
                            report_rig_error(
                                &message_bus_for_connect,
                                format!(
                                    "Rig connect failed ({e}) — RIG badge is ✗. Check the radio \
                                     is powered on and the USB cable; verify with \
                                     `pancetta test-rig`, then restart pancetta."
                                ),
                            )
                            .await;
                        }
                    }
                }

                // Signal that the initial connect + read sequence is done (or
                // gave up).  The receiver in start_hamlib_component is waiting
                // with a bounded timeout; send errors are harmless (timeout
                // already elapsed).
                let _ = initial_read_tx.send(());

                // Process messages
                while !shutdown.load(Ordering::Acquire) {
                    if child_task_crashed(&shutdown, &spawned_handles) {
                        anyhow::bail!("Hamlib polling or PTT-watchdog child terminated");
                    }
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
                                        if *state && tx_restart_inhibit.load(Ordering::Acquire) != 0
                                        {
                                            warn!(
                                                "Discarding PTT-on while Hamlib restart inhibit is active"
                                            );
                                            continue;
                                        }
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
                                                target: "pancetta::tx.ptt",
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

        let hamlib_abort = hamlib_handle.abort_handle();
        self.named_task_handles
            .push((ComponentId::Hamlib, hamlib_handle));

        // On the rig-enabled path: block here (bounded) until the spawned task
        // has completed its initial connect + get_frequency sequence.  This
        // guarantees that `operating_frequency_hz` is non-zero before
        // `start_qso_component` runs, so a QSO that completes in the first
        // slot logs the correct band.
        //
        // On the mock / rig-disabled path: the receiver fires immediately after
        // the spawned task calls `initial_read_tx.send(())`, so there's no
        // meaningful delay.
        //
        // If the rig is slow or absent the timeout fires, we log a warning and
        // carry on — the poll loop (every 500 ms) will catch up; only the first
        // few seconds of operation could stamp band=0.
        //
        // PAN-5 verify fix (hamlib.rs restart handshake race): this wait is NOT
        // gated on `first_start`. teardown_hamlib() never clears self.rig_handle,
        // so first_start is false on every restart — gating this wait on it meant
        // a restart fell straight through to the children_rx wait below with only
        // its hardcoded 1s timeout, no time for the spawned task's connect()
        // (RigctldClient budgets 5000ms + retries) + set_ptt(Off) + get_frequency()
        // to complete. That guaranteed the 1s timeout fired on every restart,
        // aborting the task and bail!-ing startup — on exactly the condition
        // (rig/rigctld unreachable) a Hamlib restart exists to recover from.
        // Waiting here on every start means children_tx.send has typically
        // already fired (or is about to) by the time the 1s children_rx wait
        // below runs, regardless of first_start.
        if rig_enabled {
            match tokio::time::timeout(RIG_INITIAL_READ_TIMEOUT, initial_read_rx).await {
                Ok(_) => {
                    info!(
                        target: "rig",
                        "Rig initial frequency read complete before QSO pipeline start"
                    );
                }
                Err(_) => {
                    warn!(
                        target: "rig",
                        "Rig frequency not read within {}s at startup — band may be wrong \
                         until the rig responds or you press a band key",
                        RIG_INITIAL_READ_TIMEOUT.as_secs()
                    );
                }
            }
        }

        self.hamlib_children = Some(
            match tokio::time::timeout(Duration::from_secs(1), children_rx).await {
                Ok(Ok(children)) => children,
                Ok(Err(_)) => {
                    hamlib_abort.abort();
                    anyhow::bail!("Hamlib task exited before publishing child teardown handles");
                }
                Err(_) => {
                    hamlib_abort.abort();
                    anyhow::bail!("Hamlib child teardown handles were not published within 1s");
                }
            },
        );

        // PAN-19 MEDIUM #2: `teardown_hamlib` only drains+aborts
        // `hamlib_orphans` at the START of the NEXT teardown call. A
        // watchdog orphaned by a teardown whose 3 PTT-off attempts all
        // failed (see `teardown_hamlib` below) would otherwise survive
        // past THIS successful restart, holding a stale `ptt_on_since`
        // from the OLD generation and capable of firing `set_ptt(Off)` on
        // the NEW generation's rig client mid-transmission. Now that this
        // new generation has successfully started (children handles
        // published above), any such orphan is definitely stale -- abort
        // it here too, rather than waiting for a subsequent teardown.
        for orphan in self.hamlib_orphans.drain(..) {
            orphan.abort();
        }

        info!("Hamlib component started");
        Ok(())
    }
}

/// Platform-appropriate hamlib install hint (mirrors `pancetta doctor`).
fn hamlib_install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "brew install hamlib"
    } else if cfg!(target_os = "windows") {
        "install hamlib from https://hamlib.github.io and put rigctld.exe on PATH"
    } else {
        "sudo apt install libhamlib-utils"
    }
}

/// Surface a rig-setup failure to the operator: an ephemeral TUI error line
/// (`MessageType::Error` → error log/status) PLUS a retained
/// `DiagnosticEvent` (target `rig.cat`, Shift+D overlay) so the reason for a
/// RIG ✗ badge survives after the status line scrolls away. Previously these
/// failures were `warn!`-to-file only — invisible under the TUI's alternate
/// screen. Sends are best-effort (`let _ =`): headless runs have no TUI channel.
async fn report_rig_error(message_bus: &MessageBus, text: String) {
    let err = ComponentMessage::new(
        ComponentId::Hamlib,
        ComponentId::Tui,
        MessageType::Error {
            component_id: ComponentId::Hamlib,
            error_message: text.clone(),
            error_code: None,
        },
        Instant::now(),
    );
    let _ = message_bus.send_message(err).await;
    let diag = ComponentMessage::new(
        ComponentId::Hamlib,
        ComponentId::Tui,
        MessageType::DiagnosticEvent {
            target: "rig.cat",
            level: pancetta_core::DiagnosticLevel::Warn,
            text,
            qso_id: None,
            callsign: None,
        },
        Instant::now(),
    );
    let _ = message_bus.send_message(diag).await;
}

#[cfg(test)]
mod child_task_crashed_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// PAN-19 MEDIUM #1: a child that has already exited (expectedly, once
    /// `shutdown` is observed) must NOT be classified as a crash once
    /// `shutdown` is set -- even though the OUTER loop's own
    /// `!shutdown.load(...)` guard may have already been checked (and
    /// passed) before the child actually exited, this function is the
    /// second, authoritative check and must re-confirm `shutdown` itself.
    #[tokio::test]
    async fn ignores_a_child_that_exited_once_shutdown_is_set() {
        let shutdown = AtomicBool::new(true);
        let handle = tokio::spawn(async {});
        // Let the trivial task actually finish.
        for _ in 0..1000 {
            if handle.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            handle.is_finished(),
            "test setup: child should have finished"
        );

        assert!(
            !child_task_crashed(&shutdown, std::slice::from_ref(&handle)),
            "a child observed exiting during shutdown must not be treated as a crash"
        );
    }

    /// The flip side: outside of shutdown, a finished child IS a real crash
    /// and must still be flagged.
    #[tokio::test]
    async fn flags_a_real_crash_when_not_shutting_down() {
        let shutdown = AtomicBool::new(false);
        let handle = tokio::spawn(async {});
        for _ in 0..1000 {
            if handle.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            handle.is_finished(),
            "test setup: child should have finished"
        );

        assert!(
            child_task_crashed(&shutdown, std::slice::from_ref(&handle)),
            "a child that exits outside of shutdown must still be flagged as a crash"
        );
    }

    #[tokio::test]
    async fn no_crash_when_all_children_still_running() {
        let shutdown = AtomicBool::new(false);
        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        assert!(!handle.is_finished());

        assert!(!child_task_crashed(
            &shutdown,
            std::slice::from_ref(&handle)
        ));
        handle.abort();
    }

    /// PAN-19 round-1 review (Codex P2): the two tests above don't actually
    /// distinguish CHECK ORDER -- `shutdown` is already fully settled to its
    /// final value before `child_task_crashed` is ever called, so both the
    /// old (`!shutdown.load() && is_finished()`) and new
    /// (`is_finished() && !shutdown.load()`) orderings return the same
    /// answer. The real bug only shows up in the tiny window where
    /// `shutdown` flips (and a child exits because of it) concurrently WITH
    /// the check itself: reading `shutdown` first can observe the stale
    /// `false` moments before the flip, then `is_finished()` (checked
    /// second, further "downstream" of the flip) observes the now-exited
    /// child -- misclassifying an expected shutdown exit as a crash. Reading
    /// `is_finished()` first closes this: a child can only be observed
    /// finished once its exit (and everything causally before it, including
    /// whatever flipped `shutdown`) is visible, so the SECOND read
    /// (`shutdown`, checked only once a finished child is already known)
    /// reliably sees the fresh value.
    ///
    /// This races a real background task -- which stores `shutdown = true`
    /// and then returns -- against a tight polling loop, for many
    /// independent trials, comparing the ACTUAL (fixed) `child_task_crashed`
    /// against a locally reimplemented OLD-ordered formula under identical
    /// conditions. The fixed function must never misfire; the old-ordered
    /// formula is expected to misfire at least once across enough trials,
    /// demonstrating the race is real and that check order is what closes
    /// it (not just re-checking `shutdown` at all, which the MEDIUM #1 fix
    /// already did in either order).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn check_order_closes_the_shutdown_flip_race_that_the_old_order_missed() {
        const TRIALS: usize = 20_000;
        let mut old_order_misfired = false;

        for _ in 0..TRIALS {
            let shutdown = Arc::new(AtomicBool::new(false));
            let shutdown_for_task = shutdown.clone();
            let handle = tokio::spawn(async move {
                // Simulates the real shutdown sequence: flip the flag, THEN
                // exit -- exactly the "child observes shutdown and returns"
                // path both orderings are trying to classify correctly.
                shutdown_for_task.store(true, Ordering::Release);
            });

            // Race a tight, non-yielding poll against the task above on a
            // different worker thread (multi_thread runtime, no `.await`
            // in this loop body so this thread doesn't voluntarily give up
            // its slot) -- maximizing the chance of observing the handles
            // in whatever intermediate states are actually reachable.
            let handles = [handle];
            loop {
                let old_order_result =
                    !shutdown.load(Ordering::Acquire) && handles.iter().any(|h| h.is_finished());
                let new_order_result = child_task_crashed(&shutdown, &handles);

                // The fixed function's core invariant: NEVER misclassify a
                // shutdown-caused exit as a crash. Checked every iteration,
                // not just at the end -- this must hold at every observed
                // instant, not merely once settled.
                assert!(
                    !new_order_result,
                    "child_task_crashed (fixed order) misclassified a shutdown-caused exit \
                     as a crash"
                );

                if old_order_result {
                    old_order_misfired = true;
                }

                if handles[0].is_finished() {
                    break;
                }
            }
        }

        assert!(
            old_order_misfired,
            "expected the OLD check order (shutdown read before is_finished()) to \
             misclassify at least one shutdown-caused exit as a crash across {TRIALS} trials \
             -- if this never triggers, either the race genuinely isn't reachable on this \
             platform/scheduler or TRIALS needs to be higher; the fixed order's own \
             never-misfires assertion above already ran unconditionally every iteration \
             regardless of whether this one fires"
        );
    }
}

#[cfg(all(test, feature = "pancetta-hamlib"))]
mod children_publish_race_tests {
    //! PAN-19 HIGH regression guard.
    //!
    //! `start_hamlib_component` doesn't accept an injectable rig, so these
    //! tests can't drive a genuinely slow connect through the real function
    //! end to end. Instead this mirrors the exact concurrency SHAPE the fix
    //! establishes -- spawn the child tasks and publish their abort handles
    //! via a oneshot FIRST, then await the (possibly slow) rig connect --
    //! using a real `pancetta_hamlib::MockRig` configured with a multi-second
    //! connect delay, so the "does the children publish depend on connect
    //! completing" property is exercised with genuine async timing, not
    //! just asserted by inspection.
    use super::*;
    use pancetta_hamlib::{MockRig, MockRigConfig, RigControl};

    #[tokio::test]
    async fn children_handles_publish_before_a_slow_rig_connect_completes() {
        let rig = Arc::new(MockRig::new(MockRigConfig {
            // Comfortably past the old hardcoded 1s `children_rx` timeout
            // that used to fire (and bail) under this exact condition, but
            // well under this test's own bound.
            connection_delay_ms: 1_500,
            ..Default::default()
        }));
        let (children_tx, children_rx) = tokio::sync::oneshot::channel::<()>();

        let rig_for_task = rig.clone();
        tokio::spawn(async move {
            // Fixed shape: children published BEFORE the connect sequence,
            // not after.
            let poll_stub = tokio::spawn(async { std::future::pending::<()>().await });
            let watchdog_stub = tokio::spawn(async { std::future::pending::<()>().await });
            let _ = children_tx.send(());
            poll_stub.abort();
            watchdog_stub.abort();

            // The slow part -- must NOT gate the send above.
            let _ = rig_for_task.connect().await;
        });

        tokio::time::timeout(Duration::from_millis(300), children_rx)
            .await
            .expect(
                "children handles must publish promptly, independent of how long \
                     rig.connect() takes",
            )
            .expect("children_tx sender must not have been dropped without sending");
    }
}

#[cfg(all(test, feature = "pancetta-hamlib"))]
mod restart_orphan_tests {
    //! PAN-19 MEDIUM #2: a watchdog orphaned by a teardown whose 3 PTT-off
    //! attempts all failed (see `teardown_hamlib`'s `hamlib_orphans.push`)
    //! must not survive past the NEXT successful restart -- it would carry
    //! a stale `ptt_on_since` from the OLD generation and could still fire
    //! `set_ptt(Off)` on the NEW generation's rig client mid-transmission.
    use super::*;
    use pancetta_config::Config;
    use std::sync::atomic::AtomicBool;

    /// Local copy of the same direct-construction pattern used by
    /// `mod.rs`'s `test_coordinator_creation` / `health.rs`'s
    /// `test_coordinator` -- no shared helper exists yet for this module.
    async fn test_coordinator() -> super::super::ApplicationCoordinator {
        let config = Config::default();
        let shutdown = Arc::new(AtomicBool::new(false));
        super::super::ApplicationCoordinator::new(
            config,
            None,
            true,  // no_audio
            true,  // headless
            false, // metrics
            9090,
            None, // no WAV
            None, // no test-tx
            1500.0,
            shutdown,
            Vec::new(), // no config warnings
        )
        .await
        .expect("coordinator creation should succeed")
    }

    #[tokio::test]
    async fn successful_start_aborts_a_watchdog_orphaned_by_a_prior_failed_teardown() {
        let mut coordinator = test_coordinator().await;

        // Simulate a PRIOR teardown whose 3 PTT-off attempts all failed:
        // `teardown_hamlib` pushed the (still running) watchdog's
        // `AbortHandle` onto `hamlib_orphans` instead of aborting it.
        let orphan_task = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        coordinator.hamlib_orphans.push(orphan_task.abort_handle());
        assert!(!orphan_task.is_finished());

        // Config::default() has rig control disabled, so this takes the
        // mock-rig path -- no real hardware/rigctld needed, and it
        // completes quickly and deterministically.
        coordinator
            .start_hamlib_component()
            .await
            .expect("mock-rig hamlib start should succeed");

        assert!(
            coordinator.hamlib_orphans.is_empty(),
            "orphans must be drained once the new Hamlib generation successfully starts"
        );

        for _ in 0..1000 {
            if orphan_task.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            orphan_task.is_finished(),
            "the watchdog orphaned by a prior failed teardown must be aborted once the new \
             generation starts, not left running with a stale ptt_on_since from the old one"
        );

        coordinator
            .shutdown_signal
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(all(test, feature = "pancetta-hamlib"))]
mod teardown_stall_tests {
    //! PAN-19 LOW: `teardown_hamlib`'s PTT-off retry loop runs on the
    //! coordinator's main supervision task, so its up-to-~10s worst case
    //! (3 attempts x up to 3s send + a 500ms inter-attempt sleep) stalls
    //! `check_task_handles`'s own re-entry -- a second concurrent component
    //! failure sits undiscovered in `named_task_handles` until teardown
    //! returns. These tests cover the safer, smaller fix that was chosen
    //! over a full concurrency restructure: skip the inter-attempt sleep
    //! specifically when another component's task has already finished.
    use super::*;
    use pancetta_config::Config;
    use pancetta_hamlib::{MockRig, RigControl};
    use std::sync::atomic::AtomicBool;

    async fn test_coordinator() -> super::super::ApplicationCoordinator {
        let config = Config::default();
        let shutdown = Arc::new(AtomicBool::new(false));
        super::super::ApplicationCoordinator::new(
            config,
            None,
            true,  // no_audio
            true,  // headless
            false, // metrics
            9090,
            None, // no WAV
            None, // no test-tx
            1500.0,
            shutdown,
            Vec::new(), // no config warnings
        )
        .await
        .expect("coordinator creation should succeed")
    }

    /// An UNconnected `MockRig`'s `set_ptt` deterministically fails every
    /// time ("Mock rig not connected"), so this exercises all 3 retry
    /// attempts (and, absent the fix, both inter-attempt sleeps) every run.
    fn disconnected_mock_rig() -> Arc<MockRig> {
        Arc::new(MockRig::default())
    }

    #[tokio::test]
    async fn skips_inter_attempt_sleep_when_another_failure_is_already_pending() {
        let mut coordinator = test_coordinator().await;
        coordinator.rig_handle = Some(disconnected_mock_rig());

        // Simulate a second component's task having already finished,
        // sitting undiscovered in `named_task_handles` -- the scenario this
        // fix targets.
        let finished: tokio::task::JoinHandle<anyhow::Result<()>> = tokio::spawn(async { Ok(()) });
        for _ in 0..1000 {
            if finished.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(finished.is_finished(), "test setup: should have finished");
        coordinator
            .named_task_handles
            .push((ComponentId::DxCluster, finished));

        let start = std::time::Instant::now();
        coordinator.teardown_hamlib().await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(400),
            "teardown_hamlib should skip its inter-attempt sleeps once another component's \
             finished task is already pending -- took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn still_sleeps_between_attempts_when_nothing_else_is_pending() {
        let mut coordinator = test_coordinator().await;
        coordinator.rig_handle = Some(disconnected_mock_rig());

        let start = std::time::Instant::now();
        coordinator.teardown_hamlib().await;
        let elapsed = start.elapsed();

        // 2 inter-attempt sleeps x 500ms between the 3 (always-failing)
        // attempts -- this must NOT have been skipped when nothing else
        // needs attention, preserving the existing retry pacing.
        assert!(
            elapsed >= Duration::from_millis(900),
            "teardown_hamlib should still sleep between PTT-off retry attempts when nothing \
             else is pending -- took {elapsed:?}"
        );
    }
}

#[cfg(all(test, feature = "pancetta-hamlib"))]
mod teardown_replay_tests {
    //! PAN-19 LOW: the message re-injection loop at the end of
    //! `teardown_hamlib` used a *blocking* `crossbeam_channel::Sender::send`
    //! inside an async fn. Normally harmless -- the drain immediately above
    //! it just freed exactly as many slots as it collected messages to
    //! replay, so a fresh `teardown_hamlib` call can never itself construct
    //! a full channel at replay time (replay count is provably <= drain
    //! count <= channel capacity, with no `.await` between the two loops
    //! for anything else to interleave through). The real risk is a
    //! genuinely concurrent producer on another OS thread racing into that
    //! gap -- which, being a true multi-thread race on a single-instruction
    //! -wide window, isn't reliably reproducible by a test driving real
    //! wall-clock timing (confirmed: an earlier version of this test tried
    //! exactly that with a sustained background "saturator" task and did
    //! not manage to trigger the blocking path even once). So instead of
    //! chasing that race, this tests the actual mechanism the fix relies
    //! on directly and deterministically: a full bounded `crossbeam_channel`
    //! `try_send` returns `Err` immediately rather than blocking -- the
    //! exact call `teardown_hamlib`'s replay loop now makes (see the
    //! `sender.try_send(message)` call right above `teardown_hamlib`'s
    //! final `}` in this file) -- plus a normal-path integration test
    //! proving `teardown_hamlib` itself still completes fast end to end.
    use super::*;
    use pancetta_config::Config;

    async fn test_coordinator() -> super::super::ApplicationCoordinator {
        let config = Config::default();
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        super::super::ApplicationCoordinator::new(
            config,
            None,
            true,  // no_audio
            true,  // headless
            false, // metrics
            9090,
            None, // no WAV
            None, // no test-tx
            1500.0,
            shutdown,
            Vec::new(), // no config warnings
        )
        .await
        .expect("coordinator creation should succeed")
    }

    fn set_freq_msg() -> ComponentMessage {
        ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetFrequency {
                vfo: 0,
                frequency: 14_074_000,
            }),
            Instant::now(),
        )
    }

    /// The exact mechanism: `try_send` on a full bounded channel returns
    /// `Err(Full)` immediately rather than blocking the calling thread.
    #[test]
    fn try_send_on_a_full_channel_returns_immediately_instead_of_blocking() {
        let (sender, _receiver) = crossbeam_channel::bounded::<ComponentMessage>(1);
        sender
            .try_send(set_freq_msg())
            .expect("first send fills the one slot");

        let start = std::time::Instant::now();
        let result = sender.try_send(set_freq_msg());
        let elapsed = start.elapsed();

        assert!(
            matches!(result, Err(crossbeam_channel::TrySendError::Full(_))),
            "try_send on a full channel must return Err(Full), not succeed or panic"
        );
        assert!(
            elapsed < Duration::from_millis(50),
            "try_send must return immediately rather than blocking -- took {elapsed:?}"
        );
    }

    /// Normal-path integration coverage: `teardown_hamlib`'s replay of a
    /// drained "safe" message back onto a freshly-emptied channel must
    /// still work and complete quickly (the drain-then-replay sequence can
    /// never itself overflow the channel it just emptied -- see the module
    /// doc comment above).
    #[tokio::test]
    async fn teardown_replays_a_safe_message_and_completes_quickly() {
        let mut coordinator = test_coordinator().await;
        coordinator.rig_handle = None; // skip the PTT-off retry loop entirely

        let (sender, receiver) = coordinator
            .message_bus
            .get_or_create_channel(ComponentId::Hamlib)
            .await
            .unwrap();
        sender.try_send(set_freq_msg()).unwrap();

        let start = std::time::Instant::now();
        let result =
            tokio::time::timeout(Duration::from_secs(2), coordinator.teardown_hamlib()).await;
        let elapsed = start.elapsed();

        assert!(
            result.is_ok(),
            "teardown_hamlib should not hang on the normal path"
        );
        assert!(elapsed < Duration::from_millis(500), "took {elapsed:?}");
        assert!(
            receiver.try_recv().is_ok(),
            "the safe SetFrequency message should have been replayed back onto the channel"
        );
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
