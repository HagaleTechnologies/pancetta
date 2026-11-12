//! Lossless, conservative merging of a local LoTW confirmation report.
use crate::adif::{freq_hz_to_band, AdifProcessor, AdifRecord};
use crate::adif_import::DUPLICATE_WINDOW_MINUTES;
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use std::collections::BTreeMap;

/// A validated replacement and one category for every report record.
#[derive(Debug, Default)]
pub struct ConfirmationPlan {
    pub records: usize,
    pub updated_qsos: usize,
    pub unchanged: usize,
    pub unmatched: usize,
    pub ambiguous: usize,
    pub invalid: usize,
    pub diagnostics: Vec<(usize, String)>,
    pub patched: Vec<u8>,
}

fn value<'a>(record: &'a AdifRecord, name: &str) -> Option<&'a str> {
    record.fields.get(name).map(|f| f.value.trim())
}
fn positive(value: &str) -> bool {
    value.eq_ignore_ascii_case("Y") || value.eq_ignore_ascii_case("V")
}
fn date(value: &str) -> Option<NaiveDate> {
    (value.len() == 8 && value.bytes().all(|b| b.is_ascii_digit()) && !value.starts_with("0000"))
        .then(|| NaiveDate::parse_from_str(value, "%Y%m%d").ok())
        .flatten()
}
#[derive(Debug)]
struct Identity {
    call: String,
    band: String,
    mode: String,
    start: NaiveDateTime,
}
fn identity(record: &AdifRecord) -> Option<Identity> {
    let call = value(record, "CALL")?.to_uppercase();
    if call.is_empty() {
        return None;
    }
    let mode = value(record, "SUBMODE")
        .filter(|s| !s.is_empty())
        .or_else(|| value(record, "MODE"))?
        .to_uppercase();
    if mode.is_empty() {
        return None;
    }
    let known_band = |s: &str| {
        matches!(
            s,
            "2190M"
                | "630M"
                | "560M"
                | "160M"
                | "80M"
                | "60M"
                | "40M"
                | "30M"
                | "20M"
                | "17M"
                | "15M"
                | "12M"
                | "10M"
                | "8M"
                | "6M"
                | "5M"
                | "4M"
                | "2M"
                | "1.25M"
                | "70CM"
                | "33CM"
                | "23CM"
                | "13CM"
                | "9CM"
                | "6CM"
                | "3CM"
                | "1.25CM"
                | "6MM"
                | "4MM"
                | "2.5MM"
                | "2MM"
                | "1MM"
        )
    };
    let declared = value(record, "BAND")
        .filter(|s| !s.is_empty())
        .map(str::to_uppercase);
    if declared.as_deref().is_some_and(|s| !known_band(s)) {
        return None;
    }
    let inferred = value(record, "FREQ")
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|f| f.is_finite() && *f > 0.0)
        .map(|f| freq_hz_to_band(f * 1_000_000.0))
        .filter(|s| known_band(s));
    if declared
        .as_ref()
        .zip(inferred.as_ref())
        .is_some_and(|(a, b)| a != b)
    {
        return None;
    }
    let band = declared.or(inferred)?;
    let day = date(value(record, "QSO_DATE")?)?;
    let raw_time = value(record, "TIME_ON")?;
    if !matches!(raw_time.len(), 4 | 6) || !raw_time.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let time = if raw_time.len() == 4 {
        format!("{raw_time}00")
    } else {
        raw_time.to_string()
    };
    let time = NaiveTime::parse_from_str(&time, "%H%M%S").ok()?;
    Some(Identity {
        call,
        band,
        mode,
        start: day.and_time(time),
    })
}
fn report_date(record: &AdifRecord) -> Result<NaiveDate, &'static str> {
    let explicit =
        value(record, "LOTW_QSL_RCVD").is_some() || value(record, "LOTW_QSLRDATE").is_some();
    let pair = |flag, day| -> Result<NaiveDate, &'static str> {
        let flag = value(record, flag).ok_or("not a positive LoTW confirmation")?;
        if !positive(flag) {
            return Err("not a positive LoTW confirmation");
        }
        date(value(record, day).unwrap_or("")).ok_or("invalid LoTW QSL date")
    };
    let result = if explicit {
        pair("LOTW_QSL_RCVD", "LOTW_QSLRDATE")?
    } else {
        pair("QSL_RCVD", "QSLRDATE")?
    };
    if explicit
        && (value(record, "QSL_RCVD").is_some() || value(record, "QSLRDATE").is_some())
        && pair("QSL_RCVD", "QSLRDATE")? != result
    {
        return Err("invalid LoTW QSL date");
    }
    Ok(result)
}

