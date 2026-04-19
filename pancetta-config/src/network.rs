//! Network services configuration module
//!
//! This module handles configuration for external network services including
//! PSKReporter, QRZ.com, LOTW, eQSL, and other amateur radio web services.

use crate::{ConfigError, ConfigResult, ConfigSection};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Network services configuration
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// PSKReporter service configuration
    pub psk_reporter: PskReporterConfig,

    /// QRZ.com service configuration
    pub qrz: QrzConfig,

    /// ARRL Logbook of the World configuration
    pub lotw: LotwConfig,

    /// eQSL service configuration
    pub eqsl: EqslConfig,

    /// Clublog service configuration
    pub clublog: ClublogConfig,

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

    /// Custom service integrations
    #[serde(default)]
    pub custom_services: HashMap<String, CustomServiceConfig>,
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

/// QRZ.com service configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QrzConfig {
    /// Enable QRZ.com integration
    pub enabled: bool,

    /// QRZ.com username
    pub username: Option<String>,

    /// QRZ.com password (encrypted)
    pub password_encrypted: Option<String>,

    /// QRZ.com XML API key
    pub api_key: Option<String>,

    /// QRZ.com API endpoint
    pub api_endpoint: String,

    /// Session management
    pub session: QrzSessionConfig,

    /// Lookup preferences
    pub lookup: QrzLookupConfig,

    /// Cache settings
    pub cache: CacheConfig,

    /// Logbook integration
    pub logbook: QrzLogbookConfig,
}

/// QRZ session management
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QrzSessionConfig {
    /// Session timeout in minutes
    pub timeout_minutes: u32,

    /// Auto-refresh sessions
    pub auto_refresh: bool,

    /// Session cache file
    pub cache_file: Option<String>,
}

/// QRZ lookup preferences
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QrzLookupConfig {
    /// Automatically lookup callsigns
    pub auto_lookup: bool,

    /// Lookup timeout in seconds
    pub timeout_seconds: u32,

    /// Fields to retrieve
    pub fields: Vec<String>,

    /// Include image URLs
    pub include_images: bool,

    /// Include biographical data
    pub include_bio: bool,
}

/// Cache configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Enable caching
    pub enabled: bool,

    /// Cache duration in hours
    pub duration_hours: u32,

    /// Maximum cache size in MB
    pub max_size_mb: u32,

    /// Cache directory
    pub directory: Option<String>,

    /// Auto-cleanup old entries
    pub auto_cleanup: bool,
}

/// QRZ logbook integration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QrzLogbookConfig {
    /// Enable logbook upload
    pub upload_enabled: bool,

    /// Automatically upload QSOs
    pub auto_upload: bool,

    /// Upload batch size
    pub batch_size: u32,

    /// Upload confirmation required
    pub confirmation_required: bool,

    /// ADIF field mapping
    pub field_mapping: HashMap<String, String>,
}

/// ARRL Logbook of the World configuration
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct LotwConfig {
    /// Enable LOTW integration
    pub enabled: bool,

    /// LOTW username
    pub username: Option<String>,

    /// LOTW password (encrypted)
    pub password_encrypted: Option<String>,

    /// Certificate settings
    pub certificate: LotwCertificateConfig,

    /// Upload settings
    pub upload: LotwUploadConfig,

    /// Download settings
    pub download: LotwDownloadConfig,

    /// TQSL integration
    pub tqsl: TqslConfig,
}

/// LOTW certificate configuration
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct LotwCertificateConfig {
    /// Certificate file path
    pub cert_file: Option<String>,

    /// Private key file path
    pub key_file: Option<String>,

    /// Certificate password (encrypted)
    pub password_encrypted: Option<String>,

    /// Auto-renewal settings
    pub auto_renewal: CertRenewalConfig,
}

/// Certificate renewal configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CertRenewalConfig {
    /// Enable auto-renewal
    pub enabled: bool,

    /// Days before expiry to renew
    pub renewal_days: u32,

    /// Notification settings
    pub notifications: bool,
}

