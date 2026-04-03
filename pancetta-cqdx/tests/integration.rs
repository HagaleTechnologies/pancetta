//! Integration tests for the cqdx.io client + cache interaction.

use pancetta_cqdx::{CqdxClient, CqdxCache};
use pancetta_cqdx::types::*;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Full startup flow: fetch entities, fetch needed, verify cache state.
#[tokio::test]
async fn test_startup_flow_entities_and_needed() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/entities"))
        .and(header("Authorization", "Bearer pat_test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entities": [
                { "id": 291, "name": "United States", "prefix": "K", "continent": "NA", "cqZone": 5, "ituZone": 8 },
                { "id": 339, "name": "Japan", "prefix": "JA", "continent": "AS", "cqZone": 25, "ituZone": 45 },
                { "id": 327, "name": "Bouvet Island", "prefix": "3Y/B", "continent": "AF", "cqZone": 38, "ituZone": 67 }
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

    let client = CqdxClient::new(server.uri(), "pat_test".to_string());
    let mut cache = CqdxCache::new();

    let entities = client.fetch_entities().await.unwrap();
    assert_eq!(entities.len(), 3);
    cache.load_entities(entities);

    let needed = client.fetch_needed().await.unwrap();
    assert_eq!(needed.len(), 1);
    cache.load_needed(needed);

    assert!(cache.is_needed_dxcc("3Y/B1234"));
    assert!(!cache.is_needed_dxcc("K1ABC"));
    assert!(!cache.is_needed_dxcc("JA1XYZ"));
}

/// Priority poll updates rarity scores in cache.
#[tokio::test]
async fn test_priority_poll_updates_rarity() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/spots/priorities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "priorities": [
                {
                    "callsign": "3Y0J",
                    "grid": "JD15",
                    "frequency": 14074000_u64,
                    "mode": "FT8",
                    "snr": -12,
                    "entity": "Bouvet Island",
                    "rarity": 0.98,
                    "needed": true,
                    "lastSpotted": "2026-04-03T14:22:00Z",
                    "spotCount": 5
                },
                {
                    "callsign": "K1ABC",
                    "frequency": 14074000_u64,
                    "mode": "FT8",
                    "rarity": 0.02,
                    "needed": false,
                    "lastSpotted": "2026-04-03T14:22:00Z",
                    "spotCount": 1
                }
            ]
        })))
        .mount(&server)
        .await;

    let client = CqdxClient::new(server.uri(), "pat_test".to_string());
    let mut cache = CqdxCache::new();

    let spots = client.fetch_priorities(None, None, 20).await.unwrap();
    cache.update_priorities(spots);

    assert!((cache.rarity("3Y0J") - 0.98).abs() < f64::EPSILON);
    assert!((cache.rarity("K1ABC") - 0.02).abs() < f64::EPSILON);
    assert!((cache.rarity("UNKNOWN") - 0.5).abs() < f64::EPSILON);
}

/// Spot and QSO reporting doesn't fail on valid server response.
#[tokio::test]
async fn test_spot_and_qso_reporting() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/v1/spots/ingest"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/v1/qsos"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let client = CqdxClient::new(server.uri(), "pat_test".to_string());

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
    assert!(cache.priority_spots().is_empty());
}
