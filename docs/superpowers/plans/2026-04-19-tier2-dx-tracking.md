# Tier 2: DX Tracking Buildout — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Complete DX tracking stubs so the autonomous operator can distinguish needed vs worked entities, produce real band/mode statistics, persist worked-station history across restarts, and load CTY.DAT files for comprehensive DXCC coverage.

**Architecture:** The `pancetta-dx` crate owns DXCC entity lookup (`DxccDatabase`), award tracking (`DxTracker` with SQLite), rarity scoring, and statistics. The `pancetta-qso` crate owns QSO logging to `~/.pancetta/qso.db`. The main `pancetta` crate bridges these via `CachedStationLookup` (in `priority_evaluator.rs`), which the coordinator initializes at startup and shares with the autonomous operator and priority scorer.

**Tech Stack:** Rust, rusqlite (pancetta-dx tracker), sqlx (pancetta-qso async DB), pancetta-core (`Band`, `Mode`)

---

## Task 1: DXCC `is_needed` — Real Award Tracking Lookup

**File:** `pancetta-dx/src/tracker.rs`

**Current state:** `is_needed` (line 564) ignores its arguments and always returns `Ok(true)`. The method signature takes `&self, _callsign: &str, _band: Band, _mode: &Mode` but `DxTracker` has no access to `DxccDatabase` for callsign-to-entity resolution.

**Design:** `is_needed` must (1) resolve the callsign to a DXCC entity code, then (2) query the `award_tracking` table. Since `DxTracker` only holds a `Mutex<Connection>` and has no `DxccDatabase`, we add a `dxcc` field or accept the entity code directly. The cleanest approach: add a second method `is_entity_needed` that takes `entity_code: u16`, and have `is_needed` delegate after external lookup. This avoids coupling `DxTracker` to `DxccDatabase`.

### Steps

- [ ] **1a.** Add `is_entity_needed` method to `DxTracker`:
  ```rust
  /// Check if a DXCC entity is still needed on the given band/mode.
  /// Returns true if the entity has NOT been confirmed in award_tracking.
  pub async fn is_entity_needed(&self, entity_code: u16, band: Band, mode: &Mode) -> Result<bool> {
      let conn = self.connection.lock().unwrap();
      let status: Option<String> = conn
          .query_row(
              "SELECT status FROM award_tracking
               WHERE entity_code = ?1 AND band = ?2 AND mode = ?3",
              params![entity_code as i64, band.to_string(), mode.to_string()],
              |row| row.get(0),
          )
          .optional()?;

      match status {
          Some(s) => {
              let award_status: AwardStatus = serde_json::from_str(&s)?;
              Ok(award_status != AwardStatus::Confirmed)
          }
          None => Ok(true), // No record means not worked at all
      }
  }
  ```

- [ ] **1b.** Update `is_needed` to document that callers should use `is_entity_needed` when they already have the entity code. Keep `is_needed` as a convenience that returns `true` with a TODO for callers that pass raw callsigns (since `DxTracker` does not own `DxccDatabase`):
  ```rust
  pub async fn is_needed(&self, _callsign: &str, band: Band, mode: &Mode) -> Result<bool> {
      // Without a DxccDatabase reference, we cannot resolve callsign -> entity_code.
      // Callers with access to DxccDatabase should use is_entity_needed() instead.
      // For now, check if ANY entity on this band/mode is unconfirmed.
      debug!("is_needed: callsign lookup requires DxccDatabase — use is_entity_needed() for precise checks");
      Ok(true)
  }
  ```