/// Match a LoTW report against the entire current log without changing a file.
/// The caller re-reads under the logbook lock before planning, then backs up,
/// marks the index stale and writes `patched` in place only when changes exist.
pub fn plan_confirmations(logbook: &[u8], report: &[u8]) -> Result<ConfirmationPlan, String> {
    std::str::from_utf8(logbook)
        .map_err(|_| "Cannot safely read the logbook; nothing was changed.")?;
    std::str::from_utf8(report).map_err(|_| "unsupported report encoding")?;
    let processor = AdifProcessor::new();
    let local = processor.parse_bytes(logbook);
    let incoming = processor.parse_bytes(report);
    if local.unsafe_syntax {
        return Err("Cannot safely read the logbook; nothing was changed.".into());
    }
    if incoming.unsafe_syntax && incoming.records.is_empty() {
        return Err("cannot safely read confirmation report".into());
    }
    if !report.iter().all(u8::is_ascii_whitespace)
        && incoming.records.is_empty()
        && incoming.header.fields.is_empty()
        && !incoming.header_seen
    {
        return Err("cannot read confirmation report".into());
    }
    if !logbook.iter().all(u8::is_ascii_whitespace)
        && local.records.is_empty()
        && local.header.fields.is_empty()
        && !local.header_seen
    {
        return Err("Cannot safely read the logbook; nothing was changed.".into());
    }
    let mut identities = Vec::new();
    for (i, record) in local.records.iter().enumerate() {
        let record = record
            .as_ref()
            .map_err(|_| "Cannot safely read the logbook; nothing was changed.")?;
        processor
            .record_to_qso(record)
            .map_err(|_| "Cannot safely read the logbook; nothing was changed.")?;
        for name in [
            "CALL",
            "BAND",
            "FREQ",
            "MODE",
            "SUBMODE",
            "QSO_DATE",
            "TIME_ON",
            "LOTW_QSL_RCVD",
            "LOTW_QSLRDATE",
        ] {
            if local.spans[i]
                .fields
                .iter()
                .filter(|f| f.name == name)
                .count()
                > 1
            {
                return Err("Cannot safely read the logbook; nothing was changed.".into());
            }
        }
        identities.push(identity(record));
    }
    let mut plan = ConfirmationPlan {
        records: incoming.records.len(),
        patched: logbook.to_vec(),
        ..Default::default()
    };
    let mut updates: BTreeMap<usize, (NaiveDate, usize)> = BTreeMap::new();
    for (i, record) in incoming.records.iter().enumerate() {
        let record = match record {
            Ok(r) => r,
            Err(_) => {
                plan.invalid += 1;
                plan.diagnostics
                    .push((i + 1, "missing matching fields".into()));
                continue;
            }
        };
        let Some(id) = identity(record) else {
            plan.invalid += 1;
            plan.diagnostics
                .push((i + 1, "missing matching fields".into()));
            continue;
        };
        let day = match report_date(record) {
            Ok(d) => d,
            Err(reason) => {
                plan.invalid += 1;
                plan.diagnostics.push((i + 1, reason.into()));
                continue;
            }
        };
        // Duplicate report fields are not reliable evidence, even if a map's
        // final value happens to look like a valid positive confirmation.
        if [
            "CALL",
            "BAND",
            "FREQ",
            "MODE",
            "SUBMODE",
            "QSO_DATE",
            "TIME_ON",
            "LOTW_QSL_RCVD",
            "LOTW_QSLRDATE",
            "QSL_RCVD",
            "QSLRDATE",
        ]
        .iter()
        .any(|name| {
            incoming.spans[i]
                .fields
                .iter()
                .filter(|f| f.name == *name)
                .count()
                > 1
        }) {
            plan.invalid += 1;
            plan.diagnostics
                .push((i + 1, "missing matching fields".into()));
            continue;
        }
        let matches: Vec<_> = identities
            .iter()
            .enumerate()
            .filter_map(|(index, other)| {
                other
                    .as_ref()
                    .filter(|other| {
                        other.call == id.call
                            && other.band == id.band
                            && other.mode == id.mode
                            && (other.start - id.start).num_seconds().abs()
                                <= DUPLICATE_WINDOW_MINUTES * 60
                    })
                    .map(|_| index)
            })
            .collect();
        match matches.as_slice() {
            [] => {
                plan.unmatched += 1;
                plan.diagnostics.push((i + 1, "no matching QSO".into()));
            }
            [index] => {
                let entry = updates.entry(*index).or_insert((day, 0));
                entry.0 = entry.0.min(day);
                entry.1 += 1;
            }
            _ => {
                plan.ambiguous += 1;
                plan.diagnostics
                    .push((i + 1, "multiple matching QSOs".into()));
            }
        }
    }
    let mut edits = Vec::new();
    for (index, (day, count)) in updates {
        let record = local.records[index]
            .as_ref()
            .map_err(|_| "Cannot safely read the logbook; nothing was changed.")?;
        let day = value(record, "LOTW_QSLRDATE")
            .and_then(date)
            .map_or(day, |old| old.min(day))
            .format("%Y%m%d")
            .to_string();
        let flag = if value(record, "LOTW_QSL_RCVD").is_some_and(|s| s.eq_ignore_ascii_case("V")) {
            "V"
        } else {
            "Y"
        };
        let desired = [("LOTW_QSL_RCVD", flag), ("LOTW_QSLRDATE", day.as_str())];
        let changed = desired
            .iter()
            .any(|(name, v)| record.fields.get(*name).is_none_or(|f| f.value != *v));
        if !changed {
            plan.unchanged += count;
            continue;
        }
        plan.updated_qsos += 1;
        plan.unchanged += count - 1;
        let spans = &local.spans[index];
        let mut inserted = String::new();
        for (name, v) in desired {
            if record.fields.get(name).is_some_and(|f| f.value == v) {
                continue;
            }
            let dtype = record
                .fields
                .get(name)
                .and_then(|f| f.data_type.as_deref())
                .map_or(String::new(), |t| format!(":{t}"));
            let tag = format!("<{name}:{}{dtype}>{v}", v.len());
            if let Some(span) = spans.fields.iter().find(|f| f.name == name) {
                edits.push((span.whole.clone(), tag));
            } else {
                inserted.push_str(&tag);
            }
        }
        if !inserted.is_empty() {
            edits.push((spans.terminator..spans.terminator, inserted));
        }
    }
    edits.sort_by_key(|(span, _)| span.start);
    for (span, replacement) in edits.into_iter().rev() {
        plan.patched.splice(span, replacement.bytes());
    }
    let checked = processor.parse_bytes(&plan.patched);
    if checked.records.len() != local.records.len()
        || checked.records.iter().any(|r| {
            r.as_ref()
                .ok()
                .and_then(|r| processor.record_to_qso(r).ok())
                .is_none()
        })
    {
        return Err("Cannot safely read the logbook; nothing was changed.".into());
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn qso(call: &str, time: &str, extra: &str) -> String {
        format!("<CALL:{}>{call}<BAND:3>20M<MODE:3>FT8<QSO_DATE:8>20261110<TIME_ON:{}>{time}{extra}<EOR>", call.len(), time.len())
    }
    fn positive() -> &'static str {
        "<LOTW_QSL_RCVD:1>Y<LOTW_QSLRDATE:8>20261111"
    }
    #[test]
    fn empty_reports_do_not_change_the_logbook() {
        let local = qso("DL1ABC", "120000", "");
        for report in ["", "\n", "<EOH>", "<ADIF_VER:5>3.1.0<EOH>\n"] {
            let plan = plan_confirmations(local.as_bytes(), report.as_bytes()).unwrap();
            assert_eq!(plan.records, 0);
            assert_eq!(plan.updated_qsos, 0);
            assert_eq!(plan.patched, local.as_bytes());
        }
    }
    #[test]
    fn strict_confirmation_matching() {
        let local = qso("DL1ABC", "120000", "");
        assert_eq!(
            plan_confirmations(
                local.as_bytes(),
                qso("dl1abc", "1200", positive()).as_bytes()
            )
            .unwrap()
            .updated_qsos,
            1
        );
        for report in [
            qso("DL1ABC/P", "120000", positive()),
            qso("DL1ABC", "120000", positive()).replace("20M", "40M"),
            qso("DL1ABC", "120000", positive()).replace("FT8", "FT4"),
        ] {
            assert_eq!(
                plan_confirmations(local.as_bytes(), report.as_bytes())
                    .unwrap()
                    .unmatched,
                1
            );
        }
    }
    #[test]
    fn effective_mode_hhmm_midnight_and_band_frequency() {
        let local = qso("DL1ABC", "2359", "").replace("<MODE:3>FT8", "<MODE:4>MFSK<SUBMODE:3>FT4");
        let report = qso("DL1ABC", "0001", positive())
            .replace("20261110", "20261111")
            .replace("FT8", "FT4")
            .replace("<BAND:3>20M", "<FREQ:6>14.074");
        assert_eq!(
            plan_confirmations(local.as_bytes(), report.as_bytes())
                .unwrap()
                .updated_qsos,
            1
        );
        for bad in [
            report.replace("14.074", "99.999"),
            report.replace("<FREQ:6>14.074", "<BAND:3>40M<FREQ:6>14.074"),
            report.replace("<MODE:3>FT4", ""),
            report.replace("<QSO_DATE:8>20261111", ""),
        ] {
            assert_eq!(
                plan_confirmations(local.as_bytes(), bad.as_bytes())
                    .unwrap()
                    .invalid,
                1
            );
        }
    }
    #[test]
    fn lossless_fields_utf8_comments_and_trailing_record() {
        for length in [6, 7] {
            let extra = format!("<COMMENT:{length}:S>é<EOR> <APP_X:4:S>keep<LOTW_QSL_RCVD:1:E>N");
            let local = format!(
                "operator comment\n<ADIF_VER:5>3.1.0<EOH>\n{}",
                qso("DL1ABC", "120000", &extra).trim_end_matches("<EOR>")
            );
            let p = plan_confirmations(
                local.as_bytes(),
                qso("DL1ABC", "120000", positive()).as_bytes(),
            )
            .unwrap();
            let text = String::from_utf8(p.patched).unwrap();
            let expected = local.replace("<LOTW_QSL_RCVD:1:E>N", "<LOTW_QSL_RCVD:1:E>Y")
                + "<LOTW_QSLRDATE:8>20261111";
            assert_eq!(text, expected);
        }
    }
    #[test]
    fn unsafe_local_inputs_are_refused() {
        let report = qso("DL1ABC", "120000", positive());
        for bad in [
            qso("DL1ABC", "120000", "<LOTW_QSL_RCVD:1>Y<LOTW_QSL_RCVD:1>N"),
            qso("DL1ABC", "120000", "<COMMENT:999>bad"),
            qso("DL1ABC", "120000", "").replace("20261110", "20260230"),
        ] {
            assert!(
                plan_confirmations(bad.as_bytes(), report.as_bytes()).is_err(),
                "{bad}"
            );
        }
        assert!(plan_confirmations(&[0xff], report.as_bytes()).is_err());
        let broken_header = format!("<COMMENT:999>broken<EOH>{}", qso("DL1ABC", "120000", ""));
        assert!(plan_confirmations(broken_header.as_bytes(), report.as_bytes()).is_err());
    }
    #[test]
    fn explicit_generic_pairs_and_leap_dates() {
        let local = qso("DL1ABC", "120000", "");
        let generic = "<QSL_RCVD:1>v<QSLRDATE:8>20240229";
        let p = plan_confirmations(
            local.as_bytes(),
            qso("DL1ABC", "120000", generic).as_bytes(),
        )
        .unwrap();
        assert_eq!(p.updated_qsos, 1);
        assert!(String::from_utf8(p.patched)
            .unwrap()
            .contains("<LOTW_QSLRDATE:8>20240229"));
        for bad in [
            format!("{generic}<LOTW_QSL_RCVD:1>Y"),
            format!("{generic}{}", positive()),
            "<QSL_RCVD:1>Y<QSLRDATE:8>20230229".into(),
            "<LOTW_QSL_RCVD:1>Y".into(),
        ] {
            assert_eq!(
                plan_confirmations(local.as_bytes(), qso("DL1ABC", "120000", &bad).as_bytes())
                    .unwrap()
                    .invalid,
                1
            );
        }
    }
    #[test]
    fn confirmation_window_boundaries() {
        let local = qso("DL1ABC", "120000", "");
        for (time, count) in [
            ("112959", 0),
            ("113000", 1),
            ("113001", 1),
            ("122959", 1),
            ("123000", 1),
            ("123001", 0),
        ] {
            assert_eq!(
                plan_confirmations(local.as_bytes(), qso("DL1ABC", time, positive()).as_bytes())
                    .unwrap()
                    .updated_qsos,
                count,
                "{time}"
            );
        }
    }
    #[test]
    fn ambiguous_matches_are_skipped() {
        let local = qso("DL1ABC", "120000", "").repeat(2);
        let p = plan_confirmations(
            local.as_bytes(),
            qso("DL1ABC", "120000", positive()).as_bytes(),
        )
        .unwrap();
        assert_eq!(p.ambiguous, 1);
        assert_eq!(p.patched, local.as_bytes());
    }
    #[test]
    fn invalid_confirmation_records_are_skipped() {
        let local = qso("DL1ABC", "120000", "");
        for report in [
            qso("DL1ABC", "120000", positive()).replace("20261111", "20260229"),
            qso("DL1ABC", "120000", positive()).replace(">Y", ">N"),
            qso("DL1ABC", "120000", positive()).replace("<BAND:3>20M", ""),
        ] {
            assert_eq!(
                plan_confirmations(local.as_bytes(), report.as_bytes())
                    .unwrap()
                    .invalid,
                1
            );
        }
    }
    #[test]
    fn invalid_existing_date_is_repaired_without_touching_paper_qsl() {
        let local = qso(
            "DL1ABC",
            "120000",
            "<QSL_RCVD:1>V<QSLRDATE:8>20240101<LOTW_QSL_RCVD:1>V<LOTW_QSLRDATE:8>20260230",
        );
        let plan = plan_confirmations(
            local.as_bytes(),
            qso("DL1ABC", "120000", positive()).as_bytes(),
        )
        .unwrap();
        let patched = String::from_utf8(plan.patched).unwrap();
        assert!(patched.contains("<QSL_RCVD:1>V<QSLRDATE:8>20240101"));
        assert!(patched.contains("<LOTW_QSL_RCVD:1>V<LOTW_QSLRDATE:8>20261111"));
    }
    #[test]
    fn confirmation_merge_is_monotonic_and_order_independent() {
        let local = qso(
            "DL1ABC",
            "120000",
            "<LOTW_QSL_RCVD:1>V<LOTW_QSLRDATE:8>20261109",
        );
        let a = qso("DL1ABC", "120000", positive());
        let b = a.replace("20261111", "20261108");
        let p = plan_confirmations(local.as_bytes(), format!("{a}{b}").as_bytes()).unwrap();
        let reversed = plan_confirmations(local.as_bytes(), format!("{b}{a}").as_bytes()).unwrap();
        assert_eq!(p.patched, reversed.patched);
        assert_eq!(p.updated_qsos, 1);
        assert_eq!(p.unchanged, 1);
        let patched = String::from_utf8(p.patched).unwrap();
        assert!(patched.contains("<LOTW_QSL_RCVD:1>V"));
        assert!(patched.contains("20261108"));
        let repeat = plan_confirmations(patched.as_bytes(), format!("{a}{b}").as_bytes()).unwrap();
        assert_eq!(repeat.updated_qsos, 0);
        assert_eq!(repeat.unchanged, 2);
        assert_eq!(repeat.patched, patched.as_bytes());
    }
}
