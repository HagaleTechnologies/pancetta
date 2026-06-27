//! Network services configuration module
//!
//! This module handles configuration for external network services including
//! PSKReporter, DX cluster, cqdx.io, and other amateur radio web services.

use crate::{ConfigError, ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Network services configuration
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
// Container-level serde default: omitted fields fall back to defaults rather
// than failing to deserialize a partial config.
#[serde(default)]
pub struct NetworkConfig {
    /// PSKReporter service configuration
    pub psk_reporter: PskReporterConfig,

    /// WSPR configuration
    pub wspr: WsprConfig,

    /// DX cluster configuration
    pub dx_cluster: DxClusterConfig,

    /// Web API configuration
    pub web_api: WebApiConfig,

    /// Proxy settings
    pub proxy: ProxyConfig,

    /// SSL/TLS settings
    pub tls: TlsConfig,

    /// Rate limiting configuration
    pub rate_limiting: RateLimitingConfig,

    /// Retry and timeout settings
    pub reliability: ReliabilityConfig,

    /// cqdx.io integration configuration
    pub cqdx: CqdxConfig,

    /// ClubLog real-time QSO upload configuration (opt-in; default disabled)
    #[serde(default)]
    pub clublog: ClubLogConfig,

    /// QRZ Logbook QSO upload configuration (opt-in; default disabled)
    #[serde(default)]
    pub qrz_logbook: QrzLogbookConfig,

    /// LoTW (TQSL-signed) QSO upload configuration (opt-in; default disabled)
    #[serde(default)]
    pub lotw: LotwUploadConfig,

    /// eQSL.cc QSO upload configuration (opt-in; default disabled)
    #[serde(default)]
    pub eqsl: EqslConfig,

    /// QRZ.com paid XML callsign-lookup configuration (opt-in; default disabled)
    #[serde(default)]
    pub qrz_xml: QrzXmlConfig,

    /// Read-only remote view gateway for the Panino client (opt-in; default disabled)
    #[serde(default)]
    pub remote_gateway: RemoteGatewayConfig,

    /// Custom service integrations
    #[serde(default)]
    pub custom_services: HashMap<String, CustomServiceConfig>,
}

/// ClubLog real-time QSO upload configuration.
///
/// When [`enabled`](Self::enabled) is `true`, each completed QSO is POSTed as
/// a single ADIF record to ClubLog's `realtime.php` endpoint. All credentials
/// stay on the operator's machine (keep the config file `chmod 600`) and are
/// never logged.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct ClubLogConfig {
    /// Enable per-QSO uploads to ClubLog.
    #[serde(default)]
    pub enabled: bool,

    /// Registered ClubLog account email address (NOT a callsign).
    #[serde(default)]
    pub email: String,

    /// ClubLog account password (an Application Password is recommended).
    #[serde(default)]
    pub password: String,

    /// Station callsign the log is uploaded into. When empty, the caller
    /// falls back to the QSO's own (`our_callsign`) value.
    #[serde(default)]
    pub callsign: String,

    /// ClubLog application API key (per-application, from the ClubLog API page).
    #[serde(default)]
    pub api_key: String,
}

/// QRZ Logbook QSO upload configuration.
///
/// When [`enabled`](Self::enabled) is `true`, each completed QSO is POSTed as
/// a single ADIF record to the QRZ Logbook API (`logbook.qrz.com/api`). The
/// API key is per-logbook (from the logbook's Settings page) and is never
/// logged. Keep the config file `chmod 600`.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct QrzLogbookConfig {
    /// Enable per-QSO uploads to QRZ Logbook.
    #[serde(default)]
    pub enabled: bool,

    /// Per-logbook API access key (from the QRZ logbook Settings page).
    #[serde(default)]
    pub api_key: String,
}

/// LoTW (Logbook of the World) per-QSO upload configuration.
///
/// LoTW requires every record to be digitally signed with the operator's TQSL
/// certificate, so pancetta shells out to the operator's installed `tqsl` CLI
/// rather than raw-POSTing ADIF. When [`enabled`](Self::enabled) is `true`,
/// each completed QSO is signed + uploaded via `tqsl`. No credential value is
/// ever logged; the certificate lives in the operator's TQSL install.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct LotwUploadConfig {
    /// Enable per-QSO signed uploads to LoTW.
    #[serde(default)]
    pub enabled: bool,

    /// Path to the operator's `tqsl` binary (e.g. `/usr/bin/tqsl`,
    /// `C:\\Program Files (x86)\\TrustedQSL\\tqsl.exe`).
    #[serde(default)]
    pub tqsl_path: String,

    /// The TQSL "Station Location" name the operator configured in TQSL. TQSL
    /// uses this to select the certificate + station details for signing.
    #[serde(default)]
    pub station_location: String,
}

/// eQSL.cc per-QSO upload configuration.
///
/// When [`enabled`](Self::enabled) is `true`, each completed QSO is POSTed as
/// a single ADIF record to eQSL.cc's `importADIF.cfm` endpoint. The account
/// credentials stay on the operator's machine (keep the config file
/// `chmod 600`) and are never logged.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct EqslConfig {
    /// Enable per-QSO uploads to eQSL.cc.
    #[serde(default)]
    pub enabled: bool,

    /// eQSL.cc account username.
    #[serde(default)]
    pub username: String,

    /// eQSL.cc account password.
    #[serde(default)]
    pub password: String,

    /// Optional QTH nickname (eQSL "Profile" name) when the account has more
    /// than one location configured. Left empty for single-location accounts.
    #[serde(default)]
    pub qth_nickname: String,
}

