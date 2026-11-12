//! PAN-320 qualification-record guard (AC6): the committed
//! `docs/measurements/PAN-320-decimation.json` must accurately reflect the
//! available evidence *and* the shipped default in `AudioConfig`.
//!
//! A pending record (status `pending_station_captures`, null measured
//! totals, `subsample` selected) passes this readiness check with the
//! default false — but it is an implementation checkpoint, not a
//! completed station qualification. A complete record selects `fir31`
//! iff its FIR total is strictly greater; a tie must keep `subsample`.
//! Fabricated records the validator rejects: pending-with-zero-totals, a
//! `fir31` default without complete coverage, totals inconsistent with
//! the decode delta, and a tie selecting FIR.

use pancetta_config::AudioConfig;

const REPORT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../docs/measurements/PAN-320-decimation.json"
);

#[derive(Debug, PartialEq)]
enum Verdict {
    Pending {
        selected: String,
    },
    Complete {
        selected: String,
        sub: u64,
        fir: u64,
    },
}

/// Validate a qualification record's internal consistency and return its
/// decision. Errors name the fabrication.
fn validate(value: &serde_json::Value) -> Result<Verdict, String> {
    let status = value["status"]
        .as_str()
        .ok_or("missing status")?
        .to_string();
    let selected = value["decision"]["selected_default"]
        .as_str()
        .ok_or("missing decision.selected_default")?
        .to_string();
    if selected != "subsample" && selected != "fir31" {
        return Err(format!("unknown selected_default {selected:?}"));
    }
    let sub = value["decision"]["subsample_total"].as_u64();
    let fir = value["decision"]["fir31_total"].as_u64();
    let delta = value["decision"]["decode_delta"].as_i64();

    match status.as_str() {
        "pending_station_captures" => {
            // A pending record must carry null totals — zeros would be a
            // fabricated "measured" zero-decode experiment — and cannot
            // already select the FIR.
            if value["decision"]["subsample_total"].is_number()
                || value["decision"]["fir31_total"].is_number()
            {
                return Err(
                    "pending record carries numeric totals (fabricated measurement)".into(),
                );
            }
            if selected == "fir31" {
                return Err("pending record selects fir31 without any measurement".into());
            }
            Ok(Verdict::Pending { selected })
        }
        "complete" => {
            let (Some(sub), Some(fir)) = (sub, fir) else {
                return Err("complete record missing totals".into());
            };
            let Some(delta) = delta else {
                return Err("complete record missing decode_delta".into());
            };
            if delta != fir as i64 - sub as i64 {
                return Err(format!(
                    "decode_delta {delta} inconsistent with totals {sub}/{fir}"
                ));
            }
            // The strict count rule: fir31 iff strictly greater.
            let expected = if fir > sub { "fir31" } else { "subsample" };
            if selected != expected {
                return Err(format!(
                    "totals {sub}/{fir} select {expected}, record says {selected}"
                ));
            }
            Ok(Verdict::Complete { selected, sub, fir })
        }
        other => Err(format!("unknown status {other:?}")),
    }
}

/// The committed report must match the shipped default: pending (or a
/// complete record whose totals keep subsample) ⇒ `false`; a complete
/// record with a strictly greater FIR total ⇒ `true`.
#[test]
fn qualification_record_matches_shipped_default() {
    let text =
        std::fs::read_to_string(REPORT).unwrap_or_else(|e| panic!("cannot read {REPORT}: {e}"));
    let value: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{REPORT} is not valid JSON: {e}"));
    let verdict = validate(&value).unwrap_or_else(|e| panic!("invalid qualification record: {e}"));

    let shipped = AudioConfig::default().decimation_antialias_filter;
    match verdict {
        Verdict::Pending { selected } => {
            assert_eq!(selected, "subsample");
            assert!(
                !shipped,
                "pending qualification must ship the subsample default (false)"
            );
        }
        Verdict::Complete { selected, sub, fir } => {
            let expected = selected == "fir31";
            assert_eq!(
                shipped, expected,
                "shipped default must match the complete record ({sub}/{fir} -> {selected})"
            );
        }
    }

    // The report's synthetic labelling: a station record is not synthetic.
    assert_eq!(
        value["synthetic"], false,
        "station record must not be synthetic"
    );
}

/// The decision rule on fabricated records: every one is rejected.
#[test]
fn qualification_validator_rejects_fabrications() {
    let base = serde_json::json!({
        "status": "pending_station_captures",
        "synthetic": false,
        "decision": {
            "selected_default": "subsample",
            "subsample_total": null,
            "fir31_total": null,
            "decode_delta": null,
        }
    });
    assert!(matches!(validate(&base).unwrap(), Verdict::Pending { .. }));

    // Pending with fabricated zero totals.
    let mut bad = base.clone();
    bad["decision"]["subsample_total"] = 0.into();
    bad["decision"]["fir31_total"] = 0.into();
    assert!(validate(&bad).is_err(), "pending zero totals");

    // Pending selecting fir31.
    let mut bad = base.clone();
    bad["decision"]["selected_default"] = "fir31".into();
    assert!(validate(&bad).is_err(), "pending fir31");

    // fir31 default without complete coverage.
    let bad = serde_json::json!({
        "status": "complete",
        "decision": {
            "selected_default": "fir31",
            "subsample_total": null,
            "fir31_total": 10,
            "decode_delta": null,
        }
    });
    assert!(validate(&bad).is_err(), "fir31 without totals");

    // Inconsistent delta.
    let bad = serde_json::json!({
        "status": "complete",
        "decision": {
            "selected_default": "subsample",
            "subsample_total": 10,
            "fir31_total": 12,
            "decode_delta": 1,
        }
    });
    assert!(validate(&bad).is_err(), "inconsistent delta");

    // A tie selecting FIR.
    let bad = serde_json::json!({
        "status": "complete",
        "decision": {
            "selected_default": "fir31",
            "subsample_total": 12,
            "fir31_total": 12,
            "decode_delta": 0,
        }
    });
    assert!(validate(&bad).is_err(), "tie selecting fir31");

    // Honest complete records pass on both sides of the rule.
    let good = serde_json::json!({
        "status": "complete",
        "decision": {
            "selected_default": "subsample",
            "subsample_total": 12,
            "fir31_total": 11,
            "decode_delta": -1,
        }
    });
    assert!(validate(&good).is_ok());
    let good = serde_json::json!({
        "status": "complete",
        "decision": {
            "selected_default": "fir31",
            "subsample_total": 11,
            "fir31_total": 12,
            "decode_delta": 1,
        }
    });
    assert!(validate(&good).is_ok());
}
