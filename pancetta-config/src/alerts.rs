//! Opt-in decode alerts. Tier names follow the shared DX scorer vocabulary.
use crate::{ConfigError, ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DxTier {
    #[default]
    Atno,
    PerBandDxccNew,
    SpecialStation,
    PerBandGridNew,
    Standard,
    Suspect,
}

impl std::fmt::Display for DxTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Atno => "atno",
            Self::PerBandDxccNew => "per_band_dxcc_new",
            Self::SpecialStation => "special_station",
            Self::PerBandGridNew => "per_band_grid_new",
            Self::Standard => "standard",
            Self::Suspect => "suspect",
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BellAlertConfig {
    pub enabled: bool,
    pub min_tier: DxTier,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NtfyAlertConfig {
    pub enabled: bool,
    pub min_tier: DxTier,
    pub server: String,
    pub topic: String,
}
impl Default for NtfyAlertConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_tier: DxTier::Atno,
            server: "https://ntfy.sh".into(),
            topic: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebhookAlertConfig {
    pub enabled: bool,
    pub min_tier: DxTier,
    pub url: String,
    /// PAN-361: accept a plain `http://` URL, a loopback, private, link-local,
    /// CGNAT, unspecified or broadcast address, or a special-use local name
    /// (`localhost`, `.local`, `.home.arpa`, `.internal`, single-label) as the
    /// webhook target. Off by default, so a webhook cannot be pointed at the
    /// station's own network or a cloud metadata endpoint by accident.
    pub allow_private_targets: bool,
    /// PAN-361: webhook POSTs allowed in any rolling 60 s, summaries included
    /// (1 to 600). Alerts over the cap are held and sent as one summary.
    pub max_per_minute: u32,
}
impl Default for WebhookAlertConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_tier: DxTier::Atno,
            url: String::new(),
            allow_private_targets: false,
            max_per_minute: 10,
        }
    }
}

const TARGET_LOOPBACK: &str = "names a loopback host";
const TARGET_LINK_LOCAL: &str = "names a link-local host";
const TARGET_PRIVATE: &str = "names a private-network host";
const TARGET_UNSPECIFIED: &str = "names an unspecified or broadcast address";
const TARGET_PLAINTEXT: &str = "is not an https URL";
const TARGET_INVALID: &str = "is not a valid http or https URL";

impl WebhookAlertConfig {
    /// PAN-361: why `url` is not an https URL on a public host, or `None`
    /// when it is. The host is judged before the scheme, so
    /// `http://169.254.169.254/` reports link-local rather than plaintext.
    /// A URL that does not parse, is not http(s) or has no host returns
    /// `is not a valid http or https URL`, so callers fail closed.
    ///
    /// This judges the parsed URL only and never resolves a name: a public
    /// name that resolves to a private address passes. It does not read
    /// `allow_private_targets`; the validator and the alert dispatcher each
    /// apply the opt-in themselves.
    pub fn restricted_target(&self) -> Option<&'static str> {
        let Ok(url) = url::Url::parse(&self.url) else {
            return Some(TARGET_INVALID);
        };
        if !matches!(url.scheme(), "http" | "https") {
            return Some(TARGET_INVALID);
        }
        let host_class = match url.host() {
            None => return Some(TARGET_INVALID),
            Some(url::Host::Domain(name)) => domain_class(name),
            Some(url::Host::Ipv4(ip)) => ipv4_class(ip),
            Some(url::Host::Ipv6(ip)) => ipv6_class(ip),
        };
        host_class.or_else(|| (url.scheme() != "https").then_some(TARGET_PLAINTEXT))
    }
}

/// Special-use names that only ever resolve inside the operator's own
/// network: `localhost` (RFC 6761), `.local` (RFC 6762), `.home.arpa`
/// (RFC 8375), `.internal` (ICANN private use) and single-label names.
fn domain_class(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    let name = name.strip_suffix('.').unwrap_or(&name);
    let under = |suffix: &str| name == suffix || name.ends_with(&format!(".{suffix}"));
    if under("localhost") {
        Some(TARGET_LOOPBACK)
    } else if under("local") || under("home.arpa") || under("internal") || !name.contains('.') {
        Some(TARGET_PRIVATE)
    } else {
        None
    }
}

