//! Integration tests for the cqdx.io client + cache interaction.

use pancetta_core::time::UtcTimestamp;
use pancetta_cqdx::types::*;
use pancetta_cqdx::{CqdxCache, CqdxClient};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Full startup flow: fetch entities, fetch needed, verify cache state.
#[tokio::test]
async fn test_startup_flow_entities_and_needed() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/entities"))
        .and(header("Authorization", "Bearer pat_test_token_long_enough"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entities": [
                {
                    "adifNumber": 291, "entityName": "United States", "prefix": "K",
                    "continent": "NA", "cqZone": 5, "ituZone": 8,
                    "rarityRank": 340, "rarityTier": "common", "isDeleted": false
                },
                {
                    "adifNumber": 339, "entityName": "Japan", "prefix": "JA",
                    "continent": "AS", "cqZone": 25, "ituZone": 45,
                    "rarityRank": 300, "rarityTier": "common", "isDeleted": false
                },
                {
                    "adifNumber": 24, "entityName": "Bouvet Island", "prefix": "3Y/B",
                    "continent": "AF", "cqZone": 38, "ituZone": 67,
                    "rarityRank": 1, "rarityTier": "legendary", "isDeleted": false
                }
            ]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/entities/needed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "needed": [
                { "entityId": 24, "name": "Bouvet Island", "prefix": "3Y/B" }
            ]
        })))
        .mount(&server)
        .await;

    let client = CqdxClient::new(server.uri(), "pat_test_token_long_enough".to_string()).unwrap();
    let mut cache = CqdxCache::new();

    let entities = client.fetch_entities().await.unwrap();
    assert_eq!(entities.len(), 3);
    assert_eq!(entities[0].adif_number, 291);
    assert_eq!(entities[0].entity_name, "United States");
    cache.load_entities(entities);

    let needed = client.fetch_needed(None).await.unwrap();
    assert_eq!(needed.len(), 1);
    cache.load_needed(needed);

    assert_eq!(cache.is_needed_dxcc("3Y0J"), Some(true));
    assert_eq!(cache.is_needed_dxcc("K1ABC"), Some(false));
    assert_eq!(cache.is_needed_dxcc("JA1XYZ"), Some(false));
}

/// Live spot poll updates rarity scores in cache.
#[tokio::test]
async fn test_live_spot_poll_updates_rarity() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/spots"))
        .and(query_param("live", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "groups": [
                {
                    "dxCall": "3Y0J",
                    "band": "20m",
                    "mode": "FT8",
                    "dxDxcc": 24,
                    "dxEntityName": "Bouvet Island",
                    "dxContinent": "AF",
                    "dxCqZone": 38,
                    "dxGrid": "JD15",
                    "rarityRank": 1,
                    "rarityTier": "legendary",
                    "frequency": 14074000_u64,
                    "bestSnr": -12,
                    "reporterCount": 5,
                    "sources": ["pskreporter"],
                    "firstSeen": 1743688920_i64,
                    "lastSeen": 1743689040_i64,
                    "confidence": 4.2
                },
                {
                    "dxCall": "K1ABC",
                    "band": "20m",
                    "mode": "FT8",
                    "dxDxcc": 291,
                    "dxEntityName": "United States",
                    "dxContinent": "NA",
                    "dxCqZone": 5,
                    "dxGrid": null,
                    "rarityRank": 340,
                    "rarityTier": "common",
                    "frequency": 14074000_u64,
                    "bestSnr": null,
                    "reporterCount": 1,
                    "sources": ["pskreporter"],
                    "firstSeen": 1743688920_i64,
                    "lastSeen": 1743689040_i64,
                    "confidence": 1.5
                }
            ]
        })))
        .mount(&server)
        .await;

    let client = CqdxClient::new(server.uri(), "pat_test_token_long_enough".to_string()).unwrap();
    let mut cache = CqdxCache::new();

    let groups = client.fetch_live_spots(None, None).await.unwrap();
    cache.update_spot_groups(groups);

    // rank 1 → rarity 1.0
    assert!((cache.rarity("3Y0J") - 1.0).abs() < f64::EPSILON);
    // rank 340 → rarity ~0.0
    assert!(cache.rarity("K1ABC") < 0.01);
    // unknown → default 0.5
    assert!((cache.rarity("UNKNOWN") - 0.5).abs() < f64::EPSILON);
}

/// Spot and QSO reporting doesn't fail on valid server response.
#[tokio::test]
async fn test_spot_and_qso_reporting() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/v1/spots/report"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/v1/qsos"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let client = CqdxClient::new(server.uri(), "pat_test_token_long_enough".to_string()).unwrap();

    let spots = vec![SpotReport {
        callsign: "W1ABC".to_string(),
        grid: Some("FN42".to_string()),
        frequency: 14074000,
        mode: "FT8".to_string(),
        snr: -12,
        timestamp: UtcTimestamp::now(),
        reporter: "K2DEF".to_string(),
        reporter_grid: Some("FN31".to_string()),
    }];
    client.report_spots(spots).await.unwrap();

    let qso = QsoRecord {
        callsign: "JA1ABC".to_string(),
        remote_grid: Some("PM95".to_string()),
        local_grid: Some("FN31".to_string()),
        frequency: 14074000,
        mode: "FT8".to_string(),
        rst_sent: Some("-10".to_string()),
        rst_received: Some("-14".to_string()),
        start_time: UtcTimestamp::now(),
        end_time: UtcTimestamp::now(),
    };
    client.report_qso(qso).await.unwrap();
}

