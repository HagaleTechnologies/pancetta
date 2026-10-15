//! Byte-budgeted reads of HTTP response bodies.
//!
//! `reqwest::Response::{text, bytes, json}` buffer the whole body before the
//! caller sees its size. These read with `Response::chunk()` and stop once the
//! running total passes the budget, so at most `limit` bytes plus one chunk
//! are held. Read failures stay `DxError::Network`, which carries no URL.

// rationale: the crate-wide `DxError` is intentionally a flat (non-boxed) enum
// for ergonomic `?`; boxing it crate-wide to satisfy this lint is out of scope.
#![allow(clippy::result_large_err)]

use crate::{DxError, Result};
use reqwest::header::CONTENT_TYPE;
use reqwest::Response;

/// Budget for a body that only feeds an error message.
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 4 * 1024;

/// The whole body, or [`DxError::ResponseTooLarge`] once it passes `limit`.
pub(crate) async fn bytes_within(mut resp: Response, limit: usize) -> Result<Vec<u8>> {
    if resp.content_length().is_some_and(|n| n > limit as u64) {
        return Err(DxError::ResponseTooLarge { limit });
    }
    let (bytes, truncated) = read_capped(&mut resp, limit).await?;
    if truncated {
        return Err(DxError::ResponseTooLarge { limit });
    }
    Ok(bytes)
}

/// [`bytes_within`], decoded the way `Response::text` decodes.
pub(crate) async fn text_within(resp: Response, limit: usize) -> Result<String> {
    let charset = charset_label(&resp);
    let bytes = bytes_within(resp, limit).await?;
    Ok(decode(charset.as_deref(), &bytes))
}

/// The first `limit` bytes of the body as text; the rest is never read.
pub(crate) async fn text_prefix(mut resp: Response, limit: usize) -> Result<String> {
    let charset = charset_label(&resp);
    let (bytes, _) = read_capped(&mut resp, limit).await?;
    Ok(decode(charset.as_deref(), &bytes))
}

/// Read until the body ends or passes `limit`; `true` if it was longer.
async fn read_capped(resp: &mut Response, limit: usize) -> Result<(Vec<u8>, bool)> {
    let hint = resp
        .content_length()
        .map_or(0, |n| n.min(limit as u64) as usize);
    let mut bytes = Vec::with_capacity(hint);
    while let Some(chunk) = resp.chunk().await? {
        let room = limit - bytes.len();
        if chunk.len() > room {
            bytes.extend_from_slice(&chunk[..room]);
            return Ok((bytes, true));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, false))
}

/// The Content-Type `charset` parameter, if any.
fn charset_label(resp: &Response) -> Option<String> {
    let ct = resp.headers().get(CONTENT_TYPE)?.to_str().ok()?;
    ct.split(';').skip(1).find_map(|param| {
        let (key, value) = param.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_string())
    })
}

