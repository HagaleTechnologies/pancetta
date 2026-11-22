//! PAN-363: `{:?}` of the network config never prints a secret. The cqdx
//! token and every value in a custom service's maps print `<redacted>`;
//! the legacy `[network.proxy]` / `[network.tls]` secrets are dropped at load
//! (PAN-303) and so never reach Debug output either.

use super::*;
use crate::Config;

const CQDX_TOKEN: &str = "pat_CQDX_TOKEN_SENTINEL";
const PROXY_PASSWORD: &str = "PROXY_PASSWORD_SENTINEL";
const KEY_PASSWORD: &str = "CLIENT_CERT_KEY_PASSWORD_SENTINEL";

#[test]
fn legacy_proxy_and_client_cert_secrets_never_reach_network_debug() {
    let toml_text = format!(
        r#"
[station]
callsign = "W1AW"
grid_square = "FN31"

[network.cqdx]
token = "{CQDX_TOKEN}"

[network.proxy]
enabled = true
url = "http://proxy.example:3128"

[network.proxy.auth]
username = "proxyuser"
password = "{PROXY_PASSWORD}"

[network.tls]
verify_certificates = true

[network.tls.client_cert]
cert_path = "/tmp/client.pem"
key_path = "/tmp/client.key"
key_password = "{KEY_PASSWORD}"
"#
    );
    // Same call `ConfigLoader::parse_toml` makes after tilde expansion.
    let config: Config = toml::from_str(&toml_text).expect("legacy config parses");
    assert_eq!(
        config.network.cqdx.token.as_deref(),
        Some(CQDX_TOKEN),
        "the cqdx token must still load; only its Debug output is redacted"
    );

    let compact = format!("{:?}", config.network);
    let pretty = format!("{:#?}", config.network);
    let whole = format!("{config:?}");
    for (label, out) in [
        ("network {:?}", &compact),
        ("network {:#?}", &pretty),
        ("config {:?}", &whole),
    ] {
        for secret in [CQDX_TOKEN, PROXY_PASSWORD, KEY_PASSWORD] {
            assert!(!out.contains(secret), "{label} leaked {secret}: {out}");
        }
    }
    assert!(
        compact.contains(r#"token: Some("<redacted>")"#),
        "cqdx token must print as Some(\"<redacted>\"): {compact}"
    );
}

#[test]
fn custom_service_map_values_are_redacted_keys_kept() {
    let auth = CustomAuthConfig {
        auth_type: "basic".to_string(),
        parameters: HashMap::from([(
            "client_secret".to_string(),
            "AUTH_PARAM_SENTINEL".to_string(),
        )]),
        credentials: HashMap::from([(
            "password".to_string(),
            "AUTH_CREDENTIAL_SENTINEL".to_string(),
        )]),
    };
    let service = CustomServiceConfig {
        name: "logger".to_string(),
        url: "https://logger.example/api".to_string(),
        service_type: "rest".to_string(),
        auth: Some(auth),
        headers: HashMap::from([(
            "Authorization".to_string(),
            "Bearer HEADER_SENTINEL".to_string(),
        )]),
        parameters: HashMap::from([(
            "api_key".to_string(),
            serde_json::Value::String("SERVICE_PARAM_SENTINEL".to_string()),
        )]),
        enabled: true,
    };
    let network = NetworkConfig {
        custom_services: HashMap::from([("logger".to_string(), service)]),
        ..Default::default()
    };

    let out = format!("{network:?}");
    for secret in [
        "AUTH_PARAM_SENTINEL",
        "AUTH_CREDENTIAL_SENTINEL",
        "HEADER_SENTINEL",
        "SERVICE_PARAM_SENTINEL",
    ] {
        assert!(
            !out.contains(secret),
            "custom service leaked {secret}: {out}"
        );
    }
    for redacted_entry in [
        r#""password": "<redacted>""#,
        r#""client_secret": "<redacted>""#,
        r#""Authorization": "<redacted>""#,
        r#""api_key": "<redacted>""#,
    ] {
        assert!(
            out.contains(redacted_entry),
            "missing {redacted_entry}: {out}"
        );
    }
    for visible in [
        r#"name: "logger""#,
        r#"url: "https://logger.example/api""#,
        r#"service_type: "rest""#,
        r#"auth_type: "basic""#,
        "enabled: true",
    ] {
        assert!(out.contains(visible), "missing {visible}: {out}");
    }
}

#[test]
fn cqdx_debug_shows_non_secret_fields_and_unset_token() {
    let out = format!("{:?}", CqdxConfig::default());
    for expected in [
        "token: None",
        r#"base_url: "https://cqdx.io""#,
        "poll_interval_secs: 30",
        "authorizations_poll_interval_secs: 45",
    ] {
        assert!(out.contains(expected), "missing {expected}: {out}");
    }
}