/// QRZ.com paid XML callsign-lookup configuration.
///
/// When [`enabled`](Self::enabled) is `true`, pancetta may use the operator's
/// paid QRZ XML subscription to look up callsign metadata (name, grid, DXCC,
/// country, state) for read-side enrichment. This is a **credentialed,
/// per-operator paid subscription** that cannot be proxied through cqdx.io, so
/// the credentials stay on the operator's machine (keep the config file
/// `chmod 600`) and are never logged.
///
/// The client ([`QrzXmlClient`](../../pancetta_dx/struct.QrzXmlClient.html)) is
/// a scaffold: it is not yet wired into the decode/priority hot path (a later
/// operator decision).
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct QrzXmlConfig {
    /// Enable QRZ XML callsign lookups.
    #[serde(default)]
    pub enabled: bool,

    /// QRZ.com account username (callsign or login).
    #[serde(default)]
    pub username: String,

    /// QRZ.com account password.
    #[serde(default)]
    pub password: String,
}

/// Read-only remote view gateway (Panino client). Default OFF; localhost-bound.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteGatewayConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Bind address. Defaults to localhost only (no network exposure) until the
    /// authenticated control path (Sub-plan C) exists.
    #[serde(default = "default_gateway_bind")]
    pub bind_addr: String,
}

fn default_gateway_bind() -> String {
    "127.0.0.1:4080".to_string()
}

impl Default for RemoteGatewayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_addr: default_gateway_bind(),
        }
    }
}

/// PSKReporter service configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PskReporterConfig {
    /// Enable PSKReporter uploads
    pub enabled: bool,

    /// PSKReporter server URL
    pub server_url: String,

    /// Upload interval in seconds
    pub upload_interval_seconds: u64,

    /// Batch size for uploads
    pub batch_size: u32,

    /// Include receive reports
    pub include_receives: bool,

    /// Include transmit reports
    pub include_transmits: bool,

    /// Minimum signal-to-noise ratio for reports
    pub min_snr_db: f32,

    /// Maximum age of reports to upload (hours)
    pub max_age_hours: u32,

    /// Reporter identification
    pub reporter_info: ReporterInfo,

    /// Frequency accuracy in Hz
    pub frequency_accuracy_hz: u32,

    /// Filter settings
    pub filters: PskReporterFilters,
}

/// Reporter identification information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReporterInfo {
    /// Software name
    pub software_name: String,

    /// Software version
    pub software_version: String,

    /// Antenna information
    pub antenna_info: Option<String>,

    /// Additional comments
    pub comments: Option<String>,
}

/// PSKReporter filtering configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PskReporterFilters {
    /// Enabled modes for reporting
    pub enabled_modes: Vec<String>,

    /// Enabled bands for reporting
    pub enabled_bands: Vec<String>,

    /// Minimum frequency in Hz
    pub min_frequency: Option<u64>,

    /// Maximum frequency in Hz
    pub max_frequency: Option<u64>,

    /// Geographic filters
    pub geographic: GeographicFilters,
}

/// Geographic filtering configuration
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct GeographicFilters {
    /// Include specific DXCC entities
    pub include_dxcc: Vec<u16>,

    /// Exclude specific DXCC entities
    pub exclude_dxcc: Vec<u16>,

    /// Include specific ITU zones
    pub include_itu_zones: Vec<u8>,

    /// Exclude specific ITU zones
    pub exclude_itu_zones: Vec<u8>,

    /// Include specific CQ zones
    pub include_cq_zones: Vec<u8>,

    /// Exclude specific CQ zones
    pub exclude_cq_zones: Vec<u8>,

    /// Distance filters
    pub distance: DistanceFilters,
}

/// Distance-based filtering
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistanceFilters {
    /// Minimum distance in kilometers
    pub min_distance_km: Option<f64>,

    /// Maximum distance in kilometers
    pub max_distance_km: Option<f64>,

    /// Use great circle distance calculation
    pub great_circle: bool,
}

/// WSPR (Weak Signal Propagation Reporter) configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsprConfig {
    /// Enable WSPR integration
    pub enabled: bool,

    /// WSPR database URL
    pub database_url: String,

    /// Upload spots
    pub upload_spots: bool,

    /// Download spots
    pub download_spots: bool,

    /// Spot filtering
    pub filtering: WsprFilteringConfig,

    /// Analysis settings
    pub analysis: WsprAnalysisConfig,
}

/// WSPR filtering configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsprFilteringConfig {
    /// Minimum SNR
    pub min_snr: f32,

    /// Maximum drift
    pub max_drift_hz: f32,

    /// Band filters
    pub bands: Vec<String>,

    /// Geographic filters
    pub geographic: GeographicFilters,

    /// Time window in hours
    pub time_window_hours: u32,
}

