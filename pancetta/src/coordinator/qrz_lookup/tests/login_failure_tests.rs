//! PAN-478: a failed QRZ login is a lookup failure, never a cached callsign
//! miss, whatever its wording says.

use super::tests::{context, next, qrz_server};
use super::*;

#[tokio::test]
async fn failed_login_is_not_cached_as_a_miss() {
    for login in [
        "<QRZDatabase><Session><Error>Username user not found</Error></Session></QRZDatabase>",
        "<QRZDatabase><Session><Error>QRZ returned no callsign data</Error></Session></QRZDatabase>",
    ] {
        let (url, count, server) = qrz_server(
            login,
            "<QRZDatabase><Session><Error>Not found: DL1ABC</Error></Session></QRZDatabase>",
        )
        .await;
        let (ctx, rx) = context(true, Some(url));
        for round in 1..=2 {
            handle_lookup_callsign(&ctx, "DL1ABC".into()).await;
            let reply = next(&rx).await;
            assert!(
                matches!(&reply, TuiMessage::StatusUpdate {status, level, ..}
                if status == "DL1ABC lookup failed" && *level == pancetta_core::DiagnosticLevel::Warn),
                "login {login:?}, round {round}: {reply:?}"
            );
            // The task clears `in_flight` just after it reports; wait for that so
            // the next lookup is not refused as already in progress.
            tokio::time::timeout(Duration::from_secs(3), async {
                while ctx.in_flight.load(Ordering::Acquire) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("lookup task finished");
            // Each lookup tries the login again: nothing was cached.
            assert_eq!(
                count.load(Ordering::SeqCst),
                round,
                "login {login:?}, round {round}"
            );
        }
        assert!(
            !ctx.cache.lock().await.contains_key("DL1ABC"),
            "login {login:?}"
        );
        server.abort();
    }
}
