// PAN-361. Webhook POSTs are capped at `alerts.webhook.max_per_minute` in any
// rolling 60 s, summaries included; alerts over the cap are held and sent as
// one `decode_alert_summary` (or a plain `decode_alert` when only one was
// held) once the window has room and the target is not paused.
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

/// `n` distinct plausible callsigns: K0AA to K9AA, then K0AB, and so on.
fn calls(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| format!("K{}A{}", i % 10, char::from(b'A' + (i / 10) as u8)))
        .collect()
}

async fn server_answering(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(status))
        .mount(&server)
        .await;
    server
}

fn webhook_dispatcher(
    server: &MockServer,
    max_per_minute: u32,
    lookup: CachedStationLookup,
) -> AlertDispatcher {
    let mut config = AlertsConfig::default();
    config.webhook.enabled = true;
    config.webhook.allow_private_targets = true;
    config.webhook.max_per_minute = max_per_minute;
    config.webhook.url = format!("{}/hook", server.uri());
    AlertDispatcher::new(config, lookup, Default::default(), false).unwrap()
}

/// Feeds each call once as a pinned decode, every call pinned.
fn feed(d: &mut AlertDispatcher, calls: &[String]) -> Vec<tokio::task::JoinHandle<()>> {
    let pins = calls.to_vec();
    calls
        .iter()
        .flat_map(|call| d.on_decode(&pinned_decode(call), 14_074_000, &pins).1)
        .collect()
}

async fn complete(tasks: Vec<tokio::task::JoinHandle<()>>) {
    for task in tasks {
        task.await.unwrap();
    }
}

async fn bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| request.body_json().unwrap())
        .collect()
}

#[tokio::test]
async fn fifty_pinned_calls_in_one_slot_post_the_cap_then_one_summary() {
    let server = server_answering(204).await;
    let mut d = webhook_dispatcher(&server, 10, CachedStationLookup::new());
    let calls = calls(50);
    assert_eq!(
        calls.iter().collect::<std::collections::HashSet<_>>().len(),
        50
    );
    let tasks = feed(&mut d, &calls);
    assert_eq!(tasks.len(), 10);
    complete(tasks).await;
    let posted = bodies(&server).await;
    assert_eq!(posted.len(), 10);
    assert!(posted.iter().all(|body| body["type"] == "decode_alert"));

    assert!(d.flush_due(Instant::now()).is_none());
    let summary = d
        .flush_due(Instant::now() + Duration::from_secs(61))
        .expect("the summary is due once the window has room");
    summary.await.unwrap();
    let posted = bodies(&server).await;
    assert_eq!(posted.len(), 11);
    let summary = &posted[10];
    assert_eq!(summary["type"], "decode_alert_summary");
    assert_eq!(summary["source"], "pancetta");
    assert_eq!(summary["version"], 1);
    assert_eq!(summary["count"], 40);
    assert_eq!(summary["window_secs"], 60);
    assert_eq!(summary["max_per_minute"], 10);
    assert_eq!(summary["alerts"].as_array().unwrap().len(), 20);
    assert!(d
        .flush_due(Instant::now() + Duration::from_secs(200))
        .is_none());
}

#[tokio::test]
async fn summary_lists_the_rarest_held_alerts_first() {
    let server = server_answering(204).await;
    let lookup = CachedStationLookup::new();
    lookup.update_needed_atno(["VP8".into()].into());
    let mut d = webhook_dispatcher(&server, 1, lookup);
    let mut calls = calls(25);
    calls.push("VP8ABC".into());
    let tasks = feed(&mut d, &calls);
    assert_eq!(tasks.len(), 1);
    complete(tasks).await;
    d.flush_due(Instant::now() + Duration::from_secs(61))
        .expect("summary due")
        .await
        .unwrap();
    let posted = bodies(&server).await;
    assert_eq!(posted.len(), 2);
    assert_eq!(posted[0]["type"], "decode_alert");
    let summary = &posted[1];
    assert_eq!(summary["type"], "decode_alert_summary");
    assert_eq!(summary["count"], 25);
    let listed = summary["alerts"].as_array().unwrap();
    assert_eq!(listed.len(), 20);
    assert_eq!(listed[0]["callsign"], "VP8ABC");
    assert_eq!(listed[0]["tier_rank"], 1);
    let ranks: Vec<u64> = listed
        .iter()
        .map(|entry| entry["tier_rank"].as_u64().unwrap())
        .collect();
    assert!(ranks.windows(2).all(|pair| pair[0] <= pair[1]), "{ranks:?}");
    for entry in listed {
        assert!(entry.get("type").is_none());
        assert!(entry.get("source").is_none());
        assert!(entry.get("version").is_none());
        assert!(entry["callsign"].is_string());
    }
}