/// WSPR analysis configuration
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct WsprAnalysisConfig {
    /// Propagation analysis
    pub propagation_analysis: bool,

    /// Band comparison
    pub band_comparison: bool,

    /// Antenna pattern analysis
    pub antenna_analysis: bool,

    /// Export analysis data
    pub export_analysis: bool,
}

/// DX Cluster configuration
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DxClusterConfig {
    /// Enable DX cluster connection
    pub enabled: bool,

    /// Cluster servers
    pub servers: Vec<ClusterServer>,

    /// Connection settings
    pub connection: ClusterConnectionConfig,

    /// Filtering settings
    pub filtering: ClusterFilteringConfig,

    /// Alert settings
    pub alerts: ClusterAlertConfig,
}

/// DX cluster server definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterServer {
    /// Server name
    pub name: String,

    /// Server hostname
    pub hostname: String,

    /// Server port
    pub port: u16,

    /// Server type
    pub server_type: ClusterType,

    /// Authentication required
    pub auth_required: bool,

    /// Username
    pub username: Option<String>,

    /// Server priority
    pub priority: u8,
}

/// DX cluster types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterType {
    DxSpider,
    CC,
    ArCluster,
    Packet,
    Websocket,
}

/// Cluster connection configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterConnectionConfig {
    /// Auto-connect on startup
    pub auto_connect: bool,

    /// Reconnection attempts
    pub reconnect_attempts: u32,

    /// Reconnection delay in seconds
    pub reconnect_delay_seconds: u64,

    /// Keep-alive interval
    pub keepalive_interval_seconds: u64,

    /// Connection timeout
    pub timeout_seconds: u32,
}

/// Cluster filtering configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterFilteringConfig {
    /// Band filters
    pub bands: Vec<String>,

    /// Mode filters
    pub modes: Vec<String>,

    /// DXCC filters
    pub dxcc_filters: Vec<u16>,

    /// Minimum frequency
    pub min_frequency: Option<u64>,

    /// Maximum frequency
    pub max_frequency: Option<u64>,

    /// Duplicate filtering
    pub duplicate_window_minutes: u32,
}

/// Cluster alert configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterAlertConfig {
    /// Enable visual alerts
    pub visual_alerts: bool,

    /// Enable audio alerts
    pub audio_alerts: bool,

    /// Alert conditions
    pub conditions: Vec<AlertCondition>,

    /// Alert sounds
    pub sounds: AlertSoundConfig,
}

/// Alert condition definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertCondition {
    /// Condition name
    pub name: String,

    /// Condition expression
    pub expression: String,

    /// Alert priority
    pub priority: AlertPriority,

    /// Condition enabled
    pub enabled: bool,
}

/// Alert priority levels
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertPriority {
    Low,
    Medium,
    High,
    Critical,
}

/// Alert sound configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertSoundConfig {
    /// Sound file for low priority
    pub low_priority_sound: Option<String>,

    /// Sound file for medium priority
    pub medium_priority_sound: Option<String>,

    /// Sound file for high priority
    pub high_priority_sound: Option<String>,

    /// Sound file for critical priority
    pub critical_priority_sound: Option<String>,

    /// Alert volume
    pub volume: f32,
}

/// Web API configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebApiConfig {
    /// Enable web API server
    pub enabled: bool,

    /// API server bind address
    pub bind_address: String,

    /// API server port
    pub port: u16,

    /// API authentication
    pub authentication: ApiAuthConfig,

    /// CORS settings
    pub cors: CorsConfig,

    /// Rate limiting
    pub rate_limiting: ApiRateLimitingConfig,

    /// API documentation
    pub documentation: ApiDocumentationConfig,
}

/// API authentication configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiAuthConfig {
    /// Authentication required
    pub required: bool,

    /// Authentication method
    pub method: AuthMethod,

    /// API keys
    pub api_keys: Vec<ApiKey>,

    /// JWT settings
    pub jwt: JwtConfig,
}

/// Authentication methods
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    None,
    ApiKey,
    BasicAuth,
    Jwt,
    OAuth2,
}

/// API key definition
#[derive(Clone, Serialize, Deserialize)]
pub struct ApiKey {
    /// Key name/identifier
    pub name: String,

    /// API key value (encrypted)
    pub key_encrypted: String,

    /// Key permissions
    pub permissions: Vec<String>,

    /// Key expiration
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,

    /// Key enabled
    pub enabled: bool,
}

/// JWT configuration
#[derive(Clone, Serialize, Deserialize)]
pub struct JwtConfig {
    /// JWT secret key (encrypted)
    pub secret_encrypted: String,

    /// Token expiration time in hours
    pub expiration_hours: u32,

    /// Token issuer
    pub issuer: String,

    /// Token audience
    pub audience: String,
}

/// CORS configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorsConfig {
    /// Enable CORS
    pub enabled: bool,

    /// Allowed origins
    pub allowed_origins: Vec<String>,

    /// Allowed methods
    pub allowed_methods: Vec<String>,

    /// Allowed headers
    pub allowed_headers: Vec<String>,

    /// Allow credentials
    pub allow_credentials: bool,

    /// Max age in seconds
    pub max_age_seconds: u32,
}

