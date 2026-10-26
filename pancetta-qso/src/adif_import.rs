//! Planning an ADIF import (PAN-127): which incoming records are new
//! contacts, which the logbook already has, and which cannot be read.
//!
//! [`plan_import`] is pure: it reads two parsed files and decides. Writing the
//! planned records is the caller's job
//! ([`crate::adif_log_writer::write_import_block`] for `qsos.adi`,
//! [`crate::QsoDatabase::insert_adif_records`] for the index).
//!
//! An [`ImportReceipt`] records the block one import appended, so
//! `pancetta import --undo` can remove exactly that block (PAN-214).

use crate::adif::{freq_hz_to_band, AdifParse, AdifProcessor, AdifQso, AdifRecord};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Two records are the same contact when callsign, band and mode match and
/// their start times are at most this far apart: the tolerance LoTW uses to
/// match one QSO across two independent logs. Generous on purpose: pancetta's
/// TIME_ON is the CQ start, and other loggers may write HHMM.
pub const DUPLICATE_WINDOW_MINUTES: i64 = 30;

#[derive(Debug, Default)]
pub struct ImportPlan {
    /// Records found in the import file, readable or not.
    pub records_in_file: usize,
    /// New contacts, in file order, exactly as read.
    pub to_add: Vec<AdifRecord>,
    pub duplicates_in_logbook: usize,
    pub duplicates_in_file: usize,
    pub unreadable: Vec<UnreadableRecord>,
    /// Readable contacts already in the logbook.
    pub logbook_contacts: usize,
    /// Logbook records that could not be read, so were not used to find duplicates.
    pub logbook_unreadable: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnreadableRecord {
    /// 1-based position of the record in the import file.
    pub record_number: usize,
    pub call: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Logbook,
    File,
}

/// A contact already known, bucketed under its (callsign, mode).
struct Seen {
    band: Option<String>,
    start: DateTime<Utc>,
    source: Source,
}

impl Seen {
    fn matches(&self, band: &Option<String>, start: DateTime<Utc>) -> bool {
        let same_band = match (&self.band, band) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        same_band && (self.start - start).num_seconds().abs() <= DUPLICATE_WINDOW_MINUTES * 60
    }
}

/// Decide what importing `incoming` into `logbook` (None: no logbook yet)
/// would do. Every incoming record ends up in exactly one of `to_add`,
/// `duplicates_in_logbook`, `duplicates_in_file` or `unreadable`.
pub fn plan_import(
    processor: &AdifProcessor,
    incoming: &AdifParse,
    logbook: Option<&AdifParse>,
) -> ImportPlan {
    let mut plan = ImportPlan {
        records_in_file: incoming.records.len(),
        ..ImportPlan::default()
    };
    let mut seen: HashMap<(String, String), Vec<Seen>> = HashMap::new();

    for record in logbook.map_or(&[][..], |l| &l.records[..]) {
        let qso = record
            .as_ref()
            .ok()
            .and_then(|r| processor.record_to_qso(r).ok())
            .filter(|q| !q.call.trim().is_empty());
        match qso {
            Some(qso) => {
                plan.logbook_contacts += 1;
                seen.entry(key_of(&qso)).or_default().push(Seen {
                    band: band_of(&qso),
                    start: qso.qso_date,
                    source: Source::Logbook,
                });
            }
            None => plan.logbook_unreadable += 1,
        }
    }

    for (i, record) in incoming.records.iter().enumerate() {
        let unreadable = |call: Option<String>, reason: String| UnreadableRecord {
            record_number: i + 1,
            call,
            reason,
        };
        let record = match record {
            Ok(record) => record,
            Err(e) => {
                plan.unreadable
                    .push(unreadable(e.call.clone(), e.error.to_string()));
                continue;
            }
        };
        let qso = match processor.record_to_qso(record) {
            Ok(qso) => qso,
            Err(e) => {
                let call = record
                    .fields
                    .get("CALL")
                    .map(|f| f.value.trim().to_string())
                    .filter(|call| !call.is_empty());
                plan.unreadable.push(unreadable(call, e.to_string()));
                continue;
            }
        };
        let call = qso.call.trim();
        if call.is_empty() {
            plan.unreadable
                .push(unreadable(None, "CALL is empty".to_string()));
            continue;
        }
        if !round_trips(processor, record) {
            plan.unreadable.push(unreadable(
                Some(call.to_string()),
                "could not be written to the logbook unchanged".to_string(),
            ));
            continue;
        }

        let band = band_of(&qso);
        let known = seen.entry(key_of(&qso)).or_default();
        let matching = |source| {
            known
                .iter()
                .any(|s| s.source == source && s.matches(&band, qso.qso_date))
        };
        if matching(Source::Logbook) {
            plan.duplicates_in_logbook += 1;
        } else if matching(Source::File) {
            plan.duplicates_in_file += 1;
        } else {
            known.push(Seen {
                band,
                start: qso.qso_date,
                source: Source::File,
            });
            plan.to_add.push(record.clone());
        }
    }

    plan
}

/// Callsign (case-insensitive, suffix-sensitive) and mode.
fn key_of(q: &AdifQso) -> (String, String) {
    (q.call.trim().to_uppercase(), mode_of(q))
}

/// SUBMODE when present (MODE=MFSK SUBMODE=FT4), else MODE; trimmed, upper-case.
fn mode_of(q: &AdifQso) -> String {
    q.submode
        .as_deref()
        .map(str::trim)
        .filter(|submode| !submode.is_empty())
        .unwrap_or(q.mode.trim())
        .to_uppercase()
}

/// BAND (upper-case), else the band FREQ falls in; None when unknown, which
/// matches any band. "0MHZ"-style values (pancetta's fallback for a frequency
/// outside every band, e.g. logged without CAT) count as unknown.
fn band_of(q: &AdifQso) -> Option<String> {
    let band = q.band.trim().to_uppercase();
    let band = if band.is_empty() && q.freq > 0.0 {
        freq_hz_to_band(q.freq * 1_000_000.0)
    } else {
        band
    };
    (!band.is_empty() && !band.ends_with("MHZ")).then_some(band)
}

/// Defensive: a record pancetta's own reader cannot read back unchanged must
/// never reach qsos.adi, because one such record aborts the startup index
/// rebuild part-way.
fn round_trips(processor: &AdifProcessor, record: &AdifRecord) -> bool {
    let Ok(rendered) = processor.render_record(record) else {
        return false;
    };
    let Ok(file) = processor.parse_string(&rendered) else {
        return false;
    };
    let [reread] = file.records.as_slice() else {
        return false;
    };
    processor.record_to_qso(reread).is_ok() && values(reread) == values(record)
}

/// The record's name→value map without empty values (`format_field` drops those).
fn values(record: &AdifRecord) -> HashMap<&str, &str> {
    record
        .fields
        .iter()
        .filter(|(_, f)| !f.value.is_empty())
        .map(|(name, f)| (name.as_str(), f.value.as_str()))
        .collect()
}

/// The receipt format [`ImportReceipt::from_json`] reads (PAN-214).
pub const RECEIPT_VERSION: u32 = 1;

/// What one `pancetta import` appended, saved before the append so
/// `--undo` can remove exactly those bytes (PAN-214).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportReceipt {
    /// [`RECEIPT_VERSION`] when written.
    pub version: u32,
    pub logbook: PathBuf,
    pub import_file: PathBuf,
    pub imported_at: DateTime<Utc>,
    pub contacts: usize,
    /// The copy of the logbook taken before the import; `None` for a new logbook.
    pub backup: Option<PathBuf>,
    /// Byte offset in `logbook` where the first imported record starts.
    pub offset: u64,
    /// The records exactly as appended.
    pub added: String,
    /// When `--undo` removed the block; `None` while it is still in the logbook.
    #[serde(default)]
    pub undone_at: Option<DateTime<Utc>>,
}

