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
//! `block_on`-ing the async socket/HTTP calls.
//!
//! [`RealWsConn`]'s driver (the station-agent's session loop) runs as a normal
//! `tokio::spawn`ed async task, not a bare OS thread — so a bare `block_on`
//! inside [`WsConn::recv_text`]/[`send_text`](WsConn::send_text) would nest
//! inside that task's own poll and panic ("Cannot start a runtime from within
//! a runtime"). [`RealWsConn`] wraps each `block_on` in
//! [`tokio::task::block_in_place`], Tokio's documented escape hatch for a
//! blocking call inside an async task on a multi-thread runtime. This
//! doesn't apply to [`ReqwestPairingHttp`]: its one caller
//! (`pancetta pair`) already runs it via `tokio::task::spawn_blocking`, a
//! genuine OS thread with no outer `block_on` to nest inside.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

use pancetta_agent::pairing::{PairingError, PairingHttp};
use pancetta_agent::relay::{RecvOutcome, RelayError, WsConn, MAX_FRAME_BYTES};

/// Transport limits for the relay socket. relay.v1 sends one frame per text
/// message and [`parse_frame`](pancetta_agent::relay::parse_frame) refuses
/// anything over [`MAX_FRAME_BYTES`]; matching it here makes tungstenite
/// refuse a larger frame from its header, before reading the payload.
fn relay_ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES))
}

