//! UTC time-of-day windows written as `HH:MM-HH:MM`.
//!
//! A [`UtcWindow`] is a half-open interval `[start, end)` of minutes in the
//! UTC day. When `start > end` the window wraps midnight, so `22:00-02:00`
//! covers 22:00 through 01:59. The end may be written as `24:00` to mean the
//! end of the day (`18:00-24:00` covers 18:00 through 23:59).
//!
//! The window serializes to and from its text form, so configuration files
//! carry it as a plain string.

use std::fmt;
use std::str::FromStr;

use chrono::{NaiveTime, Timelike};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Minutes in one UTC day; the only legal value above 23:59, and only as an end.
const MINUTES_PER_DAY: u16 = 1440;

/// A half-open UTC time-of-day window `[start, end)`, wrapping midnight when
/// `start > end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UtcWindow {
    start_min: u16,
    end_min: u16,
}

impl UtcWindow {
    /// Start of the window in minutes after 00:00 UTC (0..=1439).
    pub fn start_minute(&self) -> u16 {
        self.start_min
    }

    /// End of the window in minutes after 00:00 UTC (0..=1440; 1440 is `24:00`).
    pub fn end_minute(&self) -> u16 {
        self.end_min
    }

    /// Whether `t` falls inside the half-open window `[start, end)`.
    ///
    /// When `start > end` the window wraps midnight: it contains every time at
    /// or after `start` and every time before `end`.
    pub fn contains(&self, t: NaiveTime) -> bool {
        // Truncate to the minute: the window boundaries are whole minutes.
        let minute = (t.hour() * 60 + t.minute()) as u16;
        if self.start_min < self.end_min {
            minute >= self.start_min && minute < self.end_min
        } else {
            minute >= self.start_min || minute < self.end_min
        }
    }
}

/// Error returned when text is not a valid `HH:MM-HH:MM` UTC window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UtcWindowParseError(pub String);

impl fmt::Display for UtcWindowParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid UTC window: {}", self.0)
    }
}

impl std::error::Error for UtcWindowParseError {}

/// Parse exactly two ASCII digits.
fn two_digits(s: &str) -> Option<u16> {
    let b = s.as_bytes();
    if b.len() != 2 || !b[0].is_ascii_digit() || !b[1].is_ascii_digit() {
        return None;
    }
    Some(u16::from(b[0] - b'0') * 10 + u16::from(b[1] - b'0'))
}

/// Parse `HH:MM` into (hour, minute), both exactly two digits.
fn parse_hhmm(s: &str) -> Option<(u16, u16)> {
    let (h, m) = s.split_once(':')?;
    Some((two_digits(h)?, two_digits(m)?))
}

impl FromStr for UtcWindow {
    type Err = UtcWindowParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let text = s.trim();
        let err = |why: &str| {
            UtcWindowParseError(format!(
                "{text:?}: {why} (expected HH:MM-HH:MM, e.g. 22:00-02:00)"
            ))
        };

        let (start_text, end_text) = text
            .split_once('-')
            .ok_or_else(|| err("missing '-' between start and end"))?;
        if end_text.contains('-') {
            return Err(err("more than one '-'"));
        }

        let (sh, sm) = parse_hhmm(start_text).ok_or_else(|| err("start is not HH:MM"))?;
        let (eh, em) = parse_hhmm(end_text).ok_or_else(|| err("end is not HH:MM"))?;

        if sh > 23 {
            return Err(err("start hour must be 00-23"));
        }
        if sm > 59 {
            return Err(err("start minute must be 00-59"));
        }
        if em > 59 {
            return Err(err("end minute must be 00-59"));
        }
        if eh > 24 || (eh == 24 && em != 0) {
            return Err(err("end hour must be 00-23, or exactly 24:00"));
        }

        let start_min = sh * 60 + sm;
        let end_min = eh * 60 + em;
        if start_min == end_min {
            return Err(err("start and end must differ"));
        }
        // A wrapping window ending at 24:00 would mean start > 1440, which the
        // checks above already exclude; 00:00-24:00 is the whole day.
        debug_assert!(start_min < MINUTES_PER_DAY && end_min <= MINUTES_PER_DAY);

        Ok(UtcWindow { start_min, end_min })
    }
}

impl fmt::Display for UtcWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02}:{:02}-{:02}:{:02}",
            self.start_min / 60,
            self.start_min % 60,
            self.end_min / 60,
            self.end_min % 60
        )
    }
}

impl Serialize for UtcWindow {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for UtcWindow {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
#[path = "utc_window/tests/utc_window_tests.rs"]
mod utc_window_tests;
