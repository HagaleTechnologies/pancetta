use super::*;
use crate::gridsquare::grid_to_coordinates;
use chrono::TimeZone;

const TOLERANCE_SECS: i64 = 120;

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).expect("valid test date")
}

fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0)
        .single()
        .expect("valid test instant")
}

fn assert_near(label: &str, got: Option<DateTime<Utc>>, want: DateTime<Utc>) {
    let got = got.unwrap_or_else(|| panic!("{label}: expected {want}, got None"));
    let diff = (got - want).num_seconds().abs();
    assert!(
        diff <= TOLERANCE_SECS,
        "{label}: got {got}, want {want} (off by {diff} s)"
    );
}

#[test]
fn sun_events_match_published_times() {
    let (london_lat, london_lon) = (51.5074, -0.1278);

    let summer = sun_events(london_lat, london_lon, date(2026, 6, 21));
    assert_near(
        "London 2026-06-21 sunrise",
        summer.sunrise,
        utc(2026, 6, 21, 3, 43),
    );
    assert_near(
        "London 2026-06-21 sunset",
        summer.sunset,
        utc(2026, 6, 21, 20, 21),
    );

    let winter = sun_events(london_lat, london_lon, date(2026, 12, 21));
    assert_near(
        "London 2026-12-21 sunrise",
        winter.sunrise,
        utc(2026, 12, 21, 8, 4),
    );
    assert_near(
        "London 2026-12-21 sunset",
        winter.sunset,
        utc(2026, 12, 21, 15, 53),
    );

    // Sydney's sunrise for 2026-06-21 falls on the previous UTC date.
    let sydney = sun_events(-33.87, 151.21, date(2026, 6, 21));
    assert_near(
        "Sydney 2026-06-21 sunrise",
        sydney.sunrise,
        utc(2026, 6, 20, 21, 0),
    );
    assert_near(
        "Sydney 2026-06-21 sunset",
        sydney.sunset,
        utc(2026, 6, 21, 6, 54),
    );

    // Tromsø: midnight sun at the June solstice, polar night at December's.
    let none = SunEvents {
        sunrise: None,
        sunset: None,
    };
    assert_eq!(sun_events(69.65, 18.96, date(2026, 6, 21)), none);
    assert_eq!(sun_events(69.65, 18.96, date(2026, 12, 21)), none);
}

#[test]
fn grey_line_phase_table() {
    let (fn42_lat, fn42_lon) = grid_to_coordinates("FN42").expect("FN42 is a valid grid");

    let table = [
        (utc(2026, 6, 21, 9, 20), Some(GreyLinePhase::Sunrise)),
        // FN42's sunset for 2026-06-21 is about 00:24 UTC on 2026-06-22.
        (utc(2026, 6, 22, 0, 10), Some(GreyLinePhase::Sunset)),
        (utc(2026, 6, 21, 15, 0), None),
    ];
    for (now, want) in table {
        assert_eq!(
            grey_line_phase(fn42_lat, fn42_lon, now, GREY_LINE_WINDOW_SECS),
            want,
            "FN42 at {now}"
        );
    }

    for hour in 0..24 {
        let now = utc(2026, 6, 21, hour, 0);
        assert_eq!(
            grey_line_phase(69.65, 18.96, now, 1800),
            None,
            "Tromsø at {now}"
        );
    }

    assert_eq!(GreyLinePhase::Sunrise.label(), "sunrise");
    assert_eq!(GreyLinePhase::Sunset.label(), "sunset");
}