/// Why bytes are not a readable [`ImportReceipt`] (PAN-214).
#[derive(Debug, thiserror::Error)]
pub enum ReceiptError {
    #[error("not a pancetta import receipt: {0}")]
    NotAReceipt(String),
    #[error("unsupported import receipt version {0}")]
    UnsupportedVersion(u32),
}

impl ImportReceipt {
    /// Read a receipt. The version is checked before the other fields, so a
    /// receipt from a newer pancetta reports its version, not a missing field.
    pub fn from_json(bytes: &[u8]) -> Result<Self, ReceiptError> {
        #[derive(Deserialize)]
        struct Versioned {
            version: u32,
        }
        let not_a_receipt = |e: serde_json::Error| ReceiptError::NotAReceipt(e.to_string());
        let Versioned { version } = serde_json::from_slice(bytes).map_err(not_a_receipt)?;
        if version != RECEIPT_VERSION {
            return Err(ReceiptError::UnsupportedVersion(version));
        }
        serde_json::from_slice(bytes).map_err(not_a_receipt)
    }

    /// The receipt as pretty-printed JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("an import receipt always serializes to JSON")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One ADIF record from (name, value) pairs, with byte-counted lengths.
    fn record(fields: &[(&str, &str)]) -> String {
        let mut out: String = fields
            .iter()
            .map(|(name, value)| format!("<{}:{}>{} ", name, value.len(), value))
            .collect();
        out.push_str("<EOR>\n");
        out
    }

