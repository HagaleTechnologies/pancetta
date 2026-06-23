# cqdx.io API Requirements for Pancetta Integration

> **Date:** 2026-04-03  
> **Consumer:** pancetta (Rust ham radio FT8 application)  
> **Auth model:** Personal Access Tokens (PATs) via Bearer header

## Overview

Pancetta needs 5 API endpoints on cqdx.io to replace its hardcoded stubs with live data. All endpoints are authenticated via PAT. The integration is first-party — pancetta and cqdx.io are built by the same person.

## Prompt for cqdx.io Claude Code Session

Copy-paste this to the cqdx.io Claude Code session:

---

**Implement the following 5 REST API endpoints for pancetta integration. All endpoints require Bearer token auth using Personal Access Tokens (PATs). If cqdx.io doesn't have a PAT system yet, implement one first (token creation, revocation, per-user scoping). All endpoints live under `/api/v1/`.**

**Read the full requirements in this file before starting:** `docs/cqdx-api-requirements.md` (copy this file into the cqdx repo first, or paste the endpoint specs below).

---

## Authentication

All endpoints require:
```
Authorization: Bearer pat_xxxxxxxxxxxx
```

If PAT infrastructure doesn't exist yet, implement:
- Token generation (user creates PATs from their account)
- Token revocation
- Per-user scoping (each PAT is tied to one user)
- Tokens should be stored hashed, not plaintext

Return `401 Unauthorized` for missing/invalid/revoked tokens.

## Endpoints

### 1. `GET /api/v1/entities`

Returns the full DXCC entity list with prefix-to-entity mappings.

**Response (200):**
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

**Notes:**
- This is the complete DXCC entity table (currently ~340 entities)
- Pancetta fetches this once on startup and caches it for the session
- Include all active DXCC entities with their primary prefix
- `id` should match the standard DXCC entity number (e.g., 291 = United States)

### 2. `GET /api/v1/entities/needed`

Returns DXCC entities the authenticated user still needs (not yet confirmed).

**Response (200):**
```json
{
  "needed": [
    { "entityId": 327, "name": "Bouvet Island", "prefix": "3Y/B" }
  ]
}
```

**Notes:**
- "Needed" means the user has NOT confirmed a QSO with this entity
- If the user has no log data, return ALL entities as needed
- Pancetta fetches this once on startup and caches it
- This drives the `needed_dxcc` scoring factor in pancetta's priority engine

### 3. `GET /api/v1/spots/priorities`

Pre-scored, sorted list of high-value spot targets filtered by the authenticated user's needed/worked status.

**Query parameters:**
| Param | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `band` | string | no | all bands | Filter by band (e.g., `20m`, `40m`) |
| `mode` | string | no | all modes | Filter by mode (e.g., `FT8`, `FT4`, `CW`) |
| `limit` | integer | no | 20 | Max results to return |

**Response (200):**
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

**Notes:**
- Results MUST be sorted by priority (highest first)
- `rarity` is a float 0.0 (very common, e.g., K/W stations) to 1.0 (extremely rare DX)
- `needed` is relative to the authenticated user's log
- `spotCount` = number of unique reporters who spotted this callsign recently
- Only include stations spotted within the last ~15 minutes (configurable server-side)
- Pancetta polls this every 30 seconds
- This is the most performance-sensitive endpoint — keep it fast

### 4. `POST /api/v1/spots/ingest`

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

**Notes:**
- Pancetta sends one batch per decode cycle (~every 15 seconds)
- A batch typically contains 0–30 spots
- Mode is not limited to FT8 — accept any mode string
- `reporter` is the pancetta operator's callsign; `reporterGrid` is their grid
- Store spots for use by the priorities endpoint and general spot aggregation
- Deduplicate if the same reporter reports the same callsign+freq within a short window

### 5. `POST /api/v1/qsos`

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

**Notes:**
- `version` field enables schema evolution — currently always `1`
- Both `remoteGrid` and `localGrid` are included for complete QSO geometry
- `rstSent`/`rstReceived` are strings (SNR values like "-10", or RST like "599")
- Mode is not limited to FT8
- Logging a QSO should update the user's "needed" entities (a confirmed QSO with Bouvet removes it from the needed list)
- This is a clublog-style QSO logging system — the user's confirmed QSOs live on cqdx.io

## Error Responses

All endpoints should return consistent error format:
```json
{
  "error": {
    "code": "UNAUTHORIZED",
    "message": "Invalid or expired token"
  }
}
```

Standard HTTP status codes:
- `400` — malformed request
- `401` — missing/invalid/revoked token
- `404` — endpoint not found
- `422` — valid JSON but invalid field values
- `429` — rate limited (if applicable)
- `500` — server error

## Performance Expectations

- `GET /api/v1/spots/priorities` is polled every 30 seconds — should respond in <500ms
- `POST /api/v1/spots/ingest` receives a batch every ~15 seconds — 202 quickly, process async
- `GET /api/v1/entities` and `GET /api/v1/entities/needed` are called once on startup — can be slower
- `POST /api/v1/qsos` is called infrequently (after each completed QSO) — not performance-critical

### `GET /api/v1/grids/needed`

Returns grid squares the user still needs for grid-chasing awards.

**Response (200):**
```json
{
  "grids": ["AA00", "AB01", "..."]
}
```

**Notes:**
- "Needed" means the user has NOT confirmed a QSO with this grid square
- If the user has no log data, return ALL grids as needed
- Pancetta fetches this on startup and caches it for the session
- This drives the `needed_grids` scoring factor in pancetta's priority engine
- **Priority:** Low — conservative fallback (treat all grids as needed) is acceptable until this endpoint exists.

### `GET /api/v1/entities/needed?band=<band>` — per-band DXCC needs (PROPOSED)

Today `GET /api/v1/entities/needed` returns **all-time** needs (an entity not
confirmed on ANY band — i.e. ATNO). cqdx already understands **per-band** DXCC
status; this proposal exposes it so pancetta can distinguish "needed on the band
I'm operating right now" (a band-fill) from "needed everywhere" (ATNO).

**Query parameters:**
| Param | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `band` | string | no | (all-time) | Filter to entities NOT confirmed on this band (e.g. `20m`, `40m`). Omitted → current all-time/ATNO behavior, unchanged. |

**Response (200):** same shape as `/entities/needed`, plus an optional `atno` flag:
```json
{
  "needed": [
    { "entityId": 327, "name": "Bouvet Island", "prefix": "3Y/B", "atno": true },
    { "entityId": 291, "name": "United States",  "prefix": "K",    "atno": false }
  ]
}
```

**Notes:**
- **Without `band`** (existing call): entity is "needed" iff not confirmed on ANY
  band (ATNO). **Behavior is unchanged — fully backward compatible.**
- **With `band`**: entity is "needed" iff not confirmed on THAT band, even if
  worked on other bands. This is the per-band DXCC chase. In the example,
  "United States" is needed on the queried band (band-fill) but is not an ATNO.
- `atno` (optional boolean, per entity): `true` if the entity is also needed on
  every band. Lets pancetta render an ATNO badge vs a band-fill marker from a
  single response, no second call.
- If the user has no log data, return ALL entities as needed (`atno: true`).
- Pancetta fetches the per-band set on startup and re-fetches on band change,
  keyed to the current operating band; it keeps the all-time set too (for the
  ATNO badge fallback when `atno` is absent).
- **Priority:** Medium — drives a `needed_dxcc_this_band` scoring factor and the
  DX Hunter ATNO/band-fill display. Pancetta degrades gracefully (treats the
  per-band set as empty/inert) on 404 until this ships, exactly like
  `/entities/needed-grids`.
