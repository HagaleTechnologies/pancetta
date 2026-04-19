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

    /// Helper to add a DXCC entity with common defaults
    fn add_default_entity(
        &mut self,
        code: u16,
        name: &str,
        prefix: &str,
        itu: u8,
        cq: u8,
        continent: &str,
        lat: f64,
        lon: f64,
        utc: f32,
        country: &str,
        notes: Option<&str>,
    ) {
        self.add_entity(DxccEntity {
            entity_code: code,
            name: name.to_string(),
            prefix: prefix.to_string(),
            itu_zone: itu,
            cq_zone: cq,
            continent: continent.to_string(),
            latitude: lat,
            longitude: lon,
            utc_offset: utc,
            country: country.to_string(),
            status: DxccStatus::Current,
            start_date: None,
            end_date: None,
            notes: notes.map(|s| s.to_string()),
        });
    }

    /// Load default DXCC data
    async fn load_default_data(&mut self) -> Result<()> {
        info!("Loading DXCC entity database");

        // =====================================================
        // DXCC Entity Definitions
        // =====================================================

        // --- North America ---
        self.add_default_entity(
            1, "CANADA", "VE", 9, 5, "NA", 45.0, -75.0, -5.0, "Canada", None,
        );
        self.add_default_entity(
            291,
            "UNITED STATES",
            "W",
            8,
            5,
            "NA",
            40.0,
            -95.0,
            -6.0,
            "United States",
            None,
        );
        self.add_default_entity(
            50, "MEXICO", "XE", 10, 6, "NA", 19.4, -99.1, -6.0, "Mexico", None,
        );
        self.add_default_entity(
            202,
            "PUERTO RICO",
            "KP4",
            11,
            8,
            "NA",
            18.2,
            -66.5,
            -4.0,
            "Puerto Rico",
            None,
        );

        // Legacy entity codes kept for backward compatibility with existing QSO databases
        self.add_default_entity(
            14,
            "SPAIN",
            "EA",
            37,
            14,
            "EU",
            40.0,
            -4.0,
            1.0,
            "Spain",
            Some("Legacy entity code; correct ARRL code is 281"),
        );
        self.add_default_entity(
            61,
            "JAPAN",
            "JA",
            45,
            25,
            "AS",
            36.0,
            138.0,
            9.0,
            "Japan",
            Some("Legacy entity code; correct ARRL code is 339"),
        );
        self.add_default_entity(
            78,
            "FEDERAL REPUBLIC OF GERMANY",
            "DL",
            28,
            14,
            "EU",
            51.0,
            9.0,
            1.0,
            "Germany",
            Some("Legacy entity code; correct ARRL code is 230"),
        );

        // --- Europe ---
        self.add_default_entity(
            223, "ENGLAND", "G", 27, 14, "EU", 51.5, -0.1, 0.0, "England", None,
        );
        self.add_default_entity(
            230,
            "FEDERAL REPUBLIC OF GERMANY",
            "DL",
            28,
            14,
            "EU",
            51.0,
            9.0,
            1.0,
            "Germany",
            Some("Correct ARRL DXCC code"),
        );
        self.add_default_entity(
            227, "FRANCE", "F", 27, 14, "EU", 48.9, 2.3, 1.0, "France", None,
        );
        self.add_default_entity(
            248, "ITALY", "I", 28, 15, "EU", 41.9, 12.5, 1.0, "Italy", None,
        );
        self.add_default_entity(
            281,
            "SPAIN",
            "EA",
            37,
            14,
            "EU",
            40.0,
            -4.0,
            1.0,
            "Spain",
            Some("Correct ARRL DXCC code"),
        );
        self.add_default_entity(
            263,
            "NETHERLANDS",
            "PA",
            27,
            14,
            "EU",
            52.4,
            4.9,
            1.0,
            "Netherlands",
            None,
        );
        self.add_default_entity(
            209, "BELGIUM", "ON", 27, 14, "EU", 50.8, 4.4, 1.0, "Belgium", None,
        );
        self.add_default_entity(
            287,
            "SWITZERLAND",
            "HB",
            28,
            14,
            "EU",
            46.9,
            7.4,
            1.0,
            "Switzerland",
            None,
        );
        self.add_default_entity(
            269, "POLAND", "SP", 28, 15, "EU", 52.2, 21.0, 1.0, "Poland", None,
        );
        self.add_default_entity(
            503,
            "CZECH REPUBLIC",
            "OK",
            28,
            15,
            "EU",
            50.1,
            14.4,
            1.0,
            "Czech Republic",
            None,
        );
        self.add_default_entity(
            284, "SWEDEN", "SM", 18, 14, "EU", 59.3, 18.1, 1.0, "Sweden", None,
        );
        self.add_default_entity(
            266, "NORWAY", "LA", 18, 14, "EU", 59.9, 10.7, 1.0, "Norway", None,
        );
        self.add_default_entity(
            222, "FINLAND", "OH", 18, 15, "EU", 60.2, 24.9, 2.0, "Finland", None,
        );
        self.add_default_entity(
            221, "DENMARK", "OZ", 18, 14, "EU", 55.7, 12.6, 1.0, "Denmark", None,
        );
        self.add_default_entity(
            206, "AUSTRIA", "OE", 28, 15, "EU", 48.2, 16.4, 1.0, "Austria", None,
        );
        self.add_default_entity(
            272, "PORTUGAL", "CT", 37, 14, "EU", 38.7, -9.1, 0.0, "Portugal", None,
        );
        self.add_default_entity(
            245, "IRELAND", "EI", 27, 14, "EU", 53.3, -6.3, 0.0, "Ireland", None,
        );
        self.add_default_entity(
            279, "SCOTLAND", "GM", 27, 14, "EU", 55.9, -3.2, 0.0, "Scotland", None,
        );
        self.add_default_entity(
            294, "WALES", "GW", 27, 14, "EU", 51.5, -3.2, 0.0, "Wales", None,
        );
        self.add_default_entity(
            15,
            "ASIATIC RUSSIA",
            "UA",
            30,
            16,
            "AS",
            55.8,
            37.6,
            3.0,
            "Russia",
            Some("Asiatic Russia DXCC entity"),
        );
        self.add_default_entity(
            54,
            "EUROPEAN RUSSIA",
            "UA",
            29,
            16,
            "EU",
            55.8,
            37.6,
            3.0,
            "European Russia",
            None,
        );
        self.add_default_entity(
            288, "UKRAINE", "UR", 29, 16, "EU", 50.4, 30.5, 2.0, "Ukraine", None,
        );

        // --- Asia ---
        self.add_default_entity(
            339,
            "JAPAN",
            "JA",
            45,
            25,
            "AS",
            36.0,
            138.0,
            9.0,
            "Japan",
            Some("Correct ARRL DXCC code"),
        );
        self.add_default_entity(
            318, "CHINA", "BY", 44, 24, "AS", 39.9, 116.4, 8.0, "China", None,
        );
        self.add_default_entity(
            386, "TAIWAN", "BV", 44, 24, "AS", 25.0, 121.5, 8.0, "Taiwan", None,
        );
        self.add_default_entity(
            137,
            "REPUBLIC OF KOREA",
            "HL",
            44,
            25,
            "AS",
            37.6,
            127.0,
            9.0,
            "South Korea",
            None,
        );
        self.add_default_entity(
            324, "INDIA", "VU", 41, 22, "AS", 28.6, 77.2, 5.5, "India", None,
        );
        self.add_default_entity(
            387, "THAILAND", "HS", 49, 26, "AS", 13.8, 100.5, 7.0, "Thailand", None,
        );
        self.add_default_entity(
            375,
            "PHILIPPINES",
            "DU",
            50,
            27,
            "AS",
            14.6,
            121.0,
            8.0,
            "Philippines",
            None,
        );
        self.add_default_entity(
            327,
            "INDONESIA",
            "YB",
            51,
            28,
            "AS",
            -6.2,
            106.8,
            7.0,
            "Indonesia",
            None,
        );

        // --- South America ---
        self.add_default_entity(
            108, "BRAZIL", "PY", 15, 11, "SA", -23.5, -46.6, -3.0, "Brazil", None,
        );
        self.add_default_entity(
            100,
            "ARGENTINA",
            "LU",
            14,
            13,
            "SA",
            -34.6,
            -58.4,
            -3.0,
            "Argentina",
            None,
        );
        self.add_default_entity(
            112, "CHILE", "CE", 14, 12, "SA", -33.4, -70.7, -4.0, "Chile", None,
        );
        self.add_default_entity(
            116, "COLOMBIA", "HK", 12, 9, "SA", 4.6, -74.1, -5.0, "Colombia", None,
        );
        self.add_default_entity(
            148,
            "VENEZUELA",
            "YV",
            12,
            9,
            "SA",
            10.5,
            -66.9,
            -4.0,
            "Venezuela",
            None,
        );

        // --- Oceania ---
        self.add_default_entity(
            150,
            "AUSTRALIA",
            "VK",
            59,
            30,
            "OC",
            -33.9,
            151.2,
            10.0,
            "Australia",
            None,
        );
        self.add_default_entity(
            170,
            "NEW ZEALAND",
            "ZL",
            60,
            32,
            "OC",
            -41.3,
            174.8,
            12.0,
            "New Zealand",
            None,
        );
        self.add_default_entity(
            110, "HAWAII", "KH6", 61, 31, "OC", 21.3, -157.8, -10.0, "Hawaii", None,
        );

        // --- Africa ---
        self.add_default_entity(
            462,
            "SOUTH AFRICA",
            "ZS",
            57,
            38,
            "AF",
            -33.9,
            18.4,
            2.0,
            "South Africa",
            None,
        );
        self.add_default_entity(
            450, "NIGERIA", "5N", 46, 35, "AF", 6.5, 3.4, 1.0, "Nigeria", None,
        );
        self.add_default_entity(
            430, "KENYA", "5Z", 48, 37, "AF", -1.3, 36.8, 3.0, "Kenya", None,
        );
        self.add_default_entity(
            446, "MOROCCO", "CN", 37, 33, "AF", 33.6, -7.6, 1.0, "Morocco", None,
        );

        // =====================================================
        // Prefix patterns for callsign lookup
        // =====================================================

        // Canada (1)
        self.add_prefix_pattern("^VE[0-9]", 1, 1)?;
        self.add_prefix_pattern("^VA[0-9]", 1, 1)?;
        self.add_prefix_pattern("^VB[0-9]", 1, 1)?;
        self.add_prefix_pattern("^VC[0-9]", 1, 1)?;
        self.add_prefix_pattern("^VD[0-9]", 1, 1)?;
        self.add_prefix_pattern("^VX[0-9]", 1, 1)?;
        self.add_prefix_pattern("^VY[0-9]", 1, 1)?;
        self.add_prefix_pattern("^VZ[0-9]", 1, 1)?;

        // USA (291)
        self.add_prefix_pattern("^W[0-9]", 291, 1)?;
        self.add_prefix_pattern("^K[0-9]", 291, 1)?;
        self.add_prefix_pattern("^N[0-9]", 291, 1)?;
        self.add_prefix_pattern("^AA[0-9]", 291, 2)?;
        self.add_prefix_pattern("^AB[0-9]", 291, 2)?;
        self.add_prefix_pattern("^AC[0-9]", 291, 2)?;
        self.add_prefix_pattern("^AD[0-9]", 291, 2)?;
        self.add_prefix_pattern("^AE[0-9]", 291, 2)?;
        self.add_prefix_pattern("^AF[0-9]", 291, 2)?;
        self.add_prefix_pattern("^AG[0-9]", 291, 2)?;
        self.add_prefix_pattern("^AH[0-9]", 291, 2)?;
        self.add_prefix_pattern("^AI[0-9]", 291, 2)?;
        self.add_prefix_pattern("^AJ[0-9]", 291, 2)?;
        self.add_prefix_pattern("^AK[0-9]", 291, 2)?;
        self.add_prefix_pattern("^AL[0-9]", 291, 2)?;

        // Mexico (50)
        self.add_prefix_pattern("^XE[0-9]", 50, 1)?;
        self.add_prefix_pattern("^XF[0-9]", 50, 2)?;

        // Puerto Rico (202) - 3-char prefix, match before generic K/W/N
        self.add_prefix_pattern("^KP4", 202, 1)?;
        self.add_prefix_pattern("^WP4", 202, 1)?;
        self.add_prefix_pattern("^NP4", 202, 1)?;

        // Hawaii (110) - 3-char prefix
        self.add_prefix_pattern("^KH6", 110, 1)?;
        self.add_prefix_pattern("^WH6", 110, 1)?;
        self.add_prefix_pattern("^NH6", 110, 1)?;

        // England (223)
        self.add_prefix_pattern("^G[0-9]", 223, 1)?;
        self.add_prefix_pattern("^M[0-9]", 223, 2)?;
        self.add_prefix_pattern("^2E[0-9]", 223, 2)?;

        // Spain (281 — correct ARRL DXCC code)
        self.add_prefix_pattern("^EA[0-9]", 281, 1)?;
        self.add_prefix_pattern("^EB[0-9]", 281, 2)?;
        self.add_prefix_pattern("^EC[0-9]", 281, 2)?;
        self.add_prefix_pattern("^ED[0-9]", 281, 2)?;
        self.add_prefix_pattern("^EE[0-9]", 281, 2)?;
        self.add_prefix_pattern("^EF[0-9]", 281, 2)?;
        self.add_prefix_pattern("^EG[0-9]", 281, 2)?;
        self.add_prefix_pattern("^EH[0-9]", 281, 2)?;

        // France (227)
        self.add_prefix_pattern("^F[0-9]", 227, 1)?;

        // Italy (248)
        self.add_prefix_pattern("^I[A-Z][0-9]", 248, 1)?;
        self.add_prefix_pattern("^IK[0-9]", 248, 2)?;
        self.add_prefix_pattern("^IZ[0-9]", 248, 2)?;
        self.add_prefix_pattern("^IU[0-9]", 248, 2)?;
        self.add_prefix_pattern("^IW[0-9]", 248, 2)?;

        // Netherlands (263)
        self.add_prefix_pattern("^PA[0-9]", 263, 1)?;
        self.add_prefix_pattern("^PB[0-9]", 263, 2)?;
        self.add_prefix_pattern("^PC[0-9]", 263, 2)?;
        self.add_prefix_pattern("^PD[0-9]", 263, 2)?;
        self.add_prefix_pattern("^PE[0-9]", 263, 2)?;
        self.add_prefix_pattern("^PH[0-9]", 263, 2)?;
        self.add_prefix_pattern("^PI[0-9]", 263, 2)?;

        // Belgium (209)
        self.add_prefix_pattern("^ON[0-9]", 209, 1)?;
        self.add_prefix_pattern("^OR[0-9]", 209, 2)?;
        self.add_prefix_pattern("^OT[0-9]", 209, 2)?;

        // Switzerland (287)
        self.add_prefix_pattern("^HB[0-9]", 287, 1)?;

        // Poland (269)
        self.add_prefix_pattern("^SP[0-9]", 269, 1)?;
        self.add_prefix_pattern("^SQ[0-9]", 269, 2)?;
        self.add_prefix_pattern("^SO[0-9]", 269, 2)?;
        self.add_prefix_pattern("^SN[0-9]", 269, 2)?;

        // Czech Republic (503)
        self.add_prefix_pattern("^OK[0-9]", 503, 1)?;
        self.add_prefix_pattern("^OL[0-9]", 503, 2)?;

        // Sweden (284)
        self.add_prefix_pattern("^SM[0-9]", 284, 1)?;
        self.add_prefix_pattern("^SA[0-9]", 284, 2)?;
        self.add_prefix_pattern("^SB[0-9]", 284, 2)?;
        self.add_prefix_pattern("^SC[0-9]", 284, 2)?;

        // Norway (266)
        self.add_prefix_pattern("^LA[0-9]", 266, 1)?;
        self.add_prefix_pattern("^LB[0-9]", 266, 2)?;

        // Finland (222)
        self.add_prefix_pattern("^OH[0-9]", 222, 1)?;
        self.add_prefix_pattern("^OG[0-9]", 222, 2)?;
        self.add_prefix_pattern("^OF[0-9]", 222, 2)?;

        // Denmark (221)
        self.add_prefix_pattern("^OZ[0-9]", 221, 1)?;

        // Austria (206)
        self.add_prefix_pattern("^OE[0-9]", 206, 1)?;

        // Portugal (272)
        self.add_prefix_pattern("^CT[0-9]", 272, 1)?;
        self.add_prefix_pattern("^CS[0-9]", 272, 2)?;
        self.add_prefix_pattern("^CQ[0-9]", 272, 2)?;

        // Ireland (245)
        self.add_prefix_pattern("^EI[0-9]", 245, 1)?;
        self.add_prefix_pattern("^EJ[0-9]", 245, 2)?;

        // Scotland (279)
        self.add_prefix_pattern("^GM[0-9]", 279, 1)?;
        self.add_prefix_pattern("^MM[0-9]", 279, 2)?;
        self.add_prefix_pattern("^2M[0-9]", 279, 2)?;

        // Wales (294)
        self.add_prefix_pattern("^GW[0-9]", 294, 1)?;
        self.add_prefix_pattern("^MW[0-9]", 294, 2)?;
        self.add_prefix_pattern("^2W[0-9]", 294, 2)?;

        // European Russia (54)
        self.add_prefix_pattern("^UA[0-9]", 54, 1)?;
        self.add_prefix_pattern("^RA[0-9]", 54, 2)?;
        self.add_prefix_pattern("^RU[0-9]", 54, 2)?;
        self.add_prefix_pattern("^RV[0-9]", 54, 2)?;
        self.add_prefix_pattern("^RW[0-9]", 54, 2)?;
        self.add_prefix_pattern("^RX[0-9]", 54, 2)?;
        self.add_prefix_pattern("^RZ[0-9]", 54, 2)?;

        // Ukraine (288)
        self.add_prefix_pattern("^UR[0-9]", 288, 1)?;
        self.add_prefix_pattern("^UT[0-9]", 288, 1)?;
        self.add_prefix_pattern("^UX[0-9]", 288, 2)?;
        self.add_prefix_pattern("^UY[0-9]", 288, 2)?;

        // Japan (339 — correct ARRL DXCC code)
        self.add_prefix_pattern("^JA[0-9]", 339, 1)?;
        self.add_prefix_pattern("^JE[0-9]", 339, 2)?;
        self.add_prefix_pattern("^JF[0-9]", 339, 2)?;
        self.add_prefix_pattern("^JG[0-9]", 339, 2)?;
        self.add_prefix_pattern("^JH[0-9]", 339, 2)?;
        self.add_prefix_pattern("^JI[0-9]", 339, 2)?;
        self.add_prefix_pattern("^JJ[0-9]", 339, 2)?;
        self.add_prefix_pattern("^JK[0-9]", 339, 2)?;
        self.add_prefix_pattern("^JL[0-9]", 339, 2)?;
        self.add_prefix_pattern("^JM[0-9]", 339, 2)?;
        self.add_prefix_pattern("^JN[0-9]", 339, 2)?;
        self.add_prefix_pattern("^JO[0-9]", 339, 2)?;
        self.add_prefix_pattern("^JP[0-9]", 339, 2)?;
        self.add_prefix_pattern("^JQ[0-9]", 339, 2)?;
        self.add_prefix_pattern("^JR[0-9]", 339, 2)?;
        self.add_prefix_pattern("^JS[0-9]", 339, 2)?;

        // China (318)
        self.add_prefix_pattern("^BY[0-9]", 318, 1)?;
        self.add_prefix_pattern("^BA[0-9]", 318, 2)?;
        self.add_prefix_pattern("^BD[0-9]", 318, 2)?;
        self.add_prefix_pattern("^BG[0-9]", 318, 2)?;
        self.add_prefix_pattern("^BH[0-9]", 318, 2)?;

        // Taiwan (386)
        self.add_prefix_pattern("^BV[0-9]", 386, 1)?;
        self.add_prefix_pattern("^BW[0-9]", 386, 2)?;
        self.add_prefix_pattern("^BX[0-9]", 386, 2)?;
        self.add_prefix_pattern("^BM[0-9]", 386, 2)?;

        // South Korea (137)
        self.add_prefix_pattern("^HL[0-9]", 137, 1)?;
        self.add_prefix_pattern("^DS[0-9]", 137, 2)?;
        self.add_prefix_pattern("^6K[0-9]", 137, 2)?;
        self.add_prefix_pattern("^6L[0-9]", 137, 2)?;

        // India (324)
        self.add_prefix_pattern("^VU[0-9]", 324, 1)?;
        self.add_prefix_pattern("^AT[0-9]", 324, 2)?;

        // Thailand (387)
        self.add_prefix_pattern("^HS[0-9]", 387, 1)?;
        self.add_prefix_pattern("^E2[0-9]", 387, 2)?;

        // Philippines (375)
        self.add_prefix_pattern("^DU[0-9]", 375, 1)?;
        self.add_prefix_pattern("^DV[0-9]", 375, 2)?;
        self.add_prefix_pattern("^DX[0-9]", 375, 2)?;
        self.add_prefix_pattern("^DW[0-9]", 375, 2)?;
        self.add_prefix_pattern("^DZ[0-9]", 375, 2)?;
        self.add_prefix_pattern("^4F[0-9]", 375, 2)?;

        // Indonesia (327)
        self.add_prefix_pattern("^YB[0-9]", 327, 1)?;
        self.add_prefix_pattern("^YC[0-9]", 327, 2)?;
        self.add_prefix_pattern("^YD[0-9]", 327, 2)?;
        self.add_prefix_pattern("^YE[0-9]", 327, 2)?;
        self.add_prefix_pattern("^YF[0-9]", 327, 2)?;

        // Brazil (108)
        self.add_prefix_pattern("^PY[0-9]", 108, 1)?;
        self.add_prefix_pattern("^PP[0-9]", 108, 2)?;
        self.add_prefix_pattern("^PQ[0-9]", 108, 2)?;
        self.add_prefix_pattern("^PR[0-9]", 108, 2)?;
        self.add_prefix_pattern("^PS[0-9]", 108, 2)?;
        self.add_prefix_pattern("^PT[0-9]", 108, 2)?;
        self.add_prefix_pattern("^PU[0-9]", 108, 2)?;
        self.add_prefix_pattern("^PV[0-9]", 108, 2)?;
        self.add_prefix_pattern("^PW[0-9]", 108, 2)?;
        self.add_prefix_pattern("^PX[0-9]", 108, 2)?;

        // Argentina (100)
        self.add_prefix_pattern("^LU[0-9]", 100, 1)?;
        self.add_prefix_pattern("^LW[0-9]", 100, 2)?;
        self.add_prefix_pattern("^LO[0-9]", 100, 2)?;
        self.add_prefix_pattern("^LP[0-9]", 100, 2)?;
        self.add_prefix_pattern("^LQ[0-9]", 100, 2)?;
        self.add_prefix_pattern("^LR[0-9]", 100, 2)?;
        self.add_prefix_pattern("^LS[0-9]", 100, 2)?;

        // Chile (112)
        self.add_prefix_pattern("^CE[0-9]", 112, 1)?;
        self.add_prefix_pattern("^CA[0-9]", 112, 2)?;
        self.add_prefix_pattern("^CB[0-9]", 112, 2)?;
        self.add_prefix_pattern("^CD[0-9]", 112, 2)?;

        // Colombia (116)
        self.add_prefix_pattern("^HK[0-9]", 116, 1)?;
        self.add_prefix_pattern("^HJ[0-9]", 116, 2)?;
        self.add_prefix_pattern("^5J[0-9]", 116, 2)?;
        self.add_prefix_pattern("^5K[0-9]", 116, 2)?;

        // Venezuela (148)
        self.add_prefix_pattern("^YV[0-9]", 148, 1)?;
        self.add_prefix_pattern("^YW[0-9]", 148, 2)?;
        self.add_prefix_pattern("^YX[0-9]", 148, 2)?;
        self.add_prefix_pattern("^YY[0-9]", 148, 2)?;

        // Australia (150)
        self.add_prefix_pattern("^VK[0-9]", 150, 1)?;
        self.add_prefix_pattern("^AX[0-9]", 150, 2)?;

        // New Zealand (170)
        self.add_prefix_pattern("^ZL[0-9]", 170, 1)?;
        self.add_prefix_pattern("^ZM[0-9]", 170, 2)?;

        // South Africa (462)
        self.add_prefix_pattern("^ZS[0-9]", 462, 1)?;
        self.add_prefix_pattern("^ZR[0-9]", 462, 2)?;
        self.add_prefix_pattern("^ZT[0-9]", 462, 2)?;
        self.add_prefix_pattern("^ZU[0-9]", 462, 2)?;

        // Nigeria (450)
        self.add_prefix_pattern("^5N[0-9]", 450, 1)?;
        self.add_prefix_pattern("^5O[0-9]", 450, 2)?;

        // Kenya (430)
        self.add_prefix_pattern("^5Z[0-9]", 430, 1)?;
        self.add_prefix_pattern("^5Y[0-9]", 430, 2)?;

        // Morocco (446)
        self.add_prefix_pattern("^CN[0-9]", 446, 1)?;
        self.add_prefix_pattern("^5C[0-9]", 446, 2)?;
        self.add_prefix_pattern("^5D[0-9]", 446, 2)?;

        // Germany (230 — correct ARRL DXCC code)
        self.add_prefix_pattern("^DL[0-9]", 230, 1)?;
        self.add_prefix_pattern("^DA[0-9]", 230, 2)?;
        self.add_prefix_pattern("^DB[0-9]", 230, 2)?;
        self.add_prefix_pattern("^DC[0-9]", 230, 2)?;
        self.add_prefix_pattern("^DD[0-9]", 230, 2)?;
        self.add_prefix_pattern("^DE[0-9]", 230, 2)?;
        self.add_prefix_pattern("^DF[0-9]", 230, 2)?;
        self.add_prefix_pattern("^DG[0-9]", 230, 2)?;
        self.add_prefix_pattern("^DH[0-9]", 230, 2)?;
        self.add_prefix_pattern("^DI[0-9]", 230, 2)?;
        self.add_prefix_pattern("^DJ[0-9]", 230, 2)?;
        self.add_prefix_pattern("^DK[0-9]", 230, 2)?;
        self.add_prefix_pattern("^DM[0-9]", 230, 2)?;
        self.add_prefix_pattern("^DN[0-9]", 230, 2)?;
        self.add_prefix_pattern("^DO[0-9]", 230, 2)?;

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

            if let Ok((entity, prefix_count)) = self.parse_cty_line(line) {
                self.add_entity(entity);
                entities_loaded += 1;
                prefixes_loaded += prefix_count;
            }
        }

        self.prefixes.sort_by_key(|p| p.priority);

        info!(
            "Loaded {} entities and {} prefixes from CTY.DAT",
            entities_loaded, prefixes_loaded
        );

        Ok(())
    }

    /// Parse a single line from CTY.DAT format (simplified)
    fn parse_cty_line(&mut self, _line: &str) -> Result<(DxccEntity, usize)> {
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
        assert!(db.entity_count() >= 40);
        assert!(db.current_entity_count() >= 40);
    }

    #[tokio::test]
    async fn test_callsign_lookup() {
        let db = DxccDatabase::new().await.unwrap();

        // Test US callsign
        let entity = db.lookup_callsign("W1ABC").await.unwrap();
        assert_eq!(entity.entity_code, 291);
        assert_eq!(entity.country, "United States");

        // Test Canadian callsign
        let entity = db.lookup_callsign("VE3XYZ").await.unwrap();
        assert_eq!(entity.entity_code, 1);
        assert_eq!(entity.country, "Canada");

        // Test portable callsign
        let entity = db.lookup_callsign("W1ABC/P").await.unwrap();
        assert_eq!(entity.entity_code, 291);
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
