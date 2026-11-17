// PAN-315: every HTTPS/WSS client here rejects an untrusted certificate.
use super::*;
mod fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-support/tls.rs"
    ));
}
#[tokio::test]
async fn tls_untrusted_pairing() {
    fixture::rejects_http(
        ReqwestPairingHttp::http_builder(PAIRING_HTTP_TIMEOUT)
            .no_proxy()
            .build()
            .unwrap(),
    )
    .await;
}

#[tokio::test]
async fn tls_untrusted_relay() {
    fixture::rejects_untrusted("wss", |url| async move {
        RealWsConn::connect(&url, RelayTimeouts::default()).await
    })
    .await;
}
