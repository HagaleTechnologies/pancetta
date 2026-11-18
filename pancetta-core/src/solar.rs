//! Sunrise, sunset and grey-line timing from a station's coordinates.
//!
//! Uses the NOAA solar-position equations (the ones behind the NOAA Solar
//! Calculator spreadsheet) with the standard sunrise zenith of 90.833°, which
//! folds in atmospheric refraction and the solar disc's radius. Accuracy is
//! about a minute at mid latitudes, which is ample for grey-line operating.
//!
//! [`sun_events`] answers for a calendar date at a longitude: the events are
//! the sunrise before and the sunset after that date's local solar noon, so a
//! far-east station's sunrise can fall on the previous UTC date and a
//! far-west station's sunset on the next one.

use chrono::{DateTime, Duration, NaiveDate, Utc};

/// Zenith angle of the sun's centre at sunrise and sunset, in degrees.
const SUNRISE_ZENITH_DEG: f64 = 90.833;

/// Refinement passes for each event; the solution converges well within this.
const REFINE_PASSES: usize = 4;

/// Julian day of the Unix epoch (1970-01-01 00:00 UTC).
const UNIX_EPOCH_JD: f64 = 2_440_587.5;

/// Default half-width of the grey-line window around sunrise or sunset, in
/// seconds (30 minutes either side of the event).
pub const GREY_LINE_WINDOW_SECS: i64 = 1800;

/// Sunrise and sunset around one date's local solar noon at a location.
///
/// An event is `None` when the sun does not cross the horizon that day
/// (polar day or polar night).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SunEvents {
    /// Sunrise before local solar noon, or `None` during polar day or night.
    pub sunrise: Option<DateTime<Utc>>,
    /// Sunset after local solar noon, or `None` during polar day or night.
    pub sunset: Option<DateTime<Utc>>,
}

/// Which grey-line transition a station is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GreyLinePhase {
    /// Around local sunrise.
    Sunrise,
    /// Around local sunset.
    Sunset,
}

impl GreyLinePhase {
    /// Lower-case label for display: `"sunrise"` or `"sunset"`.
    pub fn label(self) -> &'static str {
        match self {
            GreyLinePhase::Sunrise => "sunrise",
            GreyLinePhase::Sunset => "sunset",
        }
    }
}

/// Solar declination (degrees) and equation of time (minutes) at a Julian day.
fn solar_position(jd: f64) -> (f64, f64) {
    let t = (jd - 2_451_545.0) / 36_525.0;

    let mean_long = (280.46646 + t * (36_000.769_83 + t * 0.000_303_2)).rem_euclid(360.0);
    let mean_anom = 357.52911 + t * (35_999.050_29 - 0.000_153_7 * t);
    let eccent = 0.016_708_634 - t * (0.000_042_037 + 0.000_000_126_7 * t);

    let m = mean_anom.to_radians();
    let eq_ctr = m.sin() * (1.914_602 - t * (0.004_817 + 0.000_014 * t))
        + (2.0 * m).sin() * (0.019_993 - 0.000_101 * t)
        + (3.0 * m).sin() * 0.000_289;
    let true_long = mean_long + eq_ctr;

    let omega = (125.04 - 1_934.136 * t).to_radians();
    let app_long = true_long - 0.005_69 - 0.004_78 * omega.sin();

    let mean_obliq =
        23.0 + (26.0 + (21.448 - t * (46.815 + t * (0.000_59 - t * 0.001_813))) / 60.0) / 60.0;
    let obliq = mean_obliq + 0.002_56 * omega.cos();

    let decl = (obliq.to_radians().sin() * app_long.to_radians().sin()).asin();

    let y = (obliq.to_radians() / 2.0).tan().powi(2);
    let l0 = mean_long.to_radians();
    let eq_time = 4.0
        * (y * (2.0 * l0).sin() - 2.0 * eccent * m.sin()
            + 4.0 * eccent * y * m.sin() * (2.0 * l0).cos()
            - 0.5 * y * y * (4.0 * l0).sin()
            - 1.25 * eccent * eccent * (2.0 * m).sin())
        .to_degrees();

    (decl.to_degrees(), eq_time)
}

/// Sunrise hour angle in degrees, or `None` when the sun stays above or below
/// the horizon all day.
fn sunrise_hour_angle(lat_deg: f64, decl_deg: f64) -> Option<f64> {
    let lat = lat_deg.to_radians();
    let decl = decl_deg.to_radians();
    let cos_ha =
        SUNRISE_ZENITH_DEG.to_radians().cos() / (lat.cos() * decl.cos()) - lat.tan() * decl.tan();
    if !cos_ha.is_finite() || !(-1.0..=1.0).contains(&cos_ha) {
        return None;
    }
    Some(cos_ha.acos().to_degrees())
}

