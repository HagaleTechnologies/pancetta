use super::*;
use crate::test_support::serve_raw;

#[tokio::test]
async fn auth_error_redacts_credentials() {
    let xml = "<QRZDatabase><Session><Error>bad user+secret and pw+secret</Error></Session></QRZDatabase>";
    let raw = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{xml}",
        xml.len()
    );
    let addr = serve_raw(1, raw);
    let client = QrzXmlClient::new("user+secret", "pw+secret", "agent")
        .with_mock_base_url(format!("http://{addr}/xml/current/"))
        .unwrap();
    let err = client.lookup("K1ABC").await.unwrap_err();
    assert!(err.to_string().contains("<redacted>"));
    assert!(!format!("{err:?}").contains("pw+secret"));
}

#[tokio::test]
async fn lookup_error_redacts_session_key() {
    let xml = "<QRZDatabase><Session><Error>bad key+secret</Error></Session></QRZDatabase>";
    let raw = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{xml}",
        xml.len()
    );
    let addr = serve_raw(1, raw);
    let client = QrzXmlClient::new("user", "pw+secret", "agent")
        .with_mock_base_url(format!("http://{addr}/xml/current/"))
        .unwrap();
    *client.session_key.lock().await = Some("key+secret".into());
    let err = client.lookup("K1ABC").await.unwrap_err();
    assert!(err.to_string().contains("<redacted>"));
    assert!(!format!("{err:?}").contains("key+secret"));
}