    /// A FT8 contact on 20M, pancetta style.
    fn contact(call: &str, time_on: &str) -> String {
        record(&[
            ("CALL", call),
            ("QSO_DATE", "20240101"),
            ("TIME_ON", time_on),
            ("MODE", "FT8"),
            ("BAND", "20M"),
        ])
    }

    fn parse(adif: &str) -> AdifParse {
        AdifProcessor::new().parse_bytes(adif.as_bytes())
    }

    fn plan(incoming: &str, logbook: Option<&str>) -> ImportPlan {
        let logbook = logbook.map(parse);
        plan_import(&AdifProcessor::new(), &parse(incoming), logbook.as_ref())
    }

    fn calls(records: &[AdifRecord]) -> Vec<&str> {
        records
            .iter()
            .map(|r| r.fields["CALL"].value.as_str())
            .collect()
    }

    #[test]
    fn new_records_are_planned_in_file_order() {
        let incoming = [
            contact("W1AW", "120000"),
            contact("K1ABC", "121500"),
            contact("N0XYZ", "123000"),
        ]
        .concat();

        let plan = plan(&incoming, None);

        assert_eq!(plan.records_in_file, 3);
        assert_eq!(calls(&plan.to_add), ["W1AW", "K1ABC", "N0XYZ"]);
        assert_eq!(plan.duplicates_in_logbook, 0);
        assert_eq!(plan.duplicates_in_file, 0);
        assert!(plan.unreadable.is_empty());
        assert_eq!(plan.logbook_contacts, 0);
        assert_eq!(plan.logbook_unreadable, 0);
    }

    #[test]
    fn a_contact_already_in_the_logbook_is_a_duplicate() {
        let logbook = "<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:6>120000 \
                       <MODE:3>FT8 <BAND:3>20M <EOR>\n";
        let incoming = "<call:4>w1aw <qso_date:8>20240101 <time_on:4>1201 \
                        <mode:3>FT8 <band:3>20m <eor>\n";

        let plan = plan(incoming, Some(logbook));

        assert_eq!(plan.duplicates_in_logbook, 1);
        assert_eq!(plan.duplicates_in_file, 0);
        assert!(plan.to_add.is_empty());
        assert_eq!(plan.logbook_contacts, 1);
    }