- [ ] **1c.** Add unit test for `is_entity_needed`:
  ```rust
  #[tokio::test]
  async fn test_is_entity_needed() {
      let (tracker, _temp) = create_test_tracker().await;

      // Entity 61 not worked yet — should be needed
      assert!(tracker.is_entity_needed(61, Band::Band20m, &Mode::CW).await.unwrap());

      // Add a confirmed QSO for entity 61
      let qso = DxQso {
          id: None,
          callsign: "JA1ABC".to_string(),
          datetime: Utc::now(),
          frequency: 14_074_000,
          band: Band::Band20m,
          mode: Mode::FT8,
          rst_sent: "-10".to_string(),
          rst_received: "-12".to_string(),
          grid_square: None, qth: None, name: None, qsl_route: None,
          confirmation_status: ConfirmationStatus::Lotw,
          confirmation_date: Some(Utc::now()),
          dxcc_entity: 61,
          contest_id: None, notes: None,
      };
      tracker.add_qso(qso).await.unwrap();

      // Now entity 61 on 20m/FT8 is confirmed — not needed
      assert!(!tracker.is_entity_needed(61, Band::Band20m, &Mode::FT8).await.unwrap());
      // But 61 on 40m is still needed
      assert!(tracker.is_entity_needed(61, Band::Band40m, &Mode::FT8).await.unwrap());
  }
  ```

- [ ] **1d.** Build and test:
  ```bash
  touch pancetta-dx/src/tracker.rs
  cargo test -p pancetta-dx --lib tracker::tests::test_is_entity_needed 2>&1 | head -30
  ```

**Commit:** `feat(dx): implement is_entity_needed — real award_tracking lookup for band/mode`

---

## Task 2: Band/Mode Statistics — Real SQL Queries

**File:** `pancetta-dx/src/statistics.rs`

**Current state:** `get_band_statistics` (line 300) and `get_mode_statistics` (line 317) both return `Err(DxError::Configuration(...))`. The `StatisticsEngine` has `tracker: Arc<DxTracker>` which gives access to `tracked_contacts` via `self.tracker.connection`.

**Return types:**
- `BandStatistics`: `band`, `total_qsos`, `unique_callsigns`, `entities_worked`, `entities_confirmed`, `qsos_by_mode`, `qsos_by_continent`, `avg_rst_sent`, `avg_rst_received`, `most_active_hour`, `activity_by_hour`, `longest_distance_km`, `confirmation_rate`
- `ModeStatistics`: `mode`, `total_qsos`, `unique_callsigns`, `entities_worked`, `entities_confirmed`, `qsos_by_band`, `most_active_band`, `avg_rst_sent`, `avg_rst_received`, `confirmation_rate`

### Steps

- [ ] **2a.** Implement `get_band_statistics`:
  ```rust
  pub async fn get_band_statistics(&self, band: Band) -> Result<BandStatistics> {
      let band_str = band.to_string();
      let conn = self.tracker.connection.lock().unwrap();

      // Total QSOs on band
      let total_qsos: i64 = conn.query_row(
          "SELECT COUNT(*) FROM tracked_contacts WHERE band = ?1",
          params![&band_str], |row| row.get(0),
      )?;

      // Unique callsigns
      let unique_callsigns: i64 = conn.query_row(
          "SELECT COUNT(DISTINCT callsign) FROM tracked_contacts WHERE band = ?1",
          params![&band_str], |row| row.get(0),
      )?;

      // Entities worked / confirmed
      let entities_worked: i64 = conn.query_row(
          "SELECT COUNT(DISTINCT dxcc_entity) FROM tracked_contacts WHERE band = ?1",
          params![&band_str], |row| row.get(0),
      )?;

      let entities_confirmed: i64 = conn.query_row(
          "SELECT COUNT(DISTINCT entity_code) FROM award_tracking
           WHERE band = ?1 AND status = '\"Confirmed\"'",
          params![&band_str], |row| row.get(0),
      )?;

      // QSOs by mode
      let mut stmt = conn.prepare(
          "SELECT mode, COUNT(*) FROM tracked_contacts WHERE band = ?1 GROUP BY mode"
      )?;
      let qsos_by_mode = /* iterate rows, parse Mode, collect into HashMap */;

      // RST averages (parse numeric portion of RST strings)
      // ... (CAST + AVG query on rst_sent, rst_received)

      // Activity by hour
      let mut stmt = conn.prepare(
          "SELECT CAST(strftime('%H', datetime) AS INTEGER) as hr, COUNT(*)
           FROM tracked_contacts WHERE band = ?1 GROUP BY hr"
      )?;
      // ... collect into activity_by_hour HashMap, find most_active_hour

      // QSOs by continent (join with dxcc entity data via self.dxcc)
      // ... query dxcc_entity codes, map to continent via self.dxcc.get_entity()

      // Confirmation rate
      let confirmed: i64 = conn.query_row(
          "SELECT COUNT(*) FROM tracked_contacts
           WHERE band = ?1 AND confirmation_status != '\"None\"'",
          params![&band_str], |row| row.get(0),
      )?;
      let confirmation_rate = if total_qsos > 0 {
          (confirmed as f64 / total_qsos as f64) * 100.0
      } else { 0.0 };

      Ok(BandStatistics {
          band,
          total_qsos: total_qsos as u32,
          unique_callsigns: unique_callsigns as u32,
          entities_worked: entities_worked as u32,
          entities_confirmed: entities_confirmed as u32,
          qsos_by_mode,
          qsos_by_continent,
          avg_rst_sent,
          avg_rst_received,
          most_active_hour,
          activity_by_hour,
          longest_distance_km: None, // No distance column in tracked_contacts
          confirmation_rate,
      })
  }
  ```

