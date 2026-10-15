//! ADIF 3.0 format support for QSO logging
//!
//! This module implements the Amateur Data Interchange Format (ADIF) 3.0
//! specification for importing and exporting QSO data in the standard
//! amateur radio logging format.

use crate::states::*;
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use thiserror::Error;
use uuid::Uuid;

/// ADIF parsing and generation errors
#[derive(Debug, Error)]
pub enum AdifError {
    #[error("Parse error at line {line}: {message}")]
    ParseError { line: usize, message: String },

    #[error("Invalid field format: {field}")]
    InvalidField { field: String },

    #[error("Invalid date/time format: {datetime}")]
    InvalidDateTime { datetime: String },

    #[error("Missing required field: {field}")]
    MissingField { field: String },

    #[error("Invalid QSO mode: {mode}")]
    InvalidMode { mode: String },

    #[error("Invalid band: {band}")]
    InvalidBand { band: String },

    #[error("IO error: {source}")]
    Io { source: std::io::Error },

    #[error("Encoding error: {message}")]
    Encoding { message: String },
}

/// ADIF data type enumeration
#[derive(Debug, Clone, PartialEq)]
pub enum AdifDataType {
    String,
    Number,
    Date,
    Time,
    DateTime,
    Boolean,
    Enumeration(Vec<String>),
    MultilineString,
}

/// ADIF field definition
#[derive(Debug, Clone)]
pub struct AdifFieldDef {
    /// Field name
    pub name: String,

    /// Data type
    pub data_type: AdifDataType,

    /// Field description
    pub description: String,

    /// Is field required
    pub required: bool,

    /// Minimum length
    pub min_length: Option<usize>,

    /// Maximum length
    pub max_length: Option<usize>,
}

/// ADIF field value
#[derive(Debug, Clone, PartialEq)]
pub struct AdifField {
    /// Field name
    pub name: String,

    /// Field value
    pub value: String,

    /// Field length (automatically calculated)
    pub length: usize,

    /// Data type specifier
    pub data_type: Option<String>,
}

/// ADIF record representing a single QSO
#[derive(Debug, Clone)]
pub struct AdifRecord {
    /// Fields in this record
    pub fields: HashMap<String, AdifField>,
}

/// Complete ADIF file structure
#[derive(Debug, Clone)]
pub struct AdifFile {
    /// File header with metadata
    pub header: AdifHeader,

    /// QSO records
    pub records: Vec<AdifRecord>,
}

/// One record that could not be read, with its CALL when that field parsed.
#[derive(Debug)]
pub struct AdifRecordError {
    pub error: AdifError,
    pub call: Option<String>,
}

/// Every record in an ADIF file, each parsed on its own (PAN-127), so one
/// malformed record no longer hides the records around it.
#[derive(Debug)]
pub struct AdifParse {
    pub header: AdifHeader,
    pub records: Vec<Result<AdifRecord, AdifRecordError>>,
}

/// ADIF file header
#[derive(Debug, Clone, Default)]
pub struct AdifHeader {
    /// ADIF version
    pub version: String,

    /// Program that created the file
    pub program_id: String,

    /// Program version
    pub program_version: String,

    /// Creation timestamp
    pub created_timestamp: DateTime<Utc>,

    /// Additional header fields
    pub fields: HashMap<String, AdifField>,
}

/// ADIF QSO data structure for easier handling
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdifQso {
    /// QSO start date and time (UTC)
    pub qso_date: DateTime<Utc>,

    /// QSO end date and time (UTC)
    pub qso_date_off: Option<DateTime<Utc>>,

    /// Called station callsign
    pub call: String,

    /// Operating mode
    pub mode: String,

    /// Sub-mode (e.g., "FT8" for DATA mode)
    pub submode: Option<String>,

    /// Frequency in MHz
    pub freq: f64,

    /// Band designation
    pub band: String,

    /// RST sent
    pub rst_sent: Option<String>,

    /// RST received
    pub rst_rcvd: Option<String>,

    /// Transmit power in watts
    pub tx_pwr: Option<f64>,

    /// Station callsign (our callsign)
    pub station_callsign: String,

    /// Operator callsign
    pub operator: Option<String>,

    /// My grid square
    pub my_gridsquare: Option<String>,

    /// Their grid square
    pub gridsquare: Option<String>,

    /// Country name
    pub country: Option<String>,

    /// DXCC entity code
    pub dxcc: Option<u16>,

    /// CQ Zone
    pub cqz: Option<u8>,

    /// ITU Zone  
    pub ituz: Option<u8>,

    /// State/province
    pub state: Option<String>,

    /// Contest ID
    pub contest_id: Option<String>,

    /// Serial number sent
    pub stx: Option<u32>,

    /// Serial number sent string
    pub stx_string: Option<String>,

    /// Serial number received
    pub srx: Option<u32>,

    /// Serial number received string
    pub srx_string: Option<String>,

    /// QSL sent status
    pub qsl_sent: Option<String>,

    /// QSL received status
    pub qsl_rcvd: Option<String>,

    /// QSL card message
    pub qslmsg: Option<String>,

    /// Comments
    pub comment: Option<String>,

    /// Notes
    pub notes: Option<String>,

    /// Additional fields
    pub additional_fields: HashMap<String, String>,
}

/// Map an RF frequency (Hz) to its ADIF band string (e.g. "20M").
///
/// Free function so callers don't have to construct an [`AdifProcessor`] just
/// to classify a frequency. Perf (Pass 1 / A13): `utils::frequency_to_band`
/// was building a whole `AdifProcessor` (~54 String allocations in `new()`) on
/// every call — and it's called per duplicate-check / per QSO-scan, i.e.
/// thousands of times per slot on a busy band. The classification uses no
/// instance state, so it now lives here and the `&self` method delegates.
pub fn freq_hz_to_band(frequency_hz: f64) -> String {
    let freq_mhz = frequency_hz / 1_000_000.0;
    match freq_mhz {
        f if (1.8..=2.0).contains(&f) => "160M".to_string(),
        f if (3.5..=4.0).contains(&f) => "80M".to_string(),
        f if (5.3..=5.4).contains(&f) => "60M".to_string(),
        f if (7.0..=7.3).contains(&f) => "40M".to_string(),
        f if (10.1..=10.15).contains(&f) => "30M".to_string(),
        f if (14.0..=14.35).contains(&f) => "20M".to_string(),
        f if (18.068..=18.168).contains(&f) => "17M".to_string(),
        f if (21.0..=21.45).contains(&f) => "15M".to_string(),
        f if (24.89..=24.99).contains(&f) => "12M".to_string(),
        f if (28.0..=29.7).contains(&f) => "10M".to_string(),
        f if (50.0..=54.0).contains(&f) => "6M".to_string(),
        f if (144.0..=148.0).contains(&f) => "2M".to_string(),
        f if (420.0..=450.0).contains(&f) => "70CM".to_string(),
        _ => format!("{:.0}MHZ", freq_mhz),
    }
}

/// Attribution appended to every logged QSO's COMMENT field, crediting the
/// software/service stack the station runs on. Two variants so long
/// operator notes don't force a truncation: the full form (with URLs) is
/// used when it fits; otherwise the short form; if even that doesn't fit,
/// attribution is omitted rather than truncating the operator's own notes.
const ATTRIBUTION_FULL: &str =
    "Using cqdx.io & Pancetta -- https://cqdx.io https://github.com/HagaleTechnologies/pancetta";
const ATTRIBUTION_SHORT: &str = "Using cqdx.io & Pancetta";

/// Practical ceiling for the rendered COMMENT field. ADIF itself doesn't cap
/// this field, but downstream consumers (ClubLog, QRZ Logbook, eQSL, LoTW)
/// commonly bound it in their own storage; keep comfortably under a common
/// 255-char column limit so attribution never risks truncating in transit.
const COMMENT_SOFT_MAX_LEN: usize = 255;

/// Append the software attribution to `base` (existing notes + any HOUND
/// marker), preferring the full form (with URLs) if it fits within
/// [`COMMENT_SOFT_MAX_LEN`], falling back to the short form, and omitting
/// attribution entirely if `base` alone is already at or over the limit —
/// operator notes are never truncated to make room.
fn append_attribution(base: &str) -> String {
    let with_full = if base.is_empty() {
        ATTRIBUTION_FULL.to_string()
    } else {
        format!("{} {}", base, ATTRIBUTION_FULL)
    };
    if with_full.len() <= COMMENT_SOFT_MAX_LEN {
        return with_full;
    }
    let with_short = if base.is_empty() {
        ATTRIBUTION_SHORT.to_string()
    } else {
        format!("{} {}", base, ATTRIBUTION_SHORT)
    };
    if with_short.len() <= COMMENT_SOFT_MAX_LEN {
        return with_short;
    }
    base.to_string()
}

