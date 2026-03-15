//! DXCC Entity Database and Lookup
//!
//! This module provides comprehensive DXCC (DX Century Club) entity management
//! including entity lookup by callsign, prefix matching, and entity information.

use crate::{DxError, Result};
use chrono::NaiveDate;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{debug, info, warn};

/// DXCC Entity information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DxccEntity {
    /// DXCC entity code
    pub entity_code: u16,
    /// Entity name
    pub name: String,
    /// Prefix
    pub prefix: String,
    /// ITU zone
    pub itu_zone: u8,
    /// CQ zone  
    pub cq_zone: u8,
    /// Continent code
    pub continent: String,
    /// Latitude in decimal degrees
    pub latitude: f64,
    /// Longitude in decimal degrees
    pub longitude: f64,
    /// UTC offset in hours
    pub utc_offset: f32,
    /// Country/territory name
    pub country: String,
    /// DXCC status (Current, Deleted, etc.)
    pub status: DxccStatus,
    /// Date entity was added
    pub start_date: Option<NaiveDate>,
    /// Date entity was deleted (if applicable)
    pub end_date: Option<NaiveDate>,
    /// Notes about the entity
    pub notes: Option<String>,
}

/// DXCC Entity status
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DxccStatus {
    /// Current active entity
    Current,
    /// Deleted entity (no longer valid for DXCC credit)
    Deleted,
    /// Not in DXCC list (but may be valid for other awards)
    NotDxcc,
}

/// Callsign prefix information for lookup
#[derive(Debug, Clone)]
struct PrefixInfo {
    /// The prefix pattern (may include wildcards)
    prefix: String,
    /// DXCC entity code this prefix belongs to
    entity_code: u16,
    /// Priority for matching (lower = higher priority)
    priority: u8,
    /// Compiled regex for matching
    regex: Regex,
}

/// DXCC Database manager
pub struct DxccDatabase {
    /// Map of entity code to entity information
    entities: HashMap<u16, DxccEntity>,
    /// Prefix information for callsign lookup, sorted by priority
    prefixes: Vec<PrefixInfo>,
    /// Special callsign overrides
    callsign_overrides: HashMap<String, u16>,
}

impl DxccDatabase {
    /// Create new DXCC database
    pub async fn new() -> Result<Self> {
        let mut database = Self {
            entities: HashMap::new(),
            prefixes: Vec::new(),
            callsign_overrides: HashMap::new(),
        };

        database.load_default_data().await?;
        Ok(database)
    }

