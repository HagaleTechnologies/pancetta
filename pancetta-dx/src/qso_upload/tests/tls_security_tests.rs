// PAN-315. TLS trust for the QSO upload client: a server certificate
// the rustls verifier does not trust is rejected (test-support/tls.rs).
use super::*;
mod fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-support/tls.rs"
    ));
}
// ClubLog, QRZ Logbook and eQSL all call this same production builder.
#[tokio::test]
async fn tls_untrusted_upload_services() {
    fixture::rejects_http(http_builder().no_proxy().build().unwrap()).await;
}
