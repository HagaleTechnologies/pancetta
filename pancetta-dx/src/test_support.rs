//! Helpers shared by pancetta-dx's network-failure tests.

use crate::DxError;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};

/// A 503 with an empty body.
pub(crate) const STATUS_503: &str =
    "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// A 200 whose `Content-Length` promises more than is sent, so reading the
/// body fails after the status line succeeded.
pub(crate) const TRUNCATED_BODY: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\n\
     Content-Length: 500\r\nConnection: close\r\n\r\n<QRZDatabase>";

/// Send `raw` verbatim to each of the next `n` connections.
pub(crate) fn serve_raw(n: usize, raw: impl Into<String>) -> SocketAddr {
    serve_raw_bytes(n, raw.into().into_bytes())
}

/// Send `raw` bytes verbatim to each of the next `n` connections.
pub(crate) fn serve_raw_bytes(n: usize, raw: Vec<u8>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for _ in 0..n {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(&raw);
        }
    });
    addr
}

/// What a [`serve_chunked`] server managed to send.
pub(crate) struct Served {
    /// Body bytes the socket accepted.
    pub written: usize,
    /// The whole body and the closing chunk were sent.
    pub completed: bool,
}

/// Answer one request with `head` (status line and headers, no framing
/// header, no blank line), then a chunked body: `prefix` followed by `pad`
/// bytes of `b' '` in 64 KiB chunks.
///
/// The request headers and any `Content-Length` request body are read first,
/// so a POST client never sees its upload cut off. A write that stalls for
/// 10 s ends the response with `completed: false` instead of hanging.
pub(crate) fn serve_chunked(
    head: &'static str,
    prefix: &'static [u8],
    pad: usize,
) -> (SocketAddr, std::thread::JoinHandle<Served>) {
    const CHUNK: usize = 64 * 1024;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let mut served = Served {
            written: 0,
            completed: false,
        };
        let Ok((mut stream, _)) = listener.accept() else {
            return served;
        };
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(10)));
        if read_request(&mut stream).is_err() {
            return served;
        }
        let head = format!("{head}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
        if stream.write_all(head.as_bytes()).is_err() {
            return served;
        }
        let total = prefix.len() + pad;
        let mut chunk = Vec::with_capacity(CHUNK + 16);
        while served.written < total {
            let len = CHUNK.min(total - served.written);
            chunk.clear();
            chunk.extend_from_slice(format!("{len:x}\r\n").as_bytes());
            let start = served.written.min(prefix.len());
            let end = (served.written + len).min(prefix.len());
            chunk.extend_from_slice(&prefix[start..end]);
            chunk.resize(chunk.len() + len - (end - start), b' ');
            chunk.extend_from_slice(b"\r\n");
            if stream.write_all(&chunk).is_err() {
                return served;
            }
            served.written += len;
        }
        served.completed = stream.write_all(b"0\r\n\r\n").is_ok();
        served
    });
    (addr, handle)
}

/// Read one request's headers and its `Content-Length` body, if any.
fn read_request(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
    let mut req = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        req.extend_from_slice(&buf[..n]);
    };
    let content_length = String::from_utf8_lossy(&req[..header_end])
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut remaining = content_length.saturating_sub(req.len() - header_end);
    while remaining > 0 {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        remaining = remaining.saturating_sub(n);
    }
    Ok(())
}

/// Wait for a [`serve_chunked`] server without blocking the runtime thread:
/// the client's connection task must keep running to close the socket.
pub(crate) async fn served(h: std::thread::JoinHandle<Served>) -> Served {
    tokio::task::spawn_blocking(move || h.join().unwrap())
        .await
        .unwrap()
}

/// Answer every request with a 302 back to its own target, so the client
/// gives up with a redirect error about the original request.
pub(crate) fn serve_redirect_loop() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        // reqwest's default policy stops after 10 redirects.
        for _ in 0..16 {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let target = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/");
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {target}\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            );
        }
    });
    addr
}

/// A loopback address with nothing listening on it.
pub(crate) fn closed_port() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

/// Every rendering a log line, `{:?}` or a panic could produce: Display,
/// Debug, alternate Debug, and each error in the source chain.
pub(crate) fn render_error_chain(err: &DxError) -> String {
    let mut out = format!("{err}\n{err:?}\n{err:#?}\n");
    let mut source = std::error::Error::source(err);
    while let Some(s) = source {
        out.push_str(&format!("{s}\n{s:?}\n"));
        source = s.source();
    }
    out
}

/// Panic if any rendering of `err` contains one of `secrets`.
pub(crate) fn assert_error_omits(err: &DxError, secrets: &[&str]) {
    let all = render_error_chain(err);
    for secret in secrets {
        assert!(!all.contains(secret), "error leaked {secret:?}:\n{all}");
    }
}

/// Panic unless `err` is a network error that names no request URL and
/// contains none of `secrets`.
pub(crate) fn assert_network_error_omits(err: &DxError, secrets: &[&str]) {
    assert!(
        matches!(err, DxError::Network(_)),
        "expected DxError::Network, got {err:?}"
    );
    assert_error_omits(err, secrets);
    let all = render_error_chain(err);
    // reqwest's own URL renderings: Display suffix and Debug field.
    assert!(!all.contains("for url ("), "error kept its URL:\n{all}");
    assert!(!all.contains("url: \""), "error kept its URL:\n{all}");
}