    /// Load default DXCC data
    async fn load_default_data(&mut self) -> Result<()> {
        info!("Loading DXCC entity database");

        // Load core DXCC entities (this would typically be loaded from a file or database)
        self.add_entity(DxccEntity {
            entity_code: 1,
            name: "CANADA".to_string(),
            prefix: "VE".to_string(),
            itu_zone: 9,
            cq_zone: 5,
            continent: "NA".to_string(),
            latitude: 45.0,
            longitude: -75.0,
            utc_offset: -5.0,
            country: "Canada".to_string(),
            status: DxccStatus::Current,
            start_date: None,
            end_date: None,
            notes: None,
        });

        self.add_entity(DxccEntity {
            entity_code: 6,
            name: "UNITED STATES".to_string(),
            prefix: "K".to_string(),
            itu_zone: 8,
            cq_zone: 5,
            continent: "NA".to_string(),
            latitude: 40.0,
            longitude: -95.0,
            utc_offset: -6.0,
            country: "United States".to_string(),
            status: DxccStatus::Current,
            start_date: None,
            end_date: None,
            notes: None,
        });

        self.add_entity(DxccEntity {
            entity_code: 14,
            name: "SPAIN".to_string(),
            prefix: "EA".to_string(),
            itu_zone: 37,
            cq_zone: 14,
            continent: "EU".to_string(),
            latitude: 40.0,
            longitude: -4.0,
            utc_offset: 1.0,
            country: "Spain".to_string(),
            status: DxccStatus::Current,
            start_date: None,
            end_date: None,
            notes: None,
        });

        self.add_entity(DxccEntity {
            entity_code: 61,
            name: "JAPAN".to_string(),
            prefix: "JA".to_string(),
            itu_zone: 45,
            cq_zone: 25,
            continent: "AS".to_string(),
            latitude: 36.0,
            longitude: 138.0,
            utc_offset: 9.0,
            country: "Japan".to_string(),
            status: DxccStatus::Current,
            start_date: None,
            end_date: None,
            notes: None,
        });

        self.add_entity(DxccEntity {
            entity_code: 78,
            name: "FEDERAL REPUBLIC OF GERMANY".to_string(),
            prefix: "DL".to_string(),
            itu_zone: 28,
            cq_zone: 14,
            continent: "EU".to_string(),
            latitude: 51.0,
            longitude: 9.0,
            utc_offset: 1.0,
            country: "Germany".to_string(),
            status: DxccStatus::Current,
            start_date: None,
            end_date: None,
            notes: None,
        });

        self.add_entity(DxccEntity {
            entity_code: 291,
            name: "UNITED STATES".to_string(),
            prefix: "W".to_string(),
            itu_zone: 8,
            cq_zone: 5,
            continent: "NA".to_string(),
            latitude: 40.0,
            longitude: -95.0,
            utc_offset: -6.0,
            country: "United States".to_string(),
            status: DxccStatus::Current,
            start_date: None,
            end_date: None,
            notes: Some("Alternative prefix for United States".to_string()),
        });

        // Add prefix patterns for callsign lookup
        self.add_prefix_pattern("^VE[0-9]", 1, 1)?;
        self.add_prefix_pattern("^VA[0-9]", 1, 1)?;
        self.add_prefix_pattern("^VB[0-9]", 1, 1)?;
        self.add_prefix_pattern("^VC[0-9]", 1, 1)?;
        self.add_prefix_pattern("^VD[0-9]", 1, 1)?;
        self.add_prefix_pattern("^VX[0-9]", 1, 1)?;
        self.add_prefix_pattern("^VY[0-9]", 1, 1)?;
        self.add_prefix_pattern("^VZ[0-9]", 1, 1)?;

        self.add_prefix_pattern("^W[0-9]", 6, 1)?;
        self.add_prefix_pattern("^K[0-9]", 6, 1)?;
        self.add_prefix_pattern("^N[0-9]", 6, 1)?;
        self.add_prefix_pattern("^AA[0-9]", 6, 2)?;
        self.add_prefix_pattern("^AB[0-9]", 6, 2)?;
        self.add_prefix_pattern("^AC[0-9]", 6, 2)?;
        self.add_prefix_pattern("^AD[0-9]", 6, 2)?;
        self.add_prefix_pattern("^AE[0-9]", 6, 2)?;
        self.add_prefix_pattern("^AF[0-9]", 6, 2)?;
        self.add_prefix_pattern("^AG[0-9]", 6, 2)?;
        self.add_prefix_pattern("^AH[0-9]", 6, 2)?;
        self.add_prefix_pattern("^AI[0-9]", 6, 2)?;
        self.add_prefix_pattern("^AJ[0-9]", 6, 2)?;
        self.add_prefix_pattern("^AK[0-9]", 6, 2)?;
        self.add_prefix_pattern("^AL[0-9]", 6, 2)?;

        self.add_prefix_pattern("^EA[0-9]", 14, 1)?;
        self.add_prefix_pattern("^EB[0-9]", 14, 2)?;
        self.add_prefix_pattern("^EC[0-9]", 14, 2)?;
        self.add_prefix_pattern("^ED[0-9]", 14, 2)?;
        self.add_prefix_pattern("^EE[0-9]", 14, 2)?;
        self.add_prefix_pattern("^EF[0-9]", 14, 2)?;
        self.add_prefix_pattern("^EG[0-9]", 14, 2)?;
        self.add_prefix_pattern("^EH[0-9]", 14, 2)?;

        self.add_prefix_pattern("^JA[0-9]", 61, 1)?;
        self.add_prefix_pattern("^JE[0-9]", 61, 2)?;
        self.add_prefix_pattern("^JF[0-9]", 61, 2)?;
        self.add_prefix_pattern("^JG[0-9]", 61, 2)?;
        self.add_prefix_pattern("^JH[0-9]", 61, 2)?;
        self.add_prefix_pattern("^JI[0-9]", 61, 2)?;
        self.add_prefix_pattern("^JJ[0-9]", 61, 2)?;
        self.add_prefix_pattern("^JK[0-9]", 61, 2)?;
        self.add_prefix_pattern("^JL[0-9]", 61, 2)?;
        self.add_prefix_pattern("^JM[0-9]", 61, 2)?;
        self.add_prefix_pattern("^JN[0-9]", 61, 2)?;
        self.add_prefix_pattern("^JO[0-9]", 61, 2)?;
        self.add_prefix_pattern("^JP[0-9]", 61, 2)?;
        self.add_prefix_pattern("^JQ[0-9]", 61, 2)?;
        self.add_prefix_pattern("^JR[0-9]", 61, 2)?;
        self.add_prefix_pattern("^JS[0-9]", 61, 2)?;

        self.add_prefix_pattern("^DL[0-9]", 78, 1)?;
        self.add_prefix_pattern("^DA[0-9]", 78, 2)?;
        self.add_prefix_pattern("^DB[0-9]", 78, 2)?;
        self.add_prefix_pattern("^DC[0-9]", 78, 2)?;
        self.add_prefix_pattern("^DD[0-9]", 78, 2)?;
        self.add_prefix_pattern("^DE[0-9]", 78, 2)?;
        self.add_prefix_pattern("^DF[0-9]", 78, 2)?;
        self.add_prefix_pattern("^DG[0-9]", 78, 2)?;
        self.add_prefix_pattern("^DH[0-9]", 78, 2)?;
        self.add_prefix_pattern("^DI[0-9]", 78, 2)?;
        self.add_prefix_pattern("^DJ[0-9]", 78, 2)?;
        self.add_prefix_pattern("^DK[0-9]", 78, 2)?;
        self.add_prefix_pattern("^DM[0-9]", 78, 2)?;
        self.add_prefix_pattern("^DN[0-9]", 78, 2)?;
        self.add_prefix_pattern("^DO[0-9]", 78, 2)?;

        // Sort prefixes by priority for optimal matching
        self.prefixes.sort_by_key(|p| p.priority);

        info!(
            "Loaded {} DXCC entities and {} prefix patterns",
            self.entities.len(),
            self.prefixes.len()
        );

        Ok(())
    }