/// Without API data, offline identity still works and needed stays unknown.
#[test]
fn test_degraded_mode_defaults() {
    let cache = CqdxCache::new();
    assert_eq!(cache.resolve_entity("K1ABC"), Some(291));
    assert_eq!(cache.is_needed_dxcc("K1ABC"), None);
    assert!((cache.rarity("K1ABC") - 0.5).abs() < f64::EPSILON);
    assert!(cache.spot_groups().is_empty());
}

/// PAN-359 AC10: the served `/api/v1/pota` body parses. The first row is
/// the CQD-630 example verbatim; `byCallsign`, `updatedAt`, `name`, `mode`
/// and unknown fields are ignored, `frequency` may be a string, a number or
/// null, a read field of the wrong JSON type reads as empty, and a row
/// that is not a JSON object is skipped without emptying the feed.
#[tokio::test]
async fn fetch_activations_parses_served_feed() {
    let server = MockServer::start().await;
    let body = r#"{"activations":[{"callsign":"7N1ICA/1","type":"sota","reference":"JA/YN-036","name":"Onigatake","frequency":"7.005","mode":"CW","spotTime":"2026-11-06T02:48:13.285492Z"},{"callsign":"DL1ABC","type":"pota","reference":"DA-0123","name":"Some Park","frequency":"14074","mode":"FT8","spotTime":"2026-11-22T09:01:00Z","extra":{"nested":true}},{"callsign":"K1XYZ","type":"pota","reference":"K-0001","name":"Another Park","frequency":7.09,"mode":"FT8","spotTime":"2026-11-22T09:02:00Z"},{"callsign":"W2ABC","type":"sota","reference":"W2/WE-003","name":null,"frequency":null,"mode":null},{"callsign":"N0NUM","type":"pota","reference":123,"spotTime":1793934000},null,"junk",42,[]],"byCallsign":{"7N1ICA/1":[{"callsign":"7N1ICA/1","type":"sota","reference":"JA/YN-036","name":"Onigatake","frequency":"7.005","mode":"CW","spotTime":"2026-11-06T02:48:13.285492Z"}]},"updatedAt":1793934000}"#;
    Mock::given(method("GET"))
        .and(path("/api/v1/pota"))
        .and(header("Authorization", "Bearer pat_test_token_long_enough"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
        .expect(1)
        .mount(&server)
        .await;

    let client = CqdxClient::new(server.uri(), "pat_test_token_long_enough".to_string()).unwrap();
    let rows = client
        .fetch_activations()
        .await
        .expect("parses")
        .expect("offered");
    assert_eq!(
        rows,
        vec![
            Activation {
                callsign: "7N1ICA/1".into(),
                program: "sota".into(),
                reference: "JA/YN-036".into(),
                spot_time: Some("2026-11-06T02:48:13.285492Z".into()),
            },
            Activation {
                callsign: "DL1ABC".into(),
                program: "pota".into(),
                reference: "DA-0123".into(),
                spot_time: Some("2026-11-22T09:01:00Z".into()),
            },
            Activation {
                callsign: "K1XYZ".into(),
                program: "pota".into(),
                reference: "K-0001".into(),
                spot_time: Some("2026-11-22T09:02:00Z".into()),
            },
            Activation {
                callsign: "W2ABC".into(),
                program: "sota".into(),
                reference: "W2/WE-003".into(),
                spot_time: None,
            },
            // A field of another JSON type reads as empty, never failing
            // the whole feed.
            Activation {
                callsign: "N0NUM".into(),
                program: "pota".into(),
                reference: String::new(),
                spot_time: None,
            },
        ]
    );

    // The documented empty response.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/pota"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"activations":[],"byCallsign":{},"updatedAt":null}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
    let client = CqdxClient::new(server.uri(), "pat_test_token_long_enough".to_string()).unwrap();
    assert_eq!(client.fetch_activations().await.unwrap(), Some(vec![]));
}

/// PAN-359 AC10: a 404 means the feed is not offered (`Ok(None)`); other
/// failures are errors.
#[tokio::test]
async fn fetch_activations_404_is_not_offered() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/pota"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let client = CqdxClient::new(server.uri(), "pat_test_token_long_enough".to_string()).unwrap();
    assert_eq!(client.fetch_activations().await.unwrap(), None);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/pota"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let client = CqdxClient::new(server.uri(), "pat_test_token_long_enough".to_string()).unwrap();
    assert!(client.fetch_activations().await.is_err());
}
