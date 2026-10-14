//! Real transport adapters for the station-agent, behind the network-free
//! `pancetta_agent` traits.
//!
//! `pancetta_agent`'s core (crypto, verification, the relay/session state
//! machines) is deliberately **network-free** and exercised entirely with
//! scripted mocks. This module is the thin edge that binds those traits to
//! real IO — [`tokio_tungstenite`] for the relay WebSocket
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
//!
//! Code inside `block_in_place` cannot be cancelled: neither an `abort()` nor
//! a shutdown reaches the session task until the closure returns. So every
//! `block_in_place` wait in [`RealWsConn`] is wrapped in a [`RelayTimeouts`]
//! deadline (PAN-102). That bound is what keeps the session task cancellable,
//! and what turns a relay that stops responding into a session teardown.

use std::time::Duration;

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tracing::debug;

use pancetta_agent::pairing::{PairingError, PairingHttp};
use pancetta_agent::relay::{RecvOutcome, RelayError, WsConn, MAX_FRAME_BYTES};

/// Dial + TLS + WebSocket upgrade.
pub const RELAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// WebSocket upgrade complete → the relay's opening `hello` (the
/// authentication deadline). Also bounds [`RealWsConn`]'s `recv_text`.
pub const RELAY_HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest `send_text` waits for a free outbound slot.
pub const RELAY_SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest one frame's socket write + flush may stay pending.
pub const RELAY_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Depth of the adapter ↔ pump queues. Bounded so a stuck peer can't grow memory.
pub(crate) const RELAY_QUEUE_DEPTH: usize = 64;

/// Deadlines for every relay IO wait. Production uses `Default`; tests
/// inject short values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayTimeouts {
    /// Bounds [`RealWsConn::connect`] (dial + TLS + WebSocket upgrade).
    pub connect: Duration,
    /// Bounds the wait for the relay's opening `hello`, and every
    /// [`WsConn::recv_text`] on a [`RealWsConn`].
    pub hello: Duration,
    /// Bounds `send_text`'s wait for a free outbound slot.
    pub send: Duration,
    /// Bounds one frame's socket write + flush in the pump.
    pub write: Duration,
}

impl Default for RelayTimeouts {
    fn default() -> Self {
        Self {
            connect: RELAY_CONNECT_TIMEOUT,
            hello: RELAY_HELLO_TIMEOUT,
            send: RELAY_SEND_TIMEOUT,
            write: RELAY_WRITE_TIMEOUT,
        }
    }
}

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
/// the captured [`Handle`], each under a [`RelayTimeouts`] deadline. The pump's
/// reader and writer progress independently, so inbound frames keep arriving
/// while a write is stuck; a write that stays pending longer than
/// [`RelayTimeouts::write`] ends the connection. Constructed by
/// [`connect`](RealWsConn::connect).
pub struct RealWsConn {
    handle: Handle,
    /// Outbound text frames → pump → socket.
    tx: mpsc::Sender<String>,
    /// Inbound text frames ← pump ← socket. A closed channel means the
    /// connection is over.
    rx: mpsc::Receiver<String>,
    /// The pump task; aborted when a send times out so the socket drops at once.
    pump: tokio::task::AbortHandle,
    timeouts: RelayTimeouts,
}

impl RealWsConn {
    /// Dial `url` and return a synchronous [`WsConn`] over the established
    /// WebSocket. Must be called from within a Tokio runtime (uses the current
    /// [`Handle`]). Spawns a pump task that owns the socket for its lifetime.
    /// The dial, TLS and upgrade together are bounded by
    /// [`RelayTimeouts::connect`]; `relay_ws_config` bounds each inbound frame.
    pub async fn connect(url: &str, timeouts: RelayTimeouts) -> Result<Self, RelayError> {
        let (stream, _resp) = match tokio::time::timeout(
            timeouts.connect,
            tokio_tungstenite::connect_async_with_config(url, Some(relay_ws_config()), false),
        )
        .await
        {
            Ok(Ok(connected)) => connected,
            Ok(Err(e)) => return Err(RelayError::Transport(format!("connect {url}: {e}"))),
            Err(_elapsed) => {
                return Err(RelayError::Transport(format!("connect {url}: timed out")))
            }
        };
        let (sink, source) = stream.split();
        Ok(Self::spawn(sink, source, timeouts))
    }

