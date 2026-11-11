use crate::message_bus::ComponentId;
use anyhow::{Context, Result};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// PAN-277: outer budget for shutdown's direct rigctld PTT-off. Fits
/// `connect()`'s `f` verification plus `T 0`, each at the command timeout.
pub(crate) const DIRECT_PTT_OFF_BUDGET: Duration = Duration::from_secs(6);
/// PAN-277: reply wait per direct command. Must exceed one rig answer time,
/// because `connect()` verifies the link with `f` under this timeout.
pub(crate) const DIRECT_PTT_OFF_COMMAND_TIMEOUT_MS: u64 = 3000;
/// PAN-277: TCP connect timeout for the direct client.
pub(crate) const DIRECT_PTT_OFF_CONNECT_TIMEOUT_MS: u64 = 2000;

pub(crate) fn direct_ptt_off_config(host: String, port: u16) -> pancetta_hamlib::RigctldConfig {
    pancetta_hamlib::RigctldConfig {
        host,
        port,
        timeout_ms: DIRECT_PTT_OFF_CONNECT_TIMEOUT_MS,
        command_timeout_ms: DIRECT_PTT_OFF_COMMAND_TIMEOUT_MS,
        ..Default::default()
    }
}

#[derive(Debug)]
pub(crate) enum DirectPttOff {
    Unkeyed,
    Failed(anyhow::Error),
    TimedOut,
}

/// Open a fresh rigctld connection and send PTT-off, bounded by
/// [`DIRECT_PTT_OFF_BUDGET`]. Independent of the Hamlib component.
pub(crate) async fn direct_rigctld_ptt_off(host: String, port: u16) -> DirectPttOff {
    use pancetta_hamlib::RigControl;
    let client = pancetta_hamlib::RigctldClient::new(direct_ptt_off_config(host, port));
    let attempt = async {
        client.connect().await?;
        client
            .set_ptt(
                pancetta_hamlib::Vfo::Current,
                pancetta_hamlib::PttState::Off,
            )
            .await?;
        anyhow::Ok(())
    };
    match tokio::time::timeout(DIRECT_PTT_OFF_BUDGET, attempt).await {
        Ok(Ok(())) => DirectPttOff::Unkeyed,
        Ok(Err(e)) => DirectPttOff::Failed(e),
        Err(_) => DirectPttOff::TimedOut,
    }
}

