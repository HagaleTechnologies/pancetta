# cqdx.io ↔ Pancetta Integration Design

> **Status:** Approved  
> **Date:** 2026-04-03  
> **Depends on:** Phase 2 (priority scoring engine) complete  
> **Related:** `docs/superpowers/specs/2026-04-02-end-to-end-qso-design.md` Phase 4

## Goal

Replace pancetta's hardcoded stubs (rarity 0.5, empty needed sets, no spot reporting) with live data from cqdx.io. Pancetta must remain fully functional without cqdx.io (degraded mode).

## Architecture Overview

### Three Data Flows

```
┌──────────────────────────────────────────────────────────┐
│                        pancetta                          │
│                                                          │
│  ┌─────────────┐    ┌──────────────┐    ┌────────────┐  │
│  │ CqdxClient  │◄───│  Coordinator │───►│PSKReporter │  │
│  │ (reqwest)   │    │              │    │  Reporter   │  │
│  └──────┬──────┘    └──────────────┘    └────────────┘  │
│         │                                                │
│  ┌──────▼──────┐                                        │
│  │ CqdxCache   │ implements WorkedStationLookup          │
│  │ (in-memory) │                                        │
│  └─────────────┘                                        │
└──────────────────────────────────────────────────────────┘
          │                    │
          ▼                    ▼
   ┌─────────────┐     ┌──────────────┐
   │  cqdx.io    │     │ PSKReporter  │
   │  REST API   │     │   UDP/HTTP   │
   └─────────────┘     └──────────────┘
```

**Flow 1 — Inbound (cqdx.io → pancetta):** On startup, fetch DXCC entities, needed status, and rarity data. Poll `GET /api/v1/spots/priorities` every 30 seconds for high-value spot targets.

**Flow 2 — Outbound spots (pancetta → both):** After each decode cycle (~15s window), batch all decoded spots and POST to PSKReporter (UDP) and cqdx.io (`POST /api/v1/spots/ingest`). Fire-and-forget — never block the decode pipeline.

**Flow 3 — Outbound QSOs (pancetta → cqdx.io):** After a QSO completes, POST the QSO record to `POST /api/v1/qsos`. Fire-and-forget.

### New Crate: `pancetta-cqdx`

A new workspace crate containing:
- `CqdxClient` — HTTP client wrapping `reqwest` with Bearer token (PAT) auth
- `CqdxCache` — in-memory session cache implementing `WorkedStationLookup`
- Types for all API request/response structs

### Degraded Mode

When no PAT is configured or cqdx.io is unreachable:
- `CqdxClient::new()` returns `None` if no PAT in config
- Coordinator skips all cqdx.io wiring
- Existing stubs remain: rarity = 0.5, needed sets empty (= everything needed)
- PSKReporter reporting is independent and always active
- If polling fails 3 consecutive times at runtime, log warning and stop polling; resume on next successful health check

---

## cqdx.io API Requirements

### Authentication

All endpoints require a Personal Access Token (PAT) via Bearer auth:
```
Authorization: Bearer pat_xxxxxxxxxxxx
```

PATs are scoped per-user. cqdx.io manages token creation, revocation, and scoping.

### Endpoints Required

#### 1. `GET /api/v1/entities`

Returns the full DXCC entity list with prefix-to-entity mappings.

**Response:**
```json
{
  "entities": [
    {
      "id": 291,
      "name": "United States",
      "prefix": "K",
      "continent": "NA",
      "cqZone": 5,
      "ituZone": 8
    }
  ]
}
```

**Usage:** Fetched once on startup. Used to map callsign → DXCC entity for scoring.

#### 2. `GET /api/v1/entities/needed`

Returns entities the authenticated user still needs (not confirmed).

**Response:**
```json
{
  "needed": [
    { "entityId": 327, "name": "Bouvet Island", "prefix": "3Y/B" }
  ]
}
```

**Usage:** Fetched on startup, cached in `CqdxCache.needed_dxcc`. Drives the `is_needed_dxcc()` lookup.

#### 3. `GET /api/v1/spots/priorities`

Pre-scored, sorted list of high-value spot targets filtered by the authenticated user's needed/worked status.