/// ADIF parser and generator
#[derive(Clone)]
pub struct AdifProcessor {
    /// Known field definitions
    field_definitions: HashMap<String, AdifFieldDef>,
    /// Station transmit power in watts, stamped into every `TX_PWR` field
    /// this processor renders. `None` omits `TX_PWR` entirely (unconfigured).
    station_power_watts: Option<f64>,
}

impl AdifProcessor {
    /// Create a new ADIF processor
    pub fn new() -> Self {
        let mut processor = Self {
            field_definitions: HashMap::new(),
            station_power_watts: None,
        };

        processor.initialize_standard_fields();
        processor
    }

    /// Set the station transmit power (watts) stamped into every `TX_PWR`
    /// field this processor renders from here on. Pass the operator's
    /// configured `[station] power_watts`; `0` (unset) is treated as "don't
    /// stamp a power" rather than a literal `TX_PWR:0`.
    pub fn set_station_power_watts(&mut self, watts: u32) {
        self.station_power_watts = if watts == 0 { None } else { Some(watts as f64) };
    }

    /// Builder-style variant of [`set_station_power_watts`](Self::set_station_power_watts).
    pub fn with_station_power_watts(mut self, watts: u32) -> Self {
        self.set_station_power_watts(watts);
        self
    }

    /// Tag tokenizer over the raw bytes (PAN-127): line breaks carry no
    /// meaning, a file with no `<EOH>` is all records, and `<EOH>` only counts
    /// before the first record (any free text before it is ignored). Each
    /// record is parsed on its own, so one malformed record is reported in
    /// place and the records around it still read. Input that is not UTF-8
    /// is read as Latin-1.
    pub fn parse_bytes(&self, data: &[u8]) -> AdifParse {
        let text = std::str::from_utf8(data).ok(); // None ⇒ decode as Latin-1
        let mut header_fields: Option<HashMap<String, AdifField>> = None;
        let mut records = Vec::new();
        let mut fields = HashMap::new();
        let mut error: Option<AdifError> = None;
        let mut pos = 0;

        while let Some((open, close)) = next_tag(data, pos) {
            let spec = decode(data, text, open + 1, close);
            pos = close + 1;

            if spec.eq_ignore_ascii_case("EOH") {
                // The header holds nothing records need, so its field errors
                // are dropped rather than reported against a record.
                if records.is_empty() && header_fields.is_none() {
                    header_fields = Some(std::mem::take(&mut fields));
                    error = None;
                }
                continue;
            }
            if spec.eq_ignore_ascii_case("EOR") {
                push_record(&mut records, &mut fields, &mut error);
                continue;
            }
            if spec.is_empty() {
                continue;
            }

            let mut parts = spec.split(':');
            let name = parts.next().unwrap_or_default().to_uppercase();
            let length = match parts.next().map(str::parse::<usize>) {
                None => 0,
                Some(Ok(length)) => length,
                Some(Err(_)) => {
                    // First error wins; the text up to the next tag is skipped.
                    error.get_or_insert(AdifError::InvalidField {
                        field: spec.clone(),
                    });
                    continue;
                }
            };
            let data_type = parts.next().map(str::to_string);

            let (value, end) = read_value(data, text, pos, length);
            pos = end;
            fields.insert(
                name.clone(),
                AdifField {
                    name,
                    value,
                    length,
                    data_type,
                },
            );
        }

        // A trailing record with no <EOR> still counts.
        push_record(&mut records, &mut fields, &mut error);

        AdifParse {
            header: self.header_from_fields(header_fields.unwrap_or_default()),
            records,
        }
    }

    /// Parse ADIF data from a string. Fails on the first record that cannot
    /// be read; use [`Self::parse_bytes`] to read every record on its own.
    pub fn parse_string(&self, data: &str) -> Result<AdifFile, AdifError> {
        let parsed = self.parse_bytes(data.as_bytes());
        let records = parsed
            .records
            .into_iter()
            .map(|r| r.map_err(|e| e.error))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(AdifFile {
            header: parsed.header,
            records,
        })
    }

    fn header_from_fields(&self, header_fields: HashMap<String, AdifField>) -> AdifHeader {
        AdifHeader {
            version: header_fields
                .get("ADIF_VER")
                .map(|f| f.value.clone())
                .unwrap_or_else(|| "3.1.0".to_string()),
            program_id: header_fields
                .get("PROGRAMID")
                .map(|f| f.value.clone())
                .unwrap_or_else(|| "pancetta-qso".to_string()),
            program_version: header_fields
                .get("PROGRAMVERSION")
                .map(|f| f.value.clone())
                .unwrap_or_else(|| "0.1.0".to_string()),
            created_timestamp: header_fields
                .get("CREATED_TIMESTAMP")
                .and_then(|f| self.parse_datetime(&f.value).ok())
                .unwrap_or_else(Utc::now),
            fields: header_fields,
        }
    }

    /// Parse ADIF data from a reader
    pub fn parse_reader<R: Read>(&self, mut reader: R) -> Result<AdifFile, AdifError> {
        let mut data = String::new();
        reader
            .read_to_string(&mut data)
            .map_err(|e| AdifError::Io { source: e })?;

        self.parse_string(&data)
    }

    /// Generate ADIF string from file structure
    pub fn generate_string(&self, file: &AdifFile) -> Result<String, AdifError> {
        let mut output = String::new();

        // Write header
        output.push_str(&format!("ADIF Export for {}\n", file.header.program_id));
        output.push_str(&format!(
            "Generated on {}\n\n",
            file.header
                .created_timestamp
                .format("%Y-%m-%d %H:%M:%S UTC")
        ));

        // Write header fields
        output.push_str(&self.format_field("ADIF_VER", &file.header.version)?);
        output.push_str(&self.format_field("PROGRAMID", &file.header.program_id)?);
        output.push_str(&self.format_field("PROGRAMVERSION", &file.header.program_version)?);
        output.push_str(
            &self.format_field(
                "CREATED_TIMESTAMP",
                &file
                    .header
                    .created_timestamp
                    .format("%Y%m%d %H%M%S")
                    .to_string(),
            )?,
        );

        for field in file.header.fields.values() {
            output.push_str(&self.format_field(&field.name, &field.value)?);
        }

        output.push_str("<EOH>\n\n");

        // Write records
        for record in &file.records {
            for field in record.fields.values() {
                output.push_str(&self.format_field(&field.name, &field.value)?);
            }
            output.push_str("<EOR>\n\n");
        }

        Ok(output)
    }

    /// Generate ADIF data to a writer
    pub fn generate_writer<W: Write>(
        &self,
        file: &AdifFile,
        mut writer: W,
    ) -> Result<(), AdifError> {
        let data = self.generate_string(file)?;
        writer
            .write_all(data.as_bytes())
            .map_err(|e| AdifError::Io { source: e })?;

        Ok(())
    }

    /// Convert QSO metadata to ADIF QSO structure
    pub fn qso_to_adif(
        &self,
        metadata: &QsoMetadata,
        contest_info: Option<&ContestInfo>,
    ) -> AdifQso {
        let band = self.frequency_to_band(metadata.frequency);
        let freq_mhz = metadata.frequency / 1_000_000.0;

        // Build the COMMENT field: start from existing notes, append "HOUND"
        // for Hound QSOs (the human-readable DXpedition marker), then append
        // the software attribution (full form with URLs if it fits, short
        // form otherwise, omitted entirely rather than truncating operator
        // notes). Appending never overwrites existing notes.
        let existing_notes = match &metadata.notes {
            Some(existing) if !existing.is_empty() => Some(existing.as_str()),
            _ => None,
        };
        let base = match (existing_notes, metadata.hound) {
            (Some(existing), true) => format!("{} HOUND", existing),
            (Some(existing), false) => existing.to_string(),
            (None, true) => "HOUND".to_string(),
            (None, false) => String::new(),
        };
        let comment = Some(append_attribution(&base));

        // Build additional_fields from metadata.tags; for Hound QSOs inject
        // the machine-readable APP_PANCETTA_HOUND marker.  Non-Hound QSOs are
        // unchanged (no extra field → byte-identical output).
        let mut additional_fields = metadata.tags.clone();
        if metadata.hound {
            additional_fields
                .entry("APP_PANCETTA_HOUND".to_string())
                .or_insert_with(|| "true".to_string());
        }

        AdifQso {
            qso_date: metadata.start_time,
            qso_date_off: metadata.end_time,
            call: metadata.their_callsign.clone().unwrap_or_default(),
            mode: metadata.mode.clone(),
            submode: None,
            freq: freq_mhz,
            band,
            rst_sent: metadata.reports.sent.map(|r| self.signal_report_to_rst(r)),
            rst_rcvd: metadata
                .reports
                .received
                .map(|r| self.signal_report_to_rst(r)),
            tx_pwr: self.station_power_watts,
            station_callsign: metadata.our_callsign.clone(),
            operator: None, // Could be added to metadata
            my_gridsquare: metadata.grids.ours.clone(),
            gridsquare: metadata.grids.theirs.clone(),
            country: None, // Would need callsign lookup
            dxcc: None,    // Would need callsign lookup
            cqz: None,     // Would need callsign lookup
            ituz: None,    // Would need callsign lookup
            state: metadata.their_state.clone(),
            contest_id: contest_info.map(|c| c.contest_name.clone()),
            stx: contest_info.and_then(|c| c.serials.sent),
            stx_string: None,
            srx: contest_info.and_then(|c| c.serials.received),
            srx_string: None,
            qsl_sent: Some("N".to_string()),
            qsl_rcvd: Some("N".to_string()),
            qslmsg: None,
            comment,
            notes: None,
            additional_fields,
        }
    }

