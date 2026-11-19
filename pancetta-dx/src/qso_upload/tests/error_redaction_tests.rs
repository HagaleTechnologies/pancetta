use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;

fn respond(status: &str, body: String) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let status = status.to_string();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut buf = [0; 8192];
        loop {
            let n = stream.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            request.extend_from_slice(&buf[..n]);
            if let Some(head_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&request[..head_end]);
                let length: usize = header
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().ok())
                                .flatten()
                        })
                    })
                    .unwrap_or(0);
                if request.len() >= head_end + 4 + length {
                    break;
                }
            }
        }
        let response_body = if body == "<echo>" {
            let text = String::from_utf8_lossy(&request);
            text.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
        } else {
            body
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    });
    addr
}

fn absent(err: &DxError, secrets: &[&str]) {
    let display = err.to_string();
    let debug = format!("{err:?}");
    for secret in secrets {
        assert!(
            !display.contains(secret) && !debug.contains(secret),
            "{display} {debug}"
        );
    }
}

#[tokio::test]
async fn clublog_echo_redacts_password_and_api_key() {
    let password = "pw+secret";
    let api = "api+secret";
    let addr = respond("500 Internal Server Error", "<echo>".into());
    let err = ClubLogClient::new("a@example.com", password, "K1ABC", api)
        .with_endpoint(format!("http://{addr}/"))
        .upload_adif("<EOR>")
        .await
        .unwrap_err();
    let text = err.upload_failure_text();
    assert!(
        text.contains("password=<redacted>") && text.contains("api=<redacted>"),
        "{text}"
    );
    assert_eq!(err.upload_failure_class(), UploadFailureClass::Transient);
    absent(&err, &[password, api, "pw%2Bsecret", "api%2Bsecret"]);
}

#[tokio::test]
async fn qrz_logbook_errors_redact_api_key() {
    let key = "qrz+secret";
    for (status, body) in [
        ("500 Internal Server Error", "<echo>"),
        ("200 OK", "RESULT=FAIL&REASON=qrz%2Bsecret"),
    ] {
        let addr = respond(status, body.into());
        let err = QrzLogbookClient::new(key)
            .with_endpoint(format!("http://{addr}/"))
            .upload_adif("<EOR>")
            .await
            .unwrap_err();
        absent(&err, &[key, "qrz%2Bsecret"]);
        assert!(err.upload_failure_text().contains("<redacted>"));
    }
}

#[tokio::test]
async fn eqsl_errors_redact_password() {
    let password = "eqsl&secret";
    for (status, body) in [
        ("500 Internal Server Error", "<echo>"),
        ("200 OK", "Error: Bad password eqsl&amp;secret"),
    ] {
        let addr = respond(status, body.into());
        let err = EqslClient::new("user", password, None)
            .with_endpoint(format!("http://{addr}/"))
            .upload_adif("<EOR>")
            .await
            .unwrap_err();
        absent(&err, &[password, "eqsl&amp;secret"]);
        assert!(err.upload_failure_text().contains("<redacted>"));
    }
}

#[tokio::test]
async fn oversized_error_body_omits_partial_preview() {
    let (addr, served) = crate::test_support::serve_chunked(
        "HTTP/1.1 500 Internal Server Error",
        b"password=pw%2Bse",
        body::MAX_ERROR_BODY_BYTES,
    );
    let err = ClubLogClient::new("a@example.com", "pw+secret", "K1ABC", "api-secret")
        .with_endpoint(format!("http://{addr}/"))
        .upload_adif("<EOR>")
        .await
        .unwrap_err();
    assert!(err
        .upload_failure_text()
        .contains("response body omitted (too large)"));
    absent(&err, &["pw+secret", "api-secret"]);
    let _ = crate::test_support::served(served).await;
}