/// API rate limiting configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiRateLimitingConfig {
    /// Enable rate limiting
    pub enabled: bool,

    /// Requests per minute
    pub requests_per_minute: u32,

    /// Burst allowance
    pub burst_allowance: u32,

    /// Rate limit by IP
    pub by_ip: bool,

    /// Rate limit by API key
    pub by_api_key: bool,
}

/// API documentation configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiDocumentationConfig {
    /// Enable documentation endpoint
    pub enabled: bool,

    /// Documentation path
    pub path: String,

    /// Documentation title
    pub title: String,

    /// Documentation description
    pub description: String,

    /// API version
    pub version: String,
}

/// Proxy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// Enable proxy
    pub enabled: bool,

    /// Proxy type
    pub proxy_type: ProxyType,

    /// Proxy server
    pub server: String,

    /// Proxy port
    pub port: u16,

    /// Proxy authentication
    pub auth: Option<ProxyAuth>,

    /// Proxy exclusions
    pub exclusions: Vec<String>,
}

/// Proxy types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyType {
    Http,
    Https,
    Socks4,
    Socks5,
}

/// Proxy authentication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyAuth {
    /// Username
    pub username: String,

    /// Proxy password (plaintext on disk — see SECURITY.md).
    pub password: String,
}

/// TLS/SSL configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Verify certificates
    pub verify_certificates: bool,

    /// Certificate bundle path
    pub ca_bundle_path: Option<String>,

    /// Client certificate
    pub client_cert: Option<ClientCertConfig>,

    /// TLS version
    pub min_version: TlsVersion,

    /// Cipher suites
    pub cipher_suites: Vec<String>,
}

/// TLS version options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsVersion {
    Tls10,
    Tls11,
    Tls12,
    Tls13,
}

/// Client certificate configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientCertConfig {
    /// Certificate file path
    pub cert_file: String,

    /// Private key file path
    pub key_file: String,

    /// Private key password (plaintext on disk — see SECURITY.md).
    pub key_password: Option<String>,
}

/// Rate limiting configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitingConfig {
    /// Enable rate limiting
    pub enabled: bool,

    /// Global rate limits
    pub global: GlobalRateLimit,

    /// Service-specific rate limits
    pub services: HashMap<String, ServiceRateLimit>,
}

/// Global rate limiting
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalRateLimit {
    /// Requests per minute
    pub requests_per_minute: u32,

    /// Requests per hour
    pub requests_per_hour: u32,

    /// Requests per day
    pub requests_per_day: u32,
}

/// Service-specific rate limiting
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceRateLimit {
    /// Requests per minute
    pub requests_per_minute: u32,

    /// Burst allowance
    pub burst_allowance: u32,

    /// Cooldown period in seconds
    pub cooldown_seconds: u64,
}

/// Reliability configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReliabilityConfig {
    /// Connection timeout in seconds
    pub connection_timeout_seconds: u32,

    /// Request timeout in seconds
    pub request_timeout_seconds: u32,

    /// Retry configuration
    pub retry: RetryConfig,

    /// Circuit breaker settings
    pub circuit_breaker: CircuitBreakerConfig,

    /// Health check settings
    pub health_check: HealthCheckConfig,
}

/// Retry configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Maximum retry attempts
    pub max_attempts: u32,

    /// Base delay in milliseconds
    pub base_delay_ms: u64,

    /// Maximum delay in milliseconds
    pub max_delay_ms: u64,

    /// Exponential backoff multiplier
    pub backoff_multiplier: f32,

    /// Jitter factor
    pub jitter_factor: f32,
}

/// Circuit breaker configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    /// Enable circuit breaker
    pub enabled: bool,

    /// Failure threshold
    pub failure_threshold: u32,

    /// Success threshold
    pub success_threshold: u32,

    /// Timeout in milliseconds
    pub timeout_ms: u64,
}

/// Health check configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckConfig {
    /// Enable health checks
    pub enabled: bool,

    /// Check interval in seconds
    pub interval_seconds: u64,

    /// Health check timeout
    pub timeout_seconds: u32,

    /// Unhealthy threshold
    pub unhealthy_threshold: u32,
}

/// Custom service configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomServiceConfig {
    /// Service name
    pub name: String,

    /// Service URL
    pub url: String,

    /// Service type
    pub service_type: String,

    /// Authentication configuration
    pub auth: Option<CustomAuthConfig>,

    /// Custom headers
    pub headers: HashMap<String, String>,

    /// Custom parameters
    pub parameters: HashMap<String, serde_json::Value>,

    /// Service enabled
    pub enabled: bool,
}

/// Custom authentication configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomAuthConfig {
    /// Authentication type
    pub auth_type: String,

    /// Authentication parameters
    pub parameters: HashMap<String, String>,

    /// Encrypted credentials
    pub credentials: HashMap<String, String>,
}

impl Default for PskReporterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server_url: "https://pskreporter.info/cgi-bin/pskdata.pl".to_string(),
            upload_interval_seconds: 300,
            batch_size: 50,
            include_receives: true,
            include_transmits: true,
            min_snr_db: -20.0,
            max_age_hours: 24,
            reporter_info: ReporterInfo {
                software_name: "Pancetta".to_string(),
                software_version: env!("CARGO_PKG_VERSION").to_string(),
                antenna_info: None,
                comments: None,
            },
            frequency_accuracy_hz: 1,
            filters: PskReporterFilters::default(),
        }
    }
}

