//! PAN-495: the never-worked (all-bands) list loads even when the dial is
//! known from the start or the first fetch fails.

use super::tests::{cfg, TOKEN};
use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

// PAN-495: with the dial known from the start, the poller only asks for
// per-band lists; the all-bands (never-worked) list must still load or
// no station can ever be ATNO this session.
#[tokio::test]
async fn poller_loads_the_all_bands_list_when_the_dial_is_known_from_the_start() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/entities/needed"))
        .and(query_param("band", "20M"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "needed": [{"entityId": 24, "name": "Bouvet", "prefix": "3Y/B", "atno": true}]
        })))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/entities/needed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "needed": [{"entityId": 24, "name": "Bouvet", "prefix": "3Y/B", "atno": true}]
        })))
        .with_priority(2)
        .expect(1..)
        .mount(&server)
        .await;
    let lookup = Arc::new(CachedStationLookup::new());
    let config = pancetta_config::network::CqdxConfig {
        poll_interval_secs: 10,
        ..cfg(&server.uri(), Some(TOKEN))
    };
    let bridge = Arc::new(
        CqdxBridge::from_config(&config, lookup.clone())
            .unwrap()
            .with_operating_frequency(Arc::new(AtomicU64::new(14_074_000))),
    );
    let handle = bridge.spawn_spot_poller(
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicU64::new(0)),
        None,
        None,
    );
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    handle.abort();
    assert!(lookup.never_worked_loaded());
    assert_eq!(lookup.display_needed_atno("3Y0J"), (true, true));
    server.verify().await;
}
