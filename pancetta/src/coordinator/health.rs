use anyhow::Result;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, warn};

use super::{
    component_criticality, degradation_message, ComponentCriticality, ComponentState,
    ComponentStatus,
};
use crate::message_bus::{ComponentId, ComponentMessage, MessageType};

impl super::ApplicationCoordinator {
    /// Start coordinator management tasks
    pub(crate) async fn start_coordinator_tasks(&mut self) -> Result<()> {
        // Initialize component status for all registered task handles
        {
            let mut status_map = self.component_status.write().await;
            for (id, _) in &self.named_task_handles {
                status_map
                    .entry(*id)
                    .or_insert_with(ComponentStatus::new_running);
            }
        }

        // Health monitoring task -- checks task handles and message bus health
        let health_handle = self.start_health_monitor().await;

        // Configuration hot-reload task
        let config_handle = {
            let _config = self.config.clone();
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                while !shutdown.load(Ordering::Acquire) {
                    sleep(Duration::from_secs(1)).await;
                }
                Ok(())
            })
        };

        self.named_task_handles
            .push((ComponentId::Coordinator, health_handle));
        self.named_task_handles
            .push((ComponentId::Coordinator, config_handle));

        Ok(())
    }

    /// Start the health monitor task.
    ///
    /// Runs every `health_check_interval` (5s) and:
    /// 1. Reads the component_status map (populated by `check_task_handles()`)
    /// 2. Sends a health status summary to the TUI via the message bus
    ///
    /// Note: heartbeat checking via `message_bus.get_component_health()` was
    /// removed because no component ever sends heartbeat messages. Failure
    /// detection is handled by `check_task_handles()` in the main loop.
    pub(crate) async fn start_health_monitor(&self) -> JoinHandle<Result<()>> {
        let message_bus = self.message_bus.clone();
        let shutdown = self.shutdown_signal.clone();
        let component_status = self.component_status.clone();
        let mut health_interval = interval(Duration::from_secs(5));

        tokio::spawn(async move {
            while !shutdown.load(Ordering::Acquire) {
                health_interval.tick().await;

                // Build a status summary from the component_status map
                // (populated by check_task_handles in the main loop)
                let status_map = component_status.read().await;
                let mut summary_parts: Vec<String> = Vec::new();
                let mut any_failed = false;

                for (id, status) in status_map.iter() {
                    match &status.state {
                        ComponentState::Running => {
                            // Component is fine
                        }
                        ComponentState::Failed(err) => {
                            any_failed = true;
                            summary_parts.push(format!("{}: {}", id, err));
                        }
                        ComponentState::NotStarted => {
                            // Not started / disabled -- don't report
                        }
                    }
                }

                // Send health summary to TUI
                if any_failed {
                    let summary = format!("Degraded -- {}", summary_parts.join("; "));
                    let msg = ComponentMessage::new(
                        ComponentId::Coordinator,
                        ComponentId::Tui,
                        MessageType::StatusUpdate(summary),
                        Instant::now(),
                    );
                    if let Err(e) = message_bus.send_message(msg).await {
                        debug!("Failed to send health summary to TUI: {}", e);
                    }
                }
            }

            Ok(())
        })
    }

    /// Main application loop
    ///
    /// Periodically checks task handles for unexpected termination and updates
    /// the component_status map so the health monitor can report to the TUI.
    pub(crate) async fn run_main_loop(&mut self) -> Result<()> {
        info!("Entering main application loop");

        let mut stats_interval = interval(Duration::from_secs(30));
        let mut health_check_interval = interval(Duration::from_secs(5));

        while !self.shutdown_signal.load(Ordering::Acquire) {
            tokio::select! {
                _ = stats_interval.tick() => {
                    self.log_performance_stats().await;
                }
                _ = health_check_interval.tick() => {
                    self.check_task_handles().await;
                }
                _ = sleep(Duration::from_millis(100)) => {
                    // Main loop iteration
                }
            }
        }

        info!("Main application loop completed");
        Ok(())
    }

    /// Check all named task handles for unexpected termination.
    ///
    /// When a component task finishes (is_finished() == true), we inspect
    /// the result and update the component_status map. The health monitor
    /// task picks this up on its next cycle and reports to the TUI.
    ///
    /// Graceful degradation: no single component failure shuts down the
    /// application. Critical components are logged at error level, others
    /// at warn level.
    pub(crate) async fn check_task_handles(&mut self) {
        for (component_id, handle) in &self.named_task_handles {
            // Skip coordinator's own tasks and already-known failures
            if *component_id == ComponentId::Coordinator {
                continue;
            }

            if !handle.is_finished() {
                // Task is still running -- update last_seen
                let mut status_map = self.component_status.write().await;
                if let Some(status) = status_map.get_mut(component_id) {
                    if status.state == ComponentState::Running {
                        status.last_seen = Instant::now();
                    }
                }
                continue;
            }

            // Task has finished -- check if we already know about it
            let mut status_map = self.component_status.write().await;
            let status = status_map
                .entry(*component_id)
                .or_insert_with(ComponentStatus::new_running);

            if status.state != ComponentState::Running {
                // Already recorded this failure
                continue;
            }

            // First time seeing this component as finished
            let degradation = degradation_message(*component_id);
            let criticality = component_criticality(*component_id);

            status.error_count += 1;
            status.state = ComponentState::Failed(degradation.to_string());

            match criticality {
                ComponentCriticality::Important => {
                    error!(
                        "CRITICAL component {} has stopped unexpectedly: {}",
                        component_id, degradation
                    );
                }
                ComponentCriticality::NonCritical => {
                    warn!("Component {} has stopped: {}", component_id, degradation);
                }
            }

            // For Hamlib failure: ensure PTT defaults to off for safety
            if *component_id == ComponentId::Hamlib {
                warn!("PTT safety: forcing PTT off due to Hamlib disconnect");
                let ptt_off_msg = ComponentMessage::new(
                    ComponentId::Coordinator,
                    ComponentId::Hamlib,
                    MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                        state: false,
                    }),
                    Instant::now(),
                );
                // Best-effort: channel may be disconnected
                let _ = self.message_bus.send_message(ptt_off_msg).await;
            }

            // Notify TUI of the component failure
            let error_msg = ComponentMessage::new(
                ComponentId::Coordinator,
                ComponentId::Tui,
                MessageType::StatusUpdate(format!("{}: {}", component_id, degradation)),
                Instant::now(),
            );
            let _ = self.message_bus.send_message(error_msg).await;
        }
    }

    /// Log performance statistics
    pub(crate) async fn log_performance_stats(&self) {
        let message_count = self.message_count.load(Ordering::Relaxed);
        let uptime = self.startup_time.elapsed();

        let audio_status = {
            let timestamp = self.last_audio_timestamp.read().await;
            match *timestamp {
                Some(ts) => format!("active (last: {:.2}s ago)", ts.elapsed().as_secs_f64()),
                None => "inactive".to_string(),
            }
        };

        let decode_status = {
            let timestamp = self.last_decode_timestamp.read().await;
            match *timestamp {
                Some(ts) => format!("active (last: {:.2}s ago)", ts.elapsed().as_secs_f64()),
                None => "inactive".to_string(),
            }
        };

        info!(
            "Performance stats - Uptime: {:.0}s, Messages: {}, Audio: {}, Decode: {}",
            uptime.as_secs_f64(),
            message_count,
            audio_status,
            decode_status
        );
    }
}
