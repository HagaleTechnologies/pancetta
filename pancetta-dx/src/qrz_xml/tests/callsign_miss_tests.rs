//! Only a lookup reply can make a callsign miss; a failed login never does
//! (PAN-478). The coordinator caches a miss for the session, so a login error
//! read as one would hide every callsign looked up during it.

use super::tests::{spawn_mock, AUTH_OK, SESSION_TIMEOUT};
use super::*;

/// A QRZ reply whose only content is `<Session><Error>{text}</Error>`.
fn error_reply(text: &str) -> &'static str {
    Box::leak(
        format!("<QRZDatabase><Session><Error>{text}</Error></Session></QRZDatabase>")
            .into_boxed_str(),
    )
}

/// A client for the mock at `base` with the given credentials and no session key.
fn client(base: String) -> QrzXmlClient {
    QrzXmlClient::new("user", "pw", "pancetta-test")
        .with_mock_base_url(base)
        .unwrap()
}

#[tokio::test]
async fn failed_login_saying_not_found_is_not_a_miss() {
    let (base, queries) = spawn_mock(vec![error_reply("Username user not found")]);
    let client = client(base);
    let err = client.lookup("DL1ABC").await.unwrap_err();
    match err.kind() {
        DxErrorInner::ExternalService(msg) => assert_eq!(
            msg, "QRZ authentication failed: Username <redacted> not found",
            "the echoed username is masked"
        ),
        other => panic!("expected an ExternalService error, got {other:?}"),
    }
    let qs = queries.lock().unwrap();
    assert_eq!(qs.len(), 1, "expected the login only: {qs:?}");
}

#[tokio::test]
async fn failed_relogin_after_session_timeout_is_not_a_miss() {
    let (base, queries) = spawn_mock(vec![
        SESSION_TIMEOUT,
        error_reply("Username user not found"),
    ]);
    let client = client(base);
    *client.session_key.lock().await = Some("stale-key".to_string());
    let err = client.lookup("DL1ABC").await.unwrap_err();
    match err.kind() {
        DxErrorInner::ExternalService(msg) => assert!(
            msg.starts_with("QRZ authentication failed:"),
            "expected a login failure, got {msg:?}"
        ),
        other => panic!("expected an ExternalService error, got {other:?}"),
    }
    let qs = queries.lock().unwrap();
    assert_eq!(qs.len(), 2, "expected lookup then re-login: {qs:?}");
    assert!(qs[1].contains("username=user"), "re-login: {}", qs[1]);
}

#[tokio::test]
async fn empty_callsign_record_is_a_miss() {
    let (base, _queries) = spawn_mock(vec![
        AUTH_OK,
        "<QRZDatabase><Callsign></Callsign></QRZDatabase>",
    ]);
    let client = client(base);
    let err = client.lookup("DL1ABC").await.unwrap_err();
    assert!(
        matches!(err.kind(), DxErrorInner::CallsignNotFound(msg)
            if msg == "QRZ returned no callsign data for 'DL1ABC'"),
        "expected a callsign miss, got {:?}",
        err.kind()
    );
    assert_eq!(
        err.to_string(),
        "External service error: QRZ returned no callsign data for 'DL1ABC'"
    );
}