    #[test]
    fn duplicate_window_is_thirty_minutes_inclusive() {
        let logbook = contact("W1AW", "120000");

        let after_30 = plan(&contact("W1AW", "123000"), Some(&logbook));
        assert_eq!(after_30.duplicates_in_logbook, 1);
        assert!(after_30.to_add.is_empty());

        let before_30 = plan(&contact("W1AW", "113000"), Some(&logbook));
        assert_eq!(before_30.duplicates_in_logbook, 1);

        let after_31 = plan(&contact("W1AW", "123100"), Some(&logbook));
        assert_eq!(after_31.duplicates_in_logbook, 0);
        assert_eq!(calls(&after_31.to_add), ["W1AW"]);
    }

    #[test]
    fn a_different_band_or_mode_is_a_new_contact() {
        let logbook = contact("W1AW", "120000");

        let other_band = record(&[
            ("CALL", "W1AW"),
            ("QSO_DATE", "20240101"),
            ("TIME_ON", "120000"),
            ("MODE", "FT8"),
            ("BAND", "40M"),
        ]);
        let plan_band = plan(&other_band, Some(&logbook));
        assert_eq!(plan_band.duplicates_in_logbook, 0);
        assert_eq!(calls(&plan_band.to_add), ["W1AW"]);

        let other_mode = record(&[
            ("CALL", "W1AW"),
            ("QSO_DATE", "20240101"),
            ("TIME_ON", "120000"),
            ("MODE", "FT4"),
            ("BAND", "20M"),
        ]);
        let plan_mode = plan(&other_mode, Some(&logbook));
        assert_eq!(plan_mode.duplicates_in_logbook, 0);
        assert_eq!(calls(&plan_mode.to_add), ["W1AW"]);
    }

    #[test]
    fn submode_is_the_mode_for_matching() {
        let logbook = record(&[
            ("CALL", "W1AW"),
            ("QSO_DATE", "20240101"),
            ("TIME_ON", "120000"),
            ("MODE", "FT4"),
            ("BAND", "20M"),
        ]);
        let incoming = record(&[
            ("CALL", "W1AW"),
            ("QSO_DATE", "20240101"),
            ("TIME_ON", "120500"),
            ("MODE", "MFSK"),
            ("SUBMODE", "FT4"),
            ("BAND", "20M"),
        ]);

        let plan = plan(&incoming, Some(&logbook));

        assert_eq!(plan.duplicates_in_logbook, 1);
        assert!(plan.to_add.is_empty());
    }

    #[test]
    fn an_unknown_band_matches_any_band() {
        // Pancetta without CAT logs the audio offset as FREQ, so BAND=0MHZ.
        let logbook = record(&[
            ("CALL", "W1AW"),
            ("QSO_DATE", "20240101"),
            ("TIME_ON", "120000"),
            ("MODE", "FT8"),
            ("FREQ", "0.001500"),
            ("BAND", "0MHZ"),
        ]);

        let plan = plan(&contact("W1AW", "120100"), Some(&logbook));

        assert_eq!(plan.duplicates_in_logbook, 1);
        assert!(plan.to_add.is_empty());
    }

    #[test]
    fn band_falls_back_to_freq() {
        let logbook = contact("W1AW", "120000");
        let on_freq = |freq: &str| {
            record(&[
                ("CALL", "W1AW"),
                ("QSO_DATE", "20240101"),
                ("TIME_ON", "120000"),
                ("MODE", "FT8"),
                ("FREQ", freq),
            ])
        };

        let on_20m = plan(&on_freq("14.074"), Some(&logbook));
        assert_eq!(on_20m.duplicates_in_logbook, 1);
        assert!(on_20m.to_add.is_empty());

        let on_40m = plan(&on_freq("7.074"), Some(&logbook));
        assert_eq!(on_40m.duplicates_in_logbook, 0);
        assert_eq!(calls(&on_40m.to_add), ["W1AW"]);
    }