fn direct_rigctld_target() -> (String, u16) {
    (
        std::env::var("RIGCTLD_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        std::env::var("RIGCTLD_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(4532),
    )
}

/// PAN-277: join budget for the Hamlib task after `hamlib_drain_stop`.
pub(crate) const HAMLIB_SHUTDOWN_JOIN: Duration = Duration::from_secs(10);
/// PAN-277: join budget for the TX worker before the drain stop.
pub(crate) const TX_WORKER_SHUTDOWN_JOIN: Duration = Duration::from_secs(1);
/// PAN-277: wait for an armed PttGuard's fallback PTT-off to be queued.
pub(crate) const PENDING_PTT_OFF_SETTLE: Duration = Duration::from_secs(1);

fn take_component_handles(
    handles: &mut Vec<(ComponentId, tokio::task::JoinHandle<Result<()>>)>,
    id: ComponentId,
) -> Vec<tokio::task::JoinHandle<Result<()>>> {
    let mut selected = Vec::new();
    let mut index = 0;
    while index < handles.len() {
        if handles[index].0 == id {
            selected.push(handles.remove(index).1);
        } else {
            index += 1;
        }
    }
    selected
}

/// Join within `budget`; on timeout abort, then wait up to `budget` again so
/// the task's `Drop`s have run. Returns whether it finished on its own.
async fn join_or_abort(
    name: &str,
    mut handle: tokio::task::JoinHandle<Result<()>>,
    budget: Duration,
) -> bool {
    match tokio::time::timeout(budget, &mut handle).await {
        Ok(Ok(Ok(()))) => true,
        Ok(Ok(Err(e))) => {
            warn!("Shutdown: {name} task ended with error: {e}");
            true
        }
        Ok(Err(e)) => {
            warn!("Shutdown: {name} task join error: {e}");
            true
        }
        Err(_) => {
            handle.abort();
            let _ = tokio::time::timeout(budget, handle).await;
            false
        }
    }
}

impl super::ApplicationCoordinator {
    /// Whether shutdown should open its own TCP connection to `rigctld` and
    /// push a direct PTT-off (see the call site for why that independent path
    /// exists at all).
    ///
    /// Two conditions, both required:
    ///
    /// 1. `[rig.interface].enabled` — nothing to talk to otherwise.
    /// 2. NOT `--replay` ([`super::ApplicationCoordinator::replay_mode`]).
    ///    A replay run never calls `start_hamlib_component` (see the comment
    ///    at that call site in `mod.rs`), so this process has no PTT
    ///    capability and never keyed anything. The direct path is deliberately
    ///    independent of this process's Hamlib component, which means that on
    ///    a replay run it would reach out and unkey whatever *else* owns that
    ///    rigctld endpoint — another application, or a real live pancetta on
    ///    the same host — interrupting a transmission this demo never started.
    ///    An outbound write to real radio hardware is exactly the class of
    ///    "replay touches the outside world" every other replay gate closes.
    ///
    /// Gated on `replay_mode()` rather than on some "did Hamlib actually
    /// connect" signal on purpose: the whole point of the direct path is to
    /// unkey even when this process's in-process rig state is broken or the
    /// component never came up, so narrowing it by liveness would weaken the
    /// live-run safety net. `replay_mode()` is the one case where the
    /// capability provably never existed.
    pub(crate) async fn wants_direct_rigctld_ptt_off(&self) -> bool {
        if self.replay_mode() {
            return false;
        }
        self.config.read().await.rig.interface.enabled
    }

    /// Graceful shutdown of all components
    pub(crate) async fn shutdown(self) -> Result<()> {
        self.shutdown_with_gain_sweep(None).await
    }

    /// Unkey before joining sweep cleanup, but retain the rig until restoration ends.
    pub(crate) async fn shutdown_with_gain_sweep(
        mut self,
        sweep_handle: Option<tokio::task::JoinHandle<Result<()>>>,
    ) -> Result<()> {
        let span = tracing::span!(tracing::Level::INFO, "coordinator_shutdown");
        let _enter = span.enter();

        info!("Starting graceful shutdown");
        self.is_running.store(false, Ordering::Release);
        self.shutdown_signal.store(true, Ordering::Release);

        // Queue an unkey first. Concurrently stop the TX producer, settle its
        // fallback, stop and join the Hamlib drain, and try a fresh direct client.
        {
            use crate::message_bus::{ComponentId, ComponentMessage, MessageType};
            let ptt_off = ComponentMessage::new(
                ComponentId::Ft8Transmitter,
                ComponentId::Hamlib,
                MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                    state: false,
                }),
                Instant::now(),
            );
            match tokio::time::timeout(
                Duration::from_millis(500),
                self.message_bus.send_message(ptt_off),
            )
            .await
            {
                Ok(Ok(())) => info!("Shutdown: PTT-off queued for the Hamlib loop"),
                Ok(Err(e)) => warn!("Shutdown: PTT-off via message bus failed: {}", e),
                Err(_) => warn!("Shutdown: PTT-off via message bus timed out"),
            }
        }

        let direct_target = if self.wants_direct_rigctld_ptt_off().await {
            Some(direct_rigctld_target())
        } else {
            if self.replay_mode() {
                info!("Replay mode: skipping direct rigctld PTT-off — this run never started rig control");
            }
            None
        };
        let tx_handles =
            take_component_handles(&mut self.named_task_handles, ComponentId::Ft8Transmitter);
        let hamlib_handles =
            take_component_handles(&mut self.named_task_handles, ComponentId::Hamlib);
        let pending = self.pending_ptt_off.clone();
        let stop = self.hamlib_drain_stop.clone();
        let direct = async move {
            if let Some((host, port)) = direct_target {
                match direct_rigctld_ptt_off(host, port).await {
                    DirectPttOff::Unkeyed => info!("Shutdown: direct rigctld PTT-off succeeded"),
                    DirectPttOff::Failed(e) => {
                        warn!("Shutdown: direct rigctld PTT-off failed: {}", e)
                    }
                    DirectPttOff::TimedOut => warn!("Shutdown: direct rigctld PTT-off timed out"),
                }
            }
        };
        let drain = async move {
            for handle in tx_handles {
                join_or_abort("Ft8Transmitter", handle, TX_WORKER_SHUTDOWN_JOIN).await;
            }
            if tokio::time::timeout(PENDING_PTT_OFF_SETTLE, pending.settle())
                .await
                .is_err()
            {
                warn!("Shutdown: TX worker fallback PTT-off still outstanding");
            }
            stop.store(true, Ordering::Release);
            for handle in hamlib_handles {
                if join_or_abort("Hamlib", handle, HAMLIB_SHUTDOWN_JOIN).await {
                    info!("Shutdown: Hamlib loop drained its queue and stopped");
                } else {
                    warn!(
                        "Shutdown: Hamlib loop did not finish its drain in {}s; aborted it",
                        HAMLIB_SHUTDOWN_JOIN.as_secs()
                    );
                }
            }
        };
        tokio::join!(direct, drain);

        // Both unkey paths have finished before gain restoration or rig teardown.
        let sweep_result = if let Some(handle) = sweep_handle {
            handle
                .await
                .context("Receive-gain sweep task failed")
                .and_then(|result| result)
        } else {
            Ok(())
        };

        {
            let original = *self
                .gain_sweep_original
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let (Some(original), Some(rig)) = (original, self.rig_handle.as_ref()) {
                let lead = super::resolve_window_lead_secs(&self.config.read().await.decoder);
                if super::gain_sweep_runtime::restore(
                    &**rig,
                    original,
                    &self.message_bus.gain_tx_gate,
                    lead,
                )
                .await
                {
                    *self
                        .gain_sweep_original
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = None;
                } else {
                    warn!(
                        "Original receive gain could not be restored during coordinator shutdown"
                    );
                }
            }
        }

        let per_task_timeout = Duration::from_secs(1);

        for (index, (component_id, handle)) in std::mem::take(&mut self.named_task_handles)
            .into_iter()
            .enumerate()
        {
            // Abort timed-out tasks so their cleanup runs instead of detaching.
            let abort_handle = handle.abort_handle();
            match tokio::time::timeout(per_task_timeout, handle).await {
                Ok(Ok(_)) => {
                    debug!("Task {} ({}) completed successfully", index, component_id);
                }
                Ok(Err(e)) => {
                    warn!(
                        "Task {} ({}) completed with error: {}",
                        index, component_id, e
                    );
                }
                Err(_) => {
                    debug!(
                        "Task {} ({}) timed out — aborting so PttGuard / Drop can fire",
                        index, component_id
                    );
                    abort_handle.abort();
                }
            }
        }

        // PAN-57/PAN-232/PAN-274: every QSO component above has stopped, and
        // promotion starts no queued call once the flag is set, so nothing is
        // left to complete a QSO. Ask the ADIF subscribers and the upload
        // subscribers to handle what is still in their channels and finish,
        // and wait for both (each bounded) before the runtime goes away.
        tokio::join!(
            self.adif_subscribers.stop(super::qso::ADIF_SHUTDOWN_DRAIN),
            super::upload_outbox::stop_upload_outbox(
                self.upload_outbox.take(),
                self.upload_outbox_task.take(),
                super::upload_outbox::OutboxTiming::PRODUCTION,
            ),
        );

        // Join the TUI relay OS thread
        if let Some(handle) = self.tui_relay_handle.take() {
            debug!("Joining TUI relay thread");
            if let Err(e) = handle.join() {
                warn!("TUI relay thread panicked: {:?}", e);
            }
        }

        // Kill managed rigctld process
        if let Some(mut child) = self.rigctld_process.take() {
            info!("Stopping managed rigctld (PID {})", child.id());
            let _ = child.kill();
            let _ = child.wait();
        }

        info!("Graceful shutdown completed");

        sweep_result
    }
}