**Query parameters:**
- `band` (optional) — filter to a specific band (e.g., `20m`, `40m`)
- `mode` (optional) — filter by mode (e.g., `FT8`, `FT4`, `CW`)
- `limit` (optional, default 20) — max results

**Response:**
```json
{
  "priorities": [
    {
      "callsign": "3Y0J",
      "grid": "JD15",
      "frequency": 14074000,
      "mode": "FT8",
      "snr": -12,
      "entity": "Bouvet Island",
      "rarity": 0.98,
      "needed": true,
      "lastSpotted": "2026-04-03T14:22:00Z",
      "spotCount": 5
    }
  ]
}
```

**Usage:** Polled every 30 seconds. Results feed into the autonomous operator's frequency nudge behavior — if a high-value station is spotted on a different frequency, the operator may QSY to hunt it.

#### 4. `POST /api/v1/spots/ingest`

Accepts a batch of decoded spots from pancetta.

**Request:**
```json
{
  "spots": [
    {
      "callsign": "W1ABC",
      "grid": "FN42",
      "frequency": 14074000,
      "mode": "FT8",
      "snr": -12,
      "timestamp": "2026-04-03T14:22:15Z",
      "reporter": "K2DEF",
      "reporterGrid": "FN31"
    }
  ]
}
```

**Response:** `202 Accepted`

**Usage:** Batched per decode cycle (one POST per ~15-second window). Fire-and-forget.

#### 5. `POST /api/v1/qsos`

Logs a completed QSO.

**Request:**
```json
{
  "version": 1,
  "qso": {
    "callsign": "JA1ABC",
    "remoteGrid": "PM95",
    "localGrid": "FN31",
    "frequency": 14074000,
    "mode": "FT8",
    "rstSent": "-10",
    "rstReceived": "-14",
    "startTime": "2026-04-03T14:22:00Z",
    "endTime": "2026-04-03T14:24:30Z"
  }
}
```

**Response:** `201 Created`

**Usage:** Sent after each QSO completion. Fire-and-forget. The `version` field allows schema evolution.

---

## pancetta-cqdx Crate Design

### `CqdxClient`

```rust
pub struct CqdxClient {
    http: reqwest::Client,
    base_url: String,  // e.g. "https://cqdx.io"
    token: String,     // PAT
}

impl CqdxClient {
    /// Returns None if no PAT configured.
    pub fn from_config(config: &CqdxConfig) -> Option<Self>;

    pub async fn fetch_entities(&self) -> Result<Vec<DxccEntity>>;
    pub async fn fetch_needed(&self) -> Result<Vec<NeededEntity>>;
    pub async fn fetch_priorities(&self, band: Option<&str>, mode: Option<&str>, limit: u32) -> Result<Vec<PrioritySpot>>;
    pub async fn report_spots(&self, spots: Vec<SpotReport>) -> Result<()>;
    pub async fn report_qso(&self, qso: QsoReport) -> Result<()>;
}
```

### `CqdxCache`

```rust
pub struct CqdxCache {
    entities: HashMap<String, DxccEntity>,      // prefix → entity
    needed_entities: HashSet<u32>,               // entity IDs
    rarity_scores: HashMap<u32, f64>,            // entity ID → rarity 0.0–1.0
    priority_spots: Vec<PrioritySpot>,           // latest poll results
}

impl CqdxCache {
    pub fn new() -> Self;
    pub fn load_entities(&mut self, entities: Vec<DxccEntity>);
    pub fn load_needed(&mut self, needed: Vec<NeededEntity>);
    pub fn update_priorities(&mut self, spots: Vec<PrioritySpot>);

    /// Resolve callsign → DXCC entity ID using prefix matching.
    pub fn resolve_entity(&self, callsign: &str) -> Option<u32>;

    /// Get rarity score for a callsign (0.0 = common, 1.0 = rare).
    pub fn rarity(&self, callsign: &str) -> f64;

    /// Get current high-priority spots for frequency nudge decisions.
    pub fn priority_spots(&self) -> &[PrioritySpot];
}
```

`CqdxCache` implements `WorkedStationLookup` (or a new trait method is added for rarity) to bridge into the existing `PriorityScorer`.

### Configuration

```toml
[cqdx]
enabled = true
base_url = "https://cqdx.io"     # default
token = "pat_xxxxxxxxxxxx"        # PAT
poll_interval_secs = 30           # priority spot polling
```

