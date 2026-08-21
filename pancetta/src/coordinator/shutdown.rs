use anyhow::Result;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

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
    pub(crate) async fn shutdown(mut self) -> Result<()> {
        let span = tracing::span!(tracing::Level::INFO, "coordinator_shutdown");
        let _enter = span.enter();

        info!("Starting graceful shutdown");
        self.is_running.store(false, Ordering::Release);
        self.shutdown_signal.store(true, Ordering::Release);

        // Safety: send PTT-off before tearing down components to avoid
        // leaving the rig transmitting if a TX was in progress. We use
        // two parallel paths because each one can fail under shutdown
        // race conditions:
        //
        // (1) Message bus to the running Hamlib component. Honoured
        //     only as long as Hamlib is still draining its queue, which
        //     is racy under shutdown.
        // (2) A direct rigctld TCP connection from this scope, opened
        //     just for PTT-off. Independent of the Hamlib component's
        //     liveness, the message bus, and the in-flight TX worker.
        //
        // Whichever lands first wins. If both fail, the in-TX-worker
        // PttGuard drop (triggered by tokio cancellation in the
        // task-handle abort below) is the third backstop.
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
                Ok(Ok(())) => info!("Shutdown: PTT-off sent via message bus"),
                Ok(Err(e)) => warn!("Shutdown: PTT-off via message bus failed: {}", e),
                Err(_) => warn!("Shutdown: PTT-off via message bus timed out"),
            }
        }

        // Direct rigctld PTT-off. Independent of the message bus and
        // the Hamlib component's task. Skipped if rig was disabled, and
        // skipped under `--replay`, which never started rig control at all
        // (see `wants_direct_rigctld_ptt_off`).
        if self.wants_direct_rigctld_ptt_off().await {
            let rigctld_host =
                std::env::var("RIGCTLD_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
            let rigctld_port: u16 = std::env::var("RIGCTLD_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(4532);
            let direct_client =
                pancetta_hamlib::RigctldClient::new(pancetta_hamlib::RigctldConfig {
                    host: rigctld_host,
                    port: rigctld_port,
                    ..Default::default()
                });
            let direct_off = async {
                use pancetta_hamlib::RigControl;
                direct_client.connect().await?;
                direct_client
                    .set_ptt(
                        pancetta_hamlib::Vfo::Current,
                        pancetta_hamlib::PttState::Off,
                    )
                    .await?;
                anyhow::Ok::<()>(())
            };
            match tokio::time::timeout(Duration::from_millis(500), direct_off).await {
                Ok(Ok(())) => info!("Shutdown: direct rigctld PTT-off succeeded"),
                Ok(Err(e)) => warn!("Shutdown: direct rigctld PTT-off failed: {}", e),
                Err(_) => warn!("Shutdown: direct rigctld PTT-off timed out"),
            }
        } else if self.replay_mode() {
            info!(
                "Replay mode: skipping direct rigctld PTT-off — this run never started rig control"
            );
        }

        let per_task_timeout = Duration::from_secs(1);

        for (index, (component_id, handle)) in std::mem::take(&mut self.named_task_handles)
            .into_iter()
            .enumerate()
        {
            // Capture an abort handle BEFORE the timeout consumes the
            // JoinHandle. Without this, dropping the JoinHandle on
            // timeout merely detaches the task — the underlying future
            // keeps running, and the in-flight TX worker's PttGuard
            // never drops because it's still alive. Aborting at the
            // next await point causes Drop to fire, which spawns the
            // PttGuard PTT-off message.
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

        // Join the TUI relay OS thread
        if let Some(handle) = self.tui_relay_handle.take() {
            debug!("Joining TUI relay thread");
            if let Err(e) = handle.join() {
                warn!("TUI relay thread panicked: {:?}", e);
            }
        }

        // Kill managed rigctld process
        #[cfg(feature = "pancetta-hamlib")]
        if let Some(mut child) = self.rigctld_process.take() {
            info!("Stopping managed rigctld (PID {})", child.id());
            let _ = child.kill();
            let _ = child.wait();
        }

        info!("Graceful shutdown completed");

        Ok(())
    }
}
