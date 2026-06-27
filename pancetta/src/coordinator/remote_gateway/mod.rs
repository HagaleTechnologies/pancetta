//! Read-only remote-view gateway (Panino client). Default-OFF, localhost-bound.
//! Serves the v1 read-only view (decodes + QSO progress + scalar status) to
//! WebSocket clients using `pancetta_protocol` wire types over plain ws://.
//! NO control / NO remote-TX in v1: inbound client frames are ignored (logged).
//! TLS (wss) + the control/relay path are deferred to a later sub-plan.
pub(crate) mod translate;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use tokio::sync::{broadcast, RwLock};
use tokio::time::sleep;
use tracing::{debug, info, warn};

use pancetta_core::TxPolicy;
use pancetta_protocol::{ServerEvent, ServerFrame, StateSnapshot, Welcome, PROTOCOL_VERSION};

use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};

/// Maximum number of recent decodes to keep in the snapshot.
const RECENT_DECODES_CAP: usize = 100;

/// Additively relay a display event to the read-only gateway, **only** when the
/// gateway is enabled. Centralizes the gate so each emit site (decode fan-out,
/// QSO snapshot, frequency, s-meter, TX status, split) is a single additive
/// call placed *after* the existing `→Tui`/`→Qso` send — those sends are never
/// modified, preserving the TUI's behavior exactly (additive-only invariant).
///
/// Best-effort and fire-and-forget: a send error (no gateway reader yet, channel
/// full) is logged at debug and never affects the caller's pipeline. When the
/// gateway is disabled this is a single relaxed atomic load and immediate return
/// — no clone, no send.
pub(crate) async fn relay_to_gateway(
    bus: &MessageBus,
    gw_enabled: &AtomicBool,
    source: ComponentId,
    msg: MessageType,
) {
    if !gw_enabled.load(Ordering::Relaxed) {
        return;
    }
    let m = ComponentMessage::new(source, ComponentId::RemoteGateway, msg, Instant::now());
    if let Err(e) = bus.send_message(m).await {
        debug!(target: "gateway", "relay to gateway failed: {e}");
    }
}

/// Shared state threaded through each axum handler.
#[derive(Clone)]
pub(crate) struct GatewayState {
    /// Broadcast channel — the pump writes events here; each connected client
    /// has its own `broadcast::Receiver<ServerEvent>` subscription.
    pub evt_tx: broadcast::Sender<ServerEvent>,
    /// Latest full station state. Sent as the `Welcome` snapshot to each new
    /// client and kept up-to-date by the bus pump.
    pub snapshot: Arc<RwLock<StateSnapshot>>,
    /// Server version string embedded in the `Welcome` frame.
    pub server_version: String,
}

/// Build the axum `Router` for the gateway (routes: `GET /ws`).
pub(crate) fn build_router(state: GatewayState) -> Router {
    Router::new()
        .route("/ws", get(ws_handler))
        .with_state(state)
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<GatewayState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: GatewayState) {
    // Subscribe BEFORE reading the snapshot so no event is missed between the
    // snapshot read and the start of the forward loop.
    let mut rx = state.evt_tx.subscribe();

    let welcome = {
        let snap = state.snapshot.read().await.clone();
        ServerFrame::welcome(Welcome {
            protocol_version: PROTOCOL_VERSION,
            server_version: state.server_version.clone(),
            snapshot: snap,
        })
    };

    match serde_json::to_string(&welcome) {
        Ok(txt) => {
            // axum 0.8: Message::Text wraps axum::extract::ws::Utf8Bytes;
            // String implements Into<Utf8Bytes>.
            if socket.send(Message::Text(txt.into())).await.is_err() {
                return;
            }
        }
        Err(e) => {
            warn!(target: "gateway", "failed to serialize Welcome: {e}");
            return;
        }
    }

    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(event) => {
                    if let Ok(txt) = serde_json::to_string(&ServerFrame::event(event)) {
                        if socket.send(Message::Text(txt.into())).await.is_err() {
                            break;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!(target: "gateway", "client lagged, dropped {n} events");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Text(t))) => {
                    // v1 is read-only: ignore commands; control is a later sub-plan.
                    debug!(
                        target: "gateway",
                        "ignoring inbound client frame ({} bytes, read-only v1)",
                        t.len()
                    );
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {} // ping / pong / binary — ignored
                Some(Err(_)) => break,
            }
        }
    }
}