/// Most bytes read from one pairing API response (small JSON objects).
const MAX_PAIRING_RESPONSE_BYTES: usize = 64 * 1024;
/// Total time allowed for one pairing POST, connect through body.
const PAIRING_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

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
        let (stream, _resp) =
            tokio_tungstenite::connect_async_with_config(url, Some(relay_ws_config()), false)
                .await
                .map_err(|e| RelayError::Transport(format!("connect {url}: {e}")))?;
        let (mut sink, mut source) = stream.split();

        // The channels bound queue depth and `relay_ws_config` bounds each
        // frame, so a stuck or oversending peer can't grow memory without
        // limit.
        let (out_tx, mut out_rx) = mpsc::channel::<String>(64);
        let (in_tx, in_rx) = mpsc::channel::<Option<String>>(64);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Outbound: forward text frames to the socket.
                    maybe = out_rx.recv() => match maybe {
                        Some(text) => {
                            if sink.send(Message::Text(text.into())).await.is_err() {
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
                            if in_tx.send(Some(t.to_string())).await.is_err() {
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
                        Some(Err(e)) => {
                            tracing::debug!(target: "agent", "relay socket error: {e}");
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
        let handle = &self.handle;
        // See the module doc: the session loop that drives this seam runs as
        // a normal async task (`tokio::spawn`), not a bare OS thread, so a
        // bare `handle.block_on` here would nest inside the task's own poll
        // and panic ("Cannot start a runtime from within a runtime").
        // `block_in_place` is Tokio's documented escape hatch for exactly
        // this: it hands this worker's other tasks off to the rest of the
        // multi-thread pool for the closure's duration, making a nested
        // `block_on` legal again.
        tokio::task::block_in_place(|| handle.block_on(async move { tx.send(s).await }))
            .map_err(|_| RelayError::Transport("relay send channel closed".to_string()))
    }

    fn recv_text(&mut self) -> Result<Option<String>, RelayError> {
        // `None` from the pump (channel closed OR explicit close sentinel) maps
        // to a closed connection — the session driver treats `Ok(None)` as
        // drained and the component reconnects.
        let handle = &self.handle;
        let rx = &mut self.rx;
        match tokio::task::block_in_place(|| handle.block_on(rx.recv())) {
            Some(Some(text)) => Ok(Some(text)),
            Some(None) | None => Ok(None),
        }
    }

    fn recv_text_within(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<RecvOutcome, RelayError> {
        let handle = &self.handle;
        let rx = &mut self.rx;
        let out = tokio::task::block_in_place(|| {
            handle.block_on(async move {
                match tokio::time::timeout(timeout, rx.recv()).await {
                    Err(_elapsed) => RecvOutcome::Quiet,
                    Ok(Some(Some(text))) => RecvOutcome::Frame(text),
                    Ok(Some(None)) | Ok(None) => RecvOutcome::Closed,
                }
            })
        });
        Ok(out)
    }
}

/// A real pairing HTTP client adapting [`reqwest`] to the synchronous
/// [`PairingHttp`] seam. POSTs JSON to `{base_url}{path}` and parses the JSON
/// response. Bridges sync→async via a captured Tokio [`Handle`].
pub struct ReqwestPairingHttp {
    handle: Handle,
    client: reqwest::Client,
    base_url: String,
    /// Optional CSRF `Origin` header value sent on every POST. `None` → no
    /// `Origin` header (production posture; set only as a staging CSRF
    /// workaround until the API-key CSRF exemption ships).
    origin: Option<String>,
}

impl ReqwestPairingHttp {
    /// Create a pairing HTTP client rooted at `base_url` (e.g.
    /// `https://pair.example/api/v1`). Must be constructed within a Tokio
    /// runtime.
    ///
    /// `base_url` must include the API base path (e.g. `/api/v1`); the pairing
    /// client posts *relative* `/pair/*` paths onto it. When `origin` is
    /// `Some(o)`, every POST carries `Origin: o` — a staging CSRF workaround;
    /// `None` sends no `Origin` header.
    ///
    /// Each POST, connect through response body, gives up after 30 seconds.
    pub fn new(base_url: String, origin: Option<String>) -> Self {
        Self::with_timeout(base_url, origin, PAIRING_HTTP_TIMEOUT)
    }

    fn with_timeout(base_url: String, origin: Option<String>, timeout: Duration) -> Self {
        Self {
            handle: Handle::current(),
            client: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base_url: base_url.trim_end_matches('/').to_string(),
            origin,
        }
    }
}

impl PairingHttp for ReqwestPairingHttp {
    fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, PairingError> {
        let url = format!("{}{}", self.base_url, path);
        let client = self.client.clone();
        let origin = self.origin.clone();
        self.handle.block_on(async move {
            let mut req = client.post(&url).json(&body);
            if let Some(o) = origin {
                req = req.header("Origin", o);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| PairingError::Transport(format!("POST {url}: {e}")))?;
            if !resp.status().is_success() {
                return Err(PairingError::Transport(format!(
                    "POST {url}: HTTP {}",
                    resp.status()
                )));
            }
            let too_large = || {
                PairingError::MalformedResponse(format!(
                    "response larger than {MAX_PAIRING_RESPONSE_BYTES} bytes"
                ))
            };
            if resp
                .content_length()
                .is_some_and(|n| n > MAX_PAIRING_RESPONSE_BYTES as u64)
            {
                return Err(too_large());
            }
            let mut resp = resp;
            let mut body = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|e| PairingError::MalformedResponse(e.to_string()))?
            {
                if body.len() + chunk.len() > MAX_PAIRING_RESPONSE_BYTES {
                    return Err(too_large());
                }
                body.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&body)
                .map_err(|e| PairingError::MalformedResponse(e.to_string()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reqwest_pairing_http_threads_origin() {
        // No origin → production posture: no Origin header will be sent.
        let http = ReqwestPairingHttp::new("https://host/api/v1/".to_string(), None);
        assert_eq!(http.base_url, "https://host/api/v1"); // trailing slash trimmed
        assert!(http.origin.is_none());

        // Some origin → carried through for the CSRF workaround.
        let http = ReqwestPairingHttp::new(
            "https://host/api/v1".to_string(),
            Some("https://cqdx.io".to_string()),
        );
        assert_eq!(http.origin.as_deref(), Some("https://cqdx.io"));
    }

    /// Regression test for the 2026-07-21 live incident: `RealWsConn::send_text`/
    /// `recv_text` panicked ("Cannot start a runtime from within a runtime") the
    /// first time they ran inside the station-agent's real `tokio::spawn`ed
    /// session-loop task, because they nested a bare `handle.block_on` inside
    /// that task's own poll. Reproduces the exact condition — a real WebSocket,
    /// driven from inside a spawned async task on a multi-thread runtime, not
    /// the test's own root task — and would panic without the `block_in_place`
    /// fix. Needs `worker_threads > 1`: `block_in_place` itself panics on a
    /// single-worker multi-thread runtime.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_ws_conn_send_and_recv_from_within_a_spawned_task() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Server: accept one connection, echo exactly one text frame back.
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let (mut sink, mut source) = ws.split();
            if let Some(Ok(Message::Text(t))) = source.next().await {
                sink.send(Message::Text(t)).await.unwrap();
            }
        });

        // Client: exactly the production shape — connect, then drive
        // send_text/recv_text from *inside* a spawned task, not the test's
        // own root task.
        let client = tokio::spawn(async move {
            let mut conn = RealWsConn::connect(&format!("ws://{addr}"))
                .await
                .expect("connect");
            conn.send_text("hello".to_string()).expect("send_text");
            conn.recv_text().expect("recv_text")
        });

        let received = client.await.expect("client task panicked");
        assert_eq!(received, Some("hello".to_string()));
    }

    /// Serve one raw HTTP/1.1 connection on a std thread: read the request
    /// headers and any `Content-Length` body, then answer `200` with
    /// `prefix` followed by `pad` spaces, chunked in 64 KiB pieces. Write
    /// errors (the client hung up) end the response early.
    fn serve_chunked_json_once(prefix: &'static str, pad: usize) -> std::net::SocketAddr {
        use std::io::{BufRead, BufReader, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                }
            }
            let mut request_body = vec![0u8; content_length];
            let _ = reader.read_exact(&mut request_body);

            let mut stream = stream;
            let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                        Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            let mut payload = prefix.as_bytes().to_vec();
            payload.resize(prefix.len() + pad, b' ');
            for chunk in payload.chunks(64 * 1024) {
                let framed = [format!("{:x}\r\n", chunk.len()).as_bytes(), chunk, b"\r\n"].concat();
                if stream.write_all(&framed).is_err() {
                    return;
                }
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        });
        addr
    }

    /// A relay text frame exactly `MAX_FRAME_BYTES` long is delivered; a
    /// larger one closes the connection instead of being buffered whole.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_ws_conn_accepts_a_frame_at_the_relay_limit_and_closes_past_it() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Server (default limits): one frame at the relay limit, then one
        // well past it.
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = ws
                .send(Message::Text("a".repeat(MAX_FRAME_BYTES).into()))
                .await;
            let _ = ws
                .send(Message::Text("b".repeat(2 * 1024 * 1024).into()))
                .await;
            // Hold the socket open so a close comes from the client's limit.
            let _ = ws.next().await;
        });

        let client = tokio::spawn(async move {
            let mut conn = RealWsConn::connect(&format!("ws://{addr}"))
                .await
                .expect("connect");
            let first = conn.recv_text().expect("first recv_text");
            let second = conn.recv_text().expect("second recv_text");
            (first, second)
        });

        let (first, second) = tokio::time::timeout(Duration::from_secs(10), client)
            .await
            .expect("client timed out")
            .expect("client task panicked");
        assert_eq!(first.map(|t| t.len()), Some(MAX_FRAME_BYTES));
        assert_eq!(second.map(|t| t.len()), None);
    }

    /// A text message sent as many small fragments is limited by its total
    /// size, not by each fragment.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_ws_conn_closes_on_a_fragmented_message_past_the_limit() {
        use tokio::net::TcpListener;
        use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
        use tokio_tungstenite::tungstenite::protocol::frame::Frame;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Server: one text message as 40 fragments of 32 KiB (1.25 MiB).
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let fragment = "x".repeat(32 * 1024);
            let count = 40;
            for i in 0..count {
                let opcode = if i == 0 {
                    OpCode::Data(Data::Text)
                } else {
                    OpCode::Data(Data::Continue)
                };
                let frame = Frame::message(fragment.clone().into_bytes(), opcode, i == count - 1);
                if ws.send(Message::Frame(frame)).await.is_err() {
                    break;
                }
            }
            // Hold the socket open so a close comes from the client's limit.
            let _ = ws.next().await;
        });

        let client = tokio::spawn(async move {
            let mut conn = RealWsConn::connect(&format!("ws://{addr}"))
                .await
                .expect("connect");
            conn.recv_text().expect("recv_text")
        });

        let received = tokio::time::timeout(Duration::from_secs(10), client)
            .await
            .expect("client timed out")
            .expect("client task panicked");
        assert_eq!(received.map(|t| t.len()), None);
    }

    /// A pairing response body past `MAX_PAIRING_RESPONSE_BYTES` is refused
    /// rather than read and parsed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pairing_post_refuses_an_oversized_response() {
        let addr = serve_chunked_json_once(r#"{"a":1}"#, 1024 * 1024);
        let http = ReqwestPairingHttp::new(format!("http://{addr}"), None);
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::task::spawn_blocking(move || http.post("/pair/agent", serde_json::json!({}))),
        )
        .await
        .expect("post timed out")
        .expect("post panicked");
        match result {
            Err(PairingError::MalformedResponse(m)) => {
                assert!(m.contains("larger than 65536 bytes"), "message: {m}");
            }
            other => panic!("expected MalformedResponse, got {other:?}"),
        }
    }

    /// A pairing server that accepts and never answers fails the POST once
    /// the client timeout elapses.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pairing_post_gives_up_after_its_timeout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            // Accept, then hold the connection without answering.
            if let Ok((stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(10));
                drop(stream);
            }
        });

        let http = ReqwestPairingHttp::with_timeout(
            format!("http://{addr}"),
            None,
            Duration::from_millis(200),
        );
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || http.post("/pair/agent", serde_json::json!({}))),
        )
        .await
        .expect("post did not give up within 5 s")
        .expect("post panicked");
        assert!(
            matches!(result, Err(PairingError::Transport(_))),
            "expected Transport error, got {result:?}"
        );
    }
}
