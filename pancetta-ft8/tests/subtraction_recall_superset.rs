use pancetta_ft8::{
    DecodeBudget, DecodeBudgetReport, DecodedMessage, Ft8Config, Ft8Decoder, WINDOW_SAMPLES,
};
#[cfg(not(debug_assertions))]
use pancetta_ft8::{Ft8Encoder, Ft8Modulator, ProtocolParams, SAMPLE_RATE};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
#[cfg(not(debug_assertions))]
use std::time::{Duration, Instant};

fn fixtures() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wav");
    let mut paths = Vec::new();
    for group in ["wsjt", "jtdx", "basicft8", "generated"] {
        for entry in std::fs::read_dir(root.join(group)).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|extension| extension == "wav") {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths
}

fn audio(path: &Path) -> Vec<f32> {
    let reader = hound::WavReader::open(path).unwrap();
    let mut samples: Vec<f32> = reader
        .into_samples::<i16>()
        .map(|sample| sample.unwrap() as f32 / 32768.0)
        .collect();
    samples.resize(WINDOW_SAMPLES, 0.0);
    samples
}

fn max_decoder() -> Ft8Decoder {
    Ft8Decoder::new(Ft8Config {
        max_decode_passes: 2,
        time_varying_subtraction_enabled: true,
        ..Ft8Config::default()
    })
    .unwrap()
}

/// `max_decoder()` is configured for this many decode passes; the recall
/// helper sanity-checks reported pass counts against it.
const CONFIGURED_PASSES: u32 = 2;

/// One fixture's expected recall, as exact message texts.
struct RecallExpectation {
    /// Texts the first decode pass owes on every host and profile.
    first_pass: BTreeSet<String>,
    /// Texts only the second pass adds. Required when the second pass ran,
    /// excused only when the report says the legacy deadline skipped it.
    second_pass_only: BTreeSet<String>,
}

/// Expected texts in `required` that the run did not decode.
fn missing_texts<'a>(
    required: impl IntoIterator<Item = &'a String>,
    got: &BTreeSet<&str>,
) -> Vec<&'a str> {
    required
        .into_iter()
        .filter(|text| !got.contains(text.as_str()))
        .map(|text| text.as_str())
        .collect()
}

/// PAN-466: compare decoded messages against `expected`, keyed by exact
/// message text, using the pass report to decide which expectations the run
/// owes.
///
/// Membership no longer depends on the frequency/time coordinates of the
/// old `text|freq:{:.1}|dt:{:.3}` keys, so rounding-boundary flips between
/// hosts or profiles cannot change it. The pass report carries the timing
/// exposure instead: `decode_window` still consults its legacy
/// call-anchored `BudgetTracker` even under `DecodeBudget::unlimited()`,
/// so a loaded host can legitimately lose the second pass. Both configured
/// passes running owes the union of both text sets; a run the legacy
/// deadline cut short owes only the first-pass set. New decodes are always
/// accepted (the check only ever names missing texts).
fn check_recall(
    fixture: &str,
    expected: &RecallExpectation,
    messages: &[DecodedMessage],
    report: &DecodeBudgetReport,
) -> Result<(), String> {
    let got: BTreeSet<&str> = messages.iter().map(|m| m.text.as_str()).collect();
    let legacy_skipped: u32 = report
        .stages
        .iter()
        .filter(|(label, _, _, _)| *label == "multipass-legacy-deadline")
        .map(|(_, _, _, skipped)| *skipped)
        .sum();
    let passes = report.passes_run;
    let context = format!(
        "{fixture}: passes_run={passes}, legacy_skipped={legacy_skipped}, stages={:?}",
        report.stages
    );
    if passes == 0 {
        return Err(format!(
            "{context}: a decode report must show at least one pass"
        ));
    }
    if passes + legacy_skipped > CONFIGURED_PASSES {
        return Err(format!(
            "{context}: pass and skip counts exceed the {CONFIGURED_PASSES} configured passes"
        ));
    }
    if passes >= 2 && legacy_skipped == 0 {
        // Both configured passes ran: every expected text is owed.
        let union = expected
            .first_pass
            .iter()
            .chain(expected.second_pass_only.iter());
        let missing = missing_texts(union, &got);
        if missing.is_empty() {
            return Ok(());
        }
        return Err(format!("{context}: lost expected texts {missing:?}"));
    }
    if passes == 1 && legacy_skipped == 1 {
        // The legacy deadline cancelled the second pass: only the
        // first-pass set is owed. Surface the skipped comparison so a
        // `--nocapture` run shows the guard was relaxed and why.
        let first_missing = missing_texts(&expected.first_pass, &got);
        if !first_missing.is_empty() {
            return Err(format!(
                "{context}: lost first-pass texts {first_missing:?} (legacy deadline does not \
                 excuse first-pass losses)"
            ));
        }
        if !expected.second_pass_only.is_empty() {
            eprintln!(
                "{fixture}: legacy deadline skipped the second pass; not checking {} \
                 second-pass-only texts {:?}",
                expected.second_pass_only.len(),
                expected.second_pass_only
            );
        }
        return Ok(());
    }
    // One pass, no reported skip: the run stopped because the first pass
    // decoded nothing new, which is normal only for fixtures whose
    // expectations hold no second-pass texts.
    let second_missing = missing_texts(&expected.second_pass_only, &got);
    if !second_missing.is_empty() {
        return Err(format!(
            "{context}: stopped after one pass with no reported deadline skip, expected \
             second-pass-only texts {second_missing:?}"
        ));
    }
    let first_missing = missing_texts(&expected.first_pass, &got);
    if first_missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{context}: lost first-pass texts {first_missing:?}"
    ))
}