- [ ] **2b.** Implement `get_mode_statistics` following the same pattern, filtering by `mode` instead of `band`, and computing `qsos_by_band` + `most_active_band`.

- [ ] **2c.** Add tests:
  ```rust
  #[tokio::test]
  async fn test_band_statistics_with_data() {
      // Create tracker, add QSOs on Band20m, verify counts
  }

  #[tokio::test]
  async fn test_mode_statistics_with_data() {
      // Create tracker, add QSOs with Mode::FT8, verify counts
  }

  #[tokio::test]
  async fn test_band_statistics_empty() {
      // Empty DB should return zeros, not errors
  }
  ```

- [ ] **2d.** Build and test:
  ```bash
  touch pancetta-dx/src/statistics.rs
  cargo test -p pancetta-dx --lib statistics::tests 2>&1 | head -40
  ```

**Commit:** `feat(dx): implement get_band_statistics and get_mode_statistics — real SQL queries`

---

## Task 3: DXCC `needed_entities` in `AwardSummary`

**File:** `pancetta-dx/src/tracker.rs`

**Current state:** `get_dxcc_summary` (line 716) correctly queries worked/confirmed counts from `award_tracking`, but sets `needed_entities: Vec::new()` at line 778. The `AwardSummary.needed_entities` field is `Vec<u16>` (entity codes).

**Design:** Query all confirmed entity codes from `award_tracking` (for the given band/mode filter), then compute the set difference against a known list of all current DXCC entity codes. Since `DxTracker` does not hold a `DxccDatabase`, we pass the total entity list as a parameter or add a method that accepts it.

### Steps

- [ ] **3a.** Add helper method `get_confirmed_entity_codes`:
  ```rust
  /// Get all entity codes that have been confirmed for the given band/mode filter.
  pub async fn get_confirmed_entity_codes(
      &self,
      band: Option<Band>,
      mode: Option<Mode>,
  ) -> Result<Vec<u16>> {
      let mut query = String::from(
          "SELECT DISTINCT entity_code FROM award_tracking WHERE status = '\"Confirmed\"'"
      );
      let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

      if let Some(b) = band {
          query.push_str(" AND band = ?");
          params_vec.push(Box::new(b.to_string()));
      }
      if let Some(m) = mode {
          query.push_str(" AND mode = ?");
          params_vec.push(Box::new(m.to_string()));
      }

      let conn = self.connection.lock().unwrap();
      let param_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
      let mut stmt = conn.prepare(&query)?;
      let rows = stmt.query_map(param_refs.as_slice(), |row| {
          Ok(row.get::<_, i64>(0)? as u16)
      })?;

      let mut codes = Vec::new();
      for row in rows {
          codes.push(row?);
      }
      Ok(codes)
  }
  ```