/// LOTW upload configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LotwUploadConfig {
    /// Automatically upload QSOs
    pub auto_upload: bool,

    /// Upload interval in hours
    pub interval_hours: u32,

    /// Include QSL sent status
    pub include_qsl_sent: bool,

    /// ADIF export settings
    pub adif_export: AdifExportConfig,
}

/// ADIF export configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdifExportConfig {
    /// ADIF version
    pub version: String,

    /// Include all fields
    pub include_all_fields: bool,

    /// Custom field inclusions
    pub custom_fields: Vec<String>,

    /// Export format
    pub format: AdifFormat,
}

/// ADIF format options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum AdifFormat {
    /// Standard ADIF text format
    Adi,

    /// ADIF XML format
    Adx,

    /// Compressed ADIF
    Compressed,
}

/// LOTW download configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LotwDownloadConfig {
    /// Automatically download confirmations
    pub auto_download: bool,

    /// Download interval in hours
    pub interval_hours: u32,

    /// Download since last check
    pub incremental: bool,

    /// Process confirmations automatically
    pub auto_process: bool,
}

/// TQSL (Trusted QSL) integration
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TqslConfig {
    /// TQSL executable path
    pub executable_path: Option<String>,

    /// Station location
    pub station_location: Option<String>,

    /// Command line options
    pub command_options: Vec<String>,

    /// Working directory
    pub working_directory: Option<String>,
}

/// eQSL service configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqslConfig {
    /// Enable eQSL integration
    pub enabled: bool,

    /// eQSL username
    pub username: Option<String>,

    /// eQSL password (encrypted)
    pub password_encrypted: Option<String>,

    /// eQSL API endpoint
    pub api_endpoint: String,

    /// Upload settings
    pub upload: EqslUploadConfig,

    /// Download settings
    pub download: EqslDownloadConfig,

    /// eQSL card settings
    pub cards: EqslCardConfig,
}

/// eQSL upload configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqslUploadConfig {
    /// Automatically upload QSOs
    pub auto_upload: bool,

    /// Upload batch size
    pub batch_size: u32,

    /// Include QSL message
    pub include_message: bool,

    /// Default QSL message
    pub default_message: String,
}

/// eQSL download configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqslDownloadConfig {
    /// Automatically download inbox
    pub auto_download: bool,

    /// Download interval in hours
    pub interval_hours: u32,

    /// Download card images
    pub download_images: bool,

    /// Image quality
    pub image_quality: ImageQuality,
}

/// Image quality options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageQuality {
    Low,
    Medium,
    High,
    Original,
}

/// eQSL card configuration
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct EqslCardConfig {
    /// Default card template
    pub default_template: Option<String>,

    /// Card customization
    pub customization: CardCustomizationConfig,

    /// Storage settings
    pub storage: CardStorageConfig,
}

/// Card customization configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardCustomizationConfig {
    /// Custom background image
    pub background_image: Option<String>,

    /// Custom message template
    pub message_template: Option<String>,

    /// Font settings
    pub font_settings: FontSettings,

    /// Color scheme
    pub color_scheme: String,
}

/// Font settings for cards
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FontSettings {
    /// Font family
    pub family: String,

    /// Font size
    pub size: u32,

    /// Font color
    pub color: String,

    /// Bold text
    pub bold: bool,

    /// Italic text
    pub italic: bool,
}

/// Card storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardStorageConfig {
    /// Storage directory
    pub directory: String,

    /// Organize by year
    pub organize_by_year: bool,

    /// Organize by band
    pub organize_by_band: bool,

    /// File naming pattern
    pub naming_pattern: String,
}

/// Clublog service configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClublogConfig {
    /// Enable Clublog integration
    pub enabled: bool,

    /// Clublog email
    pub email: Option<String>,

    /// Clublog password (encrypted)
    pub password_encrypted: Option<String>,

    /// Clublog API key
    pub api_key: Option<String>,

    /// API endpoint
    pub api_endpoint: String,

    /// Upload settings
    pub upload: ClublogUploadConfig,

    /// OQRS settings
    pub oqrs: OqrsConfig,
}