fn ipv4_class(ip: std::net::Ipv4Addr) -> Option<&'static str> {
    let [a, b, _, _] = ip.octets();
    if ip.is_loopback() {
        Some(TARGET_LOOPBACK)
    } else if ip.is_link_local() {
        Some(TARGET_LINK_LOCAL)
    } else if a == 0 || ip.is_broadcast() {
        Some(TARGET_UNSPECIFIED)
    } else if ip.is_private() || (a == 100 && (b & 0xc0) == 64) {
        // RFC 1918 and RFC 6598 carrier-grade NAT (100.64.0.0/10).
        Some(TARGET_PRIVATE)
    } else {
        None
    }
}

fn ipv6_class(ip: std::net::Ipv6Addr) -> Option<&'static str> {
    let segments = ip.segments();
    let first = segments[0];
    let embedded =
        |high: u16, low: u16| std::net::Ipv4Addr::from((u32::from(high) << 16) | u32::from(low));
    if ip.is_loopback() {
        Some(TARGET_LOOPBACK)
    } else if ip.is_unspecified() {
        Some(TARGET_UNSPECIFIED)
    } else if let Some(v4) = ip.to_ipv4() {
        // `::ffff:a.b.c.d` (mapped) and `::a.b.c.d` (compatible) carry an
        // IPv4 target; `to_canonical()` would miss the compatible form.
        ipv4_class(v4)
    } else if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        // NAT64 well-known prefix 64:ff9b::/96 (RFC 6052): a translator
        // forwards to the IPv4 address in the low 32 bits.
        ipv4_class(embedded(segments[6], segments[7]))
    } else if segments[..3] == [0x64, 0xff9b, 0x1] {
        // Local-use NAT64 prefix 64:ff9b:1::/48 (RFC 8215).
        Some(TARGET_PRIVATE)
    } else if first == 0x2002 {
        // 6to4 2002::/16 (RFC 3056) carries its IPv4 relay target in bits 16-47.
        ipv4_class(embedded(segments[1], segments[2]))
    } else if (first & 0xffc0) == 0xfe80 {
        Some(TARGET_LINK_LOCAL)
    } else if (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfec0 {
        // Unique-local fc00::/7 and deprecated site-local fec0::/10.
        Some(TARGET_PRIVATE)
    } else {
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertsConfig {
    pub dedupe_secs: u64,
    pub bell: BellAlertConfig,
    pub ntfy: NtfyAlertConfig,
    pub webhook: WebhookAlertConfig,
}
impl Default for AlertsConfig {
    fn default() -> Self {
        Self {
            dedupe_secs: 300,
            bell: BellAlertConfig::default(),
            ntfy: NtfyAlertConfig::default(),
            webhook: WebhookAlertConfig::default(),
        }
    }
}
impl ConfigSection for AlertsConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        if self.dedupe_secs == 0 {
            return Err(ConfigError::InvalidValue {
                field: "alerts.dedupe_secs".into(),
                value: "0".into(),
            });
        }
        if !(1..=600).contains(&self.webhook.max_per_minute) {
            return Err(ConfigError::Validation(format!(
                "alerts.webhook.max_per_minute must be between 1 and 600, got {}",
                self.webhook.max_per_minute
            )));
        }
        if self.ntfy.enabled && self.ntfy.topic.trim().is_empty() {
            return Err(ConfigError::MissingRequired("alerts.ntfy.topic".into()));
        }
        for (enabled, value, field) in [
            (
                self.webhook.enabled,
                &self.webhook.url,
                "alerts.webhook.url",
            ),
            (self.ntfy.enabled, &self.ntfy.server, "alerts.ntfy.server"),
        ] {
            if enabled
                && !url::Url::parse(value).is_ok_and(|url| {
                    matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
                })
            {
                // PAN-361: never echo the URL; webhook and ntfy URLs can
                // carry credentials.
                return Err(ConfigError::Validation(format!(
                    "{field} must be an http or https URL with a host"
                )));
            }
        }
        if self.webhook.enabled && !self.webhook.allow_private_targets {
            if let Some(reason) = self.webhook.restricted_target() {
                return Err(ConfigError::Validation(format!(
                    "alerts.webhook.url {reason}; use an https URL on a public host, or set alerts.webhook.allow_private_targets = true to accept it"
                )));
            }
        }
        Ok(())
    }
    fn merge_with(&mut self, other: Self) {
        *self = other;
    }
}

#[cfg(test)]
#[path = "alerts/tests/webhook_target_tests.rs"]
mod webhook_target_tests;