/// Translate one bus message, broadcast the resulting `ServerEvent`, and fold
/// it into the rolling snapshot. Decoded frames are enriched with dial
/// frequency, station-lookup flags, and our callsign here rather than in the
/// pure `translate` layer.
async fn handle_bus_msg(
    msg: &MessageType,
    op_freq: &AtomicU64,
    lookup: &crate::priority_evaluator::CachedStationLookup,
    our_callsign: &str,
    evt_tx: &broadcast::Sender<ServerEvent>,
    snapshot: &RwLock<StateSnapshot>,
) {
    use pancetta_qso::priority::WorkedStationLookup as _;

    let event = match msg {
        MessageType::DecodedMessage(decoded) => {
            let dial_hz = op_freq.load(Ordering::Relaxed) as f64;
            let call = decoded.message.from_callsign.as_deref();
            let (worked_before, needed, atno) = match call {
                Some(c) if !c.is_empty() => {
                    let freq = dial_hz + decoded.frequency_offset;
                    (
                        lookup.is_duplicate(c, freq),
                        lookup.is_needed_dxcc(c),
                        lookup.is_atno(c),
                    )
                }
                _ => (false, false, false),
            };
            let view =
                translate::decoded_to_view(decoded, dial_hz, our_callsign, worked_before, needed, atno);
            {
                let mut s = snapshot.write().await;
                s.recent_decodes.push(view.clone());
                if s.recent_decodes.len() > RECENT_DECODES_CAP {
                    let excess = s.recent_decodes.len() - RECENT_DECODES_CAP;
                    s.recent_decodes.drain(0..excess);
                }
            }
            ServerEvent::decoded(view)
        }

        other => match translate::server_event_from_bus(other) {
            Some(ev) => {
                // Fold scalar / aggregate state into the snapshot so new
                // clients that connect later get the current picture.
                {
                    let mut s = snapshot.write().await;
                    match &ev {
                        ServerEvent::ActiveQsos { qsos, pending } => {
                            s.active_qsos = qsos.clone();
                            s.pending_calls = pending.clone();
                        }
                        ServerEvent::Frequency { frequency_hz, .. } => {
                            s.frequency_hz = *frequency_hz;
                        }
                        ServerEvent::Split { tx_hz } => {
                            s.split_tx_hz = *tx_hz;
                        }
                        _ => {}
                    }
                }
                ev
            }
            None => return, // not a gateway-relevant message
        },
    };

    // `send` errors only when there are no active subscribers — that is fine.
    let _ = evt_tx.send(event);
}

impl super::ApplicationCoordinator {
    /// Start the read-only remote-view gateway (default-OFF, localhost-bound).
    ///
    /// When disabled, a no-op drain task is spawned so additive dual-target
    /// bus sends (which include `ComponentId::RemoteGateway`) never fill the
    /// channel and spam "Channel full" warnings.
    pub(crate) async fn start_remote_gateway_component(&mut self) -> Result<()> {
        let config = self.config.read().await;
        let gw_cfg = config.network.remote_gateway.clone();

        if !gw_cfg.enabled {
            info!("remote_gateway disabled in configuration");
            drop(config);

            // Drain the channel so the bus never floods.
            let (_drain_tx, drain_rx) =
                self.message_bus.create_channel(ComponentId::RemoteGateway).await?;
            let shutdown = self.shutdown_signal.clone();
            let drain_handle = tokio::spawn(async move {
                while !shutdown.load(Ordering::Acquire) {
                    loop {
                        match drain_rx.try_recv() {
                            Ok(_) => {}
                            Err(crossbeam_channel::TryRecvError::Empty) => break,
                            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                                return Ok::<(), anyhow::Error>(());
                            }
                        }
                    }
                    sleep(Duration::from_millis(100)).await;
                }
                Ok(())
            });
            self.named_task_handles
                .push((ComponentId::RemoteGateway, drain_handle));
            return Ok(());
        }

        let bind_addr = gw_cfg.bind_addr.clone();
        let our_callsign = config.station.callsign.clone();
        drop(config);

        let (_gw_tx, gw_rx) =
            self.message_bus.create_channel(ComponentId::RemoteGateway).await?;

        // Broadcast channel: pump → all connected clients.
        let (evt_tx, _evt_rx0) = broadcast::channel::<ServerEvent>(1024);

        // Snapshot: seeded from current atomics; kept live by the pump.
        let snapshot = Arc::new(RwLock::new(StateSnapshot {
            frequency_hz: self.operating_frequency_hz.load(Ordering::Relaxed),
            split_tx_hz: self.split_tx_frequency_hz.load(Ordering::Relaxed),
            tx_policy: TxPolicy::from_u8(self.tx_policy.load(Ordering::Acquire)),
            dx_hunter: Vec::new(),
            active_qsos: Vec::new(),
            pending_calls: Vec::new(),
            recent_decodes: Vec::new(),
        }));