    /// Convert ADIF QSO to QSO metadata
    pub fn adif_to_qso(&self, adif_qso: &AdifQso) -> QsoMetadata {
        let frequency = adif_qso.freq * 1_000_000.0; // Convert MHz to Hz
        let qso_id = Uuid::new_v4();

        QsoMetadata {
            qso_id,
            our_callsign: adif_qso.station_callsign.clone(),
            their_callsign: if adif_qso.call.is_empty() {
                None
            } else {
                Some(adif_qso.call.clone())
            },
            frequency,
            // `frequency` above is already the true RF frequency read back
            // from ADIF (not an audio offset — this reconstructs a
            // historical, already-completed record, never a live/active
            // QSO), so the `completed_rf_frequency_hz.unwrap_or(frequency)`
            // fallback used by the band-scoped suppression checks resolves
            // correctly without needing this set explicitly.
            completed_rf_frequency_hz: None,
            mode: adif_qso
                .submode
                .clone()
                .unwrap_or_else(|| adif_qso.mode.clone()),
            start_time: adif_qso.qso_date,
            end_time: adif_qso.qso_date_off,
            reports: SignalReports {
                sent: adif_qso
                    .rst_sent
                    .as_ref()
                    .and_then(|r| self.rst_to_signal_report(r)),
                received: adif_qso
                    .rst_rcvd
                    .as_ref()
                    .and_then(|r| self.rst_to_signal_report(r)),
            },
            grids: GridSquares {
                ours: adif_qso.my_gridsquare.clone(),
                theirs: adif_qso.gridsquare.clone(),
            },
            their_state: adif_qso.state.clone(),
            contest_info: adif_qso
                .contest_id
                .as_ref()
                .map(|contest_name| ContestInfo {
                    contest_name: contest_name.clone(),
                    category: "".to_string(), // Not available in ADIF
                    serials: ContestSerials {
                        sent: adif_qso.stx,
                        received: adif_qso.srx,
                    },
                    points: 1, // Default
                    multiplier: None,
                }),
            tags: adif_qso.additional_fields.clone(),
            notes: adif_qso.comment.clone(),
            tx_parity: None,
            initiated_by: Default::default(),
            role: Default::default(),
            call_count: 0,
            first_call_at: None,
            last_call_at: None,
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound: false,
            partner_freq: None,
            pending_freq_drift: None,
            hound_qsyed: false,
            remote_origin: false,
            remote_client_key_id: None,
            tx_parity_provisional: false,
        }
    }

    /// Convert AdifRecord to AdifQso
    pub fn record_to_qso(&self, record: &AdifRecord) -> Result<AdifQso, AdifError> {
        let qso_date = self.parse_qso_datetime(record)?;

        Ok(AdifQso {
            qso_date,
            qso_date_off: self.parse_optional_datetime(record, "QSO_DATE_OFF", "TIME_OFF")?,
            call: self.get_required_field(record, "CALL")?,
            mode: self
                .get_field_value(record, "MODE")
                .unwrap_or_else(|| "DATA".to_string()),
            submode: self.get_field_value(record, "SUBMODE"),
            freq: self
                .get_field_value(record, "FREQ")
                .and_then(|f| f.parse().ok())
                .unwrap_or(0.0),
            band: self.get_field_value(record, "BAND").unwrap_or_default(),
            rst_sent: self.get_field_value(record, "RST_SENT"),
            rst_rcvd: self.get_field_value(record, "RST_RCVD"),
            tx_pwr: self
                .get_field_value(record, "TX_PWR")
                .and_then(|p| p.parse().ok()),
            station_callsign: self
                .get_field_value(record, "STATION_CALLSIGN")
                .unwrap_or_default(),
            operator: self.get_field_value(record, "OPERATOR"),
            my_gridsquare: self.get_field_value(record, "MY_GRIDSQUARE"),
            gridsquare: self.get_field_value(record, "GRIDSQUARE"),
            country: self.get_field_value(record, "COUNTRY"),
            dxcc: self
                .get_field_value(record, "DXCC")
                .and_then(|d| d.parse().ok()),
            cqz: self
                .get_field_value(record, "CQZ")
                .and_then(|z| z.parse().ok()),
            ituz: self
                .get_field_value(record, "ITUZ")
                .and_then(|z| z.parse().ok()),
            state: self.get_field_value(record, "STATE"),
            contest_id: self.get_field_value(record, "CONTEST_ID"),
            stx: self
                .get_field_value(record, "STX")
                .and_then(|s| s.parse().ok()),
            stx_string: self.get_field_value(record, "STX_STRING"),
            srx: self
                .get_field_value(record, "SRX")
                .and_then(|s| s.parse().ok()),
            srx_string: self.get_field_value(record, "SRX_STRING"),
            qsl_sent: self.get_field_value(record, "QSL_SENT"),
            qsl_rcvd: self.get_field_value(record, "QSL_RCVD"),
            qslmsg: self.get_field_value(record, "QSLMSG"),
            comment: self.get_field_value(record, "COMMENT"),
            notes: self.get_field_value(record, "NOTES"),
            additional_fields: self.extract_additional_fields(record),
        })
    }

    /// Convert AdifQso to AdifRecord
    pub fn qso_to_record(&self, qso: &AdifQso) -> AdifRecord {
        let mut fields = HashMap::new();

        // Required fields
        self.add_field(&mut fields, "CALL", &qso.call);
        self.add_field(
            &mut fields,
            "QSO_DATE",
            &qso.qso_date.format("%Y%m%d").to_string(),
        );
        self.add_field(
            &mut fields,
            "TIME_ON",
            &qso.qso_date.format("%H%M%S").to_string(),
        );
        self.add_field(&mut fields, "MODE", &qso.mode);

        // Optional fields
        if let Some(ref submode) = qso.submode {
            self.add_field(&mut fields, "SUBMODE", submode);
        }

        if qso.freq > 0.0 {
            self.add_field(&mut fields, "FREQ", &format!("{:.6}", qso.freq));
        }

        if !qso.band.is_empty() {
            self.add_field(&mut fields, "BAND", &qso.band);
        }

        if let Some(ref rst) = qso.rst_sent {
            self.add_field(&mut fields, "RST_SENT", rst);
        }

        if let Some(ref rst) = qso.rst_rcvd {
            self.add_field(&mut fields, "RST_RCVD", rst);
        }

        if let Some(power) = qso.tx_pwr {
            self.add_field(&mut fields, "TX_PWR", &power.to_string());
        }

        if !qso.station_callsign.is_empty() {
            self.add_field(&mut fields, "STATION_CALLSIGN", &qso.station_callsign);
        }

        // Add all other optional fields
        self.add_optional_field(&mut fields, "OPERATOR", &qso.operator);
        self.add_optional_field(&mut fields, "MY_GRIDSQUARE", &qso.my_gridsquare);
        self.add_optional_field(&mut fields, "GRIDSQUARE", &qso.gridsquare);
        self.add_optional_field(&mut fields, "COUNTRY", &qso.country);

        if let Some(dxcc) = qso.dxcc {
            self.add_field(&mut fields, "DXCC", &dxcc.to_string());
        }

        if let Some(cqz) = qso.cqz {
            self.add_field(&mut fields, "CQZ", &cqz.to_string());
        }

        if let Some(ituz) = qso.ituz {
            self.add_field(&mut fields, "ITUZ", &ituz.to_string());
        }

        self.add_optional_field(&mut fields, "STATE", &qso.state);
        self.add_optional_field(&mut fields, "CONTEST_ID", &qso.contest_id);

        if let Some(stx) = qso.stx {
            self.add_field(&mut fields, "STX", &stx.to_string());
        }

        self.add_optional_field(&mut fields, "STX_STRING", &qso.stx_string);

        if let Some(srx) = qso.srx {
            self.add_field(&mut fields, "SRX", &srx.to_string());
        }

        self.add_optional_field(&mut fields, "SRX_STRING", &qso.srx_string);
        self.add_optional_field(&mut fields, "QSL_SENT", &qso.qsl_sent);
        self.add_optional_field(&mut fields, "QSL_RCVD", &qso.qsl_rcvd);
        self.add_optional_field(&mut fields, "QSLMSG", &qso.qslmsg);
        self.add_optional_field(&mut fields, "COMMENT", &qso.comment);
        self.add_optional_field(&mut fields, "NOTES", &qso.notes);

        // Add end time if available
        if let Some(end_time) = qso.qso_date_off {
            self.add_field(
                &mut fields,
                "QSO_DATE_OFF",
                &end_time.format("%Y%m%d").to_string(),
            );
            self.add_field(
                &mut fields,
                "TIME_OFF",
                &end_time.format("%H%M%S").to_string(),
            );
        }

        // Add additional fields
        for (key, value) in &qso.additional_fields {
            self.add_field(&mut fields, key, value);
        }

        AdifRecord { fields }
    }