If `[cqdx]` is absent or `enabled = false`, pancetta operates in degraded mode.

---

## Wiring into Pancetta's Existing Systems

### Startup Flow

1. Read `[cqdx]` config section
2. If PAT present: create `CqdxClient`, fetch entities + needed, populate `CqdxCache`
3. Replace `CachedStationLookup`'s empty needed sets with cqdx.io data
4. Wire rarity into `PriorityScorer` (replace hardcoded 0.5)
5. If no PAT: skip all cqdx.io wiring, existing stubs remain active

### Priority Spot Polling (every 30s)

1. `CqdxClient::fetch_priorities(current_band, current_mode, 20)`
2. Update `CqdxCache.priority_spots`
3. If a high-rarity needed station is spotted on a different frequency within the current band, emit a `FrequencyNudge` event to the autonomous operator
4. Operator decides whether to QSY based on current state (mid-QSO = no, idle = yes)

### Spot Reporting (per decode cycle)

1. After each decode cycle, collect all decoded callsigns with grid/SNR/freq
2. Batch into one `POST /api/v1/spots/ingest` to cqdx.io (fire-and-forget via `tokio::spawn`)
3. Separately report to PSKReporter via UDP (existing or new reporter module)
4. Never block the decode pipeline on either report

### QSO Reporting

1. On `QsoCompleted` event, build `QsoReport` with both grids, versioned format
2. `POST /api/v1/qsos` via `tokio::spawn` (fire-and-forget)
3. Also continue writing to local ADIF log (existing behavior)

### Frequency Nudge Behavior

When priority polling returns a high-value station on a different frequency:
1. Check if autonomous operator is idle (not mid-QSO)
2. If idle and station scores above a configurable threshold, emit `FrequencyNudge { freq_hz, callsign, score }`
3. Operator QSYs and enters hunt mode targeting that specific callsign
4. If QSO completes or timeout, return to previous frequency

### Watchdog

The coordinator runs a watchdog that monitors decode activity:
- If no decode events occur for 2 hours, stop the priority spot poll timer and spot reporting
- Resume both when decode activity restarts
- Prevents unnecessary API traffic when the station is inactive

### PSKReporter Integration

- Mode-agnostic: report all decoded modes, not just FT8
- Follow PSKReporter protocol (UDP datagrams or HTTP, per their spec)
- Independent of cqdx.io — always active regardless of PAT config
- Batch per decode cycle, same as cqdx.io spot reporting

---

## Testing Strategy

### pancetta-cqdx Unit Tests
- Mock HTTP responses (e.g., `wiremock`) for all 5 endpoints
- Test auth header presence
- Test JSON parsing for all response types
- Test error handling (401, 500, timeout, malformed JSON)

### CqdxCache Tests
- Prefix → entity resolution
- Rarity lookup with and without data
- `WorkedStationLookup` implementation: `is_needed_dxcc` returns correct values when populated vs empty

### Coordinator Integration Tests
- Degraded mode: no PAT → no cqdx polling, existing stubs active
- Watchdog: simulate 2h inactivity → polling stops, decode resumes → polling resumes
- Spot batching: verify one POST per decode cycle, not per spot

### Existing Tests
- All existing loopback and priority tests continue to pass unchanged (they use `NullLookup`)

---

## Design Decisions

| Decision | Rationale |
|----------|-----------|
| New `pancetta-cqdx` crate | Clean separation; cqdx.io client is self-contained |
| PAT auth (not OAuth) | First-party integration; simplest secure approach |
| Poll priorities (not firehose) | Bandwidth-efficient; cqdx.io does the scoring server-side |
| Fire-and-forget outbound | Decode pipeline must never block on network I/O |
| Batch spots per cycle | One POST per 15s window, not per decoded signal |
| Versioned QSO format | `"version": 1` allows schema evolution without breaking |
| Mode-agnostic | PSKReporter and cqdx.io support many modes; don't limit to FT8 |
| Both grids in QSO | `remoteGrid` + `localGrid` for complete QSO geometry |
| Watchdog (2h) | Stop polling when station is inactive |
| Degraded mode | Pancetta works standalone; cqdx.io is an enhancement |