/// Clublog upload configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClublogUploadConfig {
    /// Automatically upload QSOs
    pub auto_upload: bool,

    /// Upload batch size
    pub batch_size: u32,

    /// Include QSL information
    pub include_qsl_info: bool,

    /// Upload confirmations
    pub upload_confirmations: bool,
}

/// OQRS (Online QSL Request System) configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OqrsConfig {
    /// Enable OQRS integration
    pub enabled: bool,

    /// Automatically process requests
    pub auto_process: bool,

    /// Email notifications
    pub email_notifications: bool,

    /// Request processing settings
    pub processing: OqrsProcessingConfig,
}

/// OQRS processing configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OqrsProcessingConfig {
    /// Default QSL route
    pub default_route: QslRoute,

    /// Automatic approval rules
    pub auto_approval_rules: Vec<ApprovalRule>,

    /// Manual review threshold
    pub manual_review_threshold: u32,
}

/// QSL route options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QslRoute {
    Direct,
    Bureau,
    Electronic,
    NoQsl,
}

/// Approval rule for OQRS
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRule {
    /// Rule name
    pub name: String,

    /// Rule condition
    pub condition: String,

    /// Action to take
    pub action: ApprovalAction,

    /// Rule priority
    pub priority: u8,

    /// Rule enabled
    pub enabled: bool,
}

/// Approval actions
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAction {
    AutoApprove,
    AutoReject,
    ManualReview,
    RequestMoreInfo,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    /// Password (encrypted)
    pub password_encrypted: String,
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

    /// Key password (encrypted)
    pub key_password_encrypted: Option<String>,
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
                software_version: "0.1.0".to_string(),
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

impl Default for QrzConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            username: None,
            password_encrypted: None,
            api_key: None,
            api_endpoint: "https://xmldata.qrz.com/xml/current/".to_string(),
            session: QrzSessionConfig::default(),
            lookup: QrzLookupConfig::default(),
            cache: CacheConfig::default(),
            logbook: QrzLogbookConfig::default(),
        }
    }
}

impl Default for QrzSessionConfig {
    fn default() -> Self {
        Self {
            timeout_minutes: 60,
            auto_refresh: true,
            cache_file: None,
        }
    }
}

impl Default for QrzLookupConfig {
    fn default() -> Self {
        Self {
            auto_lookup: false,
            timeout_seconds: 10,
            fields: vec![
                "callsign".to_string(),
                "name".to_string(),
                "addr1".to_string(),
                "addr2".to_string(),
                "state".to_string(),
                "country".to_string(),
                "grid".to_string(),
            ],
            include_images: false,
            include_bio: false,
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            duration_hours: 24,
            max_size_mb: 100,
            directory: None,
            auto_cleanup: true,
        }
    }
}

impl Default for QrzLogbookConfig {
    fn default() -> Self {
        Self {
            upload_enabled: false,
            auto_upload: false,
            batch_size: 100,
            confirmation_required: true,
            field_mapping: HashMap::new(),
        }
    }
}

impl Default for CertRenewalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            renewal_days: 30,
            notifications: true,
        }
    }
}

impl Default for LotwUploadConfig {
    fn default() -> Self {
        Self {
            auto_upload: false,
            interval_hours: 24,
            include_qsl_sent: true,
            adif_export: AdifExportConfig::default(),
        }
    }
}

impl Default for AdifExportConfig {
    fn default() -> Self {
        Self {
            version: "3.1.4".to_string(),
            include_all_fields: false,
            custom_fields: vec![],
            format: AdifFormat::Adi,
        }
    }
}

impl Default for LotwDownloadConfig {
    fn default() -> Self {
        Self {
            auto_download: false,
            interval_hours: 24,
            incremental: true,
            auto_process: true,
        }
    }
}

impl Default for EqslConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            username: None,
            password_encrypted: None,
            api_endpoint: "https://www.eqsl.cc/qslcard/ImportADIF.cfm".to_string(),
            upload: EqslUploadConfig::default(),
            download: EqslDownloadConfig::default(),
            cards: EqslCardConfig::default(),
        }
    }
}

impl Default for EqslUploadConfig {
    fn default() -> Self {
        Self {
            auto_upload: false,
            batch_size: 50,
            include_message: true,
            default_message: "73 from Pancetta".to_string(),
        }
    }
}