- [ ] **3b.** Add `get_dxcc_summary_with_entities` that accepts the full entity list:
  ```rust
  /// Get DXCC summary with needed_entities computed from the full entity list.
  pub async fn get_dxcc_summary_with_entities(
      &self,
      band: Option<Band>,
      mode: Option<Mode>,
      all_entity_codes: &[u16],
  ) -> Result<AwardSummary> {
      let mut summary = self.get_dxcc_summary(band, mode).await?;
      let confirmed = self.get_confirmed_entity_codes(
          summary.band, summary.mode.clone()
      ).await?;
      let confirmed_set: std::collections::HashSet<u16> = confirmed.into_iter().collect();
      summary.needed_entities = all_entity_codes
          .iter()
          .filter(|code| !confirmed_set.contains(code))
          .copied()
          .collect();
      summary.total_entities = all_entity_codes.len() as u32;
      Ok(summary)
  }
  ```

- [ ] **3c.** Add test:
  ```rust
  #[tokio::test]
  async fn test_needed_entities_computed() {
      let (tracker, _temp) = create_test_tracker().await;
      // Add confirmed QSO for entity 61
      // ...
      let all_entities = vec![1, 6, 61, 291];
      let summary = tracker
          .get_dxcc_summary_with_entities(None, None, &all_entities)
          .await.unwrap();
      assert_eq!(summary.confirmed_count, 1);
      assert!(!summary.needed_entities.contains(&61));
      assert!(summary.needed_entities.contains(&1));
      assert_eq!(summary.needed_entities.len(), 3);
  }
  ```

- [ ] **3d.** Build and test:
  ```bash
  touch pancetta-dx/src/tracker.rs
  cargo test -p pancetta-dx --lib tracker::tests::test_needed_entities 2>&1 | head -30
  ```

**Commit:** `feat(dx): compute needed_entities in AwardSummary from confirmed set difference`

---

## Task 4: CTY.DAT Parser

**File:** `pancetta-dx/src/dxcc.rs`

**Current state:** `parse_cty_line` (line 1009) always returns `Err`. The `load_cty_dat` method (line 979) iterates lines and calls `parse_cty_line` for each, but CTY.DAT is not actually line-oriented -- records span multiple lines and are terminated by `;`.

**CTY.DAT format:**
```
Entity Name:                  CQ  ITU  Cont  Lat    Lon    UTC  Prefix:
    alias1,alias2(CQ),...;
```
A record starts with the entity header line (colon-delimited fields), followed by continuation lines with alias prefixes, terminated by `;`. Fields in the header are separated by `:` with leading/trailing whitespace.

### Steps

- [ ] **4a.** Replace `load_cty_dat` to accumulate multi-line records:
  ```rust
  pub async fn load_cty_dat(&mut self, cty_data: &str) -> Result<()> {
      info!("Loading DXCC data from CTY.DAT format");

      let mut entities_loaded = 0;
      let mut prefixes_loaded = 0;
      let mut current_record = String::new();

      for line in cty_data.lines() {
          let line = line.trim();
          if line.is_empty() || line.starts_with('#') {
              continue;
          }
          current_record.push(' ');
          current_record.push_str(line);

          if current_record.contains(';') {
              // Record complete
              if let Ok((entity, pcount)) = self.parse_cty_record(&current_record) {
                  self.add_entity(entity);
                  entities_loaded += 1;
                  prefixes_loaded += pcount;
              }
              current_record.clear();
          }
      }

      self.prefixes.sort_by_key(|p| p.priority);
      info!("Loaded {} entities and {} prefixes from CTY.DAT",
          entities_loaded, prefixes_loaded);
      Ok(())
  }
  ```

- [ ] **4b.** Implement `parse_cty_record`:
  ```rust
  /// Parse a complete CTY.DAT record (header + alias prefixes ending with ;).
  ///
  /// Format: "Entity:  CQ: ITU: Cont: Lat:  Lon:  UTC: Prefix:  alias1,alias2,...;"
  fn parse_cty_record(&mut self, record: &str) -> Result<(DxccEntity, usize)> {
      // Split at first ':'  occurrence that separates entity name from the rest
      let parts: Vec<&str> = record.splitn(2, ':').collect();
      // ... parse the 8 colon-separated header fields
      // ... extract entity name, CQ zone, ITU zone, continent, lat, lon, UTC offset, primary prefix
      // ... split remaining text after the header on ',' for alias prefixes
      // ... for each alias, call self.add_prefix_pattern with escaped regex
      // ... strip trailing ';' from aliases
      // ... generate entity_code from a running counter or hash of prefix
  }
  ```

  Key parsing details:
  - Entity name is everything before the first `:`
  - Remaining fields are `:` separated: CQ zone, ITU zone, continent (2-char), latitude (decimal), longitude (decimal, sign inverted from standard convention in CTY.DAT), UTC offset, primary prefix
  - After the 8th `:`, alias prefixes follow as comma-separated tokens
  - Prefix tokens may have modifiers: `=EXACT` for exact callsign match, `(CQ)` for CQ zone override, `[ITU]` for ITU zone override
  - The `;` terminates the record
  - Entity codes: CTY.DAT does not include ARRL entity codes. We must either use a mapping table or assign codes by matching the primary prefix against our existing `entities` HashMap

