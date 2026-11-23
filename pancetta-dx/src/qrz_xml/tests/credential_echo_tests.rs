//! QRZ may echo request credentials in a `<Session><Error>`. A phrase that only
//! an echoed credential spells must not classify the error (PAN-474).

use super::tests::{spawn_mock, AUTH_OK, LOOKUP_OK, SESSION_TIMEOUT};
use super::*;

/// A QRZ reply whose only content is `<Session><Error>{text}</Error>`.
fn error_reply(text: &str) -> &'static str {
    Box::leak(
        format!("<QRZDatabase><Session><Error>{text}</Error></Session></QRZDatabase>")
            .into_boxed_str(),
    )
}

/// A client for `base` that already holds `key`, so the first request is a lookup.
async fn keyed_client(base: String, username: &str, password: &str, key: &str) -> QrzXmlClient {
    let client = QrzXmlClient::new(username, password, "pancetta-test")
        .with_mock_base_url(base)
        .unwrap();
    *client.session_key.lock().await = Some(key.to_string());
    client
}

/// The `ExternalService` text of a failed lookup.
async fn lookup_error(client: &QrzXmlClient) -> String {
    let err = client.lookup("K1ABC").await.unwrap_err();
    match err.kind() {
        DxErrorInner::ExternalService(msg) => msg.clone(),
        other => panic!("expected an ExternalService error, got {other:?}"),
    }
}

#[tokio::test]
async fn echoed_password_with_not_found_is_not_a_miss() {
    let (base, _queries) = spawn_mock(vec![error_reply("request rejected for pw not found 73")]);
    let client = keyed_client(base, "user", "pw not found 73", "key-secret").await;
    assert_eq!(
        lookup_error(&client).await,
        "QRZ lookup error: request rejected for <redacted>"
    );
}

#[tokio::test]
async fn echoed_session_key_with_not_found_is_not_a_miss() {
    let (base, _queries) = spawn_mock(vec![error_reply("bad session key: not found-key")]);
    let client = keyed_client(base, "user", "pw-secret", "not found-key").await;
    assert_eq!(
        lookup_error(&client).await,
        "QRZ lookup error: bad session key: <redacted>"
    );
}

#[tokio::test]
async fn echoed_password_with_session_timeout_does_not_reauthenticate() {
    // The extra replies let a wrong re-login succeed visibly instead of
    // failing on a closed listener.
    let (base, queries) = spawn_mock(vec![
        error_reply("rejected pw session timeout 73"),
        AUTH_OK,
        LOOKUP_OK,
    ]);
    let client = keyed_client(base, "user", "pw session timeout 73", "key-secret").await;
    assert_eq!(
        lookup_error(&client).await,
        "QRZ lookup error: rejected <redacted>"
    );
    let qs = queries.lock().unwrap();
    assert_eq!(qs.len(), 1, "expected one lookup and no re-auth: {qs:?}");
}

#[tokio::test]
async fn genuine_not_found_beside_an_echoed_credential_is_still_a_miss() {
    let (base, _queries) = spawn_mock(vec![error_reply("Not found: K1ABC")]);
    let client = keyed_client(base, "K1ABC", "pw-secret", "key-secret").await;
    assert_eq!(lookup_error(&client).await, "QRZ lookup error: Not found");
}

#[tokio::test]
async fn password_inside_a_session_expiry_phrase_still_reauthenticates() {
    let (base, queries) = spawn_mock(vec![SESSION_TIMEOUT, AUTH_OK, LOOKUP_OK]);
    let client = keyed_client(base, "user", "Timeout", "stale-key").await;
    let lookup = client.lookup("AA7BQ").await.unwrap();
    assert_eq!(lookup.call.as_deref(), Some("AA7BQ"));
    let qs = queries.lock().unwrap();
    assert_eq!(qs.len(), 3, "expected lookup, re-auth, lookup: {qs:?}");
    assert!(qs[1].contains("username=user"), "re-auth: {}", qs[1]);
}

#[tokio::test]
async fn password_inside_the_not_found_phrase_fails_safe_as_a_service_error() {
    // Ambiguous: "found" is both the password and part of the phrase. An
    // uncached service error is a cheaper mistake than a cached miss.
    let (base, _queries) = spawn_mock(vec![error_reply("Not found: K1ABC")]);
    let client = keyed_client(base, "user", "found", "key-secret").await;
    assert_eq!(
        lookup_error(&client).await,
        "QRZ lookup error: Not <redacted>: K1ABC"
    );
}

#[tokio::test]
async fn overlapping_echoed_credentials_never_display_a_not_found() {
    // The password starts inside the username's echo. Redaction alone would
    // replace "ab" and show "not found", which the coordinator caches as a miss.
    let (base, _queries) = spawn_mock(vec![error_reply("abnot found")]);
    let client = keyed_client(base, "ab", "bnot found", "key-secret").await;
    assert_eq!(lookup_error(&client).await, "QRZ lookup error: <redacted>");
}

#[tokio::test]
async fn a_credential_inside_the_marker_does_not_garble_it() {
    // "act" also spells part of "<redacted>"; the displayed marker stays whole.
    let (base, _queries) = spawn_mock(vec![error_reply("bad act key")]);
    let client = keyed_client(base, "user", "act", "key-secret").await;
    assert_eq!(
        lookup_error(&client).await,
        "QRZ lookup error: bad <redacted> key"
    );
}

#[tokio::test]
async fn overlapping_echoed_credentials_in_a_login_error_never_display_a_not_found() {
    // The login error uses the same mask: redaction alone would show
    // "<redacted>not found", which the coordinator caches as a miss.
    let (base, queries) = spawn_mock(vec![error_reply("abnot found")]);
    let client = QrzXmlClient::new("ab", "bnot found", "pancetta-test")
        .with_mock_base_url(base)
        .unwrap();
    let err = client.lookup("K1ABC").await.unwrap_err();
    match err.kind() {
        DxErrorInner::ExternalService(msg) => {
            assert_eq!(msg, "QRZ authentication failed: <redacted>")
        }
        other => panic!("expected an ExternalService error, got {other:?}"),
    }
    assert_eq!(queries.lock().unwrap().len(), 1);
}
