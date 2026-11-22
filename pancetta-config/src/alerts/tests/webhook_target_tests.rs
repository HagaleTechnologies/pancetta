//! PAN-361: an enabled alert webhook must be an https URL on a public host
//! unless `alerts.webhook.allow_private_targets = true`, the per-minute cap
//! is bounded, and URL errors never echo the URL (it can carry secrets).

use super::*;
use crate::Config;

const LOOPBACK: &str = "names a loopback host";
const LINK_LOCAL: &str = "names a link-local host";
const PRIVATE: &str = "names a private-network host";
const UNSPECIFIED: &str = "names an unspecified or broadcast address";
const PLAINTEXT: &str = "is not an https URL";

fn webhook_config(url: &str, allow_private_targets: Option<bool>) -> Config {
    let opt_in = allow_private_targets
        .map(|allow| format!("allow_private_targets = {allow}\n"))
        .unwrap_or_default();
    toml::from_str(&format!(
        "[alerts.webhook]\nenabled = true\nurl = \"{url}\"\n{opt_in}"
    ))
    .unwrap()
}

fn validation_error(config: &Config) -> String {
    config
        .validate()
        .expect_err("config should fail validation")
        .to_string()
}

#[test]
fn link_local_metadata_target_needs_the_opt_in() {
    let refused = webhook_config("http://169.254.169.254/latest/", None);
    let error = validation_error(&refused);
    assert!(error.contains("alerts.webhook.url"), "{error}");
    assert!(error.contains("link-local"), "{error}");
    assert!(
        error.contains("alerts.webhook.allow_private_targets = true"),
        "{error}"
    );
    assert!(!error.contains("169.254"), "{error}");

    let explicit_false = webhook_config("http://169.254.169.254/latest/", Some(false));
    assert!(explicit_false.validate().is_err());

    let opted_in = webhook_config("http://169.254.169.254/latest/", Some(true));
    opted_in.validate().unwrap();
}

#[test]
fn restricted_targets_are_refused_with_their_reason() {
    let table: &[(&str, &str)] = &[
        ("https://127.0.0.1:8080/hook", LOOPBACK),
        ("https://localhost/hook", LOOPBACK),
        ("https://LOCALHOST./hook", LOOPBACK),
        ("https://app.localhost/hook", LOOPBACK),
        ("https://[::1]/hook", LOOPBACK),
        ("https://[::ffff:127.0.0.1]/hook", LOOPBACK),
        ("https://2130706433/hook", LOOPBACK),
        ("https://0x7f.1/hook", LOOPBACK),
        ("https://127.1/hook", LOOPBACK),
        ("https://169.254.169.254/hook", LINK_LOCAL),
        ("https://[fe80::1]/hook", LINK_LOCAL),
        ("https://[::ffff:169.254.169.254]/hook", LINK_LOCAL),
        ("https://[::169.254.169.254]/hook", LINK_LOCAL),
        ("https://10.0.0.5/hook", PRIVATE),
        ("https://172.16.0.1/hook", PRIVATE),
        ("https://192.168.1.5:8123/api/webhook/x", PRIVATE),
        ("https://100.64.0.1/hook", PRIVATE),
        ("https://[fd00::1]/hook", PRIVATE),
        ("https://[fec0::1]/hook", PRIVATE),
        ("https://homeassistant.local:8123/hook", PRIVATE),
        ("https://nas.home.arpa/hook", PRIVATE),
        ("https://metadata.google.internal/hook", PRIVATE),
        ("https://router/hook", PRIVATE),
        ("https://0.0.0.0/hook", UNSPECIFIED),
        ("https://0/hook", UNSPECIFIED),
        ("https://[::]/hook", UNSPECIFIED),
        ("https://255.255.255.255/hook", UNSPECIFIED),
        ("http://example.com/hook", PLAINTEXT),
    ];
    for (url, reason) in table {
        let refused = webhook_config(url, None);
        assert_eq!(
            refused.alerts.webhook.restricted_target(),
            Some(*reason),
            "{url}"
        );
        let error = validation_error(&refused);
        assert!(error.contains("alerts.webhook.url"), "{url}: {error}");
        assert!(error.contains(reason), "{url}: {error}");
        assert!(
            error.contains("alerts.webhook.allow_private_targets = true"),
            "{url}: {error}"
        );
        webhook_config(url, Some(true))
            .validate()
            .unwrap_or_else(|error| panic!("{url} with the opt-in: {error}"));
    }
}

#[test]
fn nat64_and_6to4_targets_are_judged_by_their_ipv4_address() {
    for (url, reason) in [
        ("https://[64:ff9b::a9fe:a9fe]/latest/", Some(LINK_LOCAL)),
        ("https://[64:ff9b::7f00:1]/hook", Some(LOOPBACK)),
        ("https://[64:ff9b::c0a8:105]/hook", Some(PRIVATE)),
        ("https://[64:ff9b:1::a00:5]/hook", Some(PRIVATE)),
        ("https://[2002:c0a8:105::]/hook", Some(PRIVATE)),
        ("https://[2002:a9fe:a9fe::1]/hook", Some(LINK_LOCAL)),
        ("https://[64:ff9b::808:808]/hook", None),
        ("https://[2002:808:808::1]/hook", None),
    ] {
        let config = webhook_config(url, None);
        assert_eq!(config.alerts.webhook.restricted_target(), reason, "{url}");
        assert_eq!(config.validate().is_ok(), reason.is_none(), "{url}");
        webhook_config(url, Some(true))
            .validate()
            .unwrap_or_else(|error| panic!("{url} with the opt-in: {error}"));
    }
}