#[tokio::test]
async fn held_alerts_queue_behind_the_summary_and_the_summary_takes_a_slot() {
    let server = server_answering(204).await;
    let mut d = webhook_dispatcher(&server, 2, CachedStationLookup::new());
    let pins: Vec<String> = ["K1AAA", "K2BBB", "K3CCC", "K4DDD", "K5EEE", "K6FFF"]
        .into_iter()
        .map(String::from)
        .collect();
    let post = |d: &mut AlertDispatcher, call: &str| -> Vec<tokio::task::JoinHandle<()>> {
        d.on_decode(&pinned_decode(call), 14_074_000, &pins).1
    };

    let ab: Vec<_> = post(&mut d, "K1AAA")
        .into_iter()
        .chain(post(&mut d, "K2BBB"))
        .collect();
    assert_eq!(ab.len(), 2);
    complete(ab).await;
    assert!(post(&mut d, "K3CCC").is_empty(), "C is over the cap");
    for t in d.webhook_sent.iter_mut() {
        *t -= Duration::from_secs(61);
    }
    assert!(
        post(&mut d, "K4DDD").is_empty(),
        "D waits behind the pending summary"
    );
    d.flush_due(Instant::now())
        .expect("summary due")
        .await
        .unwrap();
    let e = post(&mut d, "K5EEE");
    assert_eq!(e.len(), 1, "E fits beside the summary");
    complete(e).await;
    assert!(
        post(&mut d, "K6FFF").is_empty(),
        "the summary took a slot, so F is held"
    );

    let posted = bodies(&server).await;
    assert_eq!(posted.len(), 4);
    assert_eq!(posted[0]["callsign"], "K1AAA");
    assert_eq!(posted[1]["callsign"], "K2BBB");
    assert_eq!(posted[2]["type"], "decode_alert_summary");
    assert_eq!(posted[2]["count"], 2);
    let held: Vec<&str> = posted[2]["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["callsign"].as_str().unwrap())
        .collect();
    assert_eq!(held, ["K3CCC", "K4DDD"]);
    assert_eq!(posted[3]["type"], "decode_alert");
    assert_eq!(posted[3]["callsign"], "K5EEE");
}

#[tokio::test]
async fn failing_endpoint_is_bounded_by_the_cap_and_the_summary_waits_out_the_pause() {
    let server = server_answering(500).await;
    let mut d = webhook_dispatcher(&server, 10, CachedStationLookup::new());
    let tasks = feed(&mut d, &calls(50));
    assert_eq!(tasks.len(), 10);
    complete(tasks).await;
    assert_eq!(server.received_requests().await.unwrap().len(), 10);

    // The breaker's pause starts at the last recorded failure, so take `now`
    // only after every task has finished.
    let now = Instant::now();
    assert!(
        d.flush_due(now + Duration::from_secs(61)).is_none(),
        "a paused target delays the summary"
    );
    d.flush_due(now + Duration::from_secs(301))
        .expect("the summary is sent once the pause ends")
        .await
        .unwrap();
    let posted = bodies(&server).await;
    assert_eq!(posted.len(), 11);
    assert_eq!(posted[10]["type"], "decode_alert_summary");
    assert_eq!(posted[10]["count"], 40);
}

#[tokio::test]
async fn a_lone_held_alert_posts_as_a_plain_decode_alert() {
    let server = server_answering(204).await;
    let mut d = webhook_dispatcher(&server, 1, CachedStationLookup::new());
    let tasks = feed(&mut d, &["K1AAA".to_string(), "K2BBB".to_string()]);
    assert_eq!(tasks.len(), 1);
    complete(tasks).await;
    d.flush_due(Instant::now() + Duration::from_secs(61))
        .expect("the held alert is due")
        .await
        .unwrap();
    let posted = bodies(&server).await;
    assert_eq!(posted.len(), 2);
    assert_eq!(posted[1]["type"], "decode_alert");
    assert_eq!(posted[1]["source"], "pancetta");
    assert_eq!(posted[1]["version"], 1);
    assert_eq!(posted[1]["callsign"], "K2BBB");
    assert!(posted[1].get("count").is_none());
}
