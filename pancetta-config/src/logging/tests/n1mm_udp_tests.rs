// PAN-382. The `[logging] n1mm_udp` key: parsing, validation, the off
// states and serialization.
use super::*;
use crate::Config;
use std::net::SocketAddr;

fn with_key(raw: &str) -> LoggingConfig {
    LoggingConfig {
        n1mm_udp: Some(raw.to_string()),
        ..LoggingConfig::default()
    }
}

#[test]
fn n1mm_udp_loads_without_warnings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "[logging]\nn1mm_udp = \"127.0.0.1:12060\"\n").expect("write config");
    let (cfg, warnings) = Config::load_from_file_with_warnings(&path).expect("config loads");
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    assert_eq!(
        cfg.logging.n1mm_udp_destination(),
        Some("127.0.0.1:12060".parse::<SocketAddr>().unwrap())
    );
}

#[test]
fn n1mm_udp_absent_or_blank_is_off() {
    for cfg in [LoggingConfig::default(), with_key(""), with_key("  ")] {
        assert!(cfg.validate_section().is_ok(), "{cfg:?} must validate");
        assert_eq!(cfg.n1mm_udp_destination(), None, "{cfg:?} must be off");
    }
}

#[test]
fn n1mm_udp_accepts_ipv6_and_broadcast_literals() {
    for raw in ["[::1]:12060", "192.168.1.255:12060"] {
        let cfg = with_key(raw);
        assert!(cfg.validate_section().is_ok(), "{raw} must validate");
        assert_eq!(
            cfg.n1mm_udp_destination(),
            Some(raw.parse::<SocketAddr>().unwrap()),
            "{raw}"
        );
    }
}

#[test]
fn n1mm_udp_rejects_a_hostname_or_bare_port() {
    for raw in ["localhost:12060", "12060", "127.0.0.1"] {
        match with_key(raw).validate_section() {
            Err(ConfigError::InvalidValue { field, value }) => {
                assert_eq!(field, "logging.n1mm_udp", "{raw}");
                assert!(
                    value.contains("must be an IP address and port"),
                    "{raw}: {value}"
                );
                assert!(value.starts_with(raw), "{raw}: {value}");
            }
            other => panic!("{raw}: expected InvalidValue, got {other:?}"),
        }
    }
}

#[test]
fn n1mm_udp_is_not_serialized_when_unset() {
    let value = serde_json::to_value(LoggingConfig::default()).expect("serialize");
    assert!(
        value.get("n1mm_udp").is_none(),
        "unset n1mm_udp must not serialize: {value}"
    );
}

#[test]
fn n1mm_udp_rejects_port_zero_and_unspecified_addresses() {
    for raw in ["127.0.0.1:0", "0.0.0.0:12060", "[::]:12060"] {
        match with_key(raw).validate_section() {
            Err(ConfigError::InvalidValue { field, value }) => {
                assert_eq!(field, "logging.n1mm_udp", "{raw}");
                assert!(
                    value.contains("must be an IP address and port"),
                    "{raw}: {value}"
                );
            }
            other => panic!("{raw}: expected InvalidValue, got {other:?}"),
        }
    }
}