        // ── Bus pump: translate bus messages → broadcast + snapshot ──────────
        let pump = {
            let shutdown = self.shutdown_signal.clone();
            let op_freq = self.operating_frequency_hz.clone();
            let lookup = self.cached_lookup.clone();
            let evt_tx_pump = evt_tx.clone();
            let snapshot_pump = snapshot.clone();
            let our_callsign_pump = our_callsign.clone();

            tokio::spawn(async move {
                while !shutdown.load(Ordering::Acquire) {
                    loop {
                        match gw_rx.try_recv() {
                            Ok(m) => {
                                handle_bus_msg(
                                    &m.message_type,
                                    &op_freq,
                                    &lookup,
                                    &our_callsign_pump,
                                    &evt_tx_pump,
                                    &snapshot_pump,
                                )
                                .await;
                            }
                            Err(crossbeam_channel::TryRecvError::Empty) => break,
                            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                                return Ok::<(), anyhow::Error>(());
                            }
                        }
                    }
                    sleep(Duration::from_millis(100)).await;
                }
                Ok(())
            })
        };

        // ── axum WebSocket server ─────────────────────────────────────────────
        let server = {
            let shutdown = self.shutdown_signal.clone();
            let state = GatewayState {
                evt_tx,
                snapshot,
                server_version: env!("CARGO_PKG_VERSION").to_string(),
            };

            tokio::spawn(async move {
                let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
                info!(
                    "remote_gateway listening on ws://{}/ws (read-only, localhost)",
                    bind_addr
                );
                let router = build_router(state);
                axum::serve(listener, router)
                    .with_graceful_shutdown(async move {
                        while !shutdown.load(Ordering::Acquire) {
                            sleep(Duration::from_millis(200)).await;
                        }
                    })
                    .await?;
                Ok::<(), anyhow::Error>(())
            })
        };

        self.named_task_handles.push((ComponentId::RemoteGateway, pump));
        self.named_task_handles.push((ComponentId::RemoteGateway, server));
        info!("remote_gateway component started");
        Ok(())
    }
}

// ── Integration test ─────────────────────────────────────────────────────────

#[cfg(test)]
mod server_tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use pancetta_core::TxPolicy;
    use pancetta_protocol::{ClientFrame, Hello, PROTOCOL_VERSION};

    fn empty_snapshot() -> StateSnapshot {
        StateSnapshot {
            frequency_hz: 14_074_000,
            split_tx_hz: 0,
            tx_policy: TxPolicy::Full,
            dx_hunter: vec![],
            active_qsos: vec![],
            pending_calls: vec![],
            recent_decodes: vec![],
        }
    }

    #[tokio::test]
    async fn handshake_then_event_fanout() {
        let (evt_tx, _) = broadcast::channel::<ServerEvent>(16);
        let snapshot = Arc::new(RwLock::new(empty_snapshot()));
        let state = GatewayState {
            evt_tx: evt_tx.clone(),
            snapshot,
            server_version: "test".into(),
        };

        // Bind on an OS-assigned ephemeral port so we never collide with
        // another test or the running application.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, build_router(state)).await.unwrap();
        });

        // Connect via tokio-tungstenite.
        let (mut ws, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
                .await
                .unwrap();

        // Send a Hello frame (server v1 is read-only so it ignores it, but
        // this exercises the inbound-ignore path and matches the wire contract).
        let hello = ClientFrame::hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "test".into(),
            client_version: "0".into(),
        });
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::to_string(&hello).unwrap().into(),
        ))
        .await
        .unwrap();

        // First server frame must be a Welcome containing the snapshot.
        let msg = ws.next().await.unwrap().unwrap();
        let frame: ServerFrame = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert!(
            matches!(frame, ServerFrame::Welcome { .. }),
            "expected Welcome as first frame, got: {frame:?}"
        );

        // Broadcast an event from the pump side.  Because the handler
        // subscribes BEFORE sending the Welcome, this event is guaranteed to
        // be delivered even if it is produced very shortly after connection.
        evt_tx
            .send(ServerEvent::TxStatus { active: true })
            .unwrap();

        // Second frame must be the Event wrapper around TxStatus.
        let msg2 = ws.next().await.unwrap().unwrap();
        let frame2: ServerFrame = serde_json::from_str(msg2.to_text().unwrap()).unwrap();
        assert!(
            matches!(
                frame2,
                ServerFrame::Event {
                    event: ServerEvent::TxStatus { active: true }
                }
            ),
            "expected TxStatus event, got: {frame2:?}"
        );
    }

    #[tokio::test]
    async fn welcome_snapshot_reflects_initial_state() {
        let (evt_tx, _) = broadcast::channel::<ServerEvent>(16);
        let mut snap = empty_snapshot();
        snap.frequency_hz = 7_074_000;
        snap.split_tx_hz = 7_075_500;
        snap.tx_policy = TxPolicy::RespondOnly;
        let snapshot = Arc::new(RwLock::new(snap));
        let state = GatewayState {
            evt_tx,
            snapshot,
            server_version: "v0.9.9-test".into(),
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, build_router(state)).await.unwrap();
        });

        let (mut ws, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
                .await
                .unwrap();

        let msg = ws.next().await.unwrap().unwrap();
        let frame: ServerFrame = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        match frame {
            ServerFrame::Welcome { welcome } => {
                assert_eq!(welcome.snapshot.frequency_hz, 7_074_000);
                assert_eq!(welcome.snapshot.split_tx_hz, 7_075_500);
                assert_eq!(welcome.snapshot.tx_policy, TxPolicy::RespondOnly);
                assert_eq!(welcome.server_version, "v0.9.9-test");
                assert_eq!(welcome.protocol_version, PROTOCOL_VERSION);
            }
            other => panic!("expected Welcome, got {other:?}"),
        }
    }
}
