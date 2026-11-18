//! PAN-385: the all-band needed-activity poll (AC15, AC16).

use super::tests::{cfg, mount, TOKEN};
use super::*;
use pancetta_qso::priority::WorkedStationLookup;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const NOW: i64 = 1_800_000_000;

fn group(call: &str, band: &str, mode: &str, dxcc: u32, last_seen: i64) -> SpotGroup {
    SpotGroup {
        dx_call: call.to_string(),
        band: band.to_string(),
        mode: mode.to_string(),
        dx_dxcc: dxcc,
        dx_entity_name: String::new(),
        dx_continent: String::new(),
        dx_cq_zone: 0,
        dx_grid: None,
        rarity_rank: Some(1),
        rarity_tier: String::new(),
        frequency: 0,
        best_snr: Some(-5),
        reporter_count: 3,
        sources: Vec::new(),
        first_seen: last_seen,
        last_seen,
        confidence: 1.0,
        is_notable: true,
        notable_type: None,
    }
}

fn counts(band: Band, needed_entities: u32, atno_entities: u32) -> BandOpportunity {
    BandOpportunity {
        band,
        needed_entities,
        atno_entities,
    }
}

#[test]
fn count_needed_by_band_counts_fresh_distinct_digital_entities() {
    let fresh = NOW - 30;
    let groups = vec![
        group("A1A", "17m", "FT8", 1, fresh),
        group("A1B", "17m", "ft8", 1, fresh),
        group("A2A", "17m", "FT4", 2, fresh),
        group("A3A", "17m", "FT8", 3, fresh),
        group("A4A", "17m", "CW", 4, fresh),
        group("A4B", "17m", "FT8", 4, NOW - 660),
        group("A1C", "17M", "FT8", 1, fresh),
        group("A5A", "20m", "FT8", 5, fresh),
        group("A1D", "15m", "FT8", 1, fresh),
    ];
    let needed = HashMap::from([
        (
            Band::Band17m,
            HashMap::from([(1, false), (2, true), (4, false)]),
        ),
        (Band::Band20m, HashMap::from([(9, false)])),
    ]);
    let got = count_needed_by_band(&groups, &needed, &[Band::Band20m, Band::Band17m], NOW);
    assert_eq!(
        got,
        vec![counts(Band::Band20m, 0, 0), counts(Band::Band17m, 2, 1)]
    );
}

fn spot_json(call: &str, band: &str, mode: &str, dxcc: u32, last_seen: i64) -> serde_json::Value {
    serde_json::json!({
        "dxCall": call,
        "band": band,
        "mode": mode,
        "dxDxcc": dxcc,
        "dxEntityName": "",
        "dxContinent": "",
        "dxCqZone": 0,
        "dxGrid": null,
        "rarityRank": 1,
        "rarityTier": "",
        "frequency": 18_100_000,
        "bestSnr": -5,
        "reporterCount": 3,
        "sources": [],
        "firstSeen": last_seen,
        "lastSeen": last_seen,
        "confidence": 1.0,
        "isNotable": true,
        "notableType": null,
    })
}

fn needed_ids_json(rows: &[(u32, bool)]) -> serde_json::Value {
    serde_json::json!({
        "needed": rows
            .iter()
            .map(|(id, atno)| serde_json::json!({"entityId": id, "name": "", "prefix": "", "atno": atno}))
            .collect::<Vec<_>>()
    })
}

const CALLS_17M: [&str; 3] = ["JA1ABC", "DL1ABC", "VK2ABC"];

