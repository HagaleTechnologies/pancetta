//! PAN-441: `UtcTimestamp` writes the exact text the chrono build wrote and
//! reads the forms chrono read, except whitespace inside the date, time or
//! offset (pinned below). Expected values are literals, never blessed.
use super::*;

const BASE: i64 = 1_793_802_600; // 2026-11-04T14:30:00Z

fn at(nanos: u32) -> UtcTimestamp {
    UtcTimestamp::from_parts(BASE, nanos).expect("in range")
}

#[test]
fn serde_debug_column_display_and_nanos_forms_match_chrono() {
    let table: [(u32, &str, &str, &str); 6] = [
        (
            0,
            "2026-11-04T14:30:00Z",
            "2026-11-04T14:30:00+00:00",
            "2026-11-04 14:30:00 UTC",
        ),
        (
            500_000_000,
            "2026-11-04T14:30:00.500Z",
            "2026-11-04T14:30:00.500+00:00",
            "2026-11-04 14:30:00.500 UTC",
        ),
        (
            510_000_000,
            "2026-11-04T14:30:00.510Z",
            "2026-11-04T14:30:00.510+00:00",
            "2026-11-04 14:30:00.510 UTC",
        ),
        (
            123_456_000,
            "2026-11-04T14:30:00.123456Z",
            "2026-11-04T14:30:00.123456+00:00",
            "2026-11-04 14:30:00.123456 UTC",
        ),
        (
            123_456_780,
            "2026-11-04T14:30:00.123456780Z",
            "2026-11-04T14:30:00.123456780+00:00",
            "2026-11-04 14:30:00.123456780 UTC",
        ),
        (
            1,
            "2026-11-04T14:30:00.000000001Z",
            "2026-11-04T14:30:00.000000001+00:00",
            "2026-11-04 14:30:00.000000001 UTC",
        ),
    ];
    for (nanos, zulu, rfc3339, display) in table {
        let ts = at(nanos);
        assert_eq!(
            serde_json::to_string(&ts).unwrap(),
            format!("\"{zulu}\""),
            "serde {nanos}"
        );
        assert_eq!(format!("{ts:?}"), zulu, "debug {nanos}");
        assert_eq!(ts.to_rfc3339(), rfc3339, "rfc3339 {nanos}");
        assert_eq!(ts.to_string(), display, "display {nanos}");
    }
    assert_eq!(
        at(500_000_000).to_rfc3339_nanos(),
        "2026-11-04T14:30:00.500000000Z"
    );
}

#[test]
fn pre_epoch_and_year_edges_match_chrono() {
    assert_eq!(
        UtcTimestamp::from_parts(-1, 500_000_000)
            .unwrap()
            .to_rfc3339(),
        "1969-12-31T23:59:59.500+00:00"
    );
    assert_eq!(
        serde_json::to_string(&UtcTimestamp::from_parts(-14_182_940, 250_000_000).unwrap())
            .unwrap(),
        "\"1969-07-20T20:17:40.250Z\""
    );
    assert_eq!(
        UtcTimestamp::from_parts(-62_167_219_200, 0)
            .unwrap()
            .to_rfc3339(),
        "0000-01-01T00:00:00+00:00"
    );
    assert_eq!(
        UtcTimestamp::from_parts(253_402_120_800, 0)
            .unwrap()
            .to_rfc3339(),
        "9999-12-29T22:00:00+00:00"
    );
    assert_eq!(UtcTimestamp::from_parts(0, 1_000_000_000), None);
    // Whole seconds floor before 1970, as chrono's `timestamp()` did.
    let pre = UtcTimestamp::from_parts(-1, 500_000_000).unwrap();
    assert_eq!(pre.as_second(), -1);
    assert_eq!(pre.as_millisecond(), -500);
    assert_eq!(pre.subsec_nanosecond(), 500_000_000);
}

#[test]
fn trailing_zero_fractions_sort_in_time_order() {
    let a = at(500_000_000);
    let b = at(510_000_000);
    assert!(serde_json::to_string(&a).unwrap() < serde_json::to_string(&b).unwrap());
    assert!(a.to_rfc3339() < b.to_rfc3339());
}