    #[test]
    fn a_contact_repeated_in_the_file_is_counted_once() {
        let incoming = [contact("W1AW", "120000"), contact("W1AW", "120000")].concat();

        let plan = plan(&incoming, None);

        assert_eq!(plan.duplicates_in_file, 1);
        assert_eq!(plan.duplicates_in_logbook, 0);
        assert_eq!(calls(&plan.to_add), ["W1AW"]);
    }

    #[test]
    fn a_logbook_match_wins_over_a_file_match() {
        let logbook = contact("W1AW", "120000");
        // 12:40 is outside the logbook's window, so it is new; 12:20 then
        // matches both the logbook (12:00) and that file entry (12:40).
        let incoming = [contact("W1AW", "124000"), contact("W1AW", "122000")].concat();

        let plan = plan(&incoming, Some(&logbook));

        assert_eq!(plan.duplicates_in_logbook, 1);
        assert_eq!(plan.duplicates_in_file, 0);
        assert_eq!(plan.to_add.len(), 1);
        assert_eq!(plan.to_add[0].fields["TIME_ON"].value, "124000");
    }

    #[test]
    fn callsign_match_ignores_case_but_not_suffix() {
        let logbook = contact("W1AW", "120000");

        let lower = plan(&contact("w1aw", "120000"), Some(&logbook));
        assert_eq!(lower.duplicates_in_logbook, 1);
        assert!(lower.to_add.is_empty());

        let portable = plan(&contact("W1AW/P", "120000"), Some(&logbook));
        assert_eq!(portable.duplicates_in_logbook, 0);
        assert_eq!(calls(&portable.to_add), ["W1AW/P"]);
    }

    #[test]
    fn unreadable_records_are_reported_by_position() {
        let incoming = [
            contact("W1AW", "120000"),
            record(&[("CALL", "K1ABC"), ("TIME_ON", "120100"), ("MODE", "FT8")]),
            "<CALL:x>K2DEF <QSO_DATE:8>20240101 <TIME_ON:6>120200 <EOR>\n".to_string(),
            "<CALL:0> <QSO_DATE:8>20240101 <TIME_ON:6>120300 <EOR>\n".to_string(),
            contact("N0XYZ", "120400"),
        ]
        .concat();

        let plan = plan(&incoming, None);

        assert_eq!(plan.records_in_file, 5);
        assert_eq!(
            plan.unreadable,
            vec![
                UnreadableRecord {
                    record_number: 2,
                    call: Some("K1ABC".to_string()),
                    reason: "Missing required field: QSO_DATE".to_string(),
                },
                UnreadableRecord {
                    record_number: 3,
                    call: None,
                    reason: "Invalid field format: CALL:x".to_string(),
                },
                UnreadableRecord {
                    record_number: 4,
                    call: None,
                    reason: "CALL is empty".to_string(),
                },
            ]
        );
        assert_eq!(calls(&plan.to_add), ["W1AW", "N0XYZ"]);
    }

    #[test]
    fn unreadable_logbook_records_are_counted_not_matched() {
        let logbook = [
            contact("K1ABC", "110000"),
            // Unreadable: a bad length spec, though its CALL parses.
            "<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:6>120000 \
             <MODE:3>FT8 <BAND:x>20M <EOR>\n"
                .to_string(),
            contact("N0XYZ", "130000"),
        ]
        .concat();

        let plan = plan(&contact("W1AW", "120000"), Some(&logbook));

        assert_eq!(plan.logbook_unreadable, 1);
        assert_eq!(plan.logbook_contacts, 2);
        assert_eq!(plan.duplicates_in_logbook, 0);
        assert_eq!(calls(&plan.to_add), ["W1AW"]);
    }

