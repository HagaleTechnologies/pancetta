use super::*;

fn t(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).expect("valid test time")
}

fn window(text: &str) -> UtcWindow {
    text.parse()
        .unwrap_or_else(|e| panic!("{text:?} should parse: {e}"))
}

#[test]
fn utc_window_parse_and_contains_table() {
    // (window, times inside, times outside)
    type Row = (&'static str, &'static [(u32, u32)], &'static [(u32, u32)]);
    let table: &[Row] = &[
        ("00:00-06:00", &[(0, 0), (5, 59)], &[(6, 0)]),
        ("22:00-02:00", &[(23, 30), (1, 59)], &[(2, 0), (21, 59)]),
        ("18:00-24:00", &[(23, 59)], &[]),
        ("00:00-24:00", &[(23, 59)], &[]),
    ];

    for (text, inside, outside) in table {
        let w = window(text);
        for &(h, m) in *inside {
            assert!(w.contains(t(h, m)), "{text} should contain {h:02}:{m:02}");
        }
        for &(h, m) in *outside {
            assert!(
                !w.contains(t(h, m)),
                "{text} should not contain {h:02}:{m:02}"
            );
        }
        assert_eq!(w.to_string(), *text, "Display must round-trip {text}");
        assert_eq!(window(&w.to_string()), w);
    }

    let w = window("18:00-24:00");
    assert_eq!(w.start_minute(), 18 * 60);
    assert_eq!(w.end_minute(), 1440);
    assert!(!w.contains(t(17, 59)));
    assert!(w.contains(t(18, 0)));

    // Seconds inside the last minute stay inside a 24:00 window.
    let late = NaiveTime::from_hms_opt(23, 59, 59).expect("valid time");
    assert!(window("00:00-24:00").contains(late));

    // Outer whitespace is trimmed.
    assert_eq!(window("  22:00-02:00\n"), window("22:00-02:00"));

    // Serde carries the text form.
    let json = serde_json::to_string(&window("22:00-02:00")).expect("serialize");
    assert_eq!(json, "\"22:00-02:00\"");
    let back: UtcWindow = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, window("22:00-02:00"));
    assert!(serde_json::from_str::<UtcWindow>("\"24:00-06:00\"").is_err());
}

#[test]
fn utc_window_rejects_malformed_text() {
    for text in [
        "00:00-00:00",
        "25:00-06:00",
        "24:00-06:00",
        "00:60-06:00",
        "00:00-24:01",
        "0:00-6:00",
        "00:00",
        "00:00-06:00-07:00",
        "",
    ] {
        let err = text
            .parse::<UtcWindow>()
            .expect_err(&format!("{text:?} must be rejected"));
        assert!(!err.to_string().is_empty());
    }
}
