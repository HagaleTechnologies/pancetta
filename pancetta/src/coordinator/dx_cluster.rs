//! DX cluster component.
//!
//! Polls the configured DX cluster (TCP `cluster.foo:7300` or similar)
//! for live spots and forwards them onto the message bus as
//! `MessageType::DxMessage(DxMessage::Spot { … })` for the TUI's DX
//! Hunter view and the autonomous operator's priority scorer.
//!
//! No-op when `[network.dx_cluster].enabled = false` (the default).

use anyhow::Result;
use pancetta_core::{Band, Mode};
use pancetta_dx::cluster::{ClusterConfig, ClusterFilter, DxClusterClient};
use pancetta_dx::DxSpot;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info};

use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};

/// Client settings for the DX-cluster worker: the first configured server
/// (or `dxc.nc7j.com:23`), and `[network.dx_cluster.connection]`'s connect
/// timeout (at least 1 s) and reconnect delay. The worker always reconnects
/// (PAN-288). `[network.dx_cluster.filtering]` becomes the client's local
/// spot filter (PAN-345); an invalid band/mode name or inverted frequency
/// bounds is an error, never a silently widened allowlist.
fn dx_cluster_client_config(
    dx: &pancetta_config::DxClusterConfig,
    callsign: &str,
) -> Result<ClusterConfig> {
    let server = dx.servers.first();
    Ok(ClusterConfig {
        hostname: server
            .map(|s| s.hostname.clone())
            .unwrap_or_else(|| "dxc.nc7j.com".to_string()),
        port: server.map(|s| s.port).unwrap_or(23),
        callsign: callsign.to_string(),
        timeout_seconds: u64::from(dx.connection.timeout_seconds.max(1)),
        reconnect_delay_seconds: dx.connection.reconnect_delay_seconds,
        auto_reconnect: true,
        filter_settings: dx_cluster_filter(&dx.filtering)?,
        use_websocket: false,
        websocket_url: None,
    })
}

/// `[network.dx_cluster.filtering]` as the client's local filter (PAN-345).
/// Names are trimmed; bands parse after ASCII lowercasing and modes through
/// the shared `Mode` parser. `dxcc_filters` is not applied by this path.
fn dx_cluster_filter(filtering: &pancetta_config::ClusterFilteringConfig) -> Result<ClusterFilter> {
    let bands = filtering
        .bands
        .iter()
        .map(|name| {
            let name = name.trim();
            name.to_ascii_lowercase()
                .parse::<Band>()
                .map_err(|_| anyhow::anyhow!("Invalid DX cluster band filter \"{name}\""))
        })
        .collect::<Result<Vec<_>>>()?;
    let modes = filtering
        .modes
        .iter()
        .map(|name| {
            let name = name.trim();
            name.parse::<Mode>()
                .map_err(|_| anyhow::anyhow!("Invalid DX cluster mode filter \"{name}\""))
        })
        .collect::<Result<Vec<_>>>()?;
    if let (Some(min), Some(max)) = (filtering.min_frequency, filtering.max_frequency) {
        if min > max {
            anyhow::bail!("DX cluster minimum frequency exceeds maximum frequency");
        }
    }
    Ok(ClusterFilter {
        bands,
        modes,
        min_frequency: filtering.min_frequency,
        max_frequency: filtering.max_frequency,
        duplicate_timeout: u64::from(filtering.duplicate_window_minutes) * 60,
        ..ClusterFilter::default()
    })
}

/// The bus message for an accepted cluster spot, addressed to the TUI. Mode
/// is the one parsed from the comment (`None` when unknown, never assumed
/// FT8); band comes from the spot's Hz frequency through the shared table.
fn dx_spot_message(spot: DxSpot) -> ComponentMessage {
    let band = Band::from_frequency(spot.frequency);
    ComponentMessage::new(
        ComponentId::DxCluster,
        ComponentId::Tui,
        MessageType::DxMessage(crate::message_bus::DxMessage::Spot {
            callsign: spot.callsign,
            frequency: spot.frequency,
            mode: spot.mode,
            band,
            spotter: spot.spotter,
            comment: spot.comment.unwrap_or_default(),
        }),
        Instant::now(),
    )
}

