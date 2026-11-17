// PAN-315: every HTTPS/WSS client here rejects an untrusted certificate.
use super::*;
mod fixture {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test-support/tls.rs"
    ));
}
#[tokio::test]
async fn tls_untrusted_alerts() {
    fixture::rejects_http(AlertDispatcher::http_builder().no_proxy().build().unwrap()).await;
}
