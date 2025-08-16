# Pancetta API Specification

## Overview

This document defines the internal and external APIs for Pancetta, including the core library API, REST/WebSocket API for future UIs, and integration points for external services.

## Core Library API

### Digital Mode Codec Interface

```rust
/// Trait for all digital mode codecs
pub trait DigitalModeCodec: Send + Sync {
    /// Decode audio samples into messages
    fn decode(&self, samples: &[f32], sample_rate: u32) -> Result<Vec<DecodedMessage>>;
    
    /// Encode a message into audio samples
    fn encode(&self, message: &Message, sample_rate: u32) -> Result<Vec<f32>>;
    
    /// Get information about this codec
    fn info(&self) -> CodecInfo;
    
    /// Check if codec is ready (time sync, etc.)
    fn is_ready(&self) -> bool;
}

/// Decoded message from codec
#[derive(Debug, Clone)]
pub struct DecodedMessage {
    pub mode: DigitalMode,
    pub message: String,
    pub snr: i8,
    pub frequency: f32,
    pub time_offset: f32,
    pub confidence: f32,
    pub timestamp: DateTime<Utc>,
}

/// Information about a codec
#[derive(Debug, Clone)]
pub struct CodecInfo {
    pub name: String,
    pub mode: DigitalMode,
    pub symbol_rate: f32,
    pub bandwidth: f32,
    pub min_snr: i8,
}
```

### QSO Manager API

```rust
/// QSO management interface
pub trait QsoManager: Send + Sync {
    /// Start a new QSO
    async fn start_qso(&self, params: QsoParams) -> Result<QsoId>;
    
    /// Send a message in current QSO
    async fn send_message(&self, qso_id: QsoId, message: &str) -> Result<()>;
    
    /// Get current QSO state
    async fn get_state(&self, qso_id: QsoId) -> Result<QsoState>;
    
    /// Abort a QSO
    async fn abort_qso(&self, qso_id: QsoId) -> Result<()>;
    
    /// Get QSO history
    async fn get_history(&self, filter: QsoFilter) -> Result<Vec<Qso>>;
}

/// QSO state machine states
#[derive(Debug, Clone, PartialEq)]
pub enum QsoState {
    Idle,
    Calling,
    Replying,
    ExchangingReport,
    ExchangingGrid,
    Confirming,
    Completed,
    Failed(String),
}

/// Parameters for starting a QSO
#[derive(Debug, Clone)]
pub struct QsoParams {
    pub remote_callsign: Option<Callsign>,
    pub mode: DigitalMode,
    pub frequency: Frequency,
    pub auto_sequence: bool,
}
```

### DX Hunter API

```rust
/// DX hunting engine interface
pub trait DxHunter: Send + Sync {
    /// Calculate rarity score for a station
    async fn calculate_score(&self, station: &Station) -> Result<RarityScore>;
    
    /// Get prioritized list of DX stations
    async fn get_priorities(&self) -> Result<Vec<DxStation>>;
    
    /// Check if entity/grid is needed
    async fn is_needed(&self, entity: &DxccEntity, grid: &GridSquare) -> Result<NeededStatus>;
    
    /// Update worked status
    async fn mark_worked(&self, station: &Station) -> Result<()>;
}

/// DX station with scoring
#[derive(Debug, Clone)]
pub struct DxStation {
    pub station: Station,
    pub score: RarityScore,
    pub entity: DxccEntity,
    pub distance_km: f64,
    pub bearing: f64,
    pub needed: NeededStatus,
}

/// Rarity scoring
#[derive(Debug, Clone)]
pub struct RarityScore {
    pub total: u32,
    pub entity_rarity: u32,
    pub distance_points: u32,
    pub band_points: u32,
    pub mode_points: u32,
}
```

### Audio Service API

```rust
/// Audio I/O service interface
pub trait AudioService: Send + Sync {
    /// List available audio devices
    async fn list_devices(&self) -> Result<Vec<AudioDevice>>;
    
    /// Start audio capture
    async fn start_capture(&self, device: &AudioDevice, callback: AudioCallback) -> Result<StreamHandle>;
    
    /// Start audio playback
    async fn start_playback(&self, device: &AudioDevice, samples: Vec<f32>) -> Result<()>;
    
    /// Get current audio levels
    fn get_levels(&self) -> AudioLevels;
    
    /// Stop audio stream
    async fn stop_stream(&self, handle: StreamHandle) -> Result<()>;
}

/// Audio device information
#[derive(Debug, Clone)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub is_input: bool,
    pub sample_rates: Vec<u32>,
    pub channels: u16,
}

/// Audio level monitoring
#[derive(Debug, Clone)]
pub struct AudioLevels {
    pub input_rms: f32,
    pub input_peak: f32,
    pub output_rms: f32,
    pub output_peak: f32,
}
```