impl Default for PskReporterFilters {
    fn default() -> Self {
        Self {
            enabled_modes: vec![
                "PSK31".to_string(),
                "PSK63".to_string(),
                "FT8".to_string(),
                "FT4".to_string(),
                "JS8".to_string(),
            ],
            enabled_bands: vec![
                "40m".to_string(),
                "30m".to_string(),
                "20m".to_string(),
                "17m".to_string(),
                "15m".to_string(),
                "12m".to_string(),
                "10m".to_string(),
                "6m".to_string(),
            ],
            min_frequency: None,
            max_frequency: None,
            geographic: GeographicFilters::default(),
        }
    }
}

impl Default for DistanceFilters {
    fn default() -> Self {
        Self {
            min_distance_km: None,
            max_distance_km: None,
            great_circle: true,
        }
    }
}

/// cqdx.io integration configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CqdxConfig {
    /// Enable cqdx.io integration
    pub enabled: bool,

    /// cqdx.io base URL
    pub base_url: String,

    /// Personal Access Token for authentication
    pub token: Option<String>,

    /// Priority spot poll interval in seconds
    pub poll_interval_secs: u64,
}

impl Default for CqdxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "https://cqdx.io".to_string(),
            token: None,
            poll_interval_secs: 30,
        }
    }
}

impl Default for WsprConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            database_url: "http://wsprnet.org/drupal/wsprnet/spotquery".to_string(),
            upload_spots: false,
            download_spots: false,
            filtering: WsprFilteringConfig::default(),
            analysis: WsprAnalysisConfig::default(),
        }
    }
}

impl Default for WsprFilteringConfig {
    fn default() -> Self {
        Self {
            min_snr: -30.0,
            max_drift_hz: 3.0,
            bands: vec!["40m".to_string(), "30m".to_string(), "20m".to_string()],
            geographic: GeographicFilters::default(),
            time_window_hours: 24,
        }
    }
}

impl Default for ClusterConnectionConfig {
    fn default() -> Self {
        Self {
            auto_connect: false,
            reconnect_attempts: 3,
            reconnect_delay_seconds: 30,
            keepalive_interval_seconds: 300,
            timeout_seconds: 30,
        }
    }
}

impl Default for ClusterFilteringConfig {
    fn default() -> Self {
        Self {
            bands: vec![],
            modes: vec![],
            dxcc_filters: vec![],
            min_frequency: None,
            max_frequency: None,
            duplicate_window_minutes: 10,
        }
    }
}

impl Default for ClusterAlertConfig {
    fn default() -> Self {
        Self {
            visual_alerts: true,
            audio_alerts: false,
            conditions: vec![],
            sounds: AlertSoundConfig::default(),
        }
    }
}

impl Default for AlertSoundConfig {
    fn default() -> Self {
        Self {
            low_priority_sound: None,
            medium_priority_sound: None,
            high_priority_sound: None,
            critical_priority_sound: None,
            volume: 0.5,
        }
    }
}

impl Default for WebApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_address: "127.0.0.1".to_string(),
            port: 8080,
            authentication: ApiAuthConfig::default(),
            cors: CorsConfig::default(),
            rate_limiting: ApiRateLimitingConfig::default(),
            documentation: ApiDocumentationConfig::default(),
        }
    }
}

impl Default for ApiAuthConfig {
    fn default() -> Self {
        Self {
            required: true,
            method: AuthMethod::ApiKey,
            api_keys: vec![],
            jwt: JwtConfig::default(),
        }
    }
}

impl Default for JwtConfig {
    fn default() -> Self {
        Self {
            secret_encrypted: String::new(),
            expiration_hours: 24,
            issuer: "pancetta".to_string(),
            audience: "pancetta-api".to_string(),
        }
    }
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_origins: vec!["*".to_string()],
            allowed_methods: vec!["GET".to_string(), "POST".to_string()],
            allowed_headers: vec!["Content-Type".to_string(), "Authorization".to_string()],
            allow_credentials: false,
            max_age_seconds: 3600,
        }
    }
}

impl Default for ApiRateLimitingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            requests_per_minute: 60,
            burst_allowance: 10,
            by_ip: true,
            by_api_key: false,
        }
    }
}

impl Default for ApiDocumentationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "/docs".to_string(),
            title: "Pancetta API".to_string(),
            description: "Amateur Radio Software API".to_string(),
            version: "1.0.0".to_string(),
        }
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            proxy_type: ProxyType::Http,
            server: String::new(),
            port: 8080,
            auth: None,
            exclusions: vec![],
        }
    }
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            verify_certificates: true,
            ca_bundle_path: None,
            client_cert: None,
            min_version: TlsVersion::Tls12,
            cipher_suites: vec![],
        }
    }
}

impl Default for RateLimitingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            global: GlobalRateLimit {
                requests_per_minute: 300,
                requests_per_hour: 3600,
                requests_per_day: 86400,
            },
            services: HashMap::new(),
        }
    }
}

