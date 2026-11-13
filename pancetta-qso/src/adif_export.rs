//! ADIF export planning. Filters combine with AND: date is inclusive UTC,
//! band uses BAND then FREQ, mode matches MODE or SUBMODE, and callsign is a
//! substring. Text comparisons ignore case. Unknown bands never match.

use crate::adif::{AdifParse, AdifProcessor, AdifQso, AdifRecord};
use crate::adif_import::{band_of, UnreadableRecord};
use chrono::NaiveDate;
use pancetta_core::Band;

#[derive(Debug, Default)]
pub struct ExportFilter {
    pub since: Option<NaiveDate>,
    pub band: Option<Band>,
    pub mode: Option<String>,
    pub callsign: Option<String>,
}

impl ExportFilter {
    pub fn matches(&self, q: &AdifQso) -> bool {
        self.since
            .is_none_or(|date| q.qso_date.date_naive() >= date)
            && self.band.is_none_or(|band| {
                band_of(q).is_some_and(|b| b.eq_ignore_ascii_case(&band.to_string()))
            })
            && self.mode.as_ref().is_none_or(|m| {
                q.mode.eq_ignore_ascii_case(m)
                    || q.submode
                        .as_ref()
                        .is_some_and(|s| s.eq_ignore_ascii_case(m))
            })
            && self
                .callsign
                .as_ref()
                .is_none_or(|call| q.call.to_uppercase().contains(&call.to_uppercase()))
    }

    pub fn is_empty(&self) -> bool {
        self.since.is_none()
            && self.band.is_none()
            && self.mode.is_none()
            && self.callsign.is_none()
    }
}

#[derive(Debug, Default)]
pub struct ExportPlan {
    /// Readable contacts before filtering.
    pub read: usize,
    pub records: Vec<AdifRecord>,
    pub unreadable: Vec<UnreadableRecord>,
}

pub fn plan_export(
    processor: &AdifProcessor,
    parsed: &AdifParse,
    filter: &ExportFilter,
) -> ExportPlan {
    let mut plan = ExportPlan::default();
    for (index, record) in parsed.records.iter().enumerate() {
        let converted = match record {
            Ok(record) => processor.record_to_qso(record).map_err(|error| {
                (
                    record.fields.get("CALL").map(|f| f.value.clone()),
                    error.to_string(),
                )
            }),
            Err(error) => Err((error.call.clone(), error.error.to_string())),
        };
        let qso = match converted {
            Ok(qso) => qso,
            Err((call, reason)) => {
                plan.unreadable.push(UnreadableRecord {
                    record_number: index + 1,
                    call,
                    reason,
                });
                continue;
            }
        };
        plan.read += 1;
        if filter.matches(&qso) {
            let metadata = processor.adif_to_qso(&qso);
            let exported = processor.qso_to_adif(&metadata, metadata.contest_info.as_ref());
            plan.records.push(processor.qso_to_record(&exported));
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(call: &str, date: &str, extra: &str) -> String {
        format!(
            "<CALL:{}>{call}<QSO_DATE:{}>{date}{extra}<EOR>",
            call.len(),
            date.len()
        )
    }

    fn export(text: &str, filter: ExportFilter) -> ExportPlan {
        let p = AdifProcessor::new();
        plan_export(&p, &p.parse_bytes(text.as_bytes()), &filter)
    }

    #[test]
    fn since_includes_the_named_day() {
        let text = ["20260930", "20261001", "20261002"]
            .map(|d| record("K1ABC", d, ""))
            .concat();
        assert_eq!(
            export(
                &text,
                ExportFilter {
                    since: Some(NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()),
                    ..Default::default()
                }
            )
            .records
            .len(),
            2
        );
    }

    #[test]
    fn band_matches_band_field_or_frequency() {
        let text = [
            "<BAND:3>20M",
            "<BAND:3>20m",
            "<FREQ:6>14.074",
            "<BAND:3>40m",
            "",
        ]
        .map(|b| record("K1ABC", "20261001", b))
        .concat();
        assert_eq!(
            export(
                &text,
                ExportFilter {
                    band: Some("20m".parse().unwrap()),
                    ..Default::default()
                }
            )
            .records
            .len(),
            3
        );
    }

    #[test]
    fn mode_matches_mode_or_submode() {
        let text = [
            "<MODE:3>FT8",
            "<MODE:3>ft8",
            "<MODE:4>MFSK<SUBMODE:3>FT4",
            "<MODE:2>CW",
        ]
        .map(|m| record("K1ABC", "20261001", m))
        .concat();
        for (mode, count) in [("ft8", 2), ("FT4", 1), ("MFSK", 1)] {
            assert_eq!(
                export(
                    &text,
                    ExportFilter {
                        mode: Some(mode.into()),
                        ..Default::default()
                    }
                )
                .records
                .len(),
                count
            );
        }
    }

    #[test]
    fn callsign_is_a_case_insensitive_substring() {
        let text =
            record("EA8/K1ABC", "20261001", "") + &record("W1XYZ", "20261001", "<COMMENT:5>K1ABC");
        assert_eq!(
            export(
                &text,
                ExportFilter {
                    callsign: Some("k1ab".into()),
                    ..Default::default()
                }
            )
            .records
            .len(),
            1
        );
    }

    #[test]
    fn filters_combine() {
        let text = record("K1ABC", "20261001", "<BAND:3>20m<MODE:3>FT8")
            + &record("W1XYZ", "20261001", "<BAND:3>40m<MODE:3>FT8");
        let filter = ExportFilter {
            since: Some(NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()),
            band: Some("20m".parse().unwrap()),
            mode: Some("ft8".into()),
            callsign: Some("abc".into()),
        };
        assert!(!filter.is_empty());
        assert_eq!(export(&text, filter).records.len(), 1);
    }

    #[test]
    fn unreadable_records_are_listed_and_skipped() {
        let text = record("K1ABC", "20261001", "")
            + "<CALL:5>W1XYZ<EOR>"
            + &record("K2ABC", "20261002", "<COMMENT:999>broken")
            + &record("K3ABC", "20261003", "");
        let plan = export(&text, ExportFilter::default());
        assert_eq!(plan.read, 2);
        assert_eq!(plan.records.len(), 2);
        assert_eq!(
            plan.unreadable
                .iter()
                .map(|r| r.record_number)
                .collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(plan.unreadable[0].call.as_deref(), Some("W1XYZ"));
        assert!(plan.unreadable[1].reason.contains("length"));
    }
}