/// Minutes after `date` 00:00 UTC of solar noon at `lon_deg`, refined so the
/// equation of time is evaluated at noon itself.
fn solar_noon_minutes(jd_midnight: f64, lon_deg: f64) -> f64 {
    let mut noon = 720.0 - 4.0 * lon_deg;
    for _ in 0..REFINE_PASSES {
        let (_, eq_time) = solar_position(jd_midnight + noon / 1440.0);
        noon = 720.0 - 4.0 * lon_deg - eq_time;
    }
    noon
}

/// Minutes after `date` 00:00 UTC of sunrise (`sign = -1`) or sunset
/// (`sign = +1`), refined so the solar position is evaluated at the event.
fn event_minutes(
    jd_midnight: f64,
    lat_deg: f64,
    lon_deg: f64,
    noon: f64,
    sign: f64,
) -> Option<f64> {
    let mut minutes = noon;
    for _ in 0..REFINE_PASSES {
        let (decl, eq_time) = solar_position(jd_midnight + minutes / 1440.0);
        let ha = sunrise_hour_angle(lat_deg, decl)?;
        minutes = 720.0 - 4.0 * (lon_deg - sign * ha) - eq_time;
    }
    minutes.is_finite().then_some(minutes)
}

/// Convert minutes after `date` 00:00 UTC to an instant, rounded to the second.
fn minutes_to_utc(date: NaiveDate, minutes: f64) -> Option<DateTime<Utc>> {
    let midnight = date.and_hms_opt(0, 0, 0)?.and_utc();
    let secs = (minutes * 60.0).round();
    if !secs.is_finite() {
        return None;
    }
    midnight.checked_add_signed(Duration::seconds(secs as i64))
}

/// Sunrise and sunset at (`lat_deg`, `lon_deg`; north and east positive)
/// around the local solar noon of `date`.
///
/// The sunrise is the one before that noon and the sunset the one after it,
/// so either may fall on a neighbouring UTC date (Sydney's sunrise for
/// 2026-06-21 is about 21:00 UTC on 2026-06-20). An event is `None` during
/// polar day or polar night, or when the coordinates are not finite.
pub fn sun_events(lat_deg: f64, lon_deg: f64, date: NaiveDate) -> SunEvents {
    let none = SunEvents {
        sunrise: None,
        sunset: None,
    };
    if !lat_deg.is_finite() || !lon_deg.is_finite() {
        return none;
    }
    let Some(midnight) = date.and_hms_opt(0, 0, 0) else {
        return none;
    };
    let jd_midnight = midnight.and_utc().timestamp() as f64 / 86_400.0 + UNIX_EPOCH_JD;
    let noon = solar_noon_minutes(jd_midnight, lon_deg);

    let at = |sign: f64| {
        event_minutes(jd_midnight, lat_deg, lon_deg, noon, sign)
            .and_then(|m| minutes_to_utc(date, m))
    };
    SunEvents {
        sunrise: at(-1.0),
        sunset: at(1.0),
    }
}

/// The grey-line phase at (`lat_deg`, `lon_deg`) at `now`, if any.
///
/// Returns the phase whose sunrise or sunset lies within `window_secs` of
/// `now` (either side), checking the events for the UTC dates before, of and
/// after `now` so a window that crosses UTC midnight is still found. When
/// both a sunrise and a sunset qualify, the nearer one wins. `None` outside
/// every window, during polar day or night, or when `window_secs` is negative.
pub fn grey_line_phase(
    lat_deg: f64,
    lon_deg: f64,
    now: DateTime<Utc>,
    window_secs: i64,
) -> Option<GreyLinePhase> {
    if window_secs < 0 {
        return None;
    }
    let today = now.date_naive();
    let dates = [today.pred_opt(), Some(today), today.succ_opt()];

    let mut best: Option<(i64, GreyLinePhase)> = None;
    for date in dates.into_iter().flatten() {
        let events = sun_events(lat_deg, lon_deg, date);
        for (event, phase) in [
            (events.sunrise, GreyLinePhase::Sunrise),
            (events.sunset, GreyLinePhase::Sunset),
        ] {
            let Some(at) = event else { continue };
            let distance = (at - now).num_seconds().abs();
            if distance <= window_secs && best.is_none_or(|(d, _)| distance < d) {
                best = Some((distance, phase));
            }
        }
    }
    best.map(|(_, phase)| phase)
}

#[cfg(test)]
#[path = "solar/tests/grey_line_tests.rs"]
mod grey_line_tests;
