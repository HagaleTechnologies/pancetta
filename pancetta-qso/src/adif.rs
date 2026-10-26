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

    /// A declared length that runs past the end of the input, or swallows a
    /// tag and lands on no field boundary (PAN-214).
    #[error("Field length does not match its value: {field}")]
    LengthMismatch { field: String },
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

/// How a file counts its value lengths (PAN-214). ADIF counts characters but
/// allows only ASCII in .adi files, where the two agree; for UTF-8 values
/// writers differ — pancetta's own writer counts bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LengthUnit {
    Bytes,
    Characters,
}

/// Every record in an ADIF file, each parsed on its own (PAN-127), so one
/// malformed record no longer hides the records around it.
#[derive(Debug)]
pub struct AdifParse {
    pub header: AdifHeader,
    pub records: Vec<Result<AdifRecord, AdifRecordError>>,
    /// The unit this file's lengths were read in (PAN-214).
    pub length_unit: LengthUnit,
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

    /// ADIF data-type indicators (`<NAME:LEN:TYPE>`) the record carried, by
    /// field name, so the index → export path writes them back (PAN-214).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub field_types: HashMap<String, String>,
}

/// ADIF field names under which an imported record's QSL status is kept in
/// [`QsoMetadata::tags`].
const QSL_SENT_FIELD: &str = "QSL_SENT";
const QSL_RCVD_FIELD: &str = "QSL_RCVD";

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
    ///
    /// The length unit is decided once per file (PAN-214): a UTF-8 file with
    /// non-ASCII bytes is read once counting bytes and once counting
    /// characters, and the reading fewer values contradict is kept (see
    /// [`read_value`]). A value both units read cleanly, such as tag-shaped
    /// text in a character-counted value, decides nothing and follows the
    /// file; a tie reads as bytes, pancetta's own convention.
    pub fn parse_bytes(&self, data: &[u8]) -> AdifParse {
        let text = std::str::from_utf8(data).ok(); // None ⇒ decode as Latin-1

        // One byte is one character: nothing to decide.
        if text.is_none() || data.is_ascii() {
            return self.parse_in(data, text, LengthUnit::Bytes).0;
        }
        let (by_bytes, against_bytes) = self.parse_in(data, text, LengthUnit::Bytes);
        let (by_chars, against_chars) = self.parse_in(data, text, LengthUnit::Characters);
        if against_chars < against_bytes {
            by_chars
        } else {
            by_bytes
        }
    }

    /// The tokenizer behind [`Self::parse_bytes`], reading lengths in `unit`
    /// where that unit lands on a field boundary. Also returns how many values
    /// contradicted `unit`: only the other unit read them cleanly (PAN-214).
    fn parse_in(&self, data: &[u8], text: Option<&str>, unit: LengthUnit) -> (AdifParse, usize) {
        let mut header_fields: Option<HashMap<String, AdifField>> = None;
        let mut records = Vec::new();
        let mut fields = HashMap::new();
        let mut error: Option<AdifError> = None;
        let mut against = 0;
        let mut pos = 0;

        loop {
            let tag = next_tag(data, pos);
            // A field tag abandoned before its `>` (`<COMMENT:5abc<EOR>`)
            // would otherwise be skipped and its value silently lost: it
            // makes the record unreadable instead (PR #451 review).
            let scanned_to = tag.map_or(data.len(), |(open, _)| open);
            if let Some((start, end)) = abandoned_field_tag(data, pos, scanned_to) {
                error.get_or_insert(AdifError::InvalidField {
                    field: decode(data, text, start, end).trim_end().to_string(),
                });
            }
            let Some((open, close)) = tag else {
                break;
            };
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
                Some(Ok(length)) => length,
                // No length (`<COMMENT>secret`) is as unreadable as a
                // non-numeric one: reading it as zero would import the
                // contact with the value silently dropped (PR #451 review).
                None | Some(Err(_)) => {
                    // First error wins; the text up to the next tag is skipped.
                    error.get_or_insert(AdifError::InvalidField {
                        field: spec.clone(),
                    });
                    continue;
                }
            };
            let data_type = parts.next().map(str::to_string);

            match read_value(data, text, pos, length, unit) {
                Cut::Value {
                    value,
                    end,
                    contradicts,
                } => {
                    against += usize::from(contradicts);
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
                // The length cannot be right: the record is unreadable, and
                // scanning resumes at the value's first byte, so an <EOR> it
                // would have swallowed still closes this record and the next
                // one reads (PAN-214).
                Cut::Mismatch => {
                    error.get_or_insert(AdifError::LengthMismatch {
                        field: spec.clone(),
                    });
                }
            }
        }

        // A trailing record with no <EOR> still counts.
        push_record(&mut records, &mut fields, &mut error);

        let parsed = AdifParse {
            header: self.header_from_fields(header_fields.unwrap_or_default()),
            records,
            length_unit: unit,
        };
        (parsed, against)
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

        // Write records, each field with its type indicator if it has one
        // (PAN-214)
        for record in &file.records {
            for field in record.fields.values() {
                output.push_str(&self.format_kept_field(field)?);
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
        // An imported record's QSL status rides in the tags (see
        // `adif_to_qso`); a QSO Pancetta logged itself has none, so it
        // exports as not yet sent or received.
        let qsl_sent = additional_fields
            .remove(QSL_SENT_FIELD)
            .unwrap_or_else(|| "N".to_string());
        let qsl_rcvd = additional_fields
            .remove(QSL_RCVD_FIELD)
            .unwrap_or_else(|| "N".to_string());
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
            // A row persisted before PAN-52 has no `adif_contest_id` key;
            // deserialization restores its imported CONTEST_ID
            // (states.rs `legacy_adif_contest_id`).
            contest_id: contest_info.and_then(|c| c.adif_contest_id.clone()),
            stx: contest_info.and_then(|c| c.serials.sent),
            stx_string: None,
            srx: contest_info.and_then(|c| c.serials.received),
            srx_string: None,
            qsl_sent: Some(qsl_sent),
            qsl_rcvd: Some(qsl_rcvd),
            qslmsg: None,
            comment,
            notes: None,
            additional_fields,
            field_types: metadata.adif_field_types.clone(),
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
                    // PAN-52: keep the file's verbatim CONTEST_ID so the
                    // index → export path re-renders it byte-identically
                    // instead of dropping it (the export side stamps
                    // CONTEST_ID from this field, not from contest_name).
                    adif_contest_id: Some(contest_name.clone()),
                    category: "".to_string(), // Not available in ADIF
                    serials: ContestSerials {
                        sent: adif_qso.stx,
                        received: adif_qso.srx,
                    },
                    points: 1, // Default
                    multiplier: None,
                }),
            tags: Self::tags_with_unmodeled_fields(adif_qso),
            adif_field_types: adif_qso.field_types.clone(),
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

    /// `additional_fields` plus the record's standard fields that
    /// [`QsoMetadata`] does not hold as read: OPERATOR, TX_PWR, COUNTRY, DXCC,
    /// CQZ, ITUZ, NOTES, COMMENT as written, QSL_SENT/QSL_RCVD, the reports as written (a CW
    /// `599`/`5NN` is not a dB report), STX/SRX, BAND (export derives it from FREQ,
    /// which a record may lack), and MODE when a SUBMODE replaced it in
    /// `mode`. Kept in the tags, they ride through the index and come back
    /// out of `qso_to_adif` as fields, so an imported contact exports as it
    /// was read (PAN-127). Non-standard fields (QSL dates, VIA, QSLMSG,
    /// LOTW_*/EQSL_*, serial strings) are already in `additional_fields`.
    fn tags_with_unmodeled_fields(adif_qso: &AdifQso) -> HashMap<String, String> {
        let mut tags = adif_qso.additional_fields.clone();
        let number = |value: Option<u32>| value.map(|n| n.to_string());
        let mode = adif_qso.submode.as_ref().map(|_| adif_qso.mode.clone());
        let band = Some(adif_qso.band.clone());
        for (name, value) in [
            ("OPERATOR", adif_qso.operator.clone()),
            ("TX_PWR", adif_qso.tx_pwr.map(|watts| watts.to_string())),
            ("COUNTRY", adif_qso.country.clone()),
            ("DXCC", number(adif_qso.dxcc.map(u32::from))),
            ("CQZ", number(adif_qso.cqz.map(u32::from))),
            ("ITUZ", number(adif_qso.ituz.map(u32::from))),
            // Only a record with a CONTEST_ID gets a `contest_info` to hold
            // its serials; without one they would be dropped.
            ("STX", number(adif_qso.stx)),
            ("SRX", number(adif_qso.srx)),
            ("NOTES", adif_qso.notes.clone()),
            // `notes` holds it too, but export appends Pancetta's attribution
            // to `notes`; the tag writes a third-party comment back as read.
            ("COMMENT", adif_qso.comment.clone()),
            (QSL_SENT_FIELD, adif_qso.qsl_sent.clone()),
            (QSL_RCVD_FIELD, adif_qso.qsl_rcvd.clone()),
            ("RST_SENT", adif_qso.rst_sent.clone()),
            ("RST_RCVD", adif_qso.rst_rcvd.clone()),
            ("BAND", band),
            ("MODE", mode),
            ("SUBMODE", adif_qso.submode.clone()),
        ] {
            if let Some(value) = value.filter(|value| !value.is_empty()) {
                tags.insert(name.to_string(), value);
            }
        }
        tags
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
            additional_fields: self.additional_fields_of(record),
            field_types: Self::field_types_of(record),
        })
    }

    /// The type indicator of every field that carried one (PAN-214).
    fn field_types_of(record: &AdifRecord) -> HashMap<String, String> {
        record
            .fields
            .iter()
            .filter_map(|(name, field)| {
                let data_type = field.data_type.as_ref().filter(|t| !t.is_empty())?;
                Some((name.clone(), data_type.clone()))
            })
            .collect()
    }

    /// [`Self::extract_additional_fields`] plus a TIME_OFF that has no
    /// QSO_DATE_OFF: `qso_date_off` needs both, so without this the end
    /// time would be dropped (PAN-127).
    fn additional_fields_of(&self, record: &AdifRecord) -> HashMap<String, String> {
        let mut additional = self.extract_additional_fields(record);
        if self.get_field_value(record, "QSO_DATE_OFF").is_none() {
            if let Some(time_off) = self
                .get_field_value(record, "TIME_OFF")
                .filter(|time_off| !time_off.is_empty())
            {
                additional.insert("TIME_OFF".to_string(), time_off);
            }
        }
        additional
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

        // An imported field's type indicator goes back on the field (PAN-214).
        for (name, data_type) in &qso.field_types {
            if let Some(field) = fields.get_mut(name) {
                field.data_type = Some(data_type.clone());
            }
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
            // An imported field keeps its type indicator (PAN-214).
            out.push_str(&self.format_kept_field(field)?);
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

/// The first field-like tag (`NAME:…`) between `from` and `to` that a later
/// `<` abandoned before its `>`, as the byte range of its spec. Every `<` in
/// that range is one [`next_tag`] started over from, so each runs to the
/// next `<` (or to `to`). A stray `<` in free text, such as `a < b`, is not
/// field-like.
fn abandoned_field_tag(data: &[u8], from: usize, to: usize) -> Option<(usize, usize)> {
    let mut at = from;
    while let Some(i) = data.get(at..to)?.iter().position(|&b| b == b'<') {
        let start = at + i + 1;
        let end = data[start..to]
            .iter()
            .position(|&b| b == b'<')
            .map_or(to, |j| start + j);
        let spec = &data[start..end];
        if let Some(colon) = spec.iter().position(|&b| b == b':') {
            let name = &spec[..colon];
            if !name.is_empty() && name.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_') {
                return Some((start, end));
            }
        }
        at = end;
    }
    None
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

/// How [`read_value`] cut a value (PAN-214).
enum Cut {
    /// The value and the byte position just past it. `contradicts` is set
    /// when the preferred unit landed on no field boundary and the other unit
    /// did, so the other unit's cut was taken.
    Value {
        value: String,
        end: usize,
        contradicts: bool,
    },
    /// The declared length runs past the end of the input, or lands on no
    /// field boundary after swallowing a real tag.
    Mismatch,
}

/// ADIF counts a value's length in characters, but pancetta (and most C/Python
/// writers) count bytes. Cut at the length in `unit`, the unit
/// [`AdifProcessor::parse_bytes`] is trying for the whole file, when that cut
/// lands on a field boundary ([`before_tag`]); otherwise at the length in the
/// other unit when only that one does, which counts as a vote against `unit`.
/// When neither does, the value keeps the `unit` cut and ADIF's text between a
/// value and the next tag is ignored, unless the cut has swallowed a real tag
/// or the input ends before the length: then the length is a mismatch. ASCII
/// spans get the same boundary checks, so an overrun in a plain ASCII record
/// is caught too (PAN-214). Non-UTF-8 input is Latin-1: one byte, one char.
fn read_value(data: &[u8], text: Option<&str>, start: usize, len: usize, unit: LengthUnit) -> Cut {
    // None: the input ends before `len` bytes / characters.
    let byte_cut = start
        .checked_add(len)
        .filter(|&end| end <= data.len())
        .map(|end| match text {
            // A byte count can land inside a character: round up to the next boundary.
            Some(text) => (end..=text.len())
                .find(|&i| text.is_char_boundary(i))
                .unwrap_or(text.len()),
            None => end,
        });
    let char_cut = match text {
        Some(text) => {
            let rest = &text[start..];
            match rest.char_indices().nth(len) {
                Some((i, _)) => Some(start + i),
                // Exactly `len` characters left is a cut at the end.
                None => (rest.chars().count() == len).then_some(text.len()),
            }
        }
        None => byte_cut,
    };
    let (preferred, other) = match unit {
        LengthUnit::Bytes => (byte_cut, char_cut),
        LengthUnit::Characters => (char_cut, byte_cut),
    };
    let fits = |cut: Option<usize>| cut.filter(|&cut| before_tag(data, cut));
    let (end, contradicts) = match (fits(preferred), fits(other)) {
        (Some(end), _) => (end, false),
        (None, Some(end)) => (end, true),
        (None, None) => match preferred.or(other) {
            None => return Cut::Mismatch,
            Some(end) if swallows_tag(data, start, end) => return Cut::Mismatch,
            // ADIF ignores text between a value and the next '<'.
            Some(end) => (end, false),
        },
    };
    Cut::Value {
        value: decode(data, text, start, end),
        end,
        contradicts,
    }
}

/// True when a real tag (`<EOR>`, `<EOH>` or `<NAME:LEN…>`, as
/// [`before_tag`] defines one) opens inside `data[start..end]` (PAN-214).
fn swallows_tag(data: &[u8], start: usize, end: usize) -> bool {
    data[start..end]
        .iter()
        .enumerate()
        .filter(|&(_, &b)| b == b'<')
        .any(|(i, _)| {
            let open = start + i;
            matches!(next_tag(data, open), Some((at, close))
                if at == open && is_tag_spec(&data[open + 1..close]))
        })
}

/// True when a cut at `at` lands on a field boundary, the test both units of
/// the per-file length vote are held to (PAN-214): only whitespace separates
/// `at` from the end, or from a `<` that opens a real tag: one [`next_tag`]
/// does not start over (no other `<` comes before its `>`) and whose spec is
/// `<EOR>`, `<EOH>` or carries a length (`<NAME:LEN…>`), or a tag the file
/// ends inside of. A stray `<` in a value, such as the one in
/// `üñabc<x<MODE:3>`, is not a tag, and neither is a closed lookalike with no
/// length, such as the `<x>` in `€ñab<x>`.
fn before_tag(data: &[u8], at: usize) -> bool {
    let Some(i) = data[at..].iter().position(|b| !b.is_ascii_whitespace()) else {
        return true;
    };
    let open = at + i;
    if data[open] != b'<' {
        return false;
    }
    match next_tag(data, open) {
        None => true,
        Some((start, close)) => start == open && is_tag_spec(&data[open + 1..close]),
    }
}

/// A tag spec that ends a value: `EOR`, `EOH`, or `NAME:LEN[:TYPE]` with a
/// decimal length.
fn is_tag_spec(spec: &[u8]) -> bool {
    if spec.eq_ignore_ascii_case(b"EOR") || spec.eq_ignore_ascii_case(b"EOH") {
        return true;
    }
    let mut parts = spec.split(|&b| b == b':');
    let name = parts.next().unwrap_or_default();
    let length = parts.next().unwrap_or_default();
    !name.is_empty() && !length.is_empty() && length.iter().all(u8::is_ascii_digit)
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
            field_types: HashMap::new(),
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
            adif_field_types: HashMap::new(),
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

    /// PR #451 review (Codex P2): a tag with no length (`<COMMENT>secret`)
    /// must make its record unreadable, as a non-numeric length does, rather
    /// than import the contact with the value silently dropped.
    #[test]
    fn parse_bytes_rejects_a_field_tag_with_no_length() {
        let adif = "<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:4>1200 <COMMENT>secret <EOR>\n\
                    <CALL:5>K1ABC <QSO_DATE:8>20240101 <TIME_ON:4>1300 <EOR>\n";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        assert_eq!(parsed.records.len(), 2);
        match &parsed.records[0] {
            Err(AdifRecordError {
                error: AdifError::InvalidField { field },
                call,
            }) => {
                assert_eq!(field, "COMMENT");
                assert_eq!(call.as_deref(), Some("W1AW"));
            }
            other => panic!("expected InvalidField(COMMENT), got {other:?}"),
        }
        assert!(parsed.records[1].is_ok(), "{:?}", parsed.records[1]);
    }

    /// PR #451 review (Codex P2): a field tag abandoned before its `>`
    /// (`<COMMENT:5abc<EOR>`) must make its record unreadable, not be
    /// skipped so the contact imports without the value.
    #[test]
    fn parse_bytes_rejects_an_unterminated_field_tag() {
        let adif = "<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:4>1200 <COMMENT:5abc<EOR>\n\
                    <CALL:5>K1ABC <QSO_DATE:8>20240101 <TIME_ON:4>1300 <EOR>\n";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        assert_eq!(parsed.records.len(), 2, "{:?}", parsed.records);
        match &parsed.records[0] {
            Err(AdifRecordError {
                error: AdifError::InvalidField { field },
                call,
            }) => {
                assert_eq!(field, "COMMENT:5abc");
                assert_eq!(call.as_deref(), Some("W1AW"));
            }
            other => panic!("expected InvalidField(COMMENT:5abc), got {other:?}"),
        }
        assert!(parsed.records[1].is_ok(), "{:?}", parsed.records[1]);
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
    fn parse_bytes_keeps_a_closed_lookalike_tag_in_a_character_counted_value() {
        // Seven characters, ten bytes: the byte count lands before `<x>`,
        // which is part of the value. `<x>` carries no length, so it is not
        // a field tag and must not end the value (PR #451 review).
        let adif = "<CALL:4>W1AW <NOTES:7>€ñab<x><MODE:3>FT8 <EOR>\n";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        let record = only_record(&parsed);
        assert_eq!(record.fields["NOTES"].value, "€ñab<x>");
        assert_eq!(record.fields["MODE"].value, "FT8");
        assert!(!record.fields.contains_key("X"), "{:?}", record.fields);
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

    /// A record's fields by name, or its error message with its CALL.
    type Outcome = Result<std::collections::BTreeMap<String, String>, (String, Option<String>)>;

    /// The outcome of each record, in file order.
    fn outcomes(parsed: &AdifParse) -> Vec<Outcome> {
        parsed
            .records
            .iter()
            .map(|r| match r {
                Ok(record) => Ok(field_map(record)),
                Err(e) => Err((e.error.to_string(), e.call.clone())),
            })
            .collect()
    }

    fn length_mismatch(field: &str) -> String {
        AdifError::LengthMismatch {
            field: field.to_string(),
        }
        .to_string()
    }

    /// PAN-214 (PR #451 review, Codex): the ticket's record in a file whose other
    /// multi-byte values count characters. Both cuts of NOTES land before a valid
    /// tag, so NOTES decides nothing and follows the file.
    #[test]
    fn parse_bytes_keeps_tag_shaped_text_in_a_character_counted_value() {
        let adif = "<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:6>120000 <NAME:5>Müñoz \
                    <NOTES:9>€€€<X:1>z<MODE:3>FT8<EOR>\n\
                    <CALL:5>K1ABC <QSO_DATE:8>20240101 <TIME_ON:6>121500 <QTH:6>Zürich \
                    <MODE:3>FT8<EOR>\n";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        assert_eq!(parsed.length_unit, LengthUnit::Characters);
        assert_eq!(ok_calls(&parsed), ["W1AW", "K1ABC"]);
        let w1aw = parsed.records[0].as_ref().unwrap();
        assert_eq!(w1aw.fields["NAME"].value, "Müñoz");
        assert_eq!(w1aw.fields["NOTES"].value, "€€€<X:1>z");
        assert_eq!(w1aw.fields["MODE"].value, "FT8");
        assert!(!w1aw.fields.contains_key("X"), "{:?}", w1aw.fields);
        let k1abc = parsed.records[1].as_ref().unwrap();
        assert_eq!(k1abc.fields["QTH"].value, "Zürich");
    }

    /// PAN-214: the same text counted in bytes reads at its byte lengths.
    #[test]
    fn parse_bytes_keeps_tag_shaped_text_in_a_byte_counted_value() {
        let adif = "<CALL:4>W1AW <NAME:7>Müñoz <NOTES:15>€€€<X:1>z <MODE:3>FT8 <EOR>";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        assert_eq!(parsed.length_unit, LengthUnit::Bytes);
        let record = only_record(&parsed);
        assert_eq!(record.fields["NAME"].value, "Müñoz");
        assert_eq!(record.fields["NOTES"].value, "€€€<X:1>z");
        assert_eq!(record.fields["MODE"].value, "FT8");
        assert!(!record.fields.contains_key("X"), "{:?}", record.fields);
    }

    /// PAN-214: with no value that only one unit reads cleanly, the file reads
    /// as bytes, pancetta's own convention.
    #[test]
    fn parse_bytes_reads_a_file_with_no_deciding_value_as_bytes() {
        let adif = "<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:6>120000 \
                    <NOTES:9>€€€<X:1>z<MODE:3>FT8<EOR>\n";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        assert_eq!(parsed.length_unit, LengthUnit::Bytes);
        let record = only_record(&parsed);
        assert_eq!(record.fields["NOTES"].value, "€€€");
        assert_eq!(record.fields["X"].value, "z");
        assert_eq!(record.fields["MODE"].value, "FT8");
    }

    /// PAN-214: in a character-counted file, a value that only its byte count
    /// reads cleanly is still read at its byte count.
    #[test]
    fn parse_bytes_reads_the_odd_value_out_in_its_own_unit() {
        let adif = "<CALL:4>W1AW <NAME:5>Müñoz <NOTES:3>€€€ <EOR>\n\
                    <CALL:5>K1ABC <QTH:7>Zürich<MODE:3>FT8<EOR>\n";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        assert_eq!(parsed.length_unit, LengthUnit::Characters);
        assert_eq!(ok_calls(&parsed), ["W1AW", "K1ABC"]);
        let w1aw = parsed.records[0].as_ref().unwrap();
        assert_eq!(w1aw.fields["NAME"].value, "Müñoz");
        assert_eq!(w1aw.fields["NOTES"].value, "€€€");
        let k1abc = parsed.records[1].as_ref().unwrap();
        assert_eq!(k1abc.fields["QTH"].value, "Zürich");
        assert_eq!(k1abc.fields["MODE"].value, "FT8");
    }

    /// The W1AW record of the overrun tests, whose COMMENT declares `length`,
    /// followed by a K1ABC record.
    fn overrun_file(length: usize) -> String {
        format!(
            "<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:6>120000 <MODE:3>FT8 \
             <COMMENT:{length}>x <EOR>\n\
             <CALL:5>K1ABC <QSO_DATE:8>20240101 <TIME_ON:6>121500 <MODE:3>FT8<EOR>\n"
        )
    }

    /// PAN-214 (PR #451 review, Codex): a length that runs past the end of the
    /// file makes only its own record unreadable; the next record still reads.
    #[test]
    fn parse_bytes_rejects_a_length_that_runs_past_the_end_of_the_file() {
        let parsed = AdifProcessor::new().parse_bytes(overrun_file(999).as_bytes());

        let outcomes = outcomes(&parsed);
        assert_eq!(outcomes.len(), 2, "{outcomes:?}");
        assert_eq!(
            outcomes[0],
            Err((length_mismatch("COMMENT:999"), Some("W1AW".to_string())))
        );
        let k1abc = outcomes[1].as_ref().expect("K1ABC reads");
        assert_eq!(k1abc["CALL"], "K1ABC");
        assert_eq!(k1abc["TIME_ON"], "121500");
    }

    /// PAN-214: a shorter overrun that ends inside the next record used to
    /// give W1AW the next record's TIME_ON and MODE.
    #[test]
    fn parse_bytes_rejects_a_length_that_runs_into_the_next_record() {
        let parsed = AdifProcessor::new().parse_bytes(overrun_file(30).as_bytes());

        let outcomes = outcomes(&parsed);
        assert_eq!(outcomes.len(), 2, "{outcomes:?}");
        assert_eq!(
            outcomes[0],
            Err((length_mismatch("COMMENT:30"), Some("W1AW".to_string())))
        );
        let k1abc = outcomes[1].as_ref().expect("K1ABC reads");
        assert_eq!(k1abc["CALL"], "K1ABC");
        assert_eq!(k1abc["TIME_ON"], "121500");
    }

    #[test]
    fn parse_bytes_rejects_a_length_that_swallows_the_next_field() {
        let adif = "<CALL:4>W1AW <COMMENT:10>x <MODE:3>FT8 <EOR>";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        assert_eq!(
            outcomes(&parsed),
            [Err((
                length_mismatch("COMMENT:10"),
                Some("W1AW".to_string())
            ))]
        );
    }

    #[test]
    fn parse_bytes_rejects_a_truncated_last_value() {
        let adif = "<CALL:4>W1AW <NAME:7>Mü";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        assert_eq!(
            outcomes(&parsed),
            [Err((length_mismatch("NAME:7"), Some("W1AW".to_string())))]
        );
    }

    /// ADIF allows any text but `<` between a value and the next tag.
    #[test]
    fn parse_bytes_still_ignores_text_after_a_value() {
        let adif = "<CALL:4>W1AWxyz <MODE:3>FT8 <EOR>";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        let record = only_record(&parsed);
        assert_eq!(record.fields["CALL"].value, "W1AW");
        assert_eq!(record.fields["MODE"].value, "FT8");
    }

    /// A value whose length fits keeps tag-shaped text, as pancetta writes it.
    #[test]
    fn parse_bytes_keeps_an_eor_inside_a_value_whose_length_fits() {
        let adif = "<CALL:4>W1AW <NOTES:5><EOR> <EOR>\n<CALL:5>K1ABC <EOR>";

        let parsed = AdifProcessor::new().parse_bytes(adif.as_bytes());

        assert_eq!(ok_calls(&parsed), ["W1AW", "K1ABC"]);
        assert_eq!(
            parsed.records[0].as_ref().unwrap().fields["NOTES"].value,
            "<EOR>"
        );
    }

    /// The startup replay reads through `parse_string`, which still fails the
    /// whole file on the first unreadable record.
    #[test]
    fn parse_string_fails_on_a_length_mismatch() {
        match AdifProcessor::new().parse_string(&overrun_file(999)) {
            Err(AdifError::LengthMismatch { field }) => assert_eq!(field, "COMMENT:999"),
            other => panic!("expected LengthMismatch(COMMENT:999), got {other:?}"),
        }
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

    // ── PAN-214: type indicators survive the index → export path ────────────

    const TYPED_RECORD: &[u8] = b"<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:6>120000 \
                                  <MODE:3>FT8 <APP_LOGGER_SCORE:2:N>42 <EOR>";

    fn typed_record(processor: &AdifProcessor) -> AdifRecord {
        let parsed = processor.parse_bytes(TYPED_RECORD);
        only_record(&parsed).clone()
    }

    #[test]
    fn record_to_qso_keeps_field_type_indicators() {
        let processor = AdifProcessor::new();

        let qso = processor.record_to_qso(&typed_record(&processor)).unwrap();

        let expected = HashMap::from([("APP_LOGGER_SCORE".to_string(), "N".to_string())]);
        assert_eq!(qso.field_types, expected);
        assert_eq!(processor.adif_to_qso(&qso).adif_field_types, expected);
    }

    #[test]
    fn qso_to_record_writes_type_indicators_back() {
        let processor = AdifProcessor::new();
        let qso = processor.record_to_qso(&typed_record(&processor)).unwrap();
        let metadata = processor.adif_to_qso(&qso);

        let record = processor.qso_to_record(&processor.qso_to_adif(&metadata, None));

        let field = &record.fields["APP_LOGGER_SCORE"];
        assert_eq!(field.value, "42");
        assert_eq!(field.data_type.as_deref(), Some("N"));
        assert_eq!(record.fields["CALL"].data_type, None);
    }

    #[test]
    fn generate_string_renders_a_type_indicator() {
        let processor = AdifProcessor::new();
        let file = AdifFile {
            header: AdifHeader::default(),
            records: vec![typed_record(&processor)],
        };

        let output = processor.generate_string(&file).unwrap();

        assert!(output.contains("<APP_LOGGER_SCORE:2:N>42 "), "{output}");
    }

    #[test]
    fn generate_record_renders_a_type_indicator() {
        let processor = AdifProcessor::new();
        let qso = processor.record_to_qso(&typed_record(&processor)).unwrap();

        let output = processor.generate_record(&qso).unwrap();

        assert!(output.contains("<APP_LOGGER_SCORE:2:N>42 "), "{output}");
    }

    #[test]
    fn generate_string_is_unchanged_for_untyped_records() {
        let processor = AdifProcessor::new();
        let record = |call: &str| AdifRecord {
            fields: HashMap::from([(
                "CALL".to_string(),
                AdifField {
                    name: "CALL".to_string(),
                    value: call.to_string(),
                    length: call.len(),
                    data_type: None,
                },
            )]),
        };
        let file = AdifFile {
            header: AdifHeader {
                version: "3.1.0".to_string(),
                program_id: "TestProg".to_string(),
                program_version: "1.2".to_string(),
                created_timestamp: Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap(),
                fields: HashMap::new(),
            },
            records: vec![record("W1AW"), record("K1ABC")],
        };

        let output = processor.generate_string(&file).unwrap();

        assert_eq!(
            output,
            "ADIF Export for TestProg\n\
             Generated on 2024-01-01 12:00:00 UTC\n\n\
             <ADIF_VER:5>3.1.0 <PROGRAMID:8>TestProg <PROGRAMVERSION:3>1.2 \
             <CREATED_TIMESTAMP:15>20240101 120000 <EOH>\n\n\
             <CALL:4>W1AW <EOR>\n\n\
             <CALL:5>K1ABC <EOR>\n\n"
        );
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

            /// A record written with character counts reads back exactly,
            /// whenever its values hold no '<' and do not end in whitespace
            /// (then a byte cut either equals the character cut or lands
            /// mid-value on text that is not a tag) (PAN-214).
            #[test]
            fn character_counted_records_read_back(
                fields in proptest::collection::btree_map(
                    "[A-Z_]{1,12}",
                    "[a-zA-Z0-9üñé€:>\n ]{0,23}[a-zA-Z0-9üñé€]",
                    1..8,
                )
            ) {
                let mut rendered = String::new();
                for (name, value) in &fields {
                    rendered.push_str(&format!("<{name}:{}>{value} ", value.chars().count()));
                }
                rendered.push_str("<EOR>");

                let reread = AdifProcessor::new().parse_string(&rendered).unwrap();

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