/// The DX-cluster worker: connects, forwards every spot the client's local
/// filter accepts, and reconnects with capped backoff after every disconnect
/// until shutdown (PAN-288).
async fn run_dx_cluster_worker(
    client_config: ClusterConfig,
    message_bus: MessageBus,
    shutdown: Arc<AtomicBool>,
) {
    let mut client = DxClusterClient::with_config(client_config);

    client
        .run_until_shutdown(&shutdown, |spot| {
            let message_bus = message_bus.clone();
            async move {
                debug!(
                    "DX spot: {} on {} Hz by {}",
                    spot.callsign, spot.frequency, spot.spotter
                );
                if let Err(e) = message_bus.send_message(dx_spot_message(spot)).await {
                    debug!("Failed to forward DX spot: {}", e);
                }
            }
        })
        .await;

    let dropped = client.dropped_spot_count();
    if dropped > 0 {
        info!("DX cluster dropped {dropped} spot(s) while its queue was full");
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
            dx_cluster_client_config(&config.network.dx_cluster, &config.station.callsign)?;
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
                run_dx_cluster_worker(client_config, message_bus, shutdown).await;
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

    fn filtering(
        bands: &[&str],
        modes: &[&str],
        min: Option<u64>,
        max: Option<u64>,
        minutes: u32,
    ) -> pancetta_config::DxClusterConfig {
        let mut dx = pancetta_config::DxClusterConfig::default();
        dx.filtering.bands = bands.iter().map(|s| s.to_string()).collect();
        dx.filtering.modes = modes.iter().map(|s| s.to_string()).collect();
        dx.filtering.min_frequency = min;
        dx.filtering.max_frequency = max;
        dx.filtering.duplicate_window_minutes = minutes;
        dx
    }

    #[test]
    fn dx_cluster_client_config_reads_filters() {
        let dx = filtering(
            &[" 20M ", "40m"],
            &["ft8", " USB "],
            Some(14_000_000),
            Some(14_350_000),
            10,
        );
        let filter = dx_cluster_client_config(&dx, "N0CALL")
            .unwrap()
            .filter_settings;
        assert_eq!(filter.bands, vec![Band::Band20m, Band::Band40m]);
        assert_eq!(filter.modes, vec![Mode::FT8, Mode::USB]);
        assert_eq!(filter.min_frequency, Some(14_000_000));
        assert_eq!(filter.max_frequency, Some(14_350_000));
        assert_eq!(filter.duplicate_timeout, 600);
        assert!(filter.dxcc_entities.is_empty(), "DXCC is not applied here");

        let zero = filtering(&[], &[], None, None, 0);
        let filter = dx_cluster_client_config(&zero, "N0CALL")
            .unwrap()
            .filter_settings;
        assert!(filter.bands.is_empty() && filter.modes.is_empty());
        assert_eq!((filter.min_frequency, filter.max_frequency), (None, None));
        assert_eq!(filter.duplicate_timeout, 0);

        // Equal bounds are a valid one-frequency window.
        let equal = filtering(&[], &[], Some(14_074_000), Some(14_074_000), 1);
        assert!(dx_cluster_client_config(&equal, "N0CALL").is_ok());

        let defaults = dx_cluster_client_config(&pancetta_config::DxClusterConfig::default(), "N0")
            .unwrap()
            .filter_settings;
        assert_eq!(defaults.duplicate_timeout, 600);
    }

    #[test]
    fn dx_cluster_client_config_rejects_invalid_filters() {
        for (dx, expected) in [
            (
                filtering(&["20m", "bogus"], &[], None, None, 10),
                "Invalid DX cluster band filter \"bogus\"",
            ),
            (
                filtering(&[], &["FT8", "FT88"], None, None, 10),
                "Invalid DX cluster mode filter \"FT88\"",
            ),
            (
                filtering(&[], &[], Some(14_350_000), Some(14_000_000), 10),
                "DX cluster minimum frequency exceeds maximum frequency",
            ),
        ] {
            let err = dx_cluster_client_config(&dx, "N0CALL").unwrap_err();
            assert_eq!(err.to_string(), expected);
        }
    }

    fn accepted_spot(comment: &str, mode: Option<Mode>) -> DxSpot {
        DxSpot {
            callsign: "JA1XYZ".to_string(),
            frequency: 14_074_000,
            mode,
            spotter: "W1ABC".to_string(),
            time: chrono::Utc::now(),
            comment: Some(comment.to_string()),
            dxcc_entity: None,
            grid_square: None,
            distance_km: None,
            bearing_degrees: None,
            rarity_score: None,
        }
    }

    #[test]
    fn spot_forwarding_preserves_metadata() {
        for (mode, comment) in [
            (Some(Mode::FT8), "FT8 -10 dB"),
            (Some(Mode::CW), "CW 25 WPM"),
            (None, "loud"),
        ] {
            let msg = dx_spot_message(accepted_spot(comment, mode));
            assert_eq!(
                (msg.source, msg.destination),
                (ComponentId::DxCluster, ComponentId::Tui)
            );
            match msg.message_type {
                MessageType::DxMessage(crate::message_bus::DxMessage::Spot {
                    callsign,
                    frequency,
                    mode: got_mode,
                    band,
                    spotter,
                    comment: got_comment,
                }) => {
                    assert_eq!(callsign, "JA1XYZ");
                    assert_eq!(frequency, 14_074_000);
                    assert_eq!(got_mode, mode);
                    assert_eq!(band, Some(Band::Band20m));
                    assert_eq!(spotter, "W1ABC");
                    assert_eq!(got_comment, comment);
                }
                other => panic!("expected a DX spot, got {other:?}"),
            }
        }
        let mut off_band = accepted_spot("FT8", Some(Mode::FT8));
        off_band.frequency = 1_000_000;
        match dx_spot_message(off_band).message_type {
            MessageType::DxMessage(crate::message_bus::DxMessage::Spot { band, .. }) => {
                assert_eq!(band, None)
            }
            other => panic!("expected a DX spot, got {other:?}"),
        }
    }

    /// AC1/AC3: operator filters run before the bus. A loopback cluster sends
    /// a CW spot on 20m (rejected by the mode allowlist), an FT8 spot on
    /// 7074.0 kHz (AC3's rejected band case: mode-allowed, outside
    /// bands = ["20m"]) and an FT8 sentinel on 14074.0 kHz (AC3's permitted
    /// band case, inside ["20m"]); the real TUI bus receiver sees only the
    /// sentinel, which arrives last, so no sleep is needed to infer that the
    /// others were dropped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn configured_cluster_filters_before_bus() {
        use std::io::{Read, Write};
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let time = chrono::Utc::now().format("%H%MZ");
            let lines = format!(
                "Welcome\r\n\
                 DX de W1ABC:     14074.0  JA1XYZ       CW 25 WPM          {time}\r\n\
                 DX de W1ABC:      7074.0  JA2XYZ       FT8 -10 dB         {time}\r\n\
                 DX de W1ABC:     14074.0  JA1XYZ       FT8 -12 dB         {time}\r\n"
            );
            stream.write_all(lines.as_bytes()).unwrap();
            // Hold the session until the client hangs up (or the timeout).
            let mut buf = [0u8; 256];
            while matches!(stream.read(&mut buf), Ok(n) if n > 0) {}
        });

        let mut dx = filtering(&["20m"], &["FT8"], None, None, 10);
        dx.servers = vec![pancetta_config::ClusterServer {
            name: "loopback".to_string(),
            hostname: "127.0.0.1".to_string(),
            port,
            server_type: pancetta_config::ClusterType::DxSpider,
            auth_required: false,
            username: None,
            priority: 1,
        }];
        let client_config = dx_cluster_client_config(&dx, "N0CALL").unwrap();

        let bus = MessageBus::new(16).unwrap();
        let (_tui_tx, tui_rx) = bus.create_channel(ComponentId::Tui).await.unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = tokio::spawn(run_dx_cluster_worker(
            client_config,
            bus.clone(),
            shutdown.clone(),
        ));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let first = loop {
            if let Ok(msg) = tui_rx.try_recv() {
                break msg;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no spot reached the TUI within 10 s"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        match first.message_type {
            MessageType::DxMessage(crate::message_bus::DxMessage::Spot {
                callsign,
                frequency,
                mode,
                band,
                comment,
                ..
            }) => {
                assert_eq!(callsign, "JA1XYZ");
                assert_eq!(frequency, 14_074_000);
                assert_eq!(mode, Some(Mode::FT8));
                assert_eq!(band, Some(Band::Band20m));
                assert!(
                    comment.starts_with("FT8 -12 dB"),
                    "only the FT8 sentinel on 20m: {comment:?}"
                );
            }
            other => panic!("expected the FT8 sentinel spot, got {other:?}"),
        }
        assert!(tui_rx.try_recv().is_err(), "nothing else reached the TUI");

        shutdown.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("worker stops after shutdown")
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(15),
            tokio::task::spawn_blocking(move || server.join()),
        )
        .await
        .expect("loopback server exits")
        .unwrap()
        .unwrap();
    }

    #[test]
    fn dx_cluster_client_config_reads_connection_settings() {
        let mut dx = pancetta_config::DxClusterConfig::default();
        dx.connection.reconnect_delay_seconds = 7;
        dx.connection.timeout_seconds = 0;
        let cfg = dx_cluster_client_config(&dx, "N0CALL").unwrap();
        assert_eq!(cfg.reconnect_delay_seconds, 7);
        assert_eq!(cfg.timeout_seconds, 1);
        assert!(cfg.auto_reconnect);
        assert_eq!(cfg.callsign, "N0CALL");

        let defaults =
            dx_cluster_client_config(&pancetta_config::DxClusterConfig::default(), "N0CALL")
                .unwrap();
        assert_eq!(defaults.reconnect_delay_seconds, 30);
        assert_eq!(defaults.timeout_seconds, 30);
        assert!(defaults.auto_reconnect);
        assert_eq!(
            (defaults.hostname.as_str(), defaults.port),
            ("dxc.nc7j.com", 23)
        );
    }
}
