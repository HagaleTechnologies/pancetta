//! Local-gateway admission: browser Origins before upgrade, then a hello token
//! before subscribing to or sending any station data.
use std::{sync::Arc, time::Duration};

use axum::http::HeaderValue;
use pancetta_protocol::ClientFrame;
use subtle::ConstantTimeEq;

pub(crate) const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const MAX_INBOUND_BYTES: usize = 64 * 1024;

// No Debug: this holds a plaintext credential.
#[derive(Clone)]
pub(crate) struct GatewayAuth {
    token: Arc<str>,
    allowed_origins: Arc<[String]>,
    pub hello_timeout: Duration,
}

pub(crate) enum AuthFailure {
    NoHelloInTime,
    NotHello,
    MissingToken,
    WrongToken,
    ClosedBeforeHello,
}

impl AuthFailure {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::NoHelloInTime => "no hello in time",
            Self::NotHello => "first frame was not a hello",
            Self::MissingToken => "hello carried no gatewayToken",
            Self::WrongToken => "gatewayToken did not match",
            Self::ClosedBeforeHello => "client closed before hello",
        }
    }
}

impl GatewayAuth {
    pub(crate) fn new(token: &str, allowed_origins: &[String]) -> Self {
        Self {
            token: Arc::from(token),
            allowed_origins: Arc::from(allowed_origins),
            hello_timeout: HELLO_TIMEOUT,
        }
    }

    pub(crate) fn origin_allowed(&self, origin: Option<&HeaderValue>) -> bool {
        match origin {
            None => true,
            Some(value) => value.to_str().is_ok_and(|origin| {
                !origin.eq_ignore_ascii_case("null")
                    && self
                        .allowed_origins
                        .iter()
                        .any(|allowed| allowed.eq_ignore_ascii_case(origin))
            }),
        }
    }

    pub(crate) fn check_hello(&self, text: &str) -> Result<(), AuthFailure> {
        let Ok(ClientFrame::Hello { hello }) = serde_json::from_str::<ClientFrame>(text) else {
            return Err(AuthFailure::NotHello);
        };
        let Some(token) = hello.gateway_token else {
            return Err(AuthFailure::MissingToken);
        };
        if bool::from(self.token.as_bytes().ct_eq(token.as_bytes())) {
            Ok(())
        } else {
            Err(AuthFailure::WrongToken)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";

    fn auth() -> GatewayAuth {
        GatewayAuth::new(TOKEN, &["http://localhost:8081".into()])
    }

    fn hello(token: Option<&str>) -> String {
        serde_json::to_string(&pancetta_protocol::ClientFrame::hello(
            pancetta_protocol::Hello {
                protocol_version: 1,
                client_name: "test".into(),
                client_version: "0".into(),
                gateway_token: token.map(str::to_owned),
            },
        ))
        .unwrap()
    }

    #[test]
    fn origin_absent_is_allowed_for_native_clients() {
        assert!(auth().origin_allowed(None));
    }

    #[test]
    fn origin_must_match_allow_list_case_insensitively() {
        assert!(auth().origin_allowed(Some(&HeaderValue::from_static("HTTP://LocalHost:8081"))));
        assert!(!auth().origin_allowed(Some(&HeaderValue::from_static("http://localhost:8082"))));
    }

    #[test]
    fn null_and_unlisted_origins_are_refused() {
        for origin in ["null", "http://untrusted.example"] {
            assert!(!auth().origin_allowed(Some(&HeaderValue::from_static(origin))));
        }
        // Fail closed even if a caller bypassed config validation.
        assert!(!GatewayAuth::new(TOKEN, &["null".into()])
            .origin_allowed(Some(&HeaderValue::from_static("null"))));
        assert!(!auth().origin_allowed(Some(&HeaderValue::from_bytes(b"\xff").unwrap())));
    }

    #[test]
    fn empty_allow_list_refuses_every_browser_origin() {
        assert!(!GatewayAuth::new(TOKEN, &[])
            .origin_allowed(Some(&HeaderValue::from_static("http://localhost:8081"))));
    }

    #[test]
    fn hello_with_matching_token_is_accepted() {
        assert!(auth().check_hello(&hello(Some(TOKEN))).is_ok());
    }

    #[test]
    fn hello_with_wrong_or_missing_token_is_refused() {
        assert!(matches!(
            auth().check_hello(&hello(None)),
            Err(AuthFailure::MissingToken)
        ));
        for token in [
            "wrong".to_owned(),
            TOKEN[..TOKEN.len() - 1].into(),
            format!("{TOKEN}x"),
        ] {
            assert!(matches!(
                auth().check_hello(&hello(Some(&token))),
                Err(AuthFailure::WrongToken)
            ));
        }
    }

    #[test]
    fn first_frame_that_is_not_hello_is_refused() {
        for text in [
            r#"{"frame":"command","command":{"cmd":"stopCq"}}"#,
            "not json",
            "",
        ] {
            assert!(matches!(
                auth().check_hello(text),
                Err(AuthFailure::NotHello)
            ));
        }
    }
}