impl Default for ReliabilityConfig {
    fn default() -> Self {
        Self {
            connection_timeout_seconds: 30,
            request_timeout_seconds: 60,
            retry: RetryConfig {
                max_attempts: 3,
                base_delay_ms: 1000,
                max_delay_ms: 10000,
                backoff_multiplier: 2.0,
                jitter_factor: 0.1,
            },
            circuit_breaker: CircuitBreakerConfig {
                enabled: true,
                failure_threshold: 5,
                success_threshold: 3,
                timeout_ms: 60000,
            },
            health_check: HealthCheckConfig {
                enabled: true,
                interval_seconds: 300,
                timeout_seconds: 10,
                unhealthy_threshold: 3,
            },
        }
    }
}

impl ConfigSection for NetworkConfig {
    fn validate_section(&self) -> ConfigResult<()> {
        // Validate PSKReporter settings
        if self.psk_reporter.enabled {
            if self.psk_reporter.upload_interval_seconds == 0 {
                return Err(ConfigError::InvalidValue {
                    field: "psk_reporter.upload_interval_seconds".to_string(),
                    value: self.psk_reporter.upload_interval_seconds.to_string(),
                });
            }

            if self.psk_reporter.batch_size == 0 {
                return Err(ConfigError::InvalidValue {
                    field: "psk_reporter.batch_size".to_string(),
                    value: self.psk_reporter.batch_size.to_string(),
                });
            }
        }

        // Validate web API settings
        if self.web_api.enabled && self.web_api.port == 0 {
            return Err(ConfigError::InvalidValue {
                field: "web_api.port".to_string(),
                value: self.web_api.port.to_string(),
            });
        }

        // Validate rate limiting
        if self.rate_limiting.enabled && self.rate_limiting.global.requests_per_minute == 0 {
            return Err(ConfigError::InvalidValue {
                field: "rate_limiting.global.requests_per_minute".to_string(),
                value: self.rate_limiting.global.requests_per_minute.to_string(),
            });
        }

        // cqdx.io validation
        if self.cqdx.enabled {
            if self.cqdx.token.is_none() || self.cqdx.token.as_ref().is_none_or(|t| t.is_empty()) {
                return Err(ConfigError::Validation(
                    "cqdx.io integration enabled but no PAT token configured".to_string(),
                ));
            }
            if self.cqdx.poll_interval_secs < 10 {
                return Err(ConfigError::Validation(
                    "cqdx.io poll interval must be at least 10 seconds".to_string(),
                ));
            }
        }

        // ClubLog upload validation — enabling requires the credentials the
        // realtime.php POST needs (email, password, api_key). The callsign may
        // be left empty (the coordinator falls back to the QSO's own call).
        if self.clublog.enabled {
            if self.clublog.email.is_empty() {
                return Err(ConfigError::Validation(
                    "ClubLog upload enabled but no email configured".to_string(),
                ));
            }
            if self.clublog.password.is_empty() {
                return Err(ConfigError::Validation(
                    "ClubLog upload enabled but no password configured".to_string(),
                ));
            }
            if self.clublog.api_key.is_empty() {
                return Err(ConfigError::Validation(
                    "ClubLog upload enabled but no api_key configured".to_string(),
                ));
            }
        }

        // QRZ Logbook upload validation — enabling requires the per-logbook API key.
        if self.qrz_logbook.enabled && self.qrz_logbook.api_key.is_empty() {
            return Err(ConfigError::Validation(
                "QRZ Logbook upload enabled but no api_key configured".to_string(),
            ));
        }

        // LoTW upload validation — signing requires the tqsl binary path AND
        // the TQSL Station Location name.
        if self.lotw.enabled {
            if self.lotw.tqsl_path.is_empty() {
                return Err(ConfigError::Validation(
                    "LoTW upload enabled but no tqsl_path configured".to_string(),
                ));
            }
            if self.lotw.station_location.is_empty() {
                return Err(ConfigError::Validation(
                    "LoTW upload enabled but no station_location configured".to_string(),
                ));
            }
        }

        // eQSL upload validation — enabling requires the account credentials.
        if self.eqsl.enabled {
            if self.eqsl.username.is_empty() {
                return Err(ConfigError::Validation(
                    "eQSL upload enabled but no username configured".to_string(),
                ));
            }
            if self.eqsl.password.is_empty() {
                return Err(ConfigError::Validation(
                    "eQSL upload enabled but no password configured".to_string(),
                ));
            }
        }

        // QRZ XML lookup validation — enabling requires the paid subscription
        // username + password.
        if self.qrz_xml.enabled {
            if self.qrz_xml.username.is_empty() {
                return Err(ConfigError::Validation(
                    "QRZ XML lookup enabled but no username configured".to_string(),
                ));
            }
            if self.qrz_xml.password.is_empty() {
                return Err(ConfigError::Validation(
                    "QRZ XML lookup enabled but no password configured".to_string(),
                ));
            }
        }

        Ok(())
    }

    fn merge_with(&mut self, other: Self) {
        // Merge service configurations unconditionally so that
        // user configs can disable services enabled by the system config.
        self.psk_reporter = other.psk_reporter;
        self.wspr = other.wspr;
        self.dx_cluster = other.dx_cluster;
        self.web_api = other.web_api;
        self.proxy = other.proxy;

        // Merge custom services
        self.custom_services.extend(other.custom_services);
    }
}