/// Decode as `Response::text` does: the charset label, else UTF-8, with BOM sniffing.
fn decode(charset: Option<&str>, bytes: &[u8]) -> String {
    let encoding = encoding_rs::Encoding::for_label(charset.unwrap_or("utf-8").as_bytes())
        .unwrap_or(encoding_rs::UTF_8);
    encoding.decode(bytes).0.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        assert_network_error_omits, serve_chunked, serve_raw, serve_raw_bytes, served,
        TRUNCATED_BODY,
    };
    use std::net::SocketAddr;

    const MIB: usize = 1024 * 1024;

    async fn get(addr: SocketAddr) -> Response {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn bytes_within_accepts_a_chunked_body_at_the_limit() {
        let (addr, h) = serve_chunked("HTTP/1.1 200 OK", b"", 1024);
        let bytes = bytes_within(get(addr).await, 1024).await.unwrap();
        assert_eq!(bytes.len(), 1024);
        assert!(served(h).await.completed);
    }

    #[tokio::test]
    async fn bytes_within_rejects_one_byte_over() {
        let (addr, h) = serve_chunked("HTTP/1.1 200 OK", b"", 1025);
        let err = bytes_within(get(addr).await, 1024).await.unwrap_err();
        assert!(
            matches!(err, DxError::ResponseTooLarge { limit: 1024 }),
            "got {err:?}"
        );
        served(h).await;
    }

    #[tokio::test]
    async fn bytes_within_refuses_a_declared_length_over_the_limit() {
        // No body follows the headers: reading it would fail as a network
        // error, so `ResponseTooLarge` shows the body was never read.
        let addr = serve_raw(
            1,
            "HTTP/1.1 200 OK\r\nContent-Length: 2048\r\nConnection: close\r\n\r\n",
        );
        let err = bytes_within(get(addr).await, 1024).await.unwrap_err();
        assert!(
            matches!(err, DxError::ResponseTooLarge { limit: 1024 }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn text_within_stops_reading_past_the_limit() {
        let (addr, h) = serve_chunked("HTTP/1.1 200 OK", b"", 64 * MIB);
        let err = text_within(get(addr).await, 64 * 1024).await.unwrap_err();
        assert!(
            matches!(err, DxError::ResponseTooLarge { limit } if limit == 64 * 1024),
            "got {err:?}"
        );
        let s = served(h).await;
        assert!(!s.completed && s.written < 32 * MIB, "wrote {}", s.written);
    }

    #[tokio::test]
    async fn text_prefix_reads_only_the_first_bytes() {
        let prefix: &'static [u8] = vec![b'x'; 8192].leak();
        let (addr, h) = serve_chunked("HTTP/1.1 200 OK", prefix, 64 * MIB);
        let text = text_prefix(get(addr).await, 4096).await.unwrap();
        assert_eq!(text, "x".repeat(4096));
        let s = served(h).await;
        assert!(!s.completed && s.written < 32 * MIB, "wrote {}", s.written);
    }

    /// A complete response with an optional Content-Type and a declared length.
    fn raw_response(content_type: Option<&str>, body: &[u8]) -> Vec<u8> {
        let mut raw = b"HTTP/1.1 200 OK\r\n".to_vec();
        if let Some(ct) = content_type {
            raw.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
        }
        raw.extend_from_slice(
            format!(
                "Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        );
        raw.extend_from_slice(body);
        raw
    }

    #[tokio::test]
    async fn text_within_decodes_like_reqwest_text() {
        let cases: [(Option<&str>, &[u8]); 4] = [
            (
                Some("text/xml; charset=iso-8859-1"),
                b"<name>Jos\xE9</name>",
            ),
            (Some("text/plain"), b"\xEF\xBB\xBFok"),
            (None, "\u{e9}".as_bytes()),
            (
                Some("text/plain; charset=\"UTF-8\""),
                "caf\u{e9}".as_bytes(),
            ),
        ];
        for (content_type, body) in cases {
            let addr = serve_raw_bytes(2, raw_response(content_type, body));
            let ours = text_within(get(addr).await, 1024).await.unwrap();
            let reqwests = get(addr).await.text().await.unwrap();
            assert_eq!(ours, reqwests, "content type {content_type:?}");
        }
        // The Latin-1 case really decodes, rather than both sides agreeing on
        // a lossy rendering.
        let addr = serve_raw_bytes(1, raw_response(Some(cases[0].0.unwrap()), cases[0].1));
        let text = text_within(get(addr).await, 1024).await.unwrap();
        assert_eq!(text, "<name>Jos\u{e9}</name>");
    }

    #[tokio::test]
    async fn body_read_failure_stays_a_url_free_network_error() {
        let addr = serve_raw(1, TRUNCATED_BODY);
        let err = text_within(get(addr).await, 1024).await.unwrap_err();
        assert_network_error_omits(&err, &[]);
    }

    #[test]
    fn response_too_large_renders_its_limit() {
        assert_eq!(
            DxError::ResponseTooLarge { limit: 65536 }.to_string(),
            "Response too large: more than 65536 bytes"
        );
    }
}
