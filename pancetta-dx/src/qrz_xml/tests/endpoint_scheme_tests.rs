use super::*;

#[test]
fn base_url_requires_https_at_construction() {
    for url in [
        "http://xmldata.qrz.com/xml/current/",
        "http://127.0.0.1/xml/current/",
        "ftp://example.com/",
        "not-a-url",
    ] {
        let err = QrzXmlClient::new("user", "pass", "agent")
            .with_base_url(url)
            .err()
            .unwrap();
        assert_eq!(
            err.to_string(),
            "Configuration error: QRZ XML base URL must use HTTPS"
        );
    }
    assert!(QrzXmlClient::new("user", "pass", "agent")
        .with_base_url("https://xmldata.qrz.com/xml/current/")
        .is_ok());
}

#[tokio::test]
async fn production_transport_requires_https() {
    let err = QrzXmlClient::new("user", "pass", "agent")
        .with_base_url("https://127.0.0.1/xml/current/")
        .unwrap();
    assert!(err.client.get("http://127.0.0.1/").send().await.is_err());
}

#[tokio::test]
async fn https_redirect_cannot_downgrade_to_http() {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    let cert = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-support/fixtures/localhost-cert.der"
    ));
    let key = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-support/fixtures/localhost-key.der"
    ));
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(cert.as_slice())],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.as_slice())),
    )
    .unwrap();
    let plain = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let secure = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let location = format!(
        "http://127.0.0.1:{}/stolen",
        plain.local_addr().unwrap().port()
    );
    let secure_port = secure.local_addr().unwrap().port();
    let serve = tokio::spawn(async move {
        let (stream, _) = secure.accept().await.unwrap();
        let mut stream = TlsAcceptor::from(Arc::new(config))
            .accept(stream)
            .await
            .unwrap();
        let response = format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let mut client = QrzXmlClient::new("user", "pass", "agent")
        .with_base_url("https://localhost/xml/current/")
        .unwrap();
    client.client = QrzXmlClient::http_builder()
        .no_proxy()
        .add_root_certificate(reqwest::Certificate::from_der(cert).unwrap())
        .build()
        .unwrap();
    let result = client
        .client
        .get(format!("https://localhost:{}/xml/current/", secure_port))
        .send()
        .await;
    assert!(result.is_err(), "plaintext redirect must fail: {result:?}");
    serve.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), plain.accept())
            .await
            .is_err()
    );
}
