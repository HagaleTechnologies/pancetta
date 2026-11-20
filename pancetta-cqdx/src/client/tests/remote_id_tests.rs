use super::tests::{sample_qso, test_client};
use super::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn log_qso_captures_remote_id() {
    for (status, body, expected_duplicate) in [
        (201, r#"{"duplicate":false}"#.to_string(), false),
        (201, r#"{"duplicate":true}"#.to_string(), true),
        (409, String::new(), true),
        (201, "x".repeat(MAX_LOG_QSO_BODY_BYTES + 1), false),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/qsos"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("X-QSO-Id", "abc123")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        let outcome = test_client(&server.uri())
            .log_qso(sample_qso())
            .await
            .unwrap();
        let expected = if expected_duplicate {
            QsoUploadOutcome::Duplicate {
                remote_id: Some("abc123".into()),
            }
        } else {
            QsoUploadOutcome::Logged {
                remote_id: Some("abc123".into()),
            }
        };
        assert_eq!(outcome, expected);
    }
}

#[tokio::test]
async fn log_qso_accepts_without_usable_remote_id() {
    for header in [
        None,
        Some("   ".to_string()),
        Some("x y".to_string()),
        Some("x".repeat(129)),
        Some("é".to_string()),
    ] {
        for duplicate in [false, true] {
            let server = MockServer::start().await;
            let mut response = ResponseTemplate::new(201)
                .set_body_json(serde_json::json!({"duplicate":duplicate}));
            if let Some(value) = header.as_deref() {
                response = response.insert_header("X-QSO-Id", value);
            }
            Mock::given(method("POST"))
                .and(path("/api/v1/qsos"))
                .respond_with(response)
                .mount(&server)
                .await;
            let outcome = test_client(&server.uri())
                .log_qso(sample_qso())
                .await
                .unwrap();
            assert_eq!(
                outcome,
                if duplicate {
                    QsoUploadOutcome::Duplicate { remote_id: None }
                } else {
                    QsoUploadOutcome::Logged { remote_id: None }
                }
            );
        }
    }
}