impl Default for EqslDownloadConfig {
    fn default() -> Self {
        Self {
            auto_download: false,
            interval_hours: 24,
            download_images: true,
            image_quality: ImageQuality::Medium,
        }
    }
}

impl Default for CardCustomizationConfig {
    fn default() -> Self {
        Self {
            background_image: None,
            message_template: None,
            font_settings: FontSettings::default(),
            color_scheme: "default".to_string(),
        }
    }
}

impl Default for FontSettings {
    fn default() -> Self {
        Self {
            family: "Arial".to_string(),
            size: 12,
            color: "#000000".to_string(),
            bold: false,
            italic: false,
        }
    }
}

impl Default for CardStorageConfig {
    fn default() -> Self {
        Self {
            directory: "~/Documents/Pancetta/eQSL Cards".to_string(),
            organize_by_year: true,
            organize_by_band: false,
            naming_pattern: "{callsign}_{date}_{time}".to_string(),
        }
    }
}

impl Default for ClublogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            email: None,
            password_encrypted: None,
            api_key: None,
            api_endpoint: "https://clublog.org/realtime.php".to_string(),
            upload: ClublogUploadConfig::default(),
            oqrs: OqrsConfig::default(),
        }
    }
}

impl Default for ClublogUploadConfig {
    fn default() -> Self {
        Self {
            auto_upload: false,
            batch_size: 100,
            include_qsl_info: true,
            upload_confirmations: true,
        }
    }
}

impl Default for OqrsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_process: false,
            email_notifications: true,
            processing: OqrsProcessingConfig::default(),
        }
    }
}

impl Default for OqrsProcessingConfig {
    fn default() -> Self {
        Self {
            default_route: QslRoute::Bureau,
            auto_approval_rules: vec![],
            manual_review_threshold: 10,
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

        // Validate QRZ settings
        if self.qrz.enabled {
            if self.qrz.username.is_none() && self.qrz.api_key.is_none() {
                return Err(ConfigError::MissingRequired(
                    "qrz.username or qrz.api_key".to_string(),
                ));
            }
        }

        // Validate web API settings
        if self.web_api.enabled {
            if self.web_api.port == 0 {
                return Err(ConfigError::InvalidValue {
                    field: "web_api.port".to_string(),
                    value: self.web_api.port.to_string(),
                });
            }
        }

        // Validate rate limiting
        if self.rate_limiting.enabled {
            if self.rate_limiting.global.requests_per_minute == 0 {
                return Err(ConfigError::InvalidValue {
                    field: "rate_limiting.global.requests_per_minute".to_string(),
                    value: self.rate_limiting.global.requests_per_minute.to_string(),
                });
            }
        }

        // cqdx.io validation
        if self.cqdx.enabled {
            if self.cqdx.token.is_none() || self.cqdx.token.as_ref().map_or(true, |t| t.is_empty())
            {
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

        Ok(())
    }

    fn merge_with(&mut self, other: Self) {
        // Merge service configurations unconditionally so that
        // user configs can disable services enabled by the system config.
        self.psk_reporter = other.psk_reporter;
        self.qrz = other.qrz;
        self.lotw = other.lotw;
        self.eqsl = other.eqsl;
        self.clublog = other.clublog;
        self.wspr = other.wspr;
        self.dx_cluster = other.dx_cluster;
        self.web_api = other.web_api;
        self.proxy = other.proxy;

        // Merge custom services
        self.custom_services.extend(other.custom_services);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_network_config() {
        let config = NetworkConfig::default();
        assert!(!config.psk_reporter.enabled);
        assert!(!config.qrz.enabled);
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
    fn test_qrz_validation() {
        let mut config = NetworkConfig::default();
        config.qrz.enabled = true;

        // Missing credentials
        assert!(config.validate_section().is_err());

        // Valid with username
        config.qrz.username = Some("test_user".to_string());
        assert!(config.validate_section().is_ok());

        // Valid with API key
        config.qrz.username = None;
        config.qrz.api_key = Some("test_key".to_string());
        assert!(config.validate_section().is_ok());
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
}
