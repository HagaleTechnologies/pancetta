// PAN-315. TLS trust for the PSKReporter client: a server certificate
// the rustls verifier does not trust is rejected (test-support/tls.rs).
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
