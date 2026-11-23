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
async fn whitespace_bearing_credentials_still_redact_at_body_edges() {
    // The credential's own edge whitespace used to be trimmed away before
    // exact matching, leaving the visible part of the secret in the error.
    let password = " pw+secret ";
    let addr = respond("500 Internal Server Error", " pw+secret ".into());
    let err = ClubLogClient::new("a@example.com", password, "K1ABC", "api-secret")
        .with_endpoint(format!("http://{addr}/"))
        .upload_adif("<EOR>")
        .await
        .unwrap_err();
    let text = err.upload_failure_text();
    assert!(text.contains("<redacted>"), "{text}");
    assert!(!text.contains("pw+secret"), "{text}");
}

#[test]
fn qrz_logbook_reason_edge_whitespace_still_redacts() {
    // Same class inside the k=v parser: the REASON value used to be trimmed
    // before decoding, so an unencoded echo of a whitespace-bearing key lost
    // the whitespace its spellings match on.
    let key = " qrz-secret ";
    let spellings: Vec<String> = form_spellings(key);
    let refs: Vec<&str> = spellings.iter().map(String::as_str).collect();
    let err = parse_qrz_response_inner("RESULT=FAIL&REASON= qrz-secret ", &refs).unwrap_err();
    let text = err.upload_failure_text();
    assert!(text.contains("<redacted>"), "{text}");
    assert!(!text.contains("qrz-secret"), "{text}");
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

#[tokio::test]
async fn clublog_overlapping_credentials_leave_no_tail() {
    // The API key starts inside the password's match: the tail "73" the old
    // scan leaked contains neither full credential, so `absent` alone cannot
    // catch it and the exact redacted text is asserted.
    let addr = respond(
        "500 Internal Server Error",
        "login rejected for w1aw73 (see docs)".into(),
    );
    let err = ClubLogClient::new("a@example.com", "w1aw", "K1ABC", "aw73")
        .with_endpoint(format!("http://{addr}/"))
        .upload_adif("<EOR>")
        .await
        .unwrap_err();
    let text = err.upload_failure_text();
    assert!(
        text.contains("login rejected for <redacted> (see docs)"),
        "{text}"
    );
    absent(&err, &["w1aw", "aw73"]);
}

#[tokio::test]
async fn padded_http_error_keeps_its_explanation() {
    // Leading whitespace used to spend the 200-character display budget, so
    // the operator saw only `…` after the status line.
    let body = format!("{}login rejected: bad API key", " ".repeat(200));
    let addr = respond("500 Internal Server Error", body);
    let err = ClubLogClient::new("a@example.com", "pw-secret", "K1ABC", "api-secret")
        .with_endpoint(format!("http://{addr}/"))
        .upload_adif("<EOR>")
        .await
        .unwrap_err();
    assert_eq!(
        err.upload_failure_text(),
        "ClubLog upload failed: HTTP 500 Internal Server Error — login rejected: bad API key"
    );
}

#[test]
fn padded_application_errors_keep_their_explanation() {
    let pad = " ".repeat(200);

    let err = parse_eqsl_response_inner(&format!("{pad}Error: Bad password"), &[]).unwrap_err();
    assert_eq!(
        err.upload_failure_class(),
        UploadFailureClass::LoginRejected
    );
    assert_eq!(
        err.upload_failure_text(),
        "eQSL upload rejected (authentication): Error: Bad password"
    );

    let err = parse_eqsl_response_inner(&format!("{pad}Something odd happened"), &[]).unwrap_err();
    let text = err.upload_failure_text();
    assert!(
        text.ends_with("Unrecognised eQSL response: Something odd happened"),
        "{text}"
    );

    let err = parse_qrz_response_inner(&format!("RESULT=FAIL&REASON={pad}bad logbook entry"), &[])
        .unwrap_err();
    assert_eq!(
        err.upload_failure_class(),
        UploadFailureClass::RecordRejected
    );
    assert_eq!(
        err.upload_failure_text(),
        "QRZ Logbook insert failed: bad logbook entry"
    );

    let err = parse_qrz_response_inner(&format!("{pad}Something odd happened"), &[]).unwrap_err();
    let text = err.upload_failure_text();
    assert!(
        text.ends_with("Unrecognised QRZ Logbook response: Something odd happened"),
        "{text}"
    );
}
