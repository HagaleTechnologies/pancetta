# cqdx.io ↔ Pancetta Integration Design

> **Status:** Implemented (updated 2026-04-03 to match CQDX's actual API)  
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

**Flow 1 — Inbound (cqdx.io → pancetta):** On startup, fetch DXCC entities (with rarity rank/tier), needed status. Poll `GET /api/v1/spots?live=true` every 30 seconds for live spot groups from the Durable Object snapshot (edge-cached with 10s TTL).

**Flow 2 — Outbound spots (pancetta → both):** After each decode cycle (~15s window), batch all decoded spots and POST to PSKReporter (UDP) and cqdx.io (`POST /api/v1/spots/report`). Fire-and-forget — never block the decode pipeline.

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

Returns the full DXCC entity list with prefix-to-entity mappings, rarity data, and deletion status.

**Response:**
```json
{
  "entities": [
    {
      "adifNumber": 291,
      "entityName": "United States",
      "prefix": "K",
      "continent": "NA",
      "cqZone": 5,
      "ituZone": 8,
      "rarityRank": 340,
      "rarityTier": "common",
      "isDeleted": false
    }
  ]
}
```

**Fields:** `rarityRank` is ClubLog Most Wanted rank (1=rarest, ~340=most common), nullable. `rarityTier` is one of `legendary`, `very_rare`, `rare`, `uncommon`, `common`. `isDeleted` flags entities removed from the DXCC list.

**Usage:** Fetched once on startup. Used to map callsign → DXCC entity for scoring. Cached 5 minutes server-side.

#### 2. `GET /api/v1/entities/needed`

Returns entities the authenticated user still needs (All Time New One — never worked).

**Response:**
```json
{
  "needed": [
    { "entityId": 327, "name": "Bouvet Island", "prefix": "3Y/B" }
  ]
}
```

**Usage:** Fetched on startup, cached in `CqdxCache.needed_entity_ids`. Drives the `is_needed_dxcc()` lookup. Not cached server-side (changes after QSO posts).

#### 3. `GET /api/v1/spots?live=true`

Returns the current Durable Object spot group snapshot — all active stations aggregated by `(dxCall, band, mode)`. Edge-cached with 10s TTL.

**Query parameters:**
- `live=true` (required) — returns DO snapshot instead of D1 archived spots
- `band` (optional) — filter to a specific band (e.g., `20m`, `40m`)
- `mode` (optional) — filter by mode (e.g., `FT8`, `FT4`, `CW`)
- `continent` (optional) — filter by DX continent

**Response:**
```json
{
  "groups": [
    {
      "dxCall": "3Y0J",
      "band": "20m",
      "mode": "FT8",
      "dxDxcc": 327,
      "dxEntityName": "Bouvet Island",
      "dxContinent": "AF",
      "dxCqZone": 38,
      "dxGrid": "JD15",
      "rarityRank": 1,
      "rarityTier": "legendary",
      "frequency": 14074000,
      "bestSnr": -12,
      "reporterCount": 5,
      "sources": ["pskreporter"],
      "firstSeen": 1743688920,
      "lastSeen": 1743689040,
      "confidence": 4.2
    }
  ]
}
```

**Note:** The response envelope key (`"groups"`) is assumed and needs validation against the live API. Timestamps are Unix epoch seconds. Rarity is an integer rank, not a float — pancetta converts via `rank_to_rarity()`: `1.0 - (rank - 1) / 339.0`.

**Usage:** Polled every 30 seconds. Pancetta scores and sorts client-side using its own priority engine. Cross-references against needed set from `/entities/needed`. Results feed into the autonomous operator's frequency nudge behavior.

#### 4. `POST /api/v1/spots/report`

Accepts a batch of decoded spots from end users. Separate from the trusted internal ingest pipeline.

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

**Rate limit:** 500 spots per minute per user. Validates callsign format, frequency range, required fields. Rejects malformed batches with 422.

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

**Rate limit:** 30 QSOs per minute per user.

**Usage:** Sent after each QSO completion. Fire-and-forget. The `version` field allows schema evolution. CQDX writes to a `qsos` table and upserts `userQsos` to mark entity/band/mode as worked (not confirmed — confirmation comes from LoTW/ClubLog sync).

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
    pub fn new(base_url: String, token: String) -> Self;

    pub async fn fetch_entities(&self) -> Result<Vec<DxccEntity>>;
    pub async fn fetch_needed(&self) -> Result<Vec<NeededEntity>>;
    pub async fn fetch_live_spots(&self, band: Option<&str>, mode: Option<&str>) -> Result<Vec<SpotGroup>>;
    pub async fn report_spots(&self, spots: Vec<SpotReport>) -> Result<()>;
    pub async fn report_qso(&self, qso: QsoRecord) -> Result<()>;
}
```

### `CqdxCache`

```rust
pub struct CqdxCache {
    prefixes: Vec<(String, u32)>,               // sorted longest-first for matching
    entities: HashMap<u32, DxccEntity>,          // ADIF number → entity
    needed_entity_ids: Option<HashSet<u32>>,     // None = no data (conservative)
    rarity_scores: HashMap<String, f64>,         // uppercase callsign → rarity 0.0–1.0
    spot_groups: Vec<SpotGroup>,                 // latest poll results
}

impl CqdxCache {
    pub fn new() -> Self;
    pub fn load_entities(&mut self, entities: Vec<DxccEntity>);
    pub fn load_needed(&mut self, needed: Vec<NeededEntity>);
    pub fn update_spot_groups(&mut self, groups: Vec<SpotGroup>);

    /// Resolve callsign → DXCC entity ADIF number using longest-prefix matching.
    pub fn resolve_entity(&self, callsign: &str) -> Option<u32>;

    /// Get rarity score for a callsign (0.0 = common, 1.0 = rare).
    /// Converted from CQDX's integer rarityRank via rank_to_rarity().
    pub fn rarity(&self, callsign: &str) -> f64;

    /// Get current live spot groups for frequency nudge decisions.
    pub fn spot_groups(&self) -> &[SpotGroup];
}
```

Rarity flows from `CqdxCache` → `CachedStationLookup` (thread-safe `Arc<RwLock<>>` wrapper implementing `WorkedStationLookup`) → `PriorityScorer`.

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

### Live Spot Polling (every 30s)

1. `CqdxClient::fetch_live_spots(current_band, current_mode)`
2. Convert `rarityRank` → float via `rank_to_rarity()`, update `CachedStationLookup` rarity scores
3. Update `CqdxCache.spot_groups`
4. If a high-rarity needed station is spotted on a different frequency within the current band, emit a `FrequencyNudge` event to the autonomous operator
5. Operator decides whether to QSY based on current state (mid-QSO = no, idle = yes)

### Spot Reporting (per decode cycle)

1. After each decode cycle, collect all decoded callsigns with grid/SNR/freq
2. Batch into one `POST /api/v1/spots/report` to cqdx.io (fire-and-forget via `tokio::spawn`)
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
| Poll live spots (not firehose) | Edge-cached 10s TTL; pancetta scores client-side using its own priority engine |
| Fire-and-forget outbound | Decode pipeline must never block on network I/O |
| Batch spots per cycle | One POST per 15s window, not per decoded signal |
| Versioned QSO format | `"version": 1` allows schema evolution without breaking |
| Mode-agnostic | PSKReporter and cqdx.io support many modes; don't limit to FT8 |
| Both grids in QSO | `remoteGrid` + `localGrid` for complete QSO geometry |
| Watchdog (2h) | Stop polling when station is inactive |
| Degraded mode | Pancetta works standalone; cqdx.io is an enhancement |
