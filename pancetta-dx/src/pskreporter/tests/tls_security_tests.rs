// PAN-315: every HTTPS/WSS client here rejects an untrusted certificate.
use super::*;
mod fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-support/tls.rs"
    ));
}
#[tokio::test]
async fn tls_untrusted_pskreporter() {
    fixture::rejects_http(
        PskReporterClient::http_builder(&PskReporterConfig::default())
            .no_proxy()
            .build()
            .unwrap(),
    )
    .await;
}
