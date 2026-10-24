//! PAN-110: published decode-quality claims must match the committed
//! production-pipeline benchmark.
//!
//! The figures FEATURES.md and docs/decoder-comparison.md quote come from
//! `research/scorecards/pan110-*.json`, written by
//! `eval --budget-anchor receipt --merge-ft8lib` (ft8_lib plus the native
//! decoder, merged as the coordinator merges them, under the receipt-anchored
//! budget). These checks fail if a quoted figure drifts from its scorecard,
//! or if an unmeasured claim (">95% at -20 dB", "Max = unlimited") returns.

const FEATURES: &str = include_str!("../../FEATURES.md");
const README: &str = include_str!("../../README.md");
const FT8_README: &str = include_str!("../../pancetta-ft8/README.md");
const FT8_LIB: &str = include_str!("../../pancetta-ft8/src/lib.rs");
const ARCHITECTURE: &str = include_str!("../../docs/ARCHITECTURE.md");
const COMPARISON: &str = include_str!("../../docs/decoder-comparison.md");

const DEEP_PATH: &str = "research/scorecards/pan110-synth-clean-deep-merged.json";
const MAX_PATH: &str = "research/scorecards/pan110-synth-clean-max-merged.json";
const FT4_PATH: &str = "research/scorecards/pan110-synth-ft4-max-merged.json";

const SCORECARDS: [(&str, &str); 3] = [
    (
        DEEP_PATH,
        include_str!("../../research/scorecards/pan110-synth-clean-deep-merged.json"),
    ),
    (
        MAX_PATH,
        include_str!("../../research/scorecards/pan110-synth-clean-max-merged.json"),
    ),
    (
        FT4_PATH,
        include_str!("../../research/scorecards/pan110-synth-ft4-max-merged.json"),
    ),
];

fn scorecard(path: &str) -> serde_json::Value {
    let (_, json) = SCORECARDS
        .iter()
        .find(|(p, _)| *p == path)
        .expect("known scorecard path");
    serde_json::from_str(json).unwrap_or_else(|e| panic!("{path} is not valid JSON: {e}"))
}

/// `decoded/attempts` of the scorecard's synth bin at `snr_db`.
fn bin_fraction(card: &serde_json::Value, tier: &str, snr_db: f64) -> String {
    let bins = card["tiers"][tier]["by_snr_db"]
        .as_array()
        .unwrap_or_else(|| panic!("scorecard has no {tier} by_snr_db"));
    let bin = bins
        .iter()
        .find(|b| b["snr_db"].as_f64() == Some(snr_db))
        .unwrap_or_else(|| panic!("no {snr_db} dB bin in {tier}"));
    format!(
        "{}/{}",
        bin["decoded"].as_u64().unwrap(),
        bin["attempts"].as_u64().unwrap()
    )
}

#[test]
fn no_published_file_repeats_an_unmeasured_claim() {
    for (name, text) in [
        ("FEATURES.md", FEATURES),
        ("README.md", README),
        ("pancetta-ft8/README.md", FT8_README),
        ("pancetta-ft8/src/lib.rs", FT8_LIB),
        ("docs/ARCHITECTURE.md", ARCHITECTURE),
    ] {
        assert!(!text.contains(">95%"), "{name} still claims >95%");
        assert!(
            !text.contains("0/unlimited"),
            "{name} still describes Max as 0/unlimited"
        );
    }
}

#[test]
fn quoted_minus_20_db_figure_matches_the_production_pipeline_scorecard() {
    let deep = scorecard(DEEP_PATH);
    let quoted = bin_fraction(&deep, "synth-clean", -20.0);
    assert!(
        COMPARISON.contains(&quoted),
        "docs/decoder-comparison.md does not quote {quoted} from {DEEP_PATH}"
    );
    assert!(
        FEATURES.contains(&quoted),
        "FEATURES.md does not quote {quoted} from {DEEP_PATH}"
    );
}

#[test]
fn benchmark_scorecards_record_the_production_pipeline_and_a_clean_commit() {
    for (path, _) in SCORECARDS {
        let card = scorecard(path);
        let p = &card["provenance"];
        assert_eq!(p["pipeline"], "ft8lib+native-merge", "{path}");
        assert_eq!(p["budget_anchor"], "receipt", "{path}");
        assert_eq!(p["ft8lib_available"], true, "{path}");
        assert!(
            !p["host_cpu"]["model"].as_str().unwrap_or("").is_empty(),
            "{path} has no CPU model"
        );
        assert_eq!(
            p["corpus"]["wav_set_sha256"].as_str().map(str::len),
            Some(64),
            "{path} has no corpus WAV-set hash"
        );
        assert_eq!(card["git"]["dirty"], false, "{path} ran on a dirty tree");
        assert_eq!(
            card["git"]["head_sha"].as_str().map(str::len),
            Some(40),
            "{path} has no commit"
        );
    }
}

#[test]
fn comparison_doc_labels_the_shipped_and_native_only_curves() {
    assert!(COMPARISON.contains("## Production pipeline (what ships)"));
    for (path, _) in SCORECARDS {
        assert!(
            COMPARISON.contains(path),
            "docs/decoder-comparison.md does not link {path}"
        );
    }
    assert!(COMPARISON.contains("native decoder only"));
}

#[test]
fn target_hardware_is_stated_as_unmeasured() {
    assert!(FEATURES.contains("unmeasured"), "FEATURES.md");
    assert!(README.contains("unmeasured"), "README.md");
}
