//! PAN-310: station-agent endpoints require encrypted transport outside loopback.
use pancetta_config::Config;
use std::sync::{Arc, Mutex};

fn config(enabled: bool) -> Config {
    let mut config = Config::default();
    config.network.station_agent.enabled = enabled;
    if enabled {
        config.network.station_agent.relay_url = Some("wss://service.example/agent".into());
        config.network.station_agent.pairing_api_url = Some("https://cqdx.io/api/v1".into());
        config.network.station_agent.tx_allow_list = vec!["test-client".into()];
    }
    config
}

fn diagnostic(field: &str, secure: &str, plain: &str) -> String {
    format!("network.station_agent.{field} must use {secure}://; {plain}:// is allowed only for loopback hosts")
}

#[test]
fn public_plaintext_pairing_is_rejected() {
    for enabled in [false, true] {
        let mut config = config(enabled);
        config.network.station_agent.pairing_api_url = Some("http://cqdx.io/api/v1".into());
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains(&diagnostic("pairing_api_url", "https", "http")),
            "{error}"
        );
    }
}

#[test]
fn public_plaintext_relay_is_rejected() {
    for enabled in [false, true] {
        let mut config = config(enabled);
        config.network.station_agent.relay_url = Some("ws://service.example/agent".into());
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains(&diagnostic("relay_url", "wss", "ws")),
            "{error}"
        );
    }
}

#[test]
fn transport_policy_matrix() {
    assert!(config(false).validate().is_ok());
    for enabled in [false, true] {
        for (field, secure, plain, wrong) in [
            ("relay_url", "wss", "ws", "https"),
            ("pairing_api_url", "https", "http", "wss"),
        ] {
            for (value, valid) in [
                (format!("{secure}://service.example/path"), true),
                (
                    format!("{}://service.example/path", secure.to_uppercase()),
                    true,
                ),
                (format!("{plain}://localhost:9000/path"), true),
                (format!("{plain}://LOCALHOST:9000/path"), true),
                (format!("{plain}://127.0.0.1:9000/path"), true),
                (format!("{plain}://127.0.0.2:9000/path"), true),
                (format!("{plain}://[::1]:9000/path"), true),
                (format!("{}://localhost/path", plain.to_uppercase()), true),
                (
                    format!("{}://service.example/path", plain.to_uppercase()),
                    false,
                ),
                (format!("{plain}://service.example/path"), false),
                (format!("{plain}://localhost.attacker.com/path"), false),
                (format!("{plain}://0.0.0.0/path"), false),
                (format!("{plain}://192.168.1.1/path"), false),
                (format!("{plain}://[2001:db8::1]/path"), false),
                (format!("{wrong}://localhost/path"), false),
                ("not a url".into(), false),
                ("file:///path".into(), false),
                (format!("{plain}://[bad"), false),
                (format!("{plain}://"), false),
                (format!(" {secure}://localhost/path"), false),
                (format!("{secure}://localhost/path "), false),
            ] {
                let mut config = config(enabled);
                let agent = &mut config.network.station_agent;
                if field == "relay_url" {
                    agent.relay_url = Some(value.clone());
                } else {
                    agent.pairing_api_url = Some(value.clone());
                }
                let result = config.validate();
                assert_eq!(
                    result.is_ok(),
                    valid,
                    "{field} {value:?} enabled={enabled}: {result:?}"
                );
                if !valid {
                    let error = result.unwrap_err().to_string();
                    assert!(
                        error.contains(&format!("network.station_agent.{field}")),
                        "{error}"
                    );
                    assert!(!error.contains(&value), "URL leaked in diagnostic: {error}");
                }
            }
        }
    }
}

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn loopback_warning_counts_and_messages() {
    for (relay, pairing, expected) in [
        (Some("ws://127.0.0.1:9000/agent"), None, vec!["relay_url"]),
        (
            None,
            Some("http://localhost/api/v1"),
            vec!["pairing_api_url"],
        ),
        (
            Some("ws://[::1]/agent"),
            Some("http://127.0.0.2/api/v1"),
            vec!["relay_url", "pairing_api_url"],
        ),
        (
            Some("wss://service.example/agent"),
            Some("https://cqdx.io/api/v1"),
            vec![],
        ),
    ] {
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let mut config = config(false);
        config.network.station_agent.relay_url = relay.map(str::to_owned);
        config.network.station_agent.pairing_api_url = pairing.map(str::to_owned);
        tracing::subscriber::with_default(subscriber, || config.validate().unwrap());
        let output = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        let events: Vec<_> = output
            .lines()
            .filter(|line| line.contains("unencrypted loopback link"))
            .collect();
        assert_eq!(events.len(), expected.len(), "{output}");
        for field in expected {
            let secure = if field == "relay_url" { "wss" } else { "https" };
            let message = format!("network.station_agent.{field} uses an unencrypted loopback link; use {secure}:// for encrypted transport");
            assert_eq!(
                events
                    .iter()
                    .filter(|line| line.contains(&message)
                        && line.contains("WARN")
                        && line.contains("config.station_agent"))
                    .count(),
                1,
                "{output}"
            );
        }
    }
}
