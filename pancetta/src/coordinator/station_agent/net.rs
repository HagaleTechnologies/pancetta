//! Real transport adapters for the station-agent, behind the network-free
//! `pancetta_agent` traits.
//!
//! `pancetta_agent`'s core (crypto, verification, the relay/session state
//! machines) is deliberately **network-free** and exercised entirely with
//! scripted mocks. This module is the thin, un-unit-tested edge that binds those
//! traits to real IO — [`tokio_tungstenite`] for the relay WebSocket
//! ([`RealWsConn`]) and [`reqwest`] for the pairing HTTP POSTs
//! ([`ReqwestPairingHttp`]). Both live here (not in `pancetta-agent`) so the
//! agent crate never gains a networking dependency.
//!
//! The `pancetta_agent` seams ([`WsConn`], [`PairingHttp`]) are **synchronous**
//! by design (so the session/pairing logic is testable without an async
//! runtime). These adapters bridge sync→async by owning a Tokio [`Handle`] and
//! `block_on`-ing the async socket/HTTP calls. The station-agent component runs
//! its session loop on a `spawn_blocking` thread precisely so these blocking
//! bridge calls never stall the async executor.

use futures_util::{SinkExt, StreamExt};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use pancetta_agent::pairing::{PairingError, PairingHttp};
use pancetta_agent::relay::{RelayError, WsConn, MAX_FRAME_BYTES};

/// A real relay WebSocket connection adapting an async [`tokio_tungstenite`]
/// stream to the synchronous [`WsConn`] seam.
///
/// The async read/write halves are driven on a dedicated pump task; this handle
/// talks to that task over bounded channels and `block_on`s the channel ops via
/// the captured [`Handle`]. Constructed by [`connect`](RealWsConn::connect).
pub struct RealWsConn {
    handle: Handle,
    /// Outbound text frames → pump → socket.
    tx: mpsc::Sender<String>,
    /// Inbound text frames ← pump ← socket. `None` sentinel = closed.
    rx: mpsc::Receiver<Option<String>>,
}

impl RealWsConn {
    /// Dial `url` and return a synchronous [`WsConn`] over the established
    /// WebSocket. Must be called from within a Tokio runtime (uses the current
    /// [`Handle`]). Spawns a pump task that owns the socket for its lifetime.
    pub async fn connect(url: &str) -> Result<Self, RelayError> {
        let (stream, _resp) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| RelayError::Transport(format!("connect {url}: {e}")))?;
        let (mut sink, mut source) = stream.split();

        // Bounded so a stuck peer can't grow memory without limit.
        let (out_tx, mut out_rx) = mpsc::channel::<String>(64);
        let (in_tx, in_rx) = mpsc::channel::<Option<String>>(64);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Outbound: forward text frames to the socket.
                    maybe = out_rx.recv() => match maybe {
                        Some(text) => {
                            if sink.send(Message::Text(text)).await.is_err() {
                                let _ = in_tx.send(None).await;
                                break;
                            }
                        }
                        None => {
                            // Adapter dropped: close the socket and stop.
                            let _ = sink.send(Message::Close(None)).await;
                            break;
                        }
                    },
                    // Inbound: forward text frames (and closes) to the adapter.
                    inbound = source.next() => match inbound {
                        Some(Ok(Message::Text(t))) => {
                            if in_tx.send(Some(t)).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Binary(_))) | Some(Ok(Message::Ping(_)))
                        | Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {
                            // Non-text frames are not part of relay.v1; ignore.
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            let _ = in_tx.send(None).await;
                            break;
                        }
                        Some(Err(_)) => {
                            let _ = in_tx.send(None).await;
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            handle: Handle::current(),
            tx: out_tx,
            rx: in_rx,
        })
    }
}

impl WsConn for RealWsConn {
    fn send_text(&mut self, s: String) -> Result<(), RelayError> {
        // Defense-in-depth: never push an oversized frame onto the wire.
        if s.len() > MAX_FRAME_BYTES {
            return Err(RelayError::FrameTooLarge {
                len: s.len(),
                max: MAX_FRAME_BYTES,
            });
        }
        let tx = self.tx.clone();
        self.handle
            .block_on(async move { tx.send(s).await })
            .map_err(|_| RelayError::Transport("relay send channel closed".to_string()))
    }

    fn recv_text(&mut self) -> Result<Option<String>, RelayError> {
        // `None` from the pump (channel closed OR explicit close sentinel) maps
        // to a closed connection — the session driver treats `Ok(None)` as
        // drained and the component reconnects.
        match self.handle.block_on(self.rx.recv()) {
            Some(Some(text)) => Ok(Some(text)),
            Some(None) | None => Ok(None),
        }
    }
}

/// A real pairing HTTP client adapting [`reqwest`] to the synchronous
/// [`PairingHttp`] seam. POSTs JSON to `{base_url}{path}` and parses the JSON
/// response. Bridges sync→async via a captured Tokio [`Handle`].
pub struct ReqwestPairingHttp {
    handle: Handle,
    client: reqwest::Client,
    base_url: String,
}

impl ReqwestPairingHttp {
    /// Create a pairing HTTP client rooted at `base_url` (e.g.
    /// `https://pair.example/api`). Must be constructed within a Tokio runtime.
    pub fn new(base_url: String) -> Self {
        Self {
            handle: Handle::current(),
            client: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }
}

impl PairingHttp for ReqwestPairingHttp {
    fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, PairingError> {
        let url = format!("{}{}", self.base_url, path);
        let client = self.client.clone();
        self.handle.block_on(async move {
            let resp = client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| PairingError::Transport(format!("POST {url}: {e}")))?;
            if !resp.status().is_success() {
                return Err(PairingError::Transport(format!(
                    "POST {url}: HTTP {}",
                    resp.status()
                )));
            }
            resp.json::<serde_json::Value>()
                .await
                .map_err(|e| PairingError::MalformedResponse(e.to_string()))
        })
    }
}