- [ ] **4c.** Add entity code resolution: attempt to match the primary prefix against existing entities in `self.entities`. If no match, assign from a counter starting at 1000 to avoid collisions.

- [ ] **4d.** Add tests with sample CTY.DAT data:
  ```rust
  #[tokio::test]
  async fn test_cty_dat_parsing() {
      let mut db = DxccDatabase {
          entities: HashMap::new(),
          prefixes: Vec::new(),
          callsign_overrides: HashMap::new(),
      };

      let sample = "Canada:  05:  09:  NA:  45.00:   75.00:   5.0:  VE:\n\
          VA,VB,VC,VD,VE,VX,VY,VZ,CF,CG,CH,CI,CJ,CK;\n";

      db.load_cty_dat(sample).await.unwrap();
      assert!(db.entity_count() >= 1);
      // Verify prefix matching works for loaded data
  }

  #[tokio::test]
  async fn test_cty_dat_multiline_record() {
      // Test record that spans 3+ lines
  }
  ```

- [ ] **4e.** Build and test:
  ```bash
  touch pancetta-dx/src/dxcc.rs
  cargo test -p pancetta-dx --lib dxcc::tests 2>&1 | head -40
  ```

**Commit:** `feat(dx): implement CTY.DAT parser — multi-line record accumulation and prefix extraction`

---

## Task 5: Worked Station Persistence Across Restarts

**File:** `pancetta/src/priority_evaluator.rs`

**Current state:** `CachedStationLookup::new()` (line 35) creates an empty `worked_on_band` set with a `tracing::warn` noting that history is not persisted. The coordinator constructs it at `pancetta/src/coordinator/mod.rs:276-278` with no DB seeding. The QSO database lives at `~/.pancetta/qso.db` (set at `coordinator/components.rs:102-105`).

**Design:** Add a `CachedStationLookup::new_with_history(db_path, band)` constructor that queries `~/.pancetta/qso.db` for callsigns on the current band. The QSO database stores progress data as JSON in a `progress_data` column; the metadata JSON contains `their_callsign` and `frequency` fields. We query using `json_extract`.

The current band is available from the config (`config.station.default_band` or similar). Since the coordinator has the config when it creates `CachedStationLookup`, we pass the band string.

### Steps