#[cfg(test)]
mod pan277_direct_ptt_off_tests {
    use super::*;
    use std::sync::{atomic::AtomicBool, Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    struct SlowRigctld {
        port: u16,
        commands: Arc<Mutex<Vec<String>>>,
        keyed: Arc<AtomicBool>,
        server: tokio::task::JoinHandle<()>,
    }
    impl SlowRigctld {
        async fn start(delay: Duration) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let commands = Arc::new(Mutex::new(Vec::new()));
            let keyed = Arc::new(AtomicBool::new(true));
            let log = commands.clone();
            let state = keyed.clone();
            let server = tokio::spawn(async move {
                let mut clients = tokio::task::JoinSet::new();
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let log = log.clone();
                    let state = state.clone();
                    clients.spawn(async move {
                        let (read, mut write) = stream.into_split();
                        let mut lines = BufReader::new(read).lines();
                        while let Ok(Some(command)) = lines.next_line().await {
                            log.lock().unwrap().push(command.clone());
                            if command == "T 0" {
                                state.store(false, Ordering::Release);
                            }
                            tokio::time::sleep(delay).await;
                            let reply = if command == "f" {
                                b"14074000\n".as_slice()
                            } else {
                                b"RPRT 0\n".as_slice()
                            };
                            if write.write_all(reply).await.is_err() {
                                break;
                            }
                        }
                    });
                }
            });
            Self {
                port,
                commands,
                keyed,
                server,
            }
        }
        fn commands(&self) -> Vec<String> {
            self.commands.lock().unwrap().clone()
        }
        fn keyed(&self) -> bool {
            self.keyed.load(Ordering::Acquire)
        }
    }
    impl Drop for SlowRigctld {
        fn drop(&mut self) {
            self.server.abort();
        }
    }
    #[tokio::test]
    async fn pan277_direct_ptt_off_unkeys_a_rigctld_that_takes_1500ms_to_answer() {
        let rig = SlowRigctld::start(Duration::from_millis(1500)).await;
        let started = Instant::now();
        let outcome = direct_rigctld_ptt_off("127.0.0.1".into(), rig.port).await;
        assert!(matches!(outcome, DirectPttOff::Unkeyed), "{outcome:?}");
        assert_eq!(rig.commands(), ["f", "T 0"]);
        assert!(!rig.keyed());
        // The client waited for both replies instead of racing past them.
        assert!(started.elapsed() >= Duration::from_millis(3000));
    }

    #[tokio::test]
    async fn pan277_direct_ptt_off_gives_up_within_its_budget_on_a_silent_rigctld() {
        let rig = SlowRigctld::start(Duration::from_secs(60)).await;
        let started = Instant::now();
        let outcome = direct_rigctld_ptt_off("127.0.0.1".into(), rig.port).await;
        assert!(!matches!(outcome, DirectPttOff::Unkeyed), "{outcome:?}");
        assert!(started.elapsed() < DIRECT_PTT_OFF_BUDGET + Duration::from_millis(500));
    }

    #[test]
    fn pan277_direct_ptt_off_budget_fits_f_and_t0_at_the_command_timeout() {
        assert!(
            DIRECT_PTT_OFF_COMMAND_TIMEOUT_MS > 1500,
            "must exceed the ticket's 1.5 s rig"
        );
        assert!(
            DIRECT_PTT_OFF_BUDGET >= Duration::from_millis(2 * DIRECT_PTT_OFF_COMMAND_TIMEOUT_MS)
        );
        let config = direct_ptt_off_config("h".into(), 1);
        assert_eq!(config.command_timeout_ms, DIRECT_PTT_OFF_COMMAND_TIMEOUT_MS);
        assert_eq!(config.timeout_ms, DIRECT_PTT_OFF_CONNECT_TIMEOUT_MS);
    }

    struct SetOnDrop(Arc<AtomicBool>);
    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    #[tokio::test]
    async fn pan277_join_or_abort_reports_a_finished_task() {
        let handle = tokio::spawn(async { anyhow::Ok(()) });
        assert!(join_or_abort("t", handle, Duration::from_secs(1)).await);
    }

    #[tokio::test]
    async fn pan277_join_or_abort_aborts_a_hung_task_and_waits_for_its_drop() {
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = dropped.clone();
        let handle = tokio::spawn(async move {
            let _guard = SetOnDrop(flag);
            std::future::pending::<()>().await;
            anyhow::Ok(())
        });
        let started = Instant::now();
        assert!(!join_or_abort("t", handle, Duration::from_millis(200)).await);
        assert!(
            dropped.load(Ordering::Acquire),
            "Drop ran before join_or_abort returned"
        );
        assert!(started.elapsed() < Duration::from_millis(600));
    }
}