#[test]
fn public_https_targets_need_no_opt_in() {
    for url in [
        "https://example.com/hook",
        "https://hooks.example.org:8443/x",
        "https://93.184.215.14/hook",
        "https://[2606:4700::1111]/hook",
        "https://[::ffff:8.8.8.8]/hook",
        "https://172.32.0.1/hook",
        "https://100.128.0.1/hook",
        "https://169.255.0.1/hook",
    ] {
        let config = webhook_config(url, None);
        assert_eq!(config.alerts.webhook.restricted_target(), None, "{url}");
        config
            .validate()
            .unwrap_or_else(|error| panic!("{url}: {error}"));
    }
}

#[test]
fn disabled_webhook_target_is_not_checked() {
    let config: Config =
        toml::from_str("[alerts.webhook]\nenabled = false\nurl = \"http://127.0.0.1/hook\"\n")
            .unwrap();
    assert!(!config.alerts.webhook.allow_private_targets);
    config.validate().unwrap();
}

#[test]
fn url_errors_never_echo_the_url() {
    let webhook: Config = toml::from_str(
        "[alerts.webhook]\nenabled = true\nurl = \"ftp://user:s3cret@example.com/hook\"\n",
    )
    .unwrap();
    let error = validation_error(&webhook);
    assert!(error.contains("alerts.webhook.url"), "{error}");
    assert!(!error.contains("s3cret"), "{error}");
    assert!(!error.contains("example.com"), "{error}");

    let ntfy: Config = toml::from_str(
        "[alerts.ntfy]\nenabled = true\ntopic = \"dx\"\nserver = \"ftp://user:s3cret@ntfy.example.com\"\n",
    )
    .unwrap();
    let error = validation_error(&ntfy);
    assert!(error.contains("alerts.ntfy.server"), "{error}");
    assert!(!error.contains("s3cret"), "{error}");
    assert!(!error.contains("example.com"), "{error}");

    let link_local = webhook_config("http://169.254.169.254/latest/", None);
    let error = validation_error(&link_local);
    assert!(!error.contains("http://169.254.169.254/latest/"), "{error}");
    assert!(!error.contains("/latest/"), "{error}");
}

#[test]
fn max_per_minute_defaults_to_ten_and_must_be_1_to_600() {
    let defaults = AlertsConfig::default();
    assert_eq!(defaults.webhook.max_per_minute, 10);
    assert!(!defaults.webhook.allow_private_targets);
    assert_eq!(WebhookAlertConfig::default().max_per_minute, 10);

    for (enabled, url) in [(false, ""), (true, "https://example.com/hook")] {
        for value in [0u32, 601] {
            let config: Config = toml::from_str(&format!(
                "[alerts.webhook]\nenabled = {enabled}\nurl = \"{url}\"\nmax_per_minute = {value}\n"
            ))
            .unwrap();
            let error = validation_error(&config);
            assert!(
                error.contains("alerts.webhook.max_per_minute"),
                "{value}: {error}"
            );
        }
        for value in [1u32, 600] {
            let config: Config = toml::from_str(&format!(
                "[alerts.webhook]\nenabled = {enabled}\nurl = \"{url}\"\nmax_per_minute = {value}\n"
            ))
            .unwrap();
            config
                .validate()
                .unwrap_or_else(|error| panic!("{value}: {error}"));
            assert_eq!(config.alerts.webhook.max_per_minute, value);
        }
    }
}

/// The webhook client applies the same address rules after DNS resolution,
/// so a public-looking name that resolves to a private address is refused.
#[test]
fn restricted_ip_judges_resolved_addresses_like_url_literals() {
    use std::net::IpAddr;
    let refused: &[(&str, &str)] = &[
        ("127.0.0.1", LOOPBACK),
        ("::1", LOOPBACK),
        ("::ffff:127.0.0.1", LOOPBACK),
        ("169.254.169.254", LINK_LOCAL),
        ("::ffff:169.254.169.254", LINK_LOCAL),
        ("::169.254.169.254", LINK_LOCAL),
        ("fe80::1", LINK_LOCAL),
        ("10.1.2.3", PRIVATE),
        ("172.16.0.1", PRIVATE),
        ("192.168.0.10", PRIVATE),
        ("100.64.0.1", PRIVATE),
        ("fd00::1", PRIVATE),
        ("fec0::1", PRIVATE),
        ("64:ff9b::a00:1", PRIVATE),
        ("2002:7f00:1::", LOOPBACK),
        ("0.0.0.0", UNSPECIFIED),
        ("::", UNSPECIFIED),
        ("255.255.255.255", UNSPECIFIED),
        ("224.0.0.1", UNSPECIFIED),
        ("240.0.0.1", UNSPECIFIED),
        ("ff02::1", UNSPECIFIED),
    ];
    for (ip, reason) in refused {
        let ip: IpAddr = ip.parse().unwrap();
        assert_eq!(restricted_ip(ip), Some(*reason), "{ip}");
    }
    for ip in [
        "93.184.215.14",
        "8.8.8.8",
        "172.32.0.1",
        "100.128.0.1",
        "2606:4700::1111",
        "::ffff:8.8.8.8",
    ] {
        let ip: IpAddr = ip.parse().unwrap();
        assert_eq!(restricted_ip(ip), None, "{ip}");
    }
}