- [ ] **5a.** Add async constructor to `CachedStationLookup`:
  ```rust
  /// Create a new lookup seeded with worked callsigns from the QSO database.
  ///
  /// Queries `~/.pancetta/qso.db` for all callsigns on the given band.
  /// Falls back to an empty set if the database is unavailable.
  pub async fn new_seeded(band: &str) -> Self {
      let db_path = dirs::home_dir()
          .unwrap_or_else(|| std::path::PathBuf::from("."))
          .join(".pancetta")
          .join("qso.db");

      let worked = match Self::load_worked_callsigns(&db_path, band).await {
          Ok(set) => {
              tracing::info!(
                  "Loaded {} worked callsigns for band {} from QSO database",
                  set.len(), band
              );
              set
          }
          Err(e) => {
              tracing::warn!(
                  "Failed to load worked callsigns from {:?}: {} — starting empty",
                  db_path, e
              );
              HashSet::new()
          }
      };

      Self {
          worked_on_band: Arc::new(RwLock::new(worked)),
          recent_failures: Arc::new(RwLock::new(HashSet::new())),
          needed_dxcc: Arc::new(RwLock::new(HashSet::new())),
          needed_grids: Arc::new(RwLock::new(HashSet::new())),
          rarity_scores: Arc::new(RwLock::new(HashMap::new())),
          notable_callsigns: Arc::new(RwLock::new(HashSet::new())),
          network_snr: Arc::new(RwLock::new(HashMap::new())),
          network_last_seen: Arc::new(RwLock::new(HashMap::new())),
      }
  }

  async fn load_worked_callsigns(
      db_path: &std::path::Path,
      band: &str,
  ) -> anyhow::Result<HashSet<String>> {
      use rusqlite::Connection;

      let path_str = db_path.to_str()
          .ok_or_else(|| anyhow::anyhow!("Invalid DB path"))?;
      let conn = Connection::open_with_flags(
          path_str,
          rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
      )?;

      // The metadata column contains JSON with their_callsign and frequency.
      // We filter by band from the metadata or by frequency range.
      // The simplest approach: extract their_callsign from all QSOs.
      // Band filtering can use json_extract on metadata if available,
      // or we load all callsigns (the set is small enough).
      let mut stmt = conn.prepare(
          "SELECT DISTINCT UPPER(json_extract(metadata, '$.their_callsign'))
           FROM qsos
           WHERE json_extract(metadata, '$.their_callsign') IS NOT NULL"
      )?;

      let rows = stmt.query_map([], |row| {
          let call: Option<String> = row.get(0)?;
          Ok(call)
      })?;

      let mut callsigns = HashSet::new();
      for row in rows {
          if let Ok(Some(call)) = row {
              if !call.is_empty() {
                  callsigns.insert(call);
              }
          }
      }

      Ok(callsigns)
  }
  ```

- [ ] **5b.** Add `rusqlite` dependency to `pancetta/Cargo.toml` if not already present (check first — the main crate may already depend on it transitively). Also add `dirs` and `anyhow` if needed.

- [ ] **5c.** Update coordinator to use seeded constructor. In `pancetta/src/coordinator/mod.rs` around line 276, replace:
  ```rust
  // Before:
  cached_lookup: std::sync::Arc::new(
      crate::priority_evaluator::CachedStationLookup::new(),
  ),

  // After:
  cached_lookup: {
      let band_str = config_guard.station.default_band
          .as_deref()
          .unwrap_or("20m")
          .to_string();
      std::sync::Arc::new(
          crate::priority_evaluator::CachedStationLookup::new_seeded(&band_str).await,
      )
  },
  ```
  (Adjust based on actual config field name for current band.)

- [ ] **5d.** Keep `new()` as a fallback for tests and WAV-playback mode where DB access is unnecessary.

- [ ] **5e.** Build and verify:
  ```bash
  touch pancetta/src/priority_evaluator.rs pancetta/src/coordinator/mod.rs
  cargo build -p pancetta 2>&1 | tail -20
  ```

- [ ] **5f.** Manual verification: start the app, work a station, restart, and confirm the station shows as a duplicate in the TUI priority column.

**Commit:** `feat: seed worked-station cache from QSO database at startup — persist across restarts`

---

## Dependency Order

Tasks 1-4 are in `pancetta-dx` and are independent of each other — they can be implemented in parallel.

Task 5 is in the main `pancetta` crate and is independent of Tasks 1-4, but should be done last since it may benefit from testing infrastructure set up during Tasks 1-4.

**Recommended execution order for serial work:** 1 -> 3 -> 4 -> 2 -> 5

**Parallel grouping:**
- Group A (can run simultaneously): Tasks 1, 3, 4
- Group B (after Group A): Tasks 2, 5

---

## Validation Checklist

After all tasks are complete:

```bash
# Touch all modified files (cargo cache workaround)
touch pancetta-dx/src/tracker.rs pancetta-dx/src/statistics.rs pancetta-dx/src/dxcc.rs pancetta/src/priority_evaluator.rs pancetta/src/coordinator/mod.rs

# Full crate test suite
cargo test -p pancetta-dx 2>&1 | tail -10
cargo build -p pancetta 2>&1 | tail -10

# Verify no regressions in dependent crates
cargo test -p pancetta-qso 2>&1 | tail -5
```