    /// Spawn the pump over `sink`/`source` and return the adapter handle.
    fn spawn<Si, St, E>(sink: Si, source: St, timeouts: RelayTimeouts) -> Self
    where
        Si: Sink<Message> + Unpin + Send + 'static,
        St: Stream<Item = Result<Message, E>> + Unpin + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        // The channels bound queue depth and `relay_ws_config` bounds each
        // frame, so a stuck or oversending peer can't grow memory without
        // limit.
        let (out_tx, out_rx) = mpsc::channel::<String>(RELAY_QUEUE_DEPTH);
        let (in_tx, in_rx) = mpsc::channel::<String>(RELAY_QUEUE_DEPTH);
        let pump = tokio::spawn(pump(sink, source, out_rx, in_tx, timeouts.write));
        Self {
            handle: Handle::current(),
            tx: out_tx,
            rx: in_rx,
            pump: pump.abort_handle(),
            timeouts,
        }
    }
}

/// Move frames between the socket halves and the adapter's queues. The writer
/// and reader run as two concurrent futures, so neither waits on the other.
/// Returning drops both halves, the socket and both channel ends, which the
/// adapter sees as a closed connection.
async fn pump<Si, St, E>(
    mut sink: Si,
    mut source: St,
    mut out_rx: mpsc::Receiver<String>,
    in_tx: mpsc::Sender<String>,
    write_timeout: Duration,
) where
    Si: Sink<Message> + Unpin,
    St: Stream<Item = Result<Message, E>> + Unpin,
    E: std::fmt::Display,
{
    let writer = async move {
        while let Some(text) = out_rx.recv().await {
            match tokio::time::timeout(write_timeout, sink.send(Message::Text(text.into()))).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return,
                Err(_elapsed) => {
                    debug!(target: "agent", "relay write timed out; closing the connection");
                    return;
                }
            }
        }
        // Adapter dropped: close the socket, bounded like any other write.
        let _ = tokio::time::timeout(write_timeout, sink.send(Message::Close(None))).await;
    };
    let reader = async move {
        while let Some(inbound) = source.next().await {
            match inbound {
                Ok(Message::Text(t)) => {
                    if in_tx.send(t.to_string()).await.is_err() {
                        return;
                    }
                }
                // Non-text frames are not part of relay.v1; ignore.
                Ok(
                    Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_),
                ) => {}
                Ok(Message::Close(_)) => return,
                Err(e) => {
                    debug!(target: "agent", "relay socket error: {e}");
                    return;
                }
            }
        }
    };
    // Neither half waits on the other. Whichever ends first ends the
    // connection: returning drops both halves, the socket and both channel ends.
    tokio::select! {
        () = writer => {}
        () = reader => {}
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
        let send_timeout = self.timeouts.send;
        // See the module doc: the session loop that drives this seam runs as
        // a normal async task (`tokio::spawn`), not a bare OS thread, so a
        // bare `handle.block_on` here would nest inside the task's own poll
        // and panic ("Cannot start a runtime from within a runtime").
        // `block_in_place` is Tokio's documented escape hatch for exactly
        // this: it hands this worker's other tasks off to the rest of the
        // multi-thread pool for the closure's duration, making a nested
        // `block_on` legal again. The wait is bounded, so the call always
        // returns to the async caller within `send`.
        let sent = tokio::task::block_in_place(|| {
            handle.block_on(tokio::time::timeout(send_timeout, tx.send(s)))
        });
        match sent {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(RelayError::Transport(
                "relay send channel closed".to_string(),
            )),
            Err(_elapsed) => {
                // The relay is not draining its queue. Drop the socket now
                // rather than wait for the pump's own write deadline.
                self.pump.abort();
                Err(RelayError::Transport("relay send timed out".to_string()))
            }
        }
    }

    /// Bounded by [`RelayTimeouts::hello`]: the relay protocol's only blocking
    /// receive is the opening `hello`, and a relay that withholds it must not
    /// hold the session.
    fn recv_text(&mut self) -> Result<Option<String>, RelayError> {
        match self.recv_text_within(self.timeouts.hello)? {
            RecvOutcome::Frame(text) => Ok(Some(text)),
            RecvOutcome::Closed => Ok(None),
            RecvOutcome::Quiet => Err(RelayError::Transport("relay receive timed out".to_string())),
        }
    }

    fn recv_text_within(&mut self, timeout: Duration) -> Result<RecvOutcome, RelayError> {
        let handle = &self.handle;
        let rx = &mut self.rx;
        let out = tokio::task::block_in_place(|| {
            handle.block_on(async move {
                match tokio::time::timeout(timeout, rx.recv()).await {
                    Err(_elapsed) => RecvOutcome::Quiet,
                    Ok(Some(text)) => RecvOutcome::Frame(text),
                    Ok(None) => RecvOutcome::Closed,
                }
            })
        });
        Ok(out)
    }

    fn send_capacity(&self) -> Option<usize> {
        Some(self.tx.capacity())
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
            let mut conn = RealWsConn::connect(&format!("ws://{addr}"), RelayTimeouts::default())
                .await
                .expect("connect");
            conn.send_text("hello".to_string()).expect("send_text");
            conn.recv_text().expect("recv_text")
        });

        let received = client.await.expect("client task panicked");
        assert_eq!(received, Some("hello".to_string()));
    }

    // ── PAN-102: bounded, decoupled relay IO ─────────────────────────────

    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Instant;
    use tokio::net::TcpListener;

    /// A frame comfortably under `MAX_FRAME_BYTES`, large enough to fill
    /// socket buffers quickly.
    const BIG_FRAME: usize = 60_000;

    /// Short deadlines for tests; each test overrides what it exercises.
    fn test_timeouts() -> RelayTimeouts {
        RelayTimeouts {
            connect: Duration::from_secs(5),
            hello: Duration::from_secs(5),
            send: Duration::from_secs(5),
            write: Duration::from_secs(5),
        }
    }

    /// A socket sink that never accepts a frame (`poll_ready` stays pending),
    /// like a relay that stopped reading once the kernel buffers filled.
    struct StalledSink;

    impl Sink<Message> for StalledSink {
        type Error = std::io::Error;
        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }
        fn start_send(self: Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            Ok(())
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    type InboundItem = Result<Message, std::io::Error>;

    /// An inbound source that never yields.
    fn silent_source() -> futures_util::stream::Pending<InboundItem> {
        futures_util::stream::pending()
    }

    /// Bind a local listener and return it with its `ws://` URL.
    async fn local_relay() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        (listener, url)
    }

    /// The headline property: with the relay no longer reading (so the
    /// pump's socket write is stuck), an inbound frame still reaches the
    /// adapter promptly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inbound_frames_still_arrive_while_a_relay_write_is_stalled() {
        let (listener, url) = local_relay().await;
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        // Relay: never reads; sends `disarm` when signalled, then holds the socket.
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let (mut sink, _source) = ws.split();
            let _ = go_rx.await;
            sink.send(Message::Text("disarm".into())).await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        let client = tokio::spawn(async move {
            let timeouts = RelayTimeouts {
                write: Duration::from_secs(30),
                send: Duration::from_secs(30),
                ..test_timeouts()
            };
            let mut conn = RealWsConn::connect(&url, timeouts).await.expect("connect");
            let frame = "x".repeat(BIG_FRAME);
            let mut pushed = 0;
            // Push until the queue stops draining: the writer may only be
            // behind at first, so re-check after a pause.
            loop {
                while conn.send_capacity() >= Some(8) && pushed < 3000 {
                    conn.send_text(frame.clone()).expect("queue has room");
                    pushed += 1;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
                if conn.send_capacity() < Some(16) || pushed >= 3000 {
                    break;
                }
            }
            assert!(
                conn.send_capacity() < Some(16),
                "the writer is stuck (pushed {pushed} frames, capacity {:?})",
                conn.send_capacity()
            );
            go_tx.send(()).unwrap();
            let t0 = Instant::now();
            let got = conn.recv_text_within(Duration::from_secs(3)).unwrap();
            assert!(
                matches!(&got, RecvOutcome::Frame(f) if f == "disarm"),
                "inbound frame must arrive while the write is stalled, got {got:?}"
            );
            assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
        });
        tokio::time::timeout(Duration::from_secs(30), client)
            .await
            .expect("test hung")
            .expect("client task panicked");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_write_that_cannot_flush_closes_the_connection() {
        let (listener, url) = local_relay().await;
        // Relay: never reads; holds the socket.
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let _ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        let client = tokio::spawn(async move {
            let timeouts = RelayTimeouts {
                write: Duration::from_millis(300),
                send: Duration::from_millis(300),
                ..test_timeouts()
            };
            let mut conn = RealWsConn::connect(&url, timeouts).await.expect("connect");
            let frame = "x".repeat(BIG_FRAME);
            let t0 = Instant::now();
            let mut failed = None;
            for _ in 0..5000 {
                if let Err(e) = conn.send_text(frame.clone()) {
                    failed = Some(e);
                    break;
                }
            }
            assert!(
                matches!(failed, Some(RelayError::Transport(_))),
                "sends must start failing once the relay stops reading: {failed:?}"
            );
            assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
            let got = conn.recv_text_within(Duration::from_secs(1)).unwrap();
            assert!(matches!(got, RecvOutcome::Closed), "got {got:?}");
        });
        tokio::time::timeout(Duration::from_secs(30), client)
            .await
            .expect("test hung")
            .expect("client task panicked");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_text_times_out_and_aborts_the_connection_when_the_queue_stays_full() {
        let client = tokio::spawn(async move {
            let timeouts = RelayTimeouts {
                send: Duration::from_millis(200),
                write: Duration::from_secs(60),
                ..test_timeouts()
            };
            let mut conn = RealWsConn::spawn(StalledSink, silent_source(), timeouts);
            // Fill the queue; the first send that finds no room is under test.
            while conn.send_capacity() > Some(0) {
                conn.send_text("x".to_string()).expect("queue has room");
            }
            let t0 = Instant::now();
            match conn.send_text("x".to_string()) {
                Err(RelayError::Transport(msg)) => assert!(msg.contains("timed out"), "{msg}"),
                other => panic!("expected a send timeout, got {other:?}"),
            }
            let waited = t0.elapsed();
            assert!(waited >= Duration::from_millis(200), "{waited:?}");
            assert!(waited < Duration::from_secs(2), "{waited:?}");

            let t1 = Instant::now();
            assert!(conn.send_text("x".to_string()).is_err());
            assert!(
                t1.elapsed() < Duration::from_millis(100),
                "{:?}",
                t1.elapsed()
            );

            let t2 = Instant::now();
            let got = conn.recv_text_within(Duration::from_secs(1)).unwrap();
            assert!(
                matches!(got, RecvOutcome::Closed),
                "pump aborted, got {got:?}"
            );
            assert!(
                t2.elapsed() < Duration::from_millis(100),
                "{:?}",
                t2.elapsed()
            );
        });
        tokio::time::timeout(Duration::from_secs(10), client)
            .await
            .expect("test hung")
            .expect("client task panicked");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pump_write_timeout_ends_the_connection() {
        let client = tokio::spawn(async move {
            let timeouts = RelayTimeouts {
                write: Duration::from_millis(200),
                send: Duration::from_secs(5),
                ..test_timeouts()
            };
            let mut conn = RealWsConn::spawn(StalledSink, silent_source(), timeouts);
            conn.send_text("x".to_string()).expect("queued");
            let t0 = Instant::now();
            let got = conn.recv_text_within(Duration::from_secs(2)).unwrap();
            assert!(matches!(got, RecvOutcome::Closed), "got {got:?}");
            assert!(
                t0.elapsed() < Duration::from_millis(1500),
                "{:?}",
                t0.elapsed()
            );
            assert!(conn.send_text("x".to_string()).is_err());
        });
        tokio::time::timeout(Duration::from_secs(10), client)
            .await
            .expect("test hung")
            .expect("client task panicked");
    }

    /// Deterministic twin of the real-socket test above.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pump_delivers_inbound_while_its_writer_is_stuck() {
        let (in_tx, mut in_rx) = tokio::sync::mpsc::unbounded_channel::<InboundItem>();
        let source = futures_util::stream::poll_fn(move |cx| in_rx.poll_recv(cx));
        let client = tokio::spawn(async move {
            let timeouts = RelayTimeouts {
                write: Duration::from_secs(60),
                ..test_timeouts()
            };
            let mut conn = RealWsConn::spawn(StalledSink, source, timeouts);
            conn.send_text("x".to_string()).expect("queued");
            // Once the writer has taken the frame off the queue it is stuck
            // in `poll_ready`; only then does the inbound frame arrive.
            while conn.send_capacity() < Some(RELAY_QUEUE_DEPTH) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            in_tx.send(Ok(Message::Text("disarm".into()))).unwrap();
            let got = conn.recv_text_within(Duration::from_secs(1)).unwrap();
            assert!(
                matches!(&got, RecvOutcome::Frame(f) if f == "disarm"),
                "got {got:?}"
            );
        });
        tokio::time::timeout(Duration::from_secs(10), client)
            .await
            .expect("test hung")
            .expect("client task panicked");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connect_times_out_when_the_websocket_upgrade_never_completes() {
        let (listener, url) = local_relay().await;
        // Accepts TCP and never answers the HTTP upgrade.
        tokio::spawn(async move {
            let (_tcp, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let timeouts = RelayTimeouts {
            connect: Duration::from_millis(300),
            ..test_timeouts()
        };
        let t0 = Instant::now();
        let res =
            tokio::time::timeout(Duration::from_secs(10), RealWsConn::connect(&url, timeouts))
                .await
                .expect("connect must not hang");
        match res {
            Err(RelayError::Transport(msg)) => assert!(msg.contains("timed out"), "{msg}"),
            Err(other) => panic!("expected a connect timeout, got {other:?}"),
            Ok(_) => panic!("expected a connect timeout, got a connection"),
        }
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recv_text_is_bounded_by_the_hello_timeout() {
        let (listener, url) = local_relay().await;
        // Upgrades, then sends nothing.
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let _ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let client = tokio::spawn(async move {
            let timeouts = RelayTimeouts {
                hello: Duration::from_millis(300),
                ..test_timeouts()
            };
            let mut conn = RealWsConn::connect(&url, timeouts).await.expect("connect");
            let t0 = Instant::now();
            match conn.recv_text() {
                Err(RelayError::Transport(msg)) => assert_eq!(msg, "relay receive timed out"),
                other => panic!("expected a receive timeout, got {other:?}"),
            }
            assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        });
        tokio::time::timeout(Duration::from_secs(10), client)
            .await
            .expect("test hung")
            .expect("client task panicked");
    }

    #[test]
    fn relay_timeouts_drop_a_stalled_relay_before_the_dead_man_window() {
        use pancetta_agent::arm::HEARTBEAT_TIMEOUT_MS;
        let t = RelayTimeouts::default();
        for (name, d) in [
            ("connect", t.connect),
            ("hello", t.hello),
            ("send", t.send),
            ("write", t.write),
        ] {
            let ms = i64::try_from(d.as_millis()).unwrap();
            assert!(ms > 0, "{name} must be a real deadline");
            assert!(
                ms < HEARTBEAT_TIMEOUT_MS,
                "{name} ({ms} ms) must expire before the dead-man window"
            );
        }
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
            let mut conn = RealWsConn::connect(&format!("ws://{addr}"), RelayTimeouts::default())
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
            let mut conn = RealWsConn::connect(&format!("ws://{addr}"), RelayTimeouts::default())
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
