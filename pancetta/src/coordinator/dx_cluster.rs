//! DX cluster component.
//!
//! Polls the configured DX cluster (TCP `cluster.foo:7300` or similar)
//! for live spots and forwards them onto the message bus as
//! `MessageType::DxMessage(DxMessage::Spot { … })` for the TUI's DX
//! Hunter view and the autonomous operator's priority scorer.
//!
//! No-op when `[network.dx_cluster].enabled = false` (the default).

use anyhow::Result;
use pancetta_dx::cluster::{ClusterConfig, DxClusterClient};
use std::time::Instant;
use tracing::{debug, info};

use crate::message_bus::{ComponentId, ComponentMessage, MessageType};

/// Client settings for the DX-cluster worker: the first configured server
/// (or `dxc.nc7j.com:23`), and `[network.dx_cluster.connection]`'s connect
/// timeout (at least 1 s) and reconnect delay. The worker always reconnects
/// (PAN-288).
fn dx_cluster_client_config(
    dx: &pancetta_config::DxClusterConfig,
    callsign: &str,
) -> ClusterConfig {
    let server = dx.servers.first();
    ClusterConfig {
        hostname: server
            .map(|s| s.hostname.clone())
            .unwrap_or_else(|| "dxc.nc7j.com".to_string()),
        port: server.map(|s| s.port).unwrap_or(23),
        callsign: callsign.to_string(),
        timeout_seconds: u64::from(dx.connection.timeout_seconds.max(1)),
        reconnect_delay_seconds: dx.connection.reconnect_delay_seconds,
        auto_reconnect: true,
        filter_settings: Default::default(),
        use_websocket: false,
        websocket_url: None,
    }
}

impl super::ApplicationCoordinator {
    pub(crate) async fn start_dx_cluster_component(&mut self) -> Result<()> {
        let config = self.config.read().await;
        if !config.network.dx_cluster.enabled {
            info!("DX cluster disabled in configuration");
            drop(config);
            // Still create channel so message bus doesn't complain
            let _ = self
                .message_bus
                .get_or_create_channel(ComponentId::DxCluster)
                .await?;
            return Ok(());
        }

        let client_config =
            dx_cluster_client_config(&config.network.dx_cluster, &config.station.callsign);
        drop(config);

        info!(
            "Starting DX cluster component ({}:{})",
            client_config.hostname, client_config.port
        );

        let (_dx_tx, _dx_rx) = self
            .message_bus
            .get_or_create_channel(ComponentId::DxCluster)
            .await?;
        let message_bus = self.message_bus.clone();

        let dx_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                let mut client = DxClusterClient::with_config(client_config);

                // Connects, forwards spots, and reconnects with capped backoff
                // after every disconnect until shutdown (PAN-288).
                client
                    .run_until_shutdown(&shutdown, |spot| {
                        let message_bus = message_bus.clone();
                        async move {
                            debug!(
                                "DX spot: {} on {} Hz by {}",
                                spot.callsign, spot.frequency, spot.spotter
                            );

                            let msg = ComponentMessage::new(
                                ComponentId::DxCluster,
                                ComponentId::Tui,
                                MessageType::DxMessage(crate::message_bus::DxMessage::Spot {
                                    callsign: spot.callsign,
                                    frequency: spot.frequency,
                                    spotter: spot.spotter,
                                    comment: spot.comment.unwrap_or_default(),
                                }),
                                Instant::now(),
                            );
                            if let Err(e) = message_bus.send_message(msg).await {
                                debug!("Failed to forward DX spot: {}", e);
                            }
                        }
                    })
                    .await;

                let dropped = client.dropped_spot_count();
                if dropped > 0 {
                    info!("DX cluster dropped {dropped} spot(s) while its queue was full");
                }

                info!("DX cluster component stopped");
                Ok(())
            })
        };

        self.named_task_handles
            .push((ComponentId::DxCluster, dx_handle));
        info!("DX cluster component started");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dx_cluster_client_config_reads_connection_settings() {
        let mut dx = pancetta_config::DxClusterConfig::default();
        dx.connection.reconnect_delay_seconds = 7;
        dx.connection.timeout_seconds = 0;
        let cfg = dx_cluster_client_config(&dx, "N0CALL");
        assert_eq!(cfg.reconnect_delay_seconds, 7);
        assert_eq!(cfg.timeout_seconds, 1);
        assert!(cfg.auto_reconnect);
        assert_eq!(cfg.callsign, "N0CALL");

        let defaults =
            dx_cluster_client_config(&pancetta_config::DxClusterConfig::default(), "N0CALL");
        assert_eq!(defaults.reconnect_delay_seconds, 30);
        assert_eq!(defaults.timeout_seconds, 30);
        assert!(defaults.auto_reconnect);
        assert_eq!(
            (defaults.hostname.as_str(), defaults.port),
            ("dxc.nc7j.com", 23)
        );
    }
}