#[test]
fn parses_every_form_chrono_parsed() {
    let want = UtcTimestamp::from_second(BASE).unwrap();
    for text in [
        "2026-11-04T14:30:00Z",
        "2026-11-04T14:30:00+00:00",
        "2026-11-04 14:30:00Z",
        "2026-11-04t14:30:00z",
        "2026-11-04T14:30:00+0000",
        "2026-11-04T15:30:00+01:00",
        "  2026-11-04T14:30:00Z",
        "2026-11-04T14:30:00Z ",
        "2026-11-04T14:30:00 +00:00",
        "2026-11-04T14:30:00 Z",
        "2026-11-04 14:30:00 UTC",
        "+2026-11-04T14:30:00Z",
    ] {
        assert_eq!(
            text.parse::<UtcTimestamp>().unwrap(),
            want,
            "FromStr {text}"
        );
        let json = format!("\"{text}\"");
        assert_eq!(
            serde_json::from_str::<UtcTimestamp>(&json).unwrap(),
            want,
            "serde {text}"
        );
    }
    assert_eq!(
        "2026-11-04T14:30:00.123456789123Z"
            .parse::<UtcTimestamp>()
            .unwrap(),
        UtcTimestamp::from_parts(BASE, 123_456_789).unwrap()
    );
    assert!("2026-11-04T14:30:00".parse::<UtcTimestamp>().is_err());
    // chrono's Display ignored width and precision flags.
    let ts = UtcTimestamp::from_second(BASE).unwrap();
    assert_eq!(format!("{ts:.10}"), "2026-11-04 14:30:00 UTC");
    assert_eq!(format!("[{ts:>30}]"), "[2026-11-04 14:30:00 UTC]");
}

#[test]
fn rejects_text_ending_in_a_multibyte_char_without_panicking() {
    // A four-byte char puts `len - 3` inside it, where a byte slice panics.
    for text in [
        "2026-11-04T14:30:00Z\u{1F600}",
        "\u{1F600}",
        "2026-11-04T14:30:00\u{00E9}",
    ] {
        assert!(text.parse::<UtcTimestamp>().is_err(), "{text:?}");
        assert!(
            serde_json::from_str::<UtcTimestamp>(&format!("\"{text}\"")).is_err(),
            "{text:?}"
        );
    }
}

#[test]
fn negative_years_read_back_what_the_writer_wrote() {
    // chrono wrote years below 0 as `-YYYY` and read that form back; jiff
    // reads only the six-digit `-00YYYY`.
    let minus_one = UtcTimestamp::from_parts(-62_198_755_200, 500_000_000).expect("in range");
    assert_eq!(minus_one.to_rfc3339_z(), "-0001-01-01T00:00:00.500Z");
    for ts in [UtcTimestamp::MIN, minus_one] {
        let json = serde_json::to_string(&ts).unwrap();
        assert_eq!(
            serde_json::from_str::<UtcTimestamp>(&json).unwrap(),
            ts,
            "{json}"
        );
        assert_eq!(ts.to_rfc3339().parse::<UtcTimestamp>().unwrap(), ts);
    }
}

/// The two documented narrowings from chrono (malformed or out-of-domain
/// text only): interior whitespace no longer parses, and `strftime` writes a
/// year below 0 with the sign inside the four-character `%Y` width.
#[test]
fn documented_narrowings_from_chrono_hold() {
    for text in [
        "2026 -11-04T14:30:00Z",
        "2026-11-04T 14:30:00Z",
        "2026-11-04T14 :30:00Z",
        "2026-11-04T14:30:00+01 :00",
    ] {
        assert!(text.parse::<UtcTimestamp>().is_err(), "{text:?} parsed");
    }
    let year_minus_27 = UtcTimestamp::from_second(-63_005_238_144).expect("in range");
    assert_eq!(
        year_minus_27.strftime("%Y%m%d %H%M%S").to_string(),
        "-0270611 165736"
    );
}
