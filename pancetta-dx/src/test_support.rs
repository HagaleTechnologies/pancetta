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
    let raw = raw.into();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for _ in 0..n {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(raw.as_bytes());
        }
    });
    addr
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