// ---------------------------------------------------------------------------
// Redacting Debug for credential-bearing configs (security review §3.1).
// These structs hold plaintext secrets; `#[derive(Debug)]` was removed so a
// stray `debug!("{:?}", cfg)` or a panic that formats one can never dump a
// password / api_key / cert secret. Non-secret fields are still shown.
// ---------------------------------------------------------------------------
const REDACTED: &str = "<redacted>";

impl std::fmt::Debug for ClubLogConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClubLogConfig")
            .field("enabled", &self.enabled)
            .field("email", &self.email)
            .field("password", &REDACTED)
            .field("callsign", &self.callsign)
            .field("api_key", &REDACTED)
            .finish()
    }
}

impl std::fmt::Debug for QrzLogbookConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QrzLogbookConfig")
            .field("enabled", &self.enabled)
            .field("api_key", &REDACTED)
            .finish()
    }
}

impl std::fmt::Debug for EqslConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EqslConfig")
            .field("enabled", &self.enabled)
            .field("username", &self.username)
            .field("password", &REDACTED)
            .field("qth_nickname", &self.qth_nickname)
            .finish()
    }
}

impl std::fmt::Debug for QrzXmlConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QrzXmlConfig")
            .field("enabled", &self.enabled)
            .field("username", &self.username)
            .field("password", &REDACTED)
            .finish()
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKey")
            .field("name", &self.name)
            .field("key_encrypted", &REDACTED)
            .field("permissions", &self.permissions)
            .field("expires_at", &self.expires_at)
            .field("enabled", &self.enabled)
            .finish()
    }
}

impl std::fmt::Debug for JwtConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtConfig")
            .field("secret_encrypted", &REDACTED)
            .field("expiration_hours", &self.expiration_hours)
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .finish()
    }
}