    // Private helper methods

    fn initialize_standard_fields(&mut self) {
        // Add standard ADIF field definitions
        let standard_fields = vec![
            ("CALL", AdifDataType::String, "Called station callsign"),
            ("QSO_DATE", AdifDataType::Date, "QSO date"),
            ("TIME_ON", AdifDataType::Time, "QSO start time"),
            ("TIME_OFF", AdifDataType::Time, "QSO end time"),
            ("MODE", AdifDataType::String, "Operating mode"),
            ("SUBMODE", AdifDataType::String, "Operating sub-mode"),
            ("FREQ", AdifDataType::Number, "Frequency in MHz"),
            ("BAND", AdifDataType::String, "Band designation"),
            ("RST_SENT", AdifDataType::String, "RST sent"),
            ("RST_RCVD", AdifDataType::String, "RST received"),
            ("TX_PWR", AdifDataType::Number, "Transmit power in watts"),
            ("STATION_CALLSIGN", AdifDataType::String, "Station callsign"),
            ("OPERATOR", AdifDataType::String, "Operator callsign"),
            ("MY_GRIDSQUARE", AdifDataType::String, "My grid square"),
            ("GRIDSQUARE", AdifDataType::String, "Their grid square"),
            ("COUNTRY", AdifDataType::String, "Country name"),
            ("DXCC", AdifDataType::Number, "DXCC entity code"),
            ("CQZ", AdifDataType::Number, "CQ Zone"),
            ("ITUZ", AdifDataType::Number, "ITU Zone"),
            ("STATE", AdifDataType::String, "State/province"),
            ("CONTEST_ID", AdifDataType::String, "Contest identifier"),
            ("STX", AdifDataType::Number, "Serial number sent"),
            ("SRX", AdifDataType::Number, "Serial number received"),
            ("QSL_SENT", AdifDataType::String, "QSL sent status"),
            ("QSL_RCVD", AdifDataType::String, "QSL received status"),
            ("COMMENT", AdifDataType::String, "Comments"),
            ("NOTES", AdifDataType::MultilineString, "Notes"),
        ];

        for (name, data_type, description) in standard_fields {
            let field_def = AdifFieldDef {
                name: name.to_string(),
                data_type,
                description: description.to_string(),
                required: name == "CALL" || name == "QSO_DATE" || name == "TIME_ON",
                min_length: None,
                max_length: None,
            };

            self.field_definitions.insert(name.to_string(), field_def);
        }
    }

    fn format_field(&self, name: &str, value: &str) -> Result<String, AdifError> {
        if value.is_empty() {
            return Ok(String::new());
        }

        Ok(format!(
            "<{}:{}>{} ",
            name.to_uppercase(),
            value.len(),
            value
        ))
    }

    /// [`Self::format_field`] for a field that was read from a file: a type
    /// indicator (`<APP_LOGGER_SCORE:2:N>`) is kept, so an imported record
    /// re-renders exactly as it was read (PAN-127).
    fn format_kept_field(&self, field: &AdifField) -> Result<String, AdifError> {
        if field.value.is_empty() {
            return Ok(String::new());
        }
        let data_type = field
            .data_type
            .as_deref()
            .filter(|data_type| !data_type.is_empty());
        Ok(match data_type {
            Some(data_type) => format!(
                "<{}:{}:{}>{} ",
                field.name.to_uppercase(),
                field.value.len(),
                data_type,
                field.value
            ),
            None => format!(
                "<{}:{}>{} ",
                field.name.to_uppercase(),
                field.value.len(),
                field.value
            ),
        })
    }

    fn parse_datetime(&self, datetime: &str) -> Result<DateTime<Utc>, AdifError> {
        // Try different datetime formats
        let formats = vec!["%Y%m%d %H%M%S", "%Y-%m-%d %H:%M:%S", "%Y%m%d_%H%M%S"];

        for format in formats {
            if let Ok(naive_dt) = NaiveDateTime::parse_from_str(datetime, format) {
                return Ok(Utc.from_utc_datetime(&naive_dt));
            }
        }

        Err(AdifError::InvalidDateTime {
            datetime: datetime.to_string(),
        })
    }

    fn parse_qso_datetime(&self, record: &AdifRecord) -> Result<DateTime<Utc>, AdifError> {
        let date = self.get_required_field(record, "QSO_DATE")?;
        let time = self
            .get_field_value(record, "TIME_ON")
            .map(seconds_if_missing)
            .unwrap_or_else(|| "000000".to_string());

        let datetime_str = format!("{} {}", date, time);
        self.parse_datetime(&datetime_str)
    }

    fn parse_optional_datetime(
        &self,
        record: &AdifRecord,
        date_field: &str,
        time_field: &str,
    ) -> Result<Option<DateTime<Utc>>, AdifError> {
        if let Some(date) = self.get_field_value(record, date_field) {
            let time = self
                .get_field_value(record, time_field)
                .map(seconds_if_missing)
                .unwrap_or_else(|| "000000".to_string());
            let datetime_str = format!("{} {}", date, time);
            Ok(Some(self.parse_datetime(&datetime_str)?))
        } else {
            Ok(None)
        }
    }

    fn get_required_field(
        &self,
        record: &AdifRecord,
        field_name: &str,
    ) -> Result<String, AdifError> {
        record
            .fields
            .get(&field_name.to_uppercase())
            .map(|f| f.value.clone())
            .ok_or_else(|| AdifError::MissingField {
                field: field_name.to_string(),
            })
    }

    fn get_field_value(&self, record: &AdifRecord, field_name: &str) -> Option<String> {
        record
            .fields
            .get(&field_name.to_uppercase())
            .map(|f| f.value.clone())
    }

    fn extract_additional_fields(&self, record: &AdifRecord) -> HashMap<String, String> {
        let mut additional = HashMap::new();

        for (name, field) in &record.fields {
            if !self.field_definitions.contains_key(name) {
                additional.insert(name.clone(), field.value.clone());
            }
        }

        additional
    }

    fn add_field(&self, fields: &mut HashMap<String, AdifField>, name: &str, value: &str) {
        if !value.is_empty() {
            fields.insert(
                name.to_uppercase(),
                AdifField {
                    name: name.to_uppercase(),
                    value: value.to_string(),
                    length: value.len(),
                    data_type: None,
                },
            );
        }
    }

    fn add_optional_field(
        &self,
        fields: &mut HashMap<String, AdifField>,
        name: &str,
        value: &Option<String>,
    ) {
        if let Some(ref val) = value {
            self.add_field(fields, name, val);
        }
    }

    pub fn frequency_to_band(&self, frequency_hz: f64) -> String {
        // Delegates to the free function — see `freq_hz_to_band`. The method is
        // kept for the existing `&self` call sites; it uses no instance state.
        freq_hz_to_band(frequency_hz)
    }

    fn signal_report_to_rst(&self, signal_report: SignalReport) -> String {
        // FT8 stores raw SNR dB values directly (ADIF 3.1 supports this)
        format!("{:+03}", signal_report)
    }

    fn rst_to_signal_report(&self, rst: &str) -> Option<SignalReport> {
        // Parse raw SNR dB value (e.g., "-15", "+03")
        rst.parse::<SignalReport>().ok()
    }

    /// Generate a standalone ADIF file header string (ends with `<EOH>\n\n`).
    ///
    /// Used by [`crate::adif_log_writer::AdifLogWriter`] when creating a new log file.
    /// The header is written exactly once; subsequent opens append records directly.
    pub fn generate_header(&self) -> String {
        let mut out = String::new();
        out.push_str("ADIF Export by pancetta\n\n");
        // Infallible — format_field only fails when value is empty, which won't happen here
        out.push_str(&self.format_field("ADIF_VER", "3.1.0").unwrap_or_default());
        out.push_str(
            &self
                .format_field("PROGRAMID", "pancetta")
                .unwrap_or_default(),
        );
        out.push_str(
            &self
                .format_field(
                    "CREATED_TIMESTAMP",
                    &chrono::Utc::now().format("%Y%m%d %H%M%S").to_string(),
                )
                .unwrap_or_default(),
        );
        out.push_str("\n<EOH>\n\n");
        out
    }