### Rig Control API

```rust
/// Transceiver control interface
pub trait RigControl: Send + Sync {
    /// Connect to rig
    async fn connect(&self, config: RigConfig) -> Result<()>;
    
    /// Get current frequency
    async fn get_frequency(&self) -> Result<Frequency>;
    
    /// Set frequency
    async fn set_frequency(&self, freq: Frequency) -> Result<()>;
    
    /// Get current mode
    async fn get_mode(&self) -> Result<Mode>;
    
    /// Set mode
    async fn set_mode(&self, mode: Mode) -> Result<()>;
    
    /// Control PTT
    async fn set_ptt(&self, state: PttState) -> Result<()>;
    
    /// Get rig capabilities
    fn get_capabilities(&self) -> RigCapabilities;
}

/// Rig configuration
#[derive(Debug, Clone)]
pub struct RigConfig {
    pub model: String,
    pub port: String,
    pub baud_rate: u32,
    pub data_bits: u8,
    pub stop_bits: u8,
    pub parity: Parity,
}
```

## REST API Specification

### Base URL
```
http://localhost:8080/api/v1
```

### Authentication
```http
Authorization: Bearer <token>
```

### Endpoints

#### Station Management

```http
GET /station
```
Get current station information

**Response:**
```json
{
  "callsign": "W1AW",
  "grid_square": "FN31",
  "power": 100,
  "antenna": "Dipole",
  "qth": "Newington, CT"
}
```

```http
PUT /station
```
Update station information

**Request:**
```json
{
  "callsign": "W1AW",
  "grid_square": "FN31",
  "power": 100
}
```

#### QSO Operations

```http
POST /qso
```
Start a new QSO

**Request:**
```json
{
  "remote_callsign": "DX0ABC",
  "mode": "FT8",
  "frequency": 14074000,
  "auto_sequence": true
}
```

**Response:**
```json
{
  "qso_id": "550e8400-e29b-41d4-a716-446655440000",
  "state": "calling"
}
```

```http
GET /qso/{qso_id}
```
Get QSO details

```http
POST /qso/{qso_id}/message
```
Send message in QSO

**Request:**
```json
{
  "message": "599 CA"
}
```

```http
DELETE /qso/{qso_id}
```
Abort QSO

#### Band Activity

```http
GET /activity
```
Get current band activity

**Response:**
```json
{
  "messages": [
    {
      "time": "2024-01-15T18:30:15Z",
      "snr": -12,
      "dt": 0.2,
      "frequency": 1245,
      "message": "CQ DX W1AW FN31",
      "mode": "FT8"
    }
  ]
}
```

#### DX Hunter

```http
GET /dx/priorities
```
Get prioritized DX stations

**Response:**
```json
{
  "stations": [
    {
      "callsign": "ZD8W",
      "grid": "II22",
      "score": 95,
      "entity": "Ascension Island",
      "distance_km": 8453,
      "bearing": 125,
      "needed": {
        "new_dxcc": true,
        "new_grid": true,
        "new_band": false
      }
    }
  ]
}
```

#### Logging

```http
GET /log
```
Get log entries

**Query Parameters:**
- `start_date`: ISO 8601 date
- `end_date`: ISO 8601 date
- `callsign`: Filter by callsign
- `mode`: Filter by mode

```http
POST /log/export
```
Export log in various formats

**Request:**
```json
{
  "format": "adif",
  "start_date": "2024-01-01",
  "end_date": "2024-01-31"
}
```

## WebSocket API

### Connection
```
ws://localhost:8080/ws
```

### Message Types

#### Subscribe to Events
```json
{
  "type": "subscribe",
  "events": ["message_decoded", "qso_state_changed", "dx_spotted"]
}
```

#### Message Decoded Event
```json
{
  "type": "message_decoded",
  "data": {
    "time": "2024-01-15T18:30:15Z",
    "snr": -12,
    "frequency": 1245,
    "message": "CQ DX W1AW FN31",
    "mode": "FT8"
  }
}
```

#### QSO State Changed Event
```json
{
  "type": "qso_state_changed",
  "data": {
    "qso_id": "550e8400-e29b-41d4-a716-446655440000",
    "old_state": "calling",
    "new_state": "exchanging_report"
  }
}
```

