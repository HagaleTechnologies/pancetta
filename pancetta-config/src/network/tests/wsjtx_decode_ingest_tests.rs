//! PAN-381: `[network.wsjtx_udp] accept_decodes` / `decode_listen` defaults,
//! validation texts and partial-table loading.

use super::*;
use crate::Config;

fn accepting(listen: &str) -> WsjtxUdpConfig {
    WsjtxUdpConfig {
        accept_decodes: true,
        decode_listen: listen.to_string(),
        ..WsjtxUdpConfig::default()
    }
}

fn validation_text(cfg: &WsjtxUdpConfig) -> String {
    match cfg.validate_section() {
        Err(e) => e.to_string(),
        Ok(()) => panic!("expected {:?} to fail validation", cfg.decode_listen),
    }
}

#[test]
fn decode_ingest_defaults_are_off_on_loopback_2238() {
    let cfg = WsjtxUdpConfig::default();
    assert!(!cfg.accept_decodes);
    assert_eq!(cfg.decode_listen, "127.0.0.1:2238");
    // The default listen address differs from the default broadcast
    // destination, so turning ingest on with defaults validates.
    assert_ne!(cfg.decode_listen, cfg.destination);
    assert!(accepting("127.0.0.1:2238").validate_section().is_ok());
}

#[test]
fn decode_listen_must_be_loopback_when_accepting() {
    assert_eq!(
        validation_text(&accepting("0.0.0.0:2238")),
        "Validation error: network.wsjtx_udp.decode_listen 0.0.0.0:2238 is not a loopback address; the decode listener accepts only a same-host WSJT-X, so bind to 127.0.0.1 or [::1]"
    );
    assert_eq!(
        validation_text(&accepting("192.168.1.20:2238")),
        "Validation error: network.wsjtx_udp.decode_listen 192.168.1.20:2238 is not a loopback address; the decode listener accepts only a same-host WSJT-X, so bind to 127.0.0.1 or [::1]"
    );
    assert!(accepting("127.0.0.1:2238").validate_section().is_ok());
    assert!(accepting("[::1]:2238").validate_section().is_ok());
}

#[test]
fn decode_listen_rejects_port_zero_and_destination_collision() {
    assert_eq!(
        validation_text(&accepting("127.0.0.1:0")),
        "Validation error: network.wsjtx_udp.decode_listen needs a fixed port (got 0) so WSJT-X can be pointed at it"
    );
    let mut colliding = accepting("127.0.0.1:2237");
    colliding.destination = "127.0.0.1:2237".to_string();
    assert_eq!(
        validation_text(&colliding),
        "Validation error: network.wsjtx_udp.decode_listen must differ from network.wsjtx_udp.destination (127.0.0.1:2237), or pancetta would receive its own datagrams"
    );
    // A destination that does not parse (validated only when `enabled`) is
    // not compared.
    let mut odd_destination = accepting("127.0.0.1:2238");
    odd_destination.destination = "not an address".to_string();
    assert!(odd_destination.validate_section().is_ok());
}

#[test]
fn decode_listen_rejects_unparsable_address() {
    let cfg = accepting("localhost:2238");
    match cfg.validate_section() {
        Err(e @ ConfigError::InvalidValue { .. }) => {
            assert_eq!(
                e.to_string(),
                "Invalid configuration value: network.wsjtx_udp.decode_listen = localhost:2238"
            );
        }
        other => panic!("expected InvalidValue, got {other:?}"),
    }
}

#[test]
fn decode_listen_is_not_validated_when_ingest_is_off() {
    for listen in [
        "0.0.0.0:2238",
        "127.0.0.1:0",
        "127.0.0.1:2237",
        "localhost:2238",
        "",
    ] {
        let cfg = WsjtxUdpConfig {
            accept_decodes: false,
            decode_listen: listen.to_string(),
            ..WsjtxUdpConfig::default()
        };
        assert!(
            cfg.validate_section().is_ok(),
            "{listen:?} must pass while accept_decodes is off"
        );
    }
}

#[test]
fn partial_table_with_accept_decodes_only_loads_default_listen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pancetta.toml");
    std::fs::write(&path, "[network.wsjtx_udp]\naccept_decodes = true\n").unwrap();
    let config = Config::load_from_file(&path).unwrap_or_else(|e| panic!("load failed: {e}"));
    let udp = &config.network.wsjtx_udp;
    assert!(udp.accept_decodes);
    assert!(
        !udp.enabled,
        "the listener does not turn the companion broadcast on"
    );
    assert_eq!(udp.decode_listen, "127.0.0.1:2238");
    assert_eq!(udp.destination, "127.0.0.1:2237");
}