    /// Add a DXCC entity to the database
    pub fn add_entity(&mut self, entity: DxccEntity) {
        self.entities.insert(entity.entity_code, entity);
    }

    /// Add a prefix pattern for callsign lookup
    fn add_prefix_pattern(&mut self, pattern: &str, entity_code: u16, priority: u8) -> Result<()> {
        let regex = Regex::new(pattern)
            .map_err(|e| DxError::Parse(format!("Invalid regex pattern '{}': {}", pattern, e)))?;

        self.prefixes.push(PrefixInfo {
            prefix: pattern.to_string(),
            entity_code,
            priority,
            regex,
        });

        Ok(())
    }

    /// Add a specific callsign override
    pub fn add_callsign_override(&mut self, callsign: String, entity_code: u16) {
        self.callsign_overrides
            .insert(callsign.to_uppercase(), entity_code);
    }

    /// Look up DXCC entity by callsign
    pub async fn lookup_callsign(&self, callsign: &str) -> Result<&DxccEntity> {
        let callsign = callsign.to_uppercase();

        // First check for exact callsign overrides
        if let Some(&entity_code) = self.callsign_overrides.get(&callsign) {
            debug!(
                "Found callsign override for {}: entity {}",
                callsign, entity_code
            );
            return self
                .get_entity(entity_code)
                .ok_or_else(|| DxError::DxccNotFound(format!("Entity {} not found", entity_code)));
        }

        // Extract base callsign (remove /portable, /mobile, etc.)
        let base_callsign = self.extract_base_callsign(&callsign);

        // Try prefix matching
        for prefix_info in &self.prefixes {
            if prefix_info.regex.is_match(&base_callsign) {
                debug!(
                    "Matched callsign {} with prefix pattern {} -> entity {}",
                    callsign, prefix_info.prefix, prefix_info.entity_code
                );
                return self.get_entity(prefix_info.entity_code).ok_or_else(|| {
                    DxError::DxccNotFound(format!("Entity {} not found", prefix_info.entity_code))
                });
            }
        }

        warn!("No DXCC entity found for callsign: {}", callsign);
        Err(DxError::DxccNotFound(callsign))
    }