    /// Generate a single ADIF record string for one QSO (ends with `<EOR>\n\n`).
    ///
    /// Used by [`crate::adif_log_writer::AdifLogWriter`] to append individual records.
    /// The conversion from [`QsoMetadata`] is handled via [`Self::qso_to_adif`] and
    /// [`Self::qso_to_record`] so no duplicate logic is introduced here.
    pub fn generate_record(&self, qso: &AdifQso) -> Result<String, AdifError> {
        let record = self.qso_to_record(qso);
        let mut out = String::new();
        for field in record.fields.values() {
            out.push_str(&self.format_field(&field.name, &field.value)?);
        }
        out.push_str("<EOR>\n\n");
        Ok(out)
    }

    /// Render `record` with every field as given: an imported contact keeps its
    /// own QSL status, comments and APP_ fields, and nothing pancetta-specific is
    /// added. CALL, QSO_DATE, TIME_ON first, then the rest by name. Empty values
    /// are left out, as [`Self::generate_record`] does. Ends with `<EOR>\n\n`.
    pub fn render_record(&self, record: &AdifRecord) -> Result<String, AdifError> {
        const FIRST: [&str; 3] = ["CALL", "QSO_DATE", "TIME_ON"];
        let mut rest: Vec<(&String, &AdifField)> = record
            .fields
            .iter()
            .filter(|(name, _)| !FIRST.contains(&name.as_str()))
            .collect();
        rest.sort_by(|a, b| a.0.cmp(b.0));

        let mut out = String::new();
        for name in FIRST {
            if let Some(field) = record.fields.get(name) {
                out.push_str(&self.format_kept_field(field)?);
            }
        }
        for (_, field) in rest {
            out.push_str(&self.format_kept_field(field)?);
        }
        out.push_str("<EOR>\n\n");
        Ok(out)
    }
}

/// ADIF allows `HHMM` as well as `HHMMSS`; read `HHMM` as `HHMM00`.
fn seconds_if_missing(time: String) -> String {
    if time.len() == 4 && time.bytes().all(|b| b.is_ascii_digit()) {
        time + "00"
    } else {
        time
    }
}

/// The next `<…>` tag at or after `from`, as the byte positions of its `<`
/// and `>`. A `<` inside a tag starts the tag over, so stray text such as
/// `a<b` never swallows the tag that follows it.
fn next_tag(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut open = from + data.get(from..)?.iter().position(|&b| b == b'<')?;
    let mut at = open + 1;
    loop {
        match data.get(at)? {
            b'>' => return Some((open, at)),
            b'<' => open = at,
            _ => {}
        }
        at += 1;
    }
}

/// `data[start..end]` as text; both ends sit on ASCII bytes (`<`, `>`), so
/// they are always char boundaries of `text`.
fn decode(data: &[u8], text: Option<&str>, start: usize, end: usize) -> String {
    match text {
        Some(text) => text[start..end].to_string(),
        None => data[start..end].iter().map(|&b| b as char).collect(),
    }
}

/// Close the record being read: its fields, or its first error together with
/// its CALL when that field parsed. A record with neither is skipped.
fn push_record(
    records: &mut Vec<Result<AdifRecord, AdifRecordError>>,
    fields: &mut HashMap<String, AdifField>,
    error: &mut Option<AdifError>,
) {
    let fields = std::mem::take(fields);
    match error.take() {
        Some(error) => {
            let call = fields
                .get("CALL")
                .map(|f| f.value.trim().to_string())
                .filter(|call| !call.is_empty());
            records.push(Err(AdifRecordError { error, call }));
        }
        None if fields.is_empty() => {}
        None => records.push(Ok(AdifRecord { fields })),
    }
}

/// ADIF counts a value's length in characters, but pancetta (and most C/Python
/// writers) count bytes. Cut at the byte count unless only the character count
/// lands just before the next tag. Non-UTF-8 input is Latin-1: one byte, one char.
fn read_value(data: &[u8], text: Option<&str>, start: usize, len: usize) -> (String, usize) {
    let byte_end = start.saturating_add(len).min(data.len());
    let Some(text) = text else {
        return (
            data[start..byte_end].iter().map(|&b| b as char).collect(),
            byte_end,
        );
    };
    if data[start..byte_end].is_ascii() {
        return (text[start..byte_end].to_string(), byte_end);
    }
    // A byte count can land inside a character: round up to the next boundary.
    let byte_cut = (byte_end..=text.len())
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(text.len());
    let char_cut = text[start..]
        .char_indices()
        .nth(len)
        .map_or(text.len(), |(i, _)| start + i);
    let end = if before_tag(data, byte_cut) || !before_tag(data, char_cut) {
        byte_cut
    } else {
        char_cut
    };
    (text[start..end].to_string(), end)
}

/// True when only whitespace separates `at` from the end, or from a `<` that
/// [`next_tag`] does not start over: no other `<` comes before its `>` (or
/// the end, for a file cut off inside its last tag). A stray `<` in a value,
/// such as the one in `üñabc<x<MODE:3>`, is not a tag.
fn before_tag(data: &[u8], at: usize) -> bool {
    match data[at..].iter().position(|b| !b.is_ascii_whitespace()) {
        None => true,
        Some(i) => {
            data[at + i] == b'<' && next_tag(data, at + i).is_none_or(|(open, _)| open == at + i)
        }
    }
}

impl Default for AdifProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AdifField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}:{}>{}", self.name, self.length, self.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_adif() {
        let adif_data = r#"
ADIF Export for Test Program

<ADIF_VER:5>3.1.0
<PROGRAMID:8>TestProg
<EOH>

<CALL:5>W1ABC<QSO_DATE:8>20230515<TIME_ON:6>123000<MODE:4>DATA<SUBMODE:3>FT8<FREQ:9>14.074000<BAND:3>20M<RST_SENT:3>-15<RST_RCVD:3>-12<EOR>

