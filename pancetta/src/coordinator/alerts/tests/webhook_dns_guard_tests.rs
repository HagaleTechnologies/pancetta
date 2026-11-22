// PAN-361. The webhook target check also runs after DNS resolution: without
// `allow_private_targets`, the webhook client drops every resolved address
// that is loopback, private, link-local, CGNAT, unspecified or multicast, so
// a public-looking name that resolves (or is rebound) to the station's own
// network is never contacted.
use super::*;
use std::net::SocketAddr;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

/// A lookup that answers every name with `addrs`, standing in for DNS.
fn fake_lookup(addrs: Vec<SocketAddr>) -> WebhookLookup {
    Arc::new(move |_name| {
        let addrs = addrs.clone();
        Box::pin(async move { Ok(addrs) })
    })
}

fn sock(ip: &str) -> SocketAddr {
    SocketAddr::new(ip.parse().unwrap(), 0)
}

async fn mock_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    server
}

fn guarded(allow_private_targets: bool) -> pancetta_config::alerts::WebhookAlertConfig {
    pancetta_config::alerts::WebhookAlertConfig {
        allow_private_targets,
        ..Default::default()
    }
}

#[test]
fn resolved_addresses_keep_only_public_unicast() {
    let kept = public_addrs(vec![
        sock("127.0.0.1"),
        sock("93.184.215.14"),
        sock("::ffff:10.0.0.1"),
        sock("2606:4700::1111"),
        sock("169.254.169.254"),
    ])
    .unwrap();
    assert_eq!(kept, vec![sock("93.184.215.14"), sock("2606:4700::1111")]);

    for only_private in [
        vec![sock("127.0.0.1")],
        vec![sock("::1"), sock("fd00::1")],
        vec![sock("169.254.169.254")],
        vec![sock("::ffff:192.168.1.1")],
        vec![sock("100.64.0.1")],
        vec![],
    ] {
        let error = public_addrs(only_private.clone()).expect_err("no public address");
        assert!(
            error.to_string().contains("no public address"),
            "{only_private:?}: {error}"
        );
    }
}

#[tokio::test]
async fn guarded_client_never_connects_to_a_name_resolving_to_loopback() {
    let server = mock_server().await;
    let port = server.address().port();
    let url = format!("http://hooks.example.com:{port}/hook");
    let lookup = fake_lookup(vec![sock("127.0.0.1")]);

    let refused = webhook_client(&guarded(false), lookup.clone()).unwrap();
    assert!(refused.post(&url).json(&json!({})).send().await.is_err());
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "the guarded client must not reach a loopback address"
    );

    // The opt-in turns the resolve-time guard off, as it does the URL check.
    let allowed = webhook_client(&guarded(true), lookup).unwrap();
    let response = allowed.post(&url).json(&json!({})).send().await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn dispatcher_webhook_deliveries_go_through_the_guarded_client() {
    let server = mock_server().await;
    let port = server.address().port();
    let mut config = AlertsConfig::default();
    config.webhook.enabled = true;
    config.webhook.allow_private_targets = true;
    config.webhook.url = format!("http://hooks.example.com:{port}/hook");
    let mut d = AlertDispatcher::new(
        config,
        CachedStationLookup::new(),
        Default::default(),
        false,
    )
    .unwrap();

    let pinned = |call: &str| {
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
    };
    let loopback = fake_lookup(vec![sock("127.0.0.1")]);

    // Control: with the opt-in, the same DNS answer reaches the server, so
    // the refusal below is the guard and not a failed lookup.
    d.webhook_client = webhook_client(&guarded(true), loopback.clone()).unwrap();
    let (_, tasks) = d.on_decode(&pinned("T88AA"), 14_074_000, &["T88AA".into()]);
    assert_eq!(tasks.len(), 1);
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    // A guarded client whose DNS answers loopback, as a rebinding name would
    // after validation, never reaches the server.
    d.webhook_client = webhook_client(&guarded(false), loopback).unwrap();
    let (_, tasks) = d.on_decode(&pinned("VP8ABC"), 14_074_000, &["VP8ABC".into()]);
    assert_eq!(tasks.len(), 1);
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(
        d.delivery[1]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failures,
        1,
        "a refused resolution counts as a failed delivery"
    );
}

#[test]
fn new_builds_a_guarded_webhook_client_only_without_the_opt_in() {
    assert!(webhook_guard_applies(&guarded(false)));
    assert!(!webhook_guard_applies(&guarded(true)));
}