    /// Get entity by entity code
    pub fn get_entity(&self, entity_code: u16) -> Option<&DxccEntity> {
        self.entities.get(&entity_code)
    }

    /// Get all entities
    pub fn get_all_entities(&self) -> impl Iterator<Item = &DxccEntity> {
        self.entities.values()
    }

    /// Get current (active) entities only
    pub fn get_current_entities(&self) -> impl Iterator<Item = &DxccEntity> {
        self.entities
            .values()
            .filter(|e| e.status == DxccStatus::Current)
    }

    /// Count of total entities
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// Count of current entities
    pub fn current_entity_count(&self) -> usize {
        self.entities
            .values()
            .filter(|e| e.status == DxccStatus::Current)
            .count()
    }

    /// Extract base callsign by removing common suffixes
    fn extract_base_callsign(&self, callsign: &str) -> String {
        let suffixes = [
            "/P",
            "/PORTABLE",
            "/M",
            "/MOBILE",
            "/MM",
            "/MARITIME_MOBILE",
            "/AM",
            "/AERONAUTICAL_MOBILE",
            "/QRP",
            "/BEACON",
            "/LH",
            "/LIGHTHOUSE",
            "/0",
            "/1",
            "/2",
            "/3",
            "/4",
            "/5",
            "/6",
            "/7",
            "/8",
            "/9",
        ];

        let mut base = callsign.to_string();
        for suffix in &suffixes {
            if let Some(pos) = base.find(suffix) {
                base.truncate(pos);
                break;
            }
        }

        base
    }

    /// Search entities by name or prefix
    pub fn search_entities(&self, query: &str) -> Vec<&DxccEntity> {
        let query = query.to_lowercase();
        self.entities
            .values()
            .filter(|entity| {
                entity.name.to_lowercase().contains(&query)
                    || entity.prefix.to_lowercase().contains(&query)
                    || entity.country.to_lowercase().contains(&query)
            })
            .collect()
    }

    /// Get entities by continent
    pub fn get_entities_by_continent(&self, continent: &str) -> Vec<&DxccEntity> {
        self.entities
            .values()
            .filter(|entity| entity.continent == continent)
            .collect()
    }

    /// Get entities by CQ zone
    pub fn get_entities_by_cq_zone(&self, cq_zone: u8) -> Vec<&DxccEntity> {
        self.entities
            .values()
            .filter(|entity| entity.cq_zone == cq_zone)
            .collect()
    }

    /// Get entities by ITU zone
    pub fn get_entities_by_itu_zone(&self, itu_zone: u8) -> Vec<&DxccEntity> {
        self.entities
            .values()
            .filter(|entity| entity.itu_zone == itu_zone)
            .collect()
    }

    /// Load DXCC data from CTY.DAT file format
    pub async fn load_cty_dat(&mut self, cty_data: &str) -> Result<()> {
        info!("Loading DXCC data from CTY.DAT format");

        let mut entities_loaded = 0;
        let mut prefixes_loaded = 0;

        for line in cty_data.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Parse CTY.DAT format (simplified implementation)
            // Full implementation would handle the complex CTY.DAT format
            if let Ok((entity, prefix_count)) = self.parse_cty_line(line) {
                self.add_entity(entity);
                entities_loaded += 1;
                prefixes_loaded += prefix_count;
            }
        }

        // Re-sort prefixes after loading
        self.prefixes.sort_by_key(|p| p.priority);

        info!(
            "Loaded {} entities and {} prefixes from CTY.DAT",
            entities_loaded, prefixes_loaded
        );