<CALL:5>K1DEF<QSO_DATE:8>20230515<TIME_ON:6>124500<MODE:4>DATA<SUBMODE:3>FT8<FREQ:9>14.074000<BAND:3>20M<RST_SENT:3>-18<RST_RCVD:3>-09<EOR>
"#;

        let processor = AdifProcessor::new();
        let result = processor.parse_string(adif_data).unwrap();

        assert_eq!(result.records.len(), 2);
        assert_eq!(result.header.version, "3.1.0");
        assert_eq!(result.header.program_id, "TestProg");

        let first_qso = processor.record_to_qso(&result.records[0]).unwrap();
        assert_eq!(first_qso.call, "W1ABC");
        assert_eq!(first_qso.mode, "DATA");
        assert_eq!(first_qso.submode, Some("FT8".to_string()));
        assert_eq!(first_qso.freq, 14.074000);
    }

    #[test]
    fn test_generate_adif() {
        let processor = AdifProcessor::new();

        let qso = AdifQso {
            qso_date: Utc::now(),
            qso_date_off: None,
            call: "W1ABC".to_string(),
            mode: "DATA".to_string(),
            submode: Some("FT8".to_string()),
            freq: 14.074000,
            band: "20M".to_string(),
            rst_sent: Some("-15".to_string()),
            rst_rcvd: Some("-12".to_string()),
            tx_pwr: Some(100.0),
            station_callsign: "K1XYZ".to_string(),
            operator: None,
            my_gridsquare: Some("FN42".to_string()),
            gridsquare: Some("FN31".to_string()),
            country: None,
            dxcc: None,
            cqz: None,
            ituz: None,
            state: None,
            contest_id: None,
            stx: None,
            stx_string: None,
            srx: None,
            srx_string: None,
            qsl_sent: Some("N".to_string()),
            qsl_rcvd: Some("N".to_string()),
            qslmsg: None,
            comment: Some("Great QSO!".to_string()),
            notes: None,
            additional_fields: HashMap::new(),
        };

        let record = processor.qso_to_record(&qso);
        assert!(record.fields.contains_key("CALL"));
        assert!(record.fields.contains_key("MODE"));
        assert!(record.fields.contains_key("SUBMODE"));
        assert_eq!(record.fields.get("CALL").unwrap().value, "W1ABC");
    }

    #[test]
    fn test_frequency_to_band() {
        let processor = AdifProcessor::new();

        assert_eq!(processor.frequency_to_band(14074000.0), "20M");
        assert_eq!(processor.frequency_to_band(7074000.0), "40M");
        assert_eq!(processor.frequency_to_band(3573000.0), "80M");
        assert_eq!(processor.frequency_to_band(50313000.0), "6M");
    }

    #[test]
    fn test_signal_report_conversion() {
        let processor = AdifProcessor::new();

        assert_eq!(processor.signal_report_to_rst(-5), "-05");
        assert_eq!(processor.signal_report_to_rst(-15), "-15");
        assert_eq!(processor.signal_report_to_rst(-20), "-20");
        assert_eq!(processor.signal_report_to_rst(-25), "-25");
        assert_eq!(processor.signal_report_to_rst(3), "+03");

        // rst_to_signal_report should parse raw SNR values
        assert_eq!(processor.rst_to_signal_report("-15"), Some(-15));
        assert_eq!(processor.rst_to_signal_report("+03"), Some(3));
    }

    // ── Hound ADIF rendering ─────────────────────────────────────────────────

    /// Build a minimal QsoMetadata for ADIF rendering tests.
    fn hound_adif_metadata(hound: bool, notes: Option<&str>) -> crate::states::QsoMetadata {
        use crate::states::{CallInitiation, GridSquares, QsoRole, SignalReports};
        use chrono::Utc;
        use uuid::Uuid;

        crate::states::QsoMetadata {
            qso_id: Uuid::new_v4(),
            our_callsign: "K5ARH".into(),
            their_callsign: Some("D2UY".into()),
            frequency: 14_074_000.0,
            completed_rf_frequency_hz: None,
            mode: "FT8".into(),
            start_time: Utc::now(),
            end_time: None,
            reports: SignalReports {
                sent: Some(-12),
                received: Some(-9),
            },
            grids: GridSquares {
                ours: Some("EM20".into()),
                theirs: Some("JI64".into()),
            },
            their_state: None,
            contest_info: None,
            tags: HashMap::new(),
            notes: notes.map(|s| s.to_string()),
            tx_parity: None,
            initiated_by: CallInitiation::Manual,
            role: QsoRole::Caller,
            call_count: 1,
            first_call_at: None,
            last_call_at: None,
            progressed_this_cycle: false,
            stall_cycles: 0,
            last_known_good_offset_hz: None,
            advance_generation: 0,
            pre_switch_offset: None,
            hound,
            partner_freq: if hound { Some(1500.0) } else { None },
            pending_freq_drift: None,
            hound_qsyed: false,
            remote_origin: false,
            remote_client_key_id: None,
            tx_parity_provisional: false,
        }
    }

    /// A completed Hound QSO must have MODE=FT8, COMMENT containing "HOUND",
    /// and APP_PANCETTA_HOUND=true.  SUBMODE must NOT be set to anything other
    /// than FT8/None (Hound is plain FT8, not a new mode).
    #[test]
    fn hound_qso_adif_mode_ft8_comment_hound_app_field() {
        let processor = AdifProcessor::new();
        let metadata = hound_adif_metadata(true, None);

        let adif_qso = processor.qso_to_adif(&metadata, None);

        // MODE stays "FT8", SUBMODE is None
        assert_eq!(adif_qso.mode, "FT8", "MODE must remain FT8 for a Hound QSO");
        assert!(
            adif_qso.submode.is_none(),
            "SUBMODE must not be set for a Hound QSO; got {:?}",
            adif_qso.submode
        );

        // COMMENT contains "HOUND"
        let comment = adif_qso.comment.as_deref().unwrap_or("");
        assert!(
            comment.contains("HOUND"),
            "COMMENT must contain 'HOUND'; got {:?}",
            adif_qso.comment
        );

        // APP_PANCETTA_HOUND is in additional_fields
        assert_eq!(
            adif_qso.additional_fields.get("APP_PANCETTA_HOUND"),
            Some(&"true".to_string()),
            "APP_PANCETTA_HOUND must be 'true' in additional_fields"
        );

        // Verify it appears in the rendered ADIF record string
        let record_str = processor.generate_record(&adif_qso).unwrap();
        assert!(
            record_str.contains("APP_PANCETTA_HOUND"),
            "rendered ADIF must contain APP_PANCETTA_HOUND; got:\n{record_str}"
        );
        assert!(
            record_str.contains("HOUND"),
            "rendered ADIF must contain HOUND in COMMENT; got:\n{record_str}"
        );
        // Regression guard: must NOT emit a bare non-APP_-prefixed HOUND field.
        // A `<HOUND:4>true` field is not a valid ADIF field name and can trip
        // LoTW validation. Only the `APP_PANCETTA_HOUND` variant is acceptable.
        assert!(
            !record_str.contains("<HOUND:"),
            "must not emit a bare non-APP HOUND field; got:\n{record_str}"
        );
    }

    /// A Hound QSO that already has operator notes must have COMMENT =
    /// "<existing notes> HOUND" — the notes are preserved and HOUND is appended.
    #[test]
    fn hound_qso_adif_comment_appends_to_existing_notes() {
        let processor = AdifProcessor::new();
        let metadata = hound_adif_metadata(true, Some("Big pile-up!"));

        let adif_qso = processor.qso_to_adif(&metadata, None);

        let comment = adif_qso.comment.as_deref().unwrap_or("");
        assert!(
            comment.contains("Big pile-up!"),
            "existing notes must be preserved in COMMENT; got {:?}",
            adif_qso.comment
        );
        assert!(
            comment.contains("HOUND"),
            "COMMENT must contain 'HOUND'; got {:?}",
            adif_qso.comment
        );
    }

    /// A non-Hound QSO (hound=false) must NOT contain APP_PANCETTA_HOUND and
    /// must NOT inject "HOUND" into the COMMENT from any internal path.
    /// The output must be byte-identical to what existed before this feature.
    #[test]
    fn non_hound_qso_adif_no_app_field_no_hound_comment() {
        let processor = AdifProcessor::new();
        let metadata = hound_adif_metadata(false, None);

        let adif_qso = processor.qso_to_adif(&metadata, None);

        assert!(
            !adif_qso
                .additional_fields
                .contains_key("APP_PANCETTA_HOUND"),
            "non-Hound QSO must NOT have APP_PANCETTA_HOUND in additional_fields"
        );

        let comment = adif_qso.comment.as_deref().unwrap_or("");
        assert!(
            !comment.contains("HOUND"),
            "non-Hound QSO COMMENT must not contain 'HOUND'; got {:?}",
            adif_qso.comment
        );

        // Verify the raw record also has no HOUND traces
        let record_str = processor.generate_record(&adif_qso).unwrap();
        assert!(
            !record_str.contains("APP_PANCETTA_HOUND"),
            "non-Hound rendered ADIF must not contain APP_PANCETTA_HOUND; got:\n{record_str}"
        );
    }

    /// TX_PWR is only stamped once the processor is configured with the
    /// operator's station power; an unconfigured processor omits it entirely
    /// rather than emitting `TX_PWR:0`.
    #[test]
    fn station_power_watts_stamps_tx_pwr_when_configured() {
        let metadata = hound_adif_metadata(false, None);

        let unconfigured = AdifProcessor::new();
        let adif_qso = unconfigured.qso_to_adif(&metadata, None);
        assert_eq!(
            adif_qso.tx_pwr, None,
            "unconfigured processor must omit TX_PWR"
        );
        let record_str = unconfigured.generate_record(&adif_qso).unwrap();
        assert!(!record_str.contains("TX_PWR"), "got:\n{record_str}");

        let configured = AdifProcessor::new().with_station_power_watts(100);
        let adif_qso = configured.qso_to_adif(&metadata, None);
        assert_eq!(adif_qso.tx_pwr, Some(100.0));
        let record_str = configured.generate_record(&adif_qso).unwrap();
        assert!(record_str.contains("TX_PWR"), "got:\n{record_str}");

        // 0 means "unconfigured", not a literal zero-watt stamp.
        let zero_watts = AdifProcessor::new().with_station_power_watts(0);
        let adif_qso = zero_watts.qso_to_adif(&metadata, None);
        assert_eq!(adif_qso.tx_pwr, None);
    }

    /// A non-Hound QSO with existing notes must preserve them verbatim as a
    /// prefix of COMMENT (2026-07-06: notes are no longer emitted alone —
    /// software attribution is appended to every QSO — but the operator's
    /// own text must never be altered or dropped).
    #[test]
    fn non_hound_qso_adif_notes_unchanged() {
        let processor = AdifProcessor::new();
        let metadata = hound_adif_metadata(false, Some("Good contact"));

        let adif_qso = processor.qso_to_adif(&metadata, None);

        let comment = adif_qso.comment.as_deref().unwrap_or("");
        assert!(
            comment.starts_with("Good contact"),
            "existing notes must be preserved verbatim as a prefix of COMMENT; got {:?}",
            adif_qso.comment
        );
    }

    /// Every QSO's COMMENT carries the cqdx.io/Pancetta attribution, full
    /// form (with URLs) when there's room — the common case (no/short notes).
    #[test]
    fn every_qso_gets_full_attribution_by_default() {
        let processor = AdifProcessor::new();
        let metadata = hound_adif_metadata(false, None);

        let adif_qso = processor.qso_to_adif(&metadata, None);

        assert_eq!(adif_qso.comment.as_deref(), Some(ATTRIBUTION_FULL));
    }

    /// When existing notes are long enough that the full attribution
    /// (with URLs) would blow past the soft max, fall back to the short
    /// form rather than dropping attribution or truncating operator notes.
    #[test]
    fn long_notes_fall_back_to_short_attribution() {
        let processor = AdifProcessor::new();
        // Long enough that notes + ATTRIBUTION_FULL exceeds COMMENT_SOFT_MAX_LEN
        // but notes + ATTRIBUTION_SHORT still fits.
        let long_notes = "x".repeat(200);
        let metadata = hound_adif_metadata(false, Some(&long_notes));

        let adif_qso = processor.qso_to_adif(&metadata, None);
        let comment = adif_qso.comment.as_deref().unwrap_or("");

        assert!(
            comment.starts_with(&long_notes),
            "operator notes must be preserved verbatim"
        );
        assert!(
            comment.contains(ATTRIBUTION_SHORT),
            "expected short attribution; got {:?}",
            adif_qso.comment
        );
        assert!(
            !comment.contains("https://"),
            "full attribution (with URLs) must not be used when it wouldn't fit; got {:?}",
            adif_qso.comment
        );
    }

    /// When even the short attribution wouldn't fit, operator notes are
    /// preserved exactly and attribution is dropped entirely — notes are
    /// never truncated to make room for it.
    #[test]
    fn very_long_notes_drop_attribution_entirely() {
        let processor = AdifProcessor::new();
        let very_long_notes = "x".repeat(COMMENT_SOFT_MAX_LEN);
        let metadata = hound_adif_metadata(false, Some(&very_long_notes));

        let adif_qso = processor.qso_to_adif(&metadata, None);

        assert_eq!(
            adif_qso.comment.as_deref(),
            Some(very_long_notes.as_str()),
            "attribution must be dropped, not truncate operator notes"
        );
    }

    /// Attribution composes with the HOUND marker: notes, then HOUND, then
    /// attribution, all in one COMMENT field.
    #[test]
    fn hound_qso_comment_includes_attribution_too() {
        let processor = AdifProcessor::new();
        let metadata = hound_adif_metadata(true, Some("Big pile-up!"));

        let adif_qso = processor.qso_to_adif(&metadata, None);
        let comment = adif_qso.comment.as_deref().unwrap_or("");

        assert!(comment.contains("Big pile-up!"));
        assert!(comment.contains("HOUND"));
        assert!(comment.contains(ATTRIBUTION_FULL));
    }

    // ── PAN-85: state round-trip through ADIF ───────────────────────────────

    #[test]
    fn adif_import_carries_state_into_metadata() {
        let adif_data = "Pancetta state import test\n\
            <ADIF_VER:5>3.1.4 <PROGRAMID:8>pancetta\n\
            <EOH>\n\
            <CALL:5>W5ABC <QSO_DATE:8>20230515 <TIME_ON:6>123000 \
            <MODE:4>DATA <SUBMODE:3>FT8 <FREQ:9>14.074000 <BAND:3>20M \
            <STATE:2>AR\n\
            <EOR>\n";

        let processor = AdifProcessor::new();
        let result = processor.parse_string(adif_data).unwrap();
        let adif_qso = processor.record_to_qso(&result.records[0]).unwrap();
        assert_eq!(adif_qso.state.as_deref(), Some("AR"));

        let metadata = processor.adif_to_qso(&adif_qso);
        assert_eq!(
            metadata.their_state.as_deref(),
            Some("AR"),
            "STATE must survive adif_to_qso"
        );
    }

    #[test]
    fn adif_export_emits_state_when_known() {
        let processor = AdifProcessor::new();
        let mut metadata = hound_adif_metadata(false, None);
        metadata.their_state = Some("AR".into());

        let adif_qso = processor.qso_to_adif(&metadata, None);
        assert_eq!(adif_qso.state.as_deref(), Some("AR"));

        let record = processor.qso_to_record(&adif_qso);
        assert_eq!(
            record.fields.get("STATE").map(|f| f.value.as_str()),
            Some("AR")
        );
    }

    #[test]
    fn adif_export_omits_state_when_unknown() {
        let processor = AdifProcessor::new();
        let metadata = hound_adif_metadata(false, None);

        let adif_qso = processor.qso_to_adif(&metadata, None);
        assert!(adif_qso.state.is_none());

        let record = processor.qso_to_record(&adif_qso);
        assert!(!record.fields.contains_key("STATE"));
    }

    // ── PAN-127: reading files other loggers write ──────────────────────────

    fn ok_calls(parsed: &AdifParse) -> Vec<&str> {
        parsed
            .records
            .iter()
            .map(|r| {
                r.as_ref().expect("record parses").fields["CALL"]
                    .value
                    .as_str()
            })
            .collect()
    }

    fn only_record(parsed: &AdifParse) -> &AdifRecord {
        assert_eq!(parsed.records.len(), 1, "{:?}", parsed.records);
        parsed.records[0].as_ref().expect("record parses")
    }

    #[test]
    fn parse_bytes_reads_a_headerless_file() {
        let adif = "<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:6>120000 <EOR>\n\
                    <CALL:5>K1ABC <QSO_DATE:8>20240101 <TIME_ON:6>121500 <EOR>\n";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        assert_eq!(ok_calls(&parsed), ["W1AW", "K1ABC"]);
    }

    #[test]
    fn parse_bytes_reads_header_and_records_on_one_line() {
        let adif = "x<ADIF_VER:5>3.1.4<EOH><CALL:4>W1AW<QSO_DATE:8>20240101<EOR>\
                    <CALL:5>K1ABC<QSO_DATE:8>20240101<EOR>";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        assert_eq!(parsed.header.version, "3.1.4");
        assert_eq!(ok_calls(&parsed), ["W1AW", "K1ABC"]);
    }

    #[test]
    fn parse_bytes_splits_two_records_on_one_line() {
        let adif = "WSJT-X ADIF Export\n<adif_ver:5>3.1.0\n<eoh>\n\
                    <call:4>W1AW <qso_date:8>20240101 <time_on:6>120000 <eor> \
                    <call:5>K1ABC <qso_date:8>20240101 <time_on:6>121500 <eor>\n";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        assert_eq!(ok_calls(&parsed), ["W1AW", "K1ABC"]);
    }

    #[test]
    fn parse_bytes_keeps_a_multiline_value_verbatim() {
        let adif = "<CALL:4>W1AW <NOTES:11>line1\nline2 <EOR>\n";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        assert_eq!(only_record(&parsed).fields["NOTES"].value, "line1\nline2");
    }

    #[test]
    fn parse_bytes_isolates_a_malformed_record() {
        let adif = "<EOH>\n\
                    <CALL:4>W1AW <QSO_DATE:8>20240101 <EOR>\n\
                    <CALL:x>K1ABC <QSO_DATE:8>20240101 <EOR>\n\
                    <CALL:5>N0XYZ <QSO_DATE:8>20240101 <EOR>\n";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        assert_eq!(parsed.records.len(), 3, "{:?}", parsed.records);
        assert_eq!(
            parsed.records[0].as_ref().unwrap().fields["CALL"].value,
            "W1AW"
        );
        match &parsed.records[1] {
            Err(AdifRecordError {
                error: AdifError::InvalidField { field },
                call: None,
            }) => assert_eq!(field, "CALL:x"),
            other => panic!("expected InvalidField(CALL:x) with no call, got {other:?}"),
        }
        assert_eq!(
            parsed.records[2].as_ref().unwrap().fields["CALL"].value,
            "N0XYZ"
        );
    }

    #[test]
    fn parse_bytes_reports_the_call_of_a_malformed_record() {
        let adif = "<CALL:5>K1ABC <TX_PWR:x>5 <QSO_DATE:8>20240101 <EOR>\n";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        assert_eq!(parsed.records.len(), 1);
        match &parsed.records[0] {
            Err(AdifRecordError {
                error: AdifError::InvalidField { field },
                call,
            }) => {
                assert_eq!(field, "TX_PWR:x");
                assert_eq!(call.as_deref(), Some("K1ABC"));
            }
            other => panic!("expected InvalidField(TX_PWR:x), got {other:?}"),
        }
    }

    #[test]
    fn parse_string_still_fails_on_a_malformed_record() {
        let adif = "<EOH>\n\
                    <CALL:4>W1AW <QSO_DATE:8>20240101 <EOR>\n\
                    <CALL:x>K1ABC <QSO_DATE:8>20240101 <EOR>\n";

        match AdifProcessor::new().parse_string(adif) {
            Err(AdifError::InvalidField { field }) => assert_eq!(field, "CALL:x"),
            other => panic!("expected InvalidField(CALL:x), got {other:?}"),
        }
    }

    #[test]
    fn parse_bytes_skips_empty_records() {
        let adif = "<CALL:4>W1AW <EOR><EOR>\n<EOR> <CALL:5>K1ABC <EOR>\n<eor>";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        assert_eq!(ok_calls(&parsed), ["W1AW", "K1ABC"]);
    }

    #[test]
    fn record_to_qso_accepts_four_digit_times() {
        let adif = "<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:4>1230 \
                    <QSO_DATE_OFF:8>20240101 <TIME_OFF:4>1231 <EOR>\n";
        let processor = AdifProcessor::new();
        let parsed = processor.parse_bytes(adif.as_bytes());

        let qso = processor.record_to_qso(only_record(&parsed)).unwrap();

        assert_eq!(
            qso.qso_date,
            Utc.with_ymd_and_hms(2024, 1, 1, 12, 30, 0).unwrap()
        );
        assert_eq!(
            qso.qso_date_off,
            Some(Utc.with_ymd_and_hms(2024, 1, 1, 12, 31, 0).unwrap())
        );
    }

    #[test]
    fn parse_bytes_reads_byte_counted_utf8_values() {
        // Byte counts, as pancetta's own `format_field` writes them.
        let adif = "<CALL:4>W1AW <NAME:7>Müñoz <QTH:7>Zürich <EOR>\n";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        let record = only_record(&parsed);
        assert_eq!(record.fields["NAME"].value, "Müñoz");
        assert_eq!(record.fields["QTH"].value, "Zürich");
    }

    #[test]
    fn parse_bytes_reads_character_counted_utf8_values() {
        // Character counts, as the ADIF spec (and WSJT-X-family writers) count.
        let adif = "<CALL:4>W1AW <NAME:5>Müñoz <QTH:6>Zürich <EOR>\n";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        let record = only_record(&parsed);
        assert_eq!(record.fields["NAME"].value, "Müñoz");
        assert_eq!(record.fields["QTH"].value, "Zürich");
    }

    #[test]
    fn parse_bytes_keeps_a_less_than_sign_in_a_character_counted_value() {
        // Seven characters, nine bytes: the byte count lands before `<x`,
        // which is part of the value, not a tag.
        let adif = "<CALL:4>W1AW <NOTES:7>üñabc<x<MODE:3>FT8 <EOR>\n";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        let record = only_record(&parsed);
        assert_eq!(record.fields["NOTES"].value, "üñabc<x");
        assert_eq!(record.fields["MODE"].value, "FT8");
    }

    #[test]
    fn parse_bytes_cuts_a_byte_counted_value_before_a_truncated_tag() {
        // A file cut off inside its last tag: the byte count still ends the
        // value, not a character count that runs into the broken tag.
        let adif = "<CALL:4>W1AW <NAME:7>Müñoz<E";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        let record = only_record(&parsed);
        assert_eq!(record.fields["NAME"].value, "Müñoz");
    }

    #[test]
    fn parse_bytes_decodes_latin1_values() {
        let adif = b"<CALL:4>W1AW <NAME:4>Jos\xe9 <EOR>\n";

        let parsed = AdifProcessor::new().parse_bytes(adif);

        let record = only_record(&parsed);
        assert_eq!(record.fields["CALL"].value, "W1AW");
        assert_eq!(record.fields["NAME"].value, "José");
    }

    #[test]
    fn render_record_puts_call_date_time_first_then_sorted() {
        let processor = AdifProcessor::new();
        let parsed = processor.parse_bytes(
            b"<MODE:3>FT8 <time_on:6>120000 <BAND:3>20M <APP_X:1>y \
              <QSO_DATE:8>20240101 <CALL:4>W1AW <COMMENT:0> <EOR>",
        );

        let rendered = processor.render_record(only_record(&parsed)).unwrap();

        assert_eq!(
            rendered,
            "<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:6>120000 \
             <APP_X:1>y <BAND:3>20M <MODE:3>FT8 <EOR>\n\n"
        );
    }

    /// The (name, value) pairs of `record`, without empty values.
    fn field_map(record: &AdifRecord) -> std::collections::BTreeMap<String, String> {
        record
            .fields
            .iter()
            .filter(|(_, f)| !f.value.is_empty())
            .map(|(name, f)| (name.clone(), f.value.clone()))
            .collect()
    }

    /// An explicit type indicator (`<APP_LOGGER_SCORE:2:N>`) is part of how
    /// the field is written, so an imported record keeps it (PAN-127).
    #[test]
    fn render_record_keeps_a_field_type_indicator() {
        let processor = AdifProcessor::new();
        let parsed = processor.parse_bytes(
            b"<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:6>120000 <MODE:3>FT8 \
              <APP_LOGGER_SCORE:2:N>42 <EOR>",
        );
        let record = only_record(&parsed);
        assert_eq!(
            record.fields["APP_LOGGER_SCORE"].data_type.as_deref(),
            Some("N")
        );

        let rendered = processor.render_record(record).unwrap();
        assert!(rendered.contains("<APP_LOGGER_SCORE:2:N>42 "), "{rendered}");

        let reread = processor.parse_string(&rendered).unwrap();
        assert_eq!(reread.records.len(), 1);
        assert_eq!(
            reread.records[0].fields["APP_LOGGER_SCORE"]
                .data_type
                .as_deref(),
            Some("N")
        );
        assert_eq!(reread.records[0].fields["APP_LOGGER_SCORE"].value, "42");
    }

    #[test]
    fn render_record_round_trips_through_parse_string() {
        let processor = AdifProcessor::new();
        let original = "<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:4>1200 <MODE:4>MFSK \
                        <SUBMODE:3>FT4 <BAND:3>20m <QSL_RCVD:1>Y <QSL_SENT:1>Y \
                        <APP_LOTW_QSL_RCVD:1>Y <APP_LOTW_RXQSL:19>2024-01-05 10:11:12 \
                        <NAME:5>Müñoz <QTH:7>Zürich <NOTES:19>line1 <b>\nline2 >x< \
                        <COMMENT:9>from N1MM <EOR>\n";
        let parsed = processor.parse_bytes(original.as_bytes());
        let record = only_record(&parsed);

        let rendered = processor.render_record(record).unwrap();
        let reread = processor.parse_string(&rendered).unwrap();

        assert_eq!(reread.records.len(), 1);
        assert_eq!(field_map(&reread.records[0]), field_map(record));
        assert_eq!(
            reread.records[0].fields["NOTES"].value,
            "line1 <b>\nline2 >x<"
        );
        assert_eq!(reread.records[0].fields["NAME"].value, "Müñoz");
        assert_eq!(reread.records[0].fields["COMMENT"].value, "from N1MM");
        assert_eq!(reread.records[0].fields["QSL_RCVD"].value, "Y");
        assert!(processor.record_to_qso(&reread.records[0]).is_ok());
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        fn value() -> impl Strategy<Value = String> {
            prop_oneof![
                any::<String>(),
                "[<>:\n a-zA-Z0-9üñé€]{1,24}",
                "(?s).{1,24}",
            ]
            .prop_filter("non-empty", |v| !v.is_empty())
        }

        proptest! {
            /// Any record pancetta renders reads back with the same fields,
            /// whatever its values hold (`<`, `>`, line breaks, non-ASCII).
            #[test]
            fn render_record_round_trips_any_record(
                fields in proptest::collection::btree_map("[A-Z_]{1,12}", value(), 1..8)
            ) {
                let processor = AdifProcessor::new();
                let record = AdifRecord {
                    fields: fields
                        .iter()
                        .map(|(name, value)| {
                            (
                                name.clone(),
                                AdifField {
                                    name: name.clone(),
                                    value: value.clone(),
                                    length: value.len(),
                                    data_type: None,
                                },
                            )
                        })
                        .collect(),
                };

                let rendered = processor.render_record(&record).unwrap();
                let reread = processor.parse_string(&rendered).unwrap();

                prop_assert_eq!(reread.records.len(), 1);
                prop_assert_eq!(field_map(&reread.records[0]), fields);
            }

            /// No input, however broken, makes the reader panic.
            #[test]
            fn parse_bytes_never_panics(
                data in prop_oneof![
                    proptest::collection::vec(any::<u8>(), 0..256),
                    "[<>:0-9A-Za-z éü€\n]{0,128}".prop_map(String::into_bytes),
                    proptest::collection::vec(
                        prop_oneof![
                            Just(b"<".to_vec()),
                            Just(b">".to_vec()),
                            Just(b":".to_vec()),
                            Just(b"<EOR>".to_vec()),
                            Just(b"<EOH>".to_vec()),
                            Just(b"<CALL:".to_vec()),
                            Just("ü".as_bytes().to_vec()),
                            Just(vec![0xe9]),
                            Just(vec![0xc3]),
                            "[0-9]{1,3}".prop_map(String::into_bytes),
                            "[A-Z ]{1,4}".prop_map(String::into_bytes),
                        ],
                        0..48,
                    )
                    .prop_map(|parts| parts.concat()),
                ]
            ) {
                let processor = AdifProcessor::new();
                let _ = processor.parse_bytes(&data);
                if let Ok(text) = std::str::from_utf8(&data) {
                    let _ = processor.parse_string(text);
                }
            }
        }
    }
}