    #[test]
    fn planned_records_keep_every_field() {
        let incoming = record(&[
            ("CALL", "W1AW"),
            ("QSO_DATE", "20240101"),
            ("TIME_ON", "1200"),
            ("MODE", "FT8"),
            ("BAND", "20M"),
            ("QSL_RCVD", "Y"),
            ("APP_LOTW_QSL_RCVD", "Y"),
            ("NAME", "Müñoz"),
        ]);

        let plan = plan(&incoming, None);

        assert_eq!(plan.to_add.len(), 1);
        let fields = &plan.to_add[0].fields;
        assert_eq!(fields["QSL_RCVD"].value, "Y");
        assert_eq!(fields["APP_LOTW_QSL_RCVD"].value, "Y");
        assert_eq!(fields["NAME"].value, "Müñoz");
        assert_eq!(fields["TIME_ON"].value, "1200");
    }

    /// PAN-214: a length that runs past the end of the file makes only its
    /// own record unreadable; the record after it is still added.
    #[test]
    fn a_record_whose_length_overruns_is_unreadable_and_the_next_is_added() {
        let incoming = "<CALL:4>W1AW <QSO_DATE:8>20240101 <TIME_ON:6>120000 \
                        <MODE:3>FT8 <COMMENT:999>x <EOR>\n\
                        <CALL:5>K1ABC <QSO_DATE:8>20240101 <TIME_ON:6>121500 \
                        <MODE:3>FT8<EOR>\n";

        let plan = plan(incoming, None);

        assert_eq!(plan.records_in_file, 2);
        assert_eq!(
            plan.unreadable,
            vec![UnreadableRecord {
                record_number: 1,
                call: Some("W1AW".to_string()),
                reason: "Field length does not match its value: COMMENT:999".to_string(),
            }]
        );
        assert_eq!(calls(&plan.to_add), ["K1ABC"]);
    }

    fn receipt() -> ImportReceipt {
        ImportReceipt {
            version: RECEIPT_VERSION,
            logbook: PathBuf::from("/home/op/.pancetta/qsos.adi"),
            import_file: PathBuf::from("/home/op/lotw.adi"),
            imported_at: "2026-10-26T10:15:00Z".parse().unwrap(),
            contacts: 2,
            backup: Some(PathBuf::from("/home/op/.pancetta/qsos.adi.bak")),
            offset: 1234,
            added: [contact("N0AAA", "120000"), contact("N0BBB", "121500")].concat(),
            undone_at: None,
        }
    }

    #[test]
    fn a_receipt_round_trips_through_json() {
        let receipt = receipt();

        let json = receipt.to_json();

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["undone_at"], serde_json::Value::Null);
        assert!(json.contains('\n'), "receipts are pretty-printed: {json}");
        assert_eq!(ImportReceipt::from_json(json.as_bytes()).unwrap(), receipt);

        let undone = ImportReceipt {
            undone_at: Some("2026-10-26T10:20:00Z".parse().unwrap()),
            ..receipt.clone()
        };
        assert_eq!(
            ImportReceipt::from_json(undone.to_json().as_bytes()).unwrap(),
            undone
        );

        let mut without_undone = value;
        without_undone.as_object_mut().unwrap().remove("undone_at");
        let read = ImportReceipt::from_json(without_undone.to_string().as_bytes()).unwrap();
        assert_eq!(read, receipt);
    }

    #[test]
    fn a_receipt_of_another_version_is_rejected() {
        let mut value: serde_json::Value = serde_json::from_str(&receipt().to_json()).unwrap();
        value["version"] = 2.into();

        let err = ImportReceipt::from_json(value.to_string().as_bytes()).unwrap_err();

        assert!(
            matches!(err, ReceiptError::UnsupportedVersion(2)),
            "got {err:?}"
        );
        assert_eq!(err.to_string(), "unsupported import receipt version 2");

        let err = ImportReceipt::from_json(b"{\"hello\": 1}").unwrap_err();
        assert!(matches!(err, ReceiptError::NotAReceipt(_)), "got {err:?}");
        let err = ImportReceipt::from_json(b"not json").unwrap_err();
        assert!(matches!(err, ReceiptError::NotAReceipt(_)), "got {err:?}");
    }
}