        Ok(())
    }

    /// Parse a single line from CTY.DAT format (simplified)
    fn parse_cty_line(&mut self, line: &str) -> Result<(DxccEntity, usize)> {
        // This is a simplified parser - real CTY.DAT parsing is more complex
        // Would need to handle the full CTY.DAT format specification

        // For now, return an error as this is placeholder
        Err(DxError::Parse(
            "CTY.DAT parsing not fully implemented".to_string(),
        ))
    }

    /// Export entities to JSON format
    pub fn export_json(&self) -> Result<String> {
        serde_json::to_string_pretty(&self.entities)
            .map_err(|e| DxError::Parse(format!("JSON export error: {}", e)))
    }

    /// Import entities from JSON format
    pub async fn import_json(&mut self, json_data: &str) -> Result<()> {
        let entities: HashMap<u16, DxccEntity> = serde_json::from_str(json_data)
            .map_err(|e| DxError::Parse(format!("JSON import error: {}", e)))?;

        self.entities = entities;
        info!("Imported {} entities from JSON", self.entities.len());

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_dxcc_database_creation() {
        let db = DxccDatabase::new().await.unwrap();
        assert!(db.entity_count() > 0);
        assert!(db.current_entity_count() > 0);
    }

    #[tokio::test]
    async fn test_callsign_lookup() {
        let db = DxccDatabase::new().await.unwrap();

        // Test US callsign
        let entity = db.lookup_callsign("W1ABC").await.unwrap();
        assert_eq!(entity.entity_code, 6);
        assert_eq!(entity.country, "United States");

        // Test Canadian callsign
        let entity = db.lookup_callsign("VE3XYZ").await.unwrap();
        assert_eq!(entity.entity_code, 1);
        assert_eq!(entity.country, "Canada");

        // Test portable callsign
        let entity = db.lookup_callsign("W1ABC/P").await.unwrap();
        assert_eq!(entity.entity_code, 6);
    }

    #[tokio::test]
    async fn test_entity_search() {
        let db = DxccDatabase::new().await.unwrap();

        let results = db.search_entities("united");
        assert!(!results.is_empty());

        let results = db.search_entities("canada");
        assert!(!results.is_empty());
    }

    #[test]
    fn test_base_callsign_extraction() {
        let db = DxccDatabase {
            entities: HashMap::new(),
            prefixes: Vec::new(),
            callsign_overrides: HashMap::new(),
        };

        assert_eq!(db.extract_base_callsign("W1ABC/P"), "W1ABC");
        assert_eq!(db.extract_base_callsign("VE3XYZ/MOBILE"), "VE3XYZ");
        assert_eq!(db.extract_base_callsign("JA1ABC"), "JA1ABC");
    }

    #[test]
    fn test_continent_filtering() {
        let mut db = DxccDatabase {
            entities: HashMap::new(),
            prefixes: Vec::new(),
            callsign_overrides: HashMap::new(),
        };

        db.add_entity(DxccEntity {
            entity_code: 1,
            name: "TEST NA".to_string(),
            prefix: "T1".to_string(),
            itu_zone: 1,
            cq_zone: 1,
            continent: "NA".to_string(),
            latitude: 0.0,
            longitude: 0.0,
            utc_offset: 0.0,
            country: "Test NA".to_string(),
            status: DxccStatus::Current,
            start_date: None,
            end_date: None,
            notes: None,
        });

        db.add_entity(DxccEntity {
            entity_code: 2,
            name: "TEST EU".to_string(),
            prefix: "T2".to_string(),
            itu_zone: 1,
            cq_zone: 1,
            continent: "EU".to_string(),
            latitude: 0.0,
            longitude: 0.0,
            utc_offset: 0.0,
            country: "Test EU".to_string(),
            status: DxccStatus::Current,
            start_date: None,
            end_date: None,
            notes: None,
        });

        let na_entities = db.get_entities_by_continent("NA");
        assert_eq!(na_entities.len(), 1);
        assert_eq!(na_entities[0].entity_code, 1);

        let eu_entities = db.get_entities_by_continent("EU");
        assert_eq!(eu_entities.len(), 1);
        assert_eq!(eu_entities[0].entity_code, 2);
    }
}
