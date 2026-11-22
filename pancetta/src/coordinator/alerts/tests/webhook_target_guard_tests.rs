// PAN-361. `AlertDispatcher::new` re-checks the webhook target, so a config
// that reached the station without validation never posts to a loopback,
// private or plaintext target unless `allow_private_targets` is set.
use super::*;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn pinned_decode(call: &str) -> pancetta_ft8::DecodedMessage {
    let mut msg = pancetta_ft8::DecodedMessage::new(
        pancetta_ft8::Ft8Message {
            from_callsign: Some(call.into()),
            grid_square: Some("FN42".into()),
            ..Default::default()
        },
        -8.0,
        1.0,
        1500.0,
        0.0,
    );
    msg.text = format!("K1ABC {call} -08");
    msg.captured_dial_hz = Some(14_074_000);
    msg
}

#[tokio::test]
async fn dispatcher_turns_off_a_private_webhook_without_the_opt_in() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let mut config = AlertsConfig::default();
    config.webhook.enabled = true;
    config.webhook.url = format!("{}/hook", server.uri());
    assert!(server.uri().starts_with("http://127.0.0.1"));
    assert!(config.webhook.restricted_target().is_some());

    let mut refused = AlertDispatcher::new(
        config.clone(),
        CachedStationLookup::new(),
        Default::default(),
        false,
    )
    .expect("a refused webhook target turns the channel off, never fails new()");
    assert!(!refused.config.webhook.enabled);
    let (bell, tasks) = refused.on_decode(&pinned_decode("VP8ABC"), 14_074_000, &["VP8ABC".into()]);
    assert!(!bell);
    assert!(tasks.is_empty(), "no webhook task without the opt-in");
    assert!(refused
        .flush_due(Instant::now() + Duration::from_secs(61))
        .is_none());
    assert!(server.received_requests().await.unwrap().is_empty());

    config.webhook.allow_private_targets = true;
    let mut allowed = AlertDispatcher::new(
        config,
        CachedStationLookup::new(),
        Default::default(),
        false,
    )
    .unwrap();
    let (_, tasks) = allowed.on_decode(&pinned_decode("VP8ABC"), 14_074_000, &["VP8ABC".into()]);
    assert_eq!(tasks.len(), 1);
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
