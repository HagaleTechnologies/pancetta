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

/// ADIF parser and generator
#[derive(Clone)]
pub struct AdifProcessor {
    /// Known field definitions
    field_definitions: HashMap<String, AdifFieldDef>,
}

impl AdifProcessor {
    /// Create a new ADIF processor
    pub fn new() -> Self {
        let mut processor = Self {
            field_definitions: HashMap::new(),
        };

        processor.initialize_standard_fields();
        processor
    }

    /// Parse ADIF data from a string
    pub fn parse_string(&self, data: &str) -> Result<AdifFile, AdifError> {
        let lines = data.lines().enumerate();
        let mut header_fields = HashMap::new();
        let mut records = Vec::new();
        let mut in_header = true;

        // Parse header
        let mut header_buffer = String::new();
        let mut record_buffer = String::new();

        for (line_num, line) in lines {
            let line = line.trim();

            if line.is_empty() {
                continue;
            }

            if in_header {
                header_buffer.push_str(line);
                header_buffer.push(' ');

                if line.to_uppercase().contains("<EOH>") {
                    in_header = false;
                    header_fields = self.parse_header_fields(&header_buffer, line_num + 1)?;
                    header_buffer.clear();
                }
            } else {
                record_buffer.push_str(line);
                record_buffer.push(' ');

                if line.to_uppercase().contains("<EOR>") {
                    let record = self.parse_record(&record_buffer, line_num + 1)?;
                    records.push(record);
                    record_buffer.clear();
                }
            }
        }

        // Handle any remaining record data
        if !record_buffer.trim().is_empty() {
            let record = self.parse_record(&record_buffer, data.lines().count())?;
            records.push(record);
        }

        let header = AdifHeader {
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
        };

        Ok(AdifFile { header, records })
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
            tx_pwr: None, // Not tracked in QSO metadata
            station_callsign: metadata.our_callsign.clone(),
            operator: None, // Could be added to metadata
            my_gridsquare: metadata.grids.ours.clone(),
            gridsquare: metadata.grids.theirs.clone(),
            country: None, // Would need callsign lookup
            dxcc: None,    // Would need callsign lookup
            cqz: None,     // Would need callsign lookup
            ituz: None,    // Would need callsign lookup
            state: None,   // Would need callsign lookup
            contest_id: contest_info.map(|c| c.contest_name.clone()),
            stx: contest_info.and_then(|c| c.serials.sent),
            stx_string: None,
            srx: contest_info.and_then(|c| c.serials.received),
            srx_string: None,
            qsl_sent: Some("N".to_string()),
            qsl_rcvd: Some("N".to_string()),
            qslmsg: None,
            comment: metadata.notes.clone(),
            notes: None,
            additional_fields: metadata.tags.clone(),
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
            call_count: 0,
            first_call_at: None,
            last_call_at: None,
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

    fn parse_header_fields(
        &self,
        header: &str,
        line_num: usize,
    ) -> Result<HashMap<String, AdifField>, AdifError> {
        self.parse_fields(header, line_num)
    }

    fn parse_record(&self, record: &str, line_num: usize) -> Result<AdifRecord, AdifError> {
        let fields = self.parse_fields(record, line_num)?;
        Ok(AdifRecord { fields })
    }

    fn parse_fields(
        &self,
        data: &str,
        line_num: usize,
    ) -> Result<HashMap<String, AdifField>, AdifError> {
        let mut fields = HashMap::new();
        let mut chars = data.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '<' {
                // Parse field
                let mut field_spec = String::new();

                for ch in chars.by_ref() {
                    if ch == '>' {
                        break;
                    }
                    field_spec.push(ch);
                }

                if field_spec.is_empty() {
                    continue;
                }

                // Skip end-of-header and end-of-record markers
                if field_spec.to_uppercase() == "EOH" || field_spec.to_uppercase() == "EOR" {
                    continue;
                }

                let field = self.parse_field_spec(&field_spec, &mut chars, line_num)?;
                fields.insert(field.name.to_uppercase(), field);
            }
        }

        Ok(fields)
    }

    fn parse_field_spec(
        &self,
        field_spec: &str,
        chars: &mut std::iter::Peekable<std::str::Chars>,
        _line_num: usize,
    ) -> Result<AdifField, AdifError> {
        let parts: Vec<&str> = field_spec.split(':').collect();

        if parts.is_empty() {
            return Err(AdifError::InvalidField {
                field: field_spec.to_string(),
            });
        }

        let name = parts[0].to_uppercase();
        let length: usize = if parts.len() > 1 {
            parts[1].parse().map_err(|_| AdifError::InvalidField {
                field: field_spec.to_string(),
            })?
        } else {
            0
        };

        let data_type = if parts.len() > 2 {
            Some(parts[2].to_string())
        } else {
            None
        };

        // Read field value
        let mut value = String::new();
        for _ in 0..length {
            if let Some(ch) = chars.next() {
                value.push(ch);
            } else {
                break;
            }
        }

        Ok(AdifField {
            name,
            value,
            length,
            data_type,
        })
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
}
