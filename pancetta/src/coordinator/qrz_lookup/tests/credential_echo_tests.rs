//! PAN-474: a QRZ error whose "not found" comes only from an echoed credential
//! is a service failure, never a cached callsign miss.

use super::tests::{context, next, speaking_qrz_server};
use super::*;

#[tokio::test]
async fn echoed_credential_service_errors_are_not_cached_as_misses() {
    // QRZ echoes the password; its "not found" is credential text, not a
    // callsign miss, so the failure stays uncached.
    let (url, count, server) = speaking_qrz_server(
        "<QRZDatabase><Session><Error>request rejected for pw not found 73</Error></Session></QRZDatabase>",
    )
    .await;
    let (ctx, rx) = context(true, Some(url));
    // The client is built from config on the first lookup.
    ctx.config.write().await.network.qrz_xml.password = "pw not found 73".into();
    for _ in 0..2 {
        handle_lookup_callsign(&ctx, "DL1ABC".into()).await;
        assert!(
            matches!(next(&rx).await, TuiMessage::StatusUpdate {status, level, ..}
            if status == "DL1ABC lookup failed" && level == pancetta_core::DiagnosticLevel::Warn)
        );
    }
    assert_eq!(count.load(Ordering::SeqCst), 3);
    assert!(!ctx.cache.lock().await.contains_key("DL1ABC"));
    server.abort();
}
