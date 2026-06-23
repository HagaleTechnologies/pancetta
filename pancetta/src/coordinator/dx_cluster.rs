//! DX cluster component.
//!
//! Polls the configured DX cluster (TCP `cluster.foo:7300` or similar)
//! for live spots and forwards them onto the message bus as
//! `MessageType::DxMessage(DxMessage::Spot { … })` for the TUI's DX
//! Hunter view and the autonomous operator's priority scorer.
//!
//! No-op when `[network.dx_cluster].enabled = false` (the default).

use anyhow::Result;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, span, warn, Level};

use crate::message_bus::{ComponentId, ComponentMessage, MessageType};

impl super::ApplicationCoordinator {
    pub(crate) async fn start_dx_cluster_component(&mut self) -> Result<()> {
        let config = self.config.read().await;
        if !config.network.dx_cluster.enabled {
            info!("DX cluster disabled in configuration");
            drop(config);
            // Still create channel so message bus doesn't complain
            let _ = self
                .message_bus
                .create_channel(ComponentId::DxCluster)
                .await?;
            return Ok(());
        }

        let cluster_hostname = config
            .network
            .dx_cluster
            .servers
            .first()
            .map(|s| s.hostname.clone())
            .unwrap_or_else(|| "dxc.nc7j.com".to_string());
        let cluster_port = config
            .network
            .dx_cluster
            .servers
            .first()
            .map(|s| s.port)
            .unwrap_or(23);
        let our_callsign = config.station.callsign.clone();
        drop(config);

        info!(
            "Starting DX cluster component ({}:{})",
            cluster_hostname, cluster_port
        );

        let (_dx_tx, _dx_rx) = self
            .message_bus
            .create_channel(ComponentId::DxCluster)
            .await?;
        let message_bus = self.message_bus.clone();

        let dx_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                use pancetta_dx::cluster::{ClusterConfig, DxClusterClient};

                let mut client = DxClusterClient::with_config(ClusterConfig {
                    hostname: cluster_hostname.clone(),
                    port: cluster_port,
                    callsign: our_callsign.clone(),
                    timeout_seconds: 30,
                    reconnect_delay_seconds: 30,
                    auto_reconnect: true,
                    filter_settings: Default::default(),
                    use_websocket: false,
                    websocket_url: None,
                });

                match client.connect().await {
                    Ok(_) => {
                        info!("Connected to DX cluster");

                        // Login with our callsign
                        if let Err(e) = client.login().await {
                            warn!("DX cluster login failed: {}. Continuing without.", e);
                        }

                        // Monitor spots and forward to TUI
                        while !shutdown.load(Ordering::Acquire) {
                            match tokio::time::timeout(
                                Duration::from_secs(5),
                                client.receive_spot(),
                            )
                            .await
                            {
                                Ok(Some(spot)) => {
                                    debug!(
                                        "DX spot: {} on {} Hz by {}",
                                        spot.callsign, spot.frequency, spot.spotter
                                    );

                                    let msg = ComponentMessage::new(
                                        ComponentId::DxCluster,
                                        ComponentId::Tui,
                                        MessageType::DxMessage(
                                            crate::message_bus::DxMessage::Spot {
                                                callsign: spot.callsign,
                                                frequency: spot.frequency,
                                                spotter: spot.spotter,
                                                comment: spot.comment.unwrap_or_default(),
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(msg).await {
                                        debug!("Failed to forward DX spot: {}", e);
                                    }
                                }
                                Ok(None) => {
                                    // No spot available, yield
                                    tokio::task::yield_now().await;
                                }
                                Err(_) => {
                                    // Timeout -- normal, just loop
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to connect to DX cluster: {}. Feature disabled.", e);
                    }
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