#### DX Spotted Event
```json
{
  "type": "dx_spotted",
  "data": {
    "callsign": "VP8LP",
    "grid": "GD18",
    "frequency": 14074000,
    "mode": "FT8",
    "snr": -18
  }
}
```

## External Service APIs

### PSKReporter Integration

```rust
/// PSKReporter client interface
pub trait PskReporterClient: Send + Sync {
    /// Submit reception reports
    async fn submit_reports(&self, reports: Vec<ReceptionReport>) -> Result<()>;
    
    /// Query spots for a callsign
    async fn query_spots(&self, callsign: &Callsign) -> Result<Vec<Spot>>;
}

/// Reception report for PSKReporter
#[derive(Debug, Clone)]
pub struct ReceptionReport {
    pub sender_callsign: Callsign,
    pub sender_grid: GridSquare,
    pub frequency: Frequency,
    pub mode: String,
    pub snr: i8,
    pub timestamp: DateTime<Utc>,
}
```

### QRZ.com Integration (Optional)

```rust
/// QRZ.com API client interface
pub trait QrzClient: Send + Sync {
    /// Lookup callsign information
    async fn lookup(&self, callsign: &Callsign) -> Result<QrzInfo>;
    
    /// Upload logbook entry
    async fn upload_qso(&self, qso: &Qso) -> Result<()>;
}

/// QRZ.com callsign information
#[derive(Debug, Clone)]
pub struct QrzInfo {
    pub callsign: String,
    pub name: String,
    pub address: String,
    pub country: String,
    pub grid: String,
    pub email: Option<String>,
}
```

## Error Handling

### Error Response Format
```json
{
  "error": {
    "code": "INVALID_FREQUENCY",
    "message": "Frequency 14074000 is outside amateur band",
    "details": {
      "frequency": 14074000,
      "band": "20m",
      "valid_range": [14000000, 14350000]
    }
  }
}
```

### HTTP Status Codes
- `200 OK` - Successful operation
- `201 Created` - Resource created
- `400 Bad Request` - Invalid request
- `401 Unauthorized` - Authentication required
- `403 Forbidden` - Access denied
- `404 Not Found` - Resource not found
- `409 Conflict` - State conflict
- `500 Internal Server Error` - Server error

## Rate Limiting

### API Rate Limits
- Standard: 100 requests per minute
- WebSocket: 1000 messages per minute
- PSKReporter: 1 batch per 5 minutes

### Rate Limit Headers
```http
X-RateLimit-Limit: 100
X-RateLimit-Remaining: 95
X-RateLimit-Reset: 1642266000
```

## Versioning

### API Version Strategy
- URL versioning: `/api/v1/`, `/api/v2/`
- Breaking changes require new version
- Deprecation notice: 6 months
- Sunset period: 12 months

### Version Response Header
```http
X-API-Version: 1.0.0
X-API-Deprecated: false
```

## API Documentation

### OpenAPI Specification
Available at `/api/docs/openapi.json`

### Interactive Documentation
Swagger UI available at `/api/docs`

## SDK Support

### Official SDKs
- Rust (native)
- TypeScript/JavaScript (generated)
- Python (generated)

### Code Generation
OpenAPI Generator for client SDKs

## Testing

### API Testing Tools
- Postman collection available
- Integration test suite
- Mock server for development

### Test Endpoints
```
GET /api/test/health
GET /api/test/echo
POST /api/test/validate
```

## Security

### Authentication Methods
- API Key (development)
- JWT tokens (production)
- OAuth 2.0 (future)

### Security Headers
```http
X-Content-Type-Options: nosniff
X-Frame-Options: DENY
X-XSS-Protection: 1; mode=block
Content-Security-Policy: default-src 'self'
```

### CORS Configuration
```http
Access-Control-Allow-Origin: http://localhost:3000
Access-Control-Allow-Methods: GET, POST, PUT, DELETE
Access-Control-Allow-Headers: Content-Type, Authorization
```

## Performance

### Response Time Targets
- GET requests: < 50ms
- POST requests: < 100ms
- WebSocket messages: < 10ms

### Caching Strategy
```http
Cache-Control: public, max-age=300
ETag: "33a64df551425fcc55e4d42a148795d9f25f89d4"
```

## Conclusion

This API specification provides a comprehensive interface for all Pancetta functionality, supporting both the native TUI and future web/mobile interfaces. The design emphasizes consistency, performance, and extensibility while maintaining clean separation between layers.