#[cfg(test)]
// rationale: test builders assign credential fields after default() for
// readability; sequential assignment reads clearer than a struct-update splat
// (mirrors pancetta-hamlib mock.rs).
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn credential_debug_is_redacted() {
        // A secret must never appear in Debug output (defense-in-depth: a stray
        // debug!("{:?}", cfg) or a panic that formats config can't leak it).
        let mut cl = ClubLogConfig::default();
        cl.password = "hunter2SECRET".into();
        cl.api_key = "clubKEY_abc".into();
        let s = format!("{cl:?}");
        assert!(!s.contains("hunter2SECRET"), "ClubLog password leaked: {s}");
        assert!(!s.contains("clubKEY_abc"), "ClubLog api_key leaked: {s}");
        assert!(s.contains("<redacted>"));

        let mut eq = EqslConfig::default();
        eq.password = "eqslPW_xyz".into();
        assert!(!format!("{eq:?}").contains("eqslPW_xyz"));

        let mut qx = QrzXmlConfig::default();
        qx.password = "qrzPW_123".into();
        assert!(!format!("{qx:?}").contains("qrzPW_123"));

        let mut qb = QrzLogbookConfig::default();
        qb.api_key = "qrzLogbookKEY".into();
        assert!(!format!("{qb:?}").contains("qrzLogbookKEY"));
    }

    #[test]
    fn test_default_network_config() {
        let config = NetworkConfig::default();
        assert!(!config.psk_reporter.enabled);
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_psk_reporter_validation() {
        let mut config = NetworkConfig::default();
        config.psk_reporter.enabled = true;

        // Valid configuration
        assert!(config.validate_section().is_ok());

        // Invalid upload interval
        config.psk_reporter.upload_interval_seconds = 0;
        assert!(config.validate_section().is_err());

        // Invalid batch size
        config.psk_reporter.upload_interval_seconds = 300; // Reset to valid
        config.psk_reporter.batch_size = 0;
        assert!(config.validate_section().is_err());
    }

    #[test]
    fn test_web_api_validation() {
        let mut config = NetworkConfig::default();
        config.web_api.enabled = true;

        // Valid configuration
        assert!(config.validate_section().is_ok());

        // Invalid port
        config.web_api.port = 0;
        assert!(config.validate_section().is_err());
    }

    #[test]
    fn test_rate_limiting_validation() {
        let mut config = NetworkConfig::default();
        config.rate_limiting.enabled = true;

        // Valid configuration
        assert!(config.validate_section().is_ok());

        // Invalid rate limit
        config.rate_limiting.global.requests_per_minute = 0;
        assert!(config.validate_section().is_err());
    }

    #[test]
    fn test_psk_reporter_filters() {
        let filters = PskReporterFilters::default();
        assert!(filters.enabled_modes.contains(&"FT8".to_string()));
        assert!(filters.enabled_bands.contains(&"20m".to_string()));
        assert!(filters.geographic.distance.great_circle);
    }

    #[test]
    fn test_dx_cluster_server() {
        let server = ClusterServer {
            name: "Test Cluster".to_string(),
            hostname: "cluster.example.com".to_string(),
            port: 7300,
            server_type: ClusterType::DxSpider,
            auth_required: false,
            username: None,
            priority: 1,
        };

        assert_eq!(server.port, 7300);
        assert!(!server.auth_required);
    }

    #[test]
    fn test_cqdx_defaults() {
        let config = CqdxConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.base_url, "https://cqdx.io");
        assert!(config.token.is_none());
        assert_eq!(config.poll_interval_secs, 30);
    }

    #[test]
    fn test_cqdx_validation_enabled_without_token() {
        let mut config = NetworkConfig::default();
        config.cqdx.enabled = true;
        config.cqdx.token = None;
        assert!(config.validate_section().is_err());
    }

    #[test]
    fn test_cqdx_validation_enabled_with_token() {
        let mut config = NetworkConfig::default();
        config.cqdx.enabled = true;
        config.cqdx.token = Some("pat_abc123".to_string());
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_cqdx_validation_disabled_no_token_ok() {
        let config = NetworkConfig::default();
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_cqdx_validation_poll_interval_bounds() {
        let mut config = NetworkConfig::default();
        config.cqdx.enabled = true;
        config.cqdx.token = Some("pat_abc123".to_string());
        config.cqdx.poll_interval_secs = 5; // too low
        assert!(config.validate_section().is_err());

        config.cqdx.poll_interval_secs = 30;
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_clublog_defaults_disabled() {
        let config = NetworkConfig::default();
        assert!(!config.clublog.enabled);
        assert!(config.clublog.email.is_empty());
        assert!(config.clublog.password.is_empty());
        assert!(config.clublog.callsign.is_empty());
        assert!(config.clublog.api_key.is_empty());
        // Disabled with empty creds must validate cleanly.
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_qrz_logbook_defaults_disabled() {
        let config = NetworkConfig::default();
        assert!(!config.qrz_logbook.enabled);
        assert!(config.qrz_logbook.api_key.is_empty());
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_clublog_validation_enabled_without_creds_fails() {
        let mut config = NetworkConfig::default();
        config.clublog.enabled = true;
        // No creds at all.
        assert!(config.validate_section().is_err());

        // Email only — still missing password + api_key.
        config.clublog.email = "op@example.com".to_string();
        assert!(config.validate_section().is_err());

        // Email + password — still missing api_key.
        config.clublog.password = "secret".to_string();
        assert!(config.validate_section().is_err());

        // All required creds present (callsign may stay empty).
        config.clublog.api_key = "appkey123".to_string();
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_qrz_logbook_validation_enabled_without_key_fails() {
        let mut config = NetworkConfig::default();
        config.qrz_logbook.enabled = true;
        assert!(config.validate_section().is_err());

        config.qrz_logbook.api_key = "qrzkey123".to_string();
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_lotw_defaults_disabled() {
        let config = NetworkConfig::default();
        assert!(!config.lotw.enabled);
        assert!(config.lotw.tqsl_path.is_empty());
        assert!(config.lotw.station_location.is_empty());
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_qrz_xml_defaults_disabled() {
        let config = NetworkConfig::default();
        assert!(!config.qrz_xml.enabled);
        assert!(config.qrz_xml.username.is_empty());
        assert!(config.qrz_xml.password.is_empty());
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_lotw_validation_enabled_without_creds_fails() {
        let mut config = NetworkConfig::default();
        config.lotw.enabled = true;
        // No tqsl_path / station_location.
        assert!(config.validate_section().is_err());

        // tqsl_path only — still missing station_location.
        config.lotw.tqsl_path = "/usr/bin/tqsl".to_string();
        assert!(config.validate_section().is_err());

        // Both present.
        config.lotw.station_location = "Home Station".to_string();
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_eqsl_defaults_disabled() {
        let config = NetworkConfig::default();
        assert!(!config.eqsl.enabled);
        assert!(config.eqsl.username.is_empty());
        assert!(config.eqsl.password.is_empty());
        assert!(config.eqsl.qth_nickname.is_empty());
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_eqsl_validation_enabled_without_creds_fails() {
        let mut config = NetworkConfig::default();
        config.eqsl.enabled = true;
        // No creds.
        assert!(config.validate_section().is_err());

        // Username only — still missing password.
        config.eqsl.username = "K5ARH".to_string();
        assert!(config.validate_section().is_err());

        // Both present (qth_nickname may stay empty).
        config.eqsl.password = "secret".to_string();
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_qrz_xml_validation_enabled_without_creds_fails() {
        let mut config = NetworkConfig::default();
        config.qrz_xml.enabled = true;
        // No creds at all.
        assert!(config.validate_section().is_err());

        // Username only — still missing password.
        config.qrz_xml.username = "K5ARH".to_string();
        assert!(config.validate_section().is_err());

        // Both present — valid.
        config.qrz_xml.password = "secret".to_string();
        assert!(config.validate_section().is_ok());
    }

    #[test]
    fn test_remote_gateway_defaults_when_section_absent() {
        // Deserializing a TOML string with NO [network.remote_gateway] section
        // must yield the default: disabled + localhost bind.
        let toml_str = "[network]\n";
        let outer: toml::Value = toml::from_str(toml_str).expect("valid toml");
        let net: NetworkConfig = outer
            .get("network")
            .cloned()
            .unwrap_or(toml::Value::Table(Default::default()))
            .try_into()
            .expect("valid NetworkConfig");
        assert!(!net.remote_gateway.enabled);
        assert_eq!(net.remote_gateway.bind_addr, "127.0.0.1:4080");
    }
}
