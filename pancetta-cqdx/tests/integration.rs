//! Integration tests for the cqdx.io client + cache interaction.

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
                    "adifNumber": 327, "entityName": "Bouvet Island", "prefix": "3Y/B",
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
                { "entityId": 327, "name": "Bouvet Island", "prefix": "3Y/B" }
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

    assert!(cache.is_needed_dxcc("3Y/B1234"));
    assert!(!cache.is_needed_dxcc("K1ABC"));
    assert!(!cache.is_needed_dxcc("JA1XYZ"));
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
                    "dxDxcc": 327,
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
        timestamp: chrono::Utc::now(),
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
        start_time: chrono::Utc::now(),
        end_time: chrono::Utc::now(),
    };
    client.report_qso(qso).await.unwrap();
}

/// Degraded mode: CqdxCache with no data returns conservative defaults.
#[test]
fn test_degraded_mode_defaults() {
    let cache = CqdxCache::new();
    assert_eq!(cache.resolve_entity("K1ABC"), None);
    assert!(cache.is_needed_dxcc("K1ABC"));
    assert!((cache.rarity("K1ABC") - 0.5).abs() < f64::EPSILON);
    assert!(cache.spot_groups().is_empty());
}