/// A cqdx server whose startup loads succeed, with per-band needed sets for
/// 17m (`{1, 2 ATNO}`) and 20m (`{9}`), and `spots` answering
/// `spots_status` exactly `spots_expect` times (`None` = at least once).
async fn opportunity_server(spots_status: u16, spots_expect: Option<u64>) -> MockServer {
    let server = MockServer::start().await;
    for (band, rows) in [
        ("17m", vec![(1, false), (2, true)]),
        ("20m", vec![(9, false)]),
    ] {
        Mock::given(method("GET"))
            .and(path("/api/v1/entities/needed"))
            .and(query_param("band", band))
            .respond_with(ResponseTemplate::new(200).set_body_json(needed_ids_json(&rows)))
            .with_priority(1)
            .expect(1..)
            .mount(&server)
            .await;
    }
    // Startup's all-band fetch (no `band` parameter).
    mount(
        &server,
        "/api/v1/entities/needed",
        200,
        needed_ids_json(&[(1, false)]),
    )
    .await;
    mount(
        &server,
        "/api/v1/entities",
        200,
        serde_json::json!({"entities": []}),
    )
    .await;
    mount(
        &server,
        "/api/v1/entities/needed-grids",
        200,
        serde_json::json!({"grids": []}),
    )
    .await;
    let now = chrono::Utc::now().timestamp() - 30;
    let body = serde_json::json!({"groups": [
        spot_json(CALLS_17M[0], "17m", "FT8", 1, now),
        spot_json(CALLS_17M[1], "17m", "FT4", 2, now),
        spot_json(CALLS_17M[2], "17m", "FT8", 3, now),
        spot_json("W1AW", "20m", "FT8", 5, now),
    ]});
    let template = if spots_status == 200 {
        ResponseTemplate::new(200).set_body_json(body)
    } else {
        ResponseTemplate::new(spots_status)
    };
    let mock = Mock::given(method("GET"))
        .and(path("/api/v1/spots"))
        .and(query_param("live", "true"))
        .respond_with(template);
    let mock = match spots_expect {
        Some(n) => mock.expect(n),
        None => mock.expect(1..),
    };
    mock.mount(&server).await;
    server
}

async fn started_bridge(
    server: &MockServer,
    poll_interval_secs: u64,
) -> (Arc<CqdxBridge>, Arc<CachedStationLookup>) {
    let lookup = Arc::new(CachedStationLookup::new());
    let config = pancetta_config::network::CqdxConfig {
        poll_interval_secs,
        ..cfg(&server.uri(), Some(TOKEN))
    };
    let bridge = Arc::new(CqdxBridge::from_config(&config, lookup.clone()).unwrap());
    bridge.startup().await.unwrap();
    assert!(!bridge.pending().blocked);
    (bridge, lookup)
}

fn spawn_for(bridge: &Arc<CqdxBridge>) -> tokio::task::JoinHandle<()> {
    bridge.spawn_opportunity_poller(
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicU64::new(0)),
        vec![Band::Band20m, Band::Band17m],
    )
}

async fn wait_for_snapshot(bridge: &CqdxBridge) -> OpportunitySnapshot {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(snapshot) = bridge.band_opportunities().await {
            return snapshot;
        }
        assert!(Instant::now() < deadline, "no snapshot within 2 s");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn opportunity_poller_builds_a_snapshot_from_all_band_spots() {
    let server = opportunity_server(200, None).await;
    let (bridge, _lookup) = started_bridge(&server, 10).await;
    let before = chrono::Utc::now();
    let handle = spawn_for(&bridge);
    let snapshot = wait_for_snapshot(&bridge).await;
    handle.abort();
    assert!(snapshot.as_of >= before, "as_of is the spots fetch time");
    assert_eq!(
        snapshot.bands,
        vec![counts(Band::Band20m, 0, 0), counts(Band::Band17m, 2, 1)]
    );
    let requests = server.received_requests().await.unwrap();
    let spots: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path() == "/api/v1/spots")
        .collect();
    assert!(!spots.is_empty());
    for r in spots {
        assert!(
            r.url.query_pairs().all(|(k, _)| k != "band" && k != "mode"),
            "all-band poll must not filter: {}",
            r.url
        );
    }
    server.verify().await;
}

#[tokio::test]
async fn opportunity_poller_leaves_callsign_maps_alone() {
    let server = opportunity_server(200, None).await;
    let (bridge, lookup) = started_bridge(&server, 10).await;
    let handle = spawn_for(&bridge);
    wait_for_snapshot(&bridge).await;
    handle.abort();
    for call in CALLS_17M {
        assert_eq!(lookup.rarity(call), 0.5, "{call}: rarity written");
        assert!(!lookup.is_notable(call), "{call}: notable written");
        assert_eq!(
            lookup.network_snr(call),
            None,
            "{call}: network SNR written"
        );
        assert_eq!(
            lookup.network_last_seen(call),
            None,
            "{call}: network last-seen written"
        );
    }
    assert!(bridge.cache().read().await.spot_groups().is_empty());
}

#[tokio::test]
async fn opportunity_poller_stops_after_a_permanent_error() {
    let server = opportunity_server(401, Some(1)).await;
    let (bridge, _lookup) = started_bridge(&server, 1).await;
    let handle = spawn_for(&bridge);
    tokio::time::sleep(Duration::from_millis(2300)).await;
    handle.abort();
    assert!(
        bridge.pending().blocked,
        "401 on spots must block the session"
    );
    assert!(bridge.band_opportunities().await.is_none());
    server.verify().await;
}
