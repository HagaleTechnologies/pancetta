use anyhow::Result;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tracing::{debug, info, warn};

impl super::ApplicationCoordinator {
    /// Graceful shutdown of all components
    pub(crate) async fn shutdown(mut self) -> Result<()> {
        let span = tracing::span!(tracing::Level::INFO, "coordinator_shutdown");
        let _enter = span.enter();

        info!("Starting graceful shutdown");
        self.is_running.store(false, Ordering::Release);
        self.shutdown_signal.store(true, Ordering::Release);

        let per_task_timeout = Duration::from_secs(1);

        for (index, (component_id, handle)) in std::mem::take(&mut self.named_task_handles)
            .into_iter()
            .enumerate()
        {
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
                    debug!("Task {} ({}) timed out, aborting", index, component_id);
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