/// A message with the given text and coordinates, for synthetic recall
/// cases: the comparison consumes only texts, so the coordinates exercise
/// the old key format's rounding boundaries.
fn synthetic_message(text: &str, frequency_offset: f64, time_offset: f64) -> DecodedMessage {
    let mut message = DecodedMessage::from_ft8lib(text, frequency_offset as f32, 1.0, 0);
    message.frequency_offset = frequency_offset;
    message.time_offset = time_offset;
    message
}

/// A pass report as the decoder would publish it for a run the legacy
/// deadline cut short after `passes_run` passes.
fn synthetic_report(passes_run: u32, legacy_skipped: u32) -> DecodeBudgetReport {
    let stages = if legacy_skipped > 0 {
        vec![("multipass-legacy-deadline", 0, 0, legacy_skipped)]
    } else {
        Vec::new()
    };
    DecodeBudgetReport {
        stages,
        budget_exhausted: false,
        passes_run,
    }
}

fn texts(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

/// PAN-466 AC2: recall membership is keyed by message text alone, so the
/// same text satisfies the check from both sides of the old
/// `text|freq:{:.1}|dt:{:.3}` rounding boundaries, and a different text at
/// the original coordinates is reported missing.
#[test]
fn recall_membership_uses_message_text() {
    let expected = RecallExpectation {
        first_pass: texts(&["CQ K1ABC FN42"]),
        second_pass_only: BTreeSet::new(),
    };
    let report = synthetic_report(2, 0);
    // Coordinates on opposite sides of the old `{:.1}`/`{:.3}` boundaries
    // (the old keys would read "…|999.9|1.199" and "…|1000.1|1.201").
    for (frequency, dt) in [(999.949, 1.19949), (1000.051, 1.20051)] {
        let messages = [synthetic_message("CQ K1ABC FN42", frequency, dt)];
        assert!(
            check_recall("synthetic", &expected, &messages, &report).is_ok(),
            "same text across a rounding boundary must satisfy recall"
        );
    }
    // A different text at the original coordinates must be named as missing.
    let messages = [synthetic_message("CQ K1ABC FN43", 1000.0, 1.200)];
    let error = check_recall("synthetic", &expected, &messages, &report).unwrap_err();
    assert!(error.contains("CQ K1ABC FN42"), "{error}");
}

/// PAN-466 AC3: when the legacy deadline cancels only the second pass, a
/// missing second-pass-only text is excused, a run that decoded it anyway
/// (a gain) is accepted, and a missing first-pass text is still named.
#[test]
fn legacy_deadline_excuses_only_second_pass_messages() {
    let expected = RecallExpectation {
        first_pass: texts(&["K1ABC W9XYZ EN37"]),
        second_pass_only: texts(&["CQ UW5EJX/MM"]),
    };
    let report = synthetic_report(1, 1);
    // Only the first pass ran: its text satisfies the check even though the
    // second-pass-only text never decoded.
    let first_only = [synthetic_message("K1ABC W9XYZ EN37", 1000.0, 1.2)];
    assert!(
        check_recall("synthetic", &expected, &first_only, &report).is_ok(),
        "the legacy deadline must excuse second-pass-only losses"
    );
    // The same report with the second-pass text decoded too (a gain).
    let both = [
        synthetic_message("K1ABC W9XYZ EN37", 1000.0, 1.2),
        synthetic_message("CQ UW5EJX/MM", 753.1, 0.4),
    ];
    assert!(check_recall("synthetic", &expected, &both, &report).is_ok());
    // A missing first-pass text is named even under the reported skip, so a
    // count-only comparison (one message is present) cannot pass.
    let wrong_text = [synthetic_message("K1ABC W9XYZ EN37X", 1000.0, 1.2)];
    let error = check_recall("synthetic", &expected, &wrong_text, &report).unwrap_err();
    assert!(error.contains("K1ABC W9XYZ EN37"), "{error}");
}

/// PAN-466 AC4: when both passes ran, an expected second-pass text is
/// required, and a run that stops after one pass without a reported
/// deadline skip is an unexplained early exit on fixtures holding
/// second-pass expectations. Silent fixtures still exit normally after one
/// pass, and impossible pass counts are rejected.
#[test]
fn second_pass_recall_requires_messages_unless_deadline_skipped_it() {
    let expected = RecallExpectation {
        first_pass: texts(&["K1ABC W9XYZ EN37"]),
        second_pass_only: texts(&["CQ UW5EJX/MM"]),
    };
    let first_only = [synthetic_message("K1ABC W9XYZ EN37", 1000.0, 1.2)];
    // Both passes ran and the second-pass-only text is missing: named.
    let error =
        check_recall("synthetic", &expected, &first_only, &synthetic_report(2, 0)).unwrap_err();
    assert!(error.contains("CQ UW5EJX/MM"), "{error}");
    // Both passes ran, everything expected is present plus a gain.
    let all_present = [
        synthetic_message("K1ABC W9XYZ EN37", 1000.0, 1.2),
        synthetic_message("CQ UW5EJX/MM", 753.1, 0.4),
        synthetic_message("K1DEF W1ABC -12", 1200.0, 1.2),
    ];
    assert!(check_recall(
        "synthetic",
        &expected,
        &all_present,
        &synthetic_report(2, 0)
    )
    .is_ok());
    // Unexplained early exit: one pass, no reported skip, second-pass
    // expectations exist.
    let error =
        check_recall("synthetic", &expected, &first_only, &synthetic_report(1, 0)).unwrap_err();
    assert!(error.contains("CQ UW5EJX/MM"), "{error}");
    // A silent fixture's normal one-pass exit stays accepted.
    let silent = RecallExpectation {
        first_pass: BTreeSet::new(),
        second_pass_only: BTreeSet::new(),
    };
    assert!(check_recall("silent", &silent, &[], &synthetic_report(1, 0)).is_ok());
    // Zero passes: no real decode call reports this.
    assert!(check_recall(
        "synthetic",
        &expected,
        &all_present,
        &synthetic_report(0, 0)
    )
    .is_err());
    // Inconsistent pass and skip counts against the configured passes.
    assert!(check_recall(
        "synthetic",
        &expected,
        &all_present,
        &synthetic_report(2, 1)
    )
    .is_err());
    assert!(check_recall(
        "synthetic",
        &expected,
        &all_present,
        &synthetic_report(1, 2)
    )
    .is_err());
}

/// Fixture path as the baseline keys it: relative to the crate root,
/// slash-separated.
fn fixture_key(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

/// The recall baseline: fixture key → expected texts per pass.
///
/// Capture procedure (PAN-466), for when the expectations legitimately
/// change: on an idle host, in the Cargo test profile
/// (`cargo test -p pancetta-ft8 --features transmit`), decode each fixture
/// with fresh Max-shape decoders and `DecodeBudget::unlimited()` — once
/// with `max_decode_passes: 1` for `first_pass`, once with `: 2`, whose
/// extra texts become `second_pass_only` — and write the file from those
/// text sets. The capturing run must report one clean pass for the
/// one-pass reference and, for every nonempty reference, two passes and
/// zero legacy skips for the Max run; otherwise the host was loaded and
/// the capture must be repeated. Ordinary test runs never regenerate this
/// file from the decoder under test: the baseline is an independent
/// expectation, and a loss against it is a regression to investigate, not
/// to re-bless.
fn load_baseline(baseline_path: &Path) -> BTreeMap<String, RecallExpectation> {
    let raw: BTreeMap<String, BTreeMap<String, Vec<String>>> = serde_json::from_slice(
        &std::fs::read(baseline_path).expect("subtraction recall baseline must be readable"),
    )
    .expect("baseline entries must hold first_pass and second_pass_only text arrays");
    let mut baseline = BTreeMap::new();
    for (fixture, entry) in raw {
        let first_pass = entry
            .get("first_pass")
            .unwrap_or_else(|| panic!("baseline entry {fixture} must name a first_pass array"))
            .clone();
        let second_pass_only = entry
            .get("second_pass_only")
            .unwrap_or_else(|| {
                panic!("baseline entry {fixture} must name a second_pass_only array")
            })
            .clone();
        assert_eq!(
            entry.len(),
            2,
            "baseline entry {fixture} must hold exactly first_pass and second_pass_only"
        );
        for (name, values) in [
            ("first_pass", &first_pass),
            ("second_pass_only", &second_pass_only),
        ] {
            assert!(
                values.windows(2).all(|pair| pair[0] < pair[1]),
                "baseline {fixture} {name} must be sorted without duplicates: {values:?}"
            );
        }
        let second: BTreeSet<String> = second_pass_only.into_iter().collect();
        assert!(
            first_pass.iter().all(|text| !second.contains(text)),
            "baseline {fixture} first_pass and second_pass_only must be disjoint"
        );
        baseline.insert(
            fixture,
            RecallExpectation {
                first_pass: first_pass.into_iter().collect(),
                second_pass_only: second,
            },
        );
    }
    baseline
}

#[test]
fn baseline_decode_sets_are_supersets_post_change() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let baseline_path = root.join("tests/fixtures/subtraction_recall_baseline.json");
    let baseline = load_baseline(&baseline_path);
    let inventory: BTreeSet<String> = fixtures()
        .iter()
        .map(|path| fixture_key(root, path))
        .collect();
    let baseline_keys: BTreeSet<String> = baseline.keys().cloned().collect();
    assert_eq!(
        baseline_keys, inventory,
        "the baseline must cover exactly the discovered WAV inventory, empty recordings included"
    );
    for path in fixtures() {
        let name = fixture_key(root, &path);
        let expected = baseline.get(&name).expect("fixture missing from baseline");
        let mut decoder = max_decoder();
        let (messages, report) = decoder
            .decode_window_budgeted(&audio(&path), DecodeBudget::unlimited())
            .unwrap();
        if let Err(message) = check_recall(&name, expected, &messages, &report) {
            panic!("{message}");
        }
    }
}

#[cfg(not(debug_assertions))]
#[test]
fn max_shape_busy_window_skips_fewer_than_ten_percent() {
    let params = ProtocolParams::ft8();
    let mut audio = vec![0.0_f32; WINDOW_SAMPLES];
    for i in 0..20 {
        let call = format!("{}{}ABC", if i < 10 { 'K' } else { 'N' }, i % 10);
        let symbols = Ft8Encoder::new().encode_cq(&call, "FN42", false).unwrap();
        let mut modulator = Ft8Modulator::new(SAMPLE_RATE, 300.0 + i as f64 * 40.0, 1.0).unwrap();
        let signal = modulator
            .modulate_symbols_protocol(&symbols, 0.0, &params)
            .unwrap();
        for (a, b) in audio.iter_mut().zip(signal) {
            *a += 0.12 * b;
        }
    }
    let mut decoder = max_decoder();
    let (_, report) = decoder
        .decode_window_budgeted(
            &audio,
            DecodeBudget::until(Instant::now() + Duration::from_millis(2000)),
        )
        .unwrap();
    let stage = report
        .stages
        .iter()
        .find(|row| row.0 == "multipass-subtract")
        .expect("subtraction stage");
    eprintln!("PAN-370 busy subtraction stage: {stage:?}");
    assert!(stage.3 <= 1 && stage.2 + stage.3 >= 18, "stage={stage:?}");
}
