//! UTC source shared by scheduling and QSO management, and the workspace's
//! one serialized instant type.
//!
//! [`UtcTimestamp`] wraps `jiff::Timestamp` and writes the exact text the
//! chrono build wrote (PAN-441): RFC 3339 with a `Z` suffix from serde and
//! `Debug`, `+00:00` from [`UtcTimestamp::to_rfc3339`], and
//! `YYYY-MM-DD HH:MM:SS UTC` from `Display`. The fraction is omitted when it
//! is zero and otherwise printed with 3, 6 or 9 digits, so stored SQLite text
//! and JSON keep their bytes and their sort order.
use jiff::{civil, tz::Offset, SignedDuration, Timestamp};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;

/// Thread-safe UTC source; production uses the system clock.
pub type UtcClock = Arc<dyn Fn() -> UtcTimestamp + Send + Sync>;

/// Construct the production UTC source.
pub fn system_utc_clock() -> UtcClock {
    Arc::new(UtcTimestamp::now)
}

/// A reproducible even-slot origin for simulations.
pub fn slot_grid_origin() -> UtcTimestamp {
    UtcTimestamp::from_second(1_767_225_600).expect("slot grid origin is in range")
}

/// An instant in UTC whose text forms match what the chrono build wrote.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct UtcTimestamp(Timestamp);

/// Which suffix and separator the shared writer uses.
#[derive(Clone, Copy)]
enum TextForm {
    /// `YYYY-MM-DDTHH:MM:SS[.frac]Z` (serde, `Debug`).
    Zulu,
    /// `YYYY-MM-DDTHH:MM:SS[.frac]+00:00` (`to_rfc3339`).
    Rfc3339,
    /// `YYYY-MM-DD HH:MM:SS[.frac] UTC` (`Display`).
    Display,
}

const NANOS_PER_SECOND: i128 = 1_000_000_000;

impl UtcTimestamp {
    /// 1970-01-01T00:00:00Z.
    pub const UNIX_EPOCH: Self = Self(Timestamp::UNIX_EPOCH);

    /// The earliest instant jiff holds (year -9999).
    pub const MIN: Self = Self(Timestamp::MIN);

    /// The latest instant jiff holds (year 9999).
    pub const MAX: Self = Self(Timestamp::MAX);

    /// The current system time.
    pub fn now() -> Self {
        Self(Timestamp::now())
    }

    /// Seconds since the epoch plus a non-negative nanosecond fraction, the
    /// way chrono's `DateTime::from_timestamp` read them. `None` when
    /// `nanos` is a second or more, or the instant is outside jiff's range.
    pub fn from_parts(secs: i64, nanos: u32) -> Option<Self> {
        if i128::from(nanos) >= NANOS_PER_SECOND {
            return None;
        }
        Self::from_nanosecond(i128::from(secs) * NANOS_PER_SECOND + i128::from(nanos))
    }

    /// Whole seconds since the epoch.
    pub fn from_second(secs: i64) -> Option<Self> {
        Timestamp::from_second(secs).ok().map(Self)
    }

    /// Milliseconds since the epoch.
    pub fn from_millisecond(ms: i64) -> Option<Self> {
        Timestamp::from_millisecond(ms).ok().map(Self)
    }

    /// Nanoseconds since the epoch.
    pub fn from_nanosecond(ns: i128) -> Option<Self> {
        Timestamp::from_nanosecond(ns).ok().map(Self)
    }

    /// Nanoseconds since the epoch as `i64` (chrono's
    /// `from_timestamp_nanos`); every `i64` value is inside jiff's range.
    pub fn from_nanosecond_i64(ns: i64) -> Self {
        Self::from_nanosecond(i128::from(ns)).expect("every i64 nanosecond is in jiff's range")
    }

    /// A UTC calendar date and time, `None` when a field is out of range
    /// (chrono's `Utc.with_ymd_and_hms(..).single()`).
    pub fn from_ymd_hms(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
    ) -> Option<Self> {
        let date = civil::Date::new(
            i16::try_from(year).ok()?,
            i8::try_from(month).ok()?,
            i8::try_from(day).ok()?,
        )
        .ok()?;
        let time = civil::Time::new(
            i8::try_from(hour).ok()?,
            i8::try_from(minute).ok()?,
            i8::try_from(second).ok()?,
            0,
        )
        .ok()?;
        Self::from_civil(date.to_datetime(time))
    }

    /// A civil date and time read as UTC.
    pub fn from_civil(dt: civil::DateTime) -> Option<Self> {
        Offset::UTC.to_timestamp(dt).ok().map(Self)
    }

    /// Whole seconds since the epoch (floor before 1970, like chrono).
    pub fn as_second(self) -> i64 {
        let ns = self.as_nanosecond();
        i64::try_from(ns.div_euclid(NANOS_PER_SECOND)).expect("jiff range fits i64 seconds")
    }

    /// Milliseconds since the epoch (floor before 1970, like chrono).
    pub fn as_millisecond(self) -> i64 {
        let ns = self.as_nanosecond();
        i64::try_from(ns.div_euclid(1_000_000)).expect("jiff range fits i64 milliseconds")
    }

    /// Nanoseconds since the epoch.
    pub fn as_nanosecond(self) -> i128 {
        self.0.as_nanosecond()
    }

    /// Nanoseconds since the epoch as `i64`, `None` outside 1677–2262 (the
    /// range chrono's `timestamp_nanos_opt` covered).
    pub fn as_nanosecond_i64(self) -> Option<i64> {
        i64::try_from(self.as_nanosecond()).ok()
    }

    /// The sub-second part, always in `0..1_000_000_000` (chrono's
    /// `timestamp_subsec_nanos`).
    pub fn subsec_nanosecond(self) -> u32 {
        u32::try_from(self.as_nanosecond().rem_euclid(NANOS_PER_SECOND))
            .expect("sub-second nanos fit u32")
    }

    /// The wrapped jiff value.
    pub fn as_jiff(self) -> Timestamp {
        self.0
    }

    /// The civil date and time in UTC.
    pub fn to_civil(self) -> civil::DateTime {
        Offset::UTC.to_datetime(self.0)
    }

    /// UTC calendar year, in chrono's `i32`.
    pub fn year(self) -> i32 {
        i32::from(self.to_civil().year())
    }

    /// UTC month 1..=12, in chrono's `u32` (jiff's `i8` overflows in
    /// arithmetic such as `hour * 60`, so the accessors widen).
    pub fn month(self) -> u32 {
        self.civil_field(|c| c.month())
    }

    /// UTC day of month 1..=31.
    pub fn day(self) -> u32 {
        self.civil_field(|c| c.day())
    }

    /// UTC hour 0..=23.
    pub fn hour(self) -> u32 {
        self.civil_field(|c| c.hour())
    }

    /// UTC minute 0..=59.
    pub fn minute(self) -> u32 {
        self.civil_field(|c| c.minute())
    }

    /// UTC second 0..=59.
    pub fn second(self) -> u32 {
        self.civil_field(|c| c.second())
    }

    /// UTC day of year 1..=366.
    pub fn ordinal(self) -> u32 {
        u32::try_from(self.to_civil().day_of_year()).expect("day of year is positive")
    }

    fn civil_field(self, f: impl Fn(civil::DateTime) -> i8) -> u32 {
        u32::try_from(f(self.to_civil())).expect("calendar field is non-negative")
    }

    /// `self - earlier` as a signed duration.
    pub fn duration_since(self, earlier: Self) -> SignedDuration {
        self.0.duration_since(earlier.0)
    }

    /// `self + d`, `None` on overflow.
    pub fn checked_add(self, d: SignedDuration) -> Option<Self> {
        self.0.checked_add(d).ok().map(Self)
    }

    /// `self - d`, `None` on overflow.
    pub fn checked_sub(self, d: SignedDuration) -> Option<Self> {
        self.0.checked_sub(d).ok().map(Self)
    }

    /// strftime-style formatting in UTC. Matches chrono's `format` for
    /// years 0 through 9999. For a year below 0, jiff's `%Y` puts the sign
    /// inside the four-character width (`-027`, where chrono wrote `-0027`).
    pub fn strftime<'f>(&self, format: &'f str) -> impl fmt::Display + 'f {
        self.0.strftime(format)
    }

    /// `YYYY-MM-DDTHH:MM:SS[.frac]+00:00`, as chrono's `to_rfc3339()`.
    pub fn to_rfc3339(&self) -> String {
        self.text(TextForm::Rfc3339)
    }

    /// `YYYY-MM-DDTHH:MM:SS[.frac]Z`, as chrono's
    /// `to_rfc3339_opts(SecondsFormat::AutoSi, true)` and serde form.
    pub fn to_rfc3339_z(&self) -> String {
        self.text(TextForm::Zulu)
    }

    /// RFC 3339 with all nine fraction digits and `Z`, as chrono's
    /// `to_rfc3339_opts(SecondsFormat::Nanos, true)`.
    pub fn to_rfc3339_nanos(&self) -> String {
        let civil = self.to_civil();
        let mut out = String::with_capacity(30);
        write_date_time(&mut out, civil, 'T').expect("writing to a String cannot fail");
        out.push_str(&format!(".{:09}Z", civil.subsec_nanosecond()));
        out
    }

    fn text(&self, form: TextForm) -> String {
        let mut out = String::with_capacity(32);
        self.write_text(&mut out, form)
            .expect("writing to a String cannot fail");
        out
    }

    fn write_text<W: fmt::Write>(&self, w: &mut W, form: TextForm) -> fmt::Result {
        let civil = self.to_civil();
        let sep = match form {
            TextForm::Display => ' ',
            TextForm::Zulu | TextForm::Rfc3339 => 'T',
        };
        write_date_time(w, civil, sep)?;
        // The civil fraction is non-negative even before 1970, where
        // jiff's own `Timestamp::subsec_nanosecond` is negative.
        let nanos = civil.subsec_nanosecond();
        if nanos != 0 {
            if nanos % 1_000_000 == 0 {
                write!(w, ".{:03}", nanos / 1_000_000)?;
            } else if nanos % 1_000 == 0 {
                write!(w, ".{:06}", nanos / 1_000)?;
            } else {
                write!(w, ".{nanos:09}")?;
            }
        }
        match form {
            TextForm::Zulu => w.write_char('Z'),
            TextForm::Rfc3339 => w.write_str("+00:00"),
            TextForm::Display => w.write_str(" UTC"),
        }
    }
}

fn write_date_time<W: fmt::Write>(w: &mut W, civil: civil::DateTime, sep: char) -> fmt::Result {
    let year = civil.year();
    if (0..=9999).contains(&year) {
        write!(w, "{year:04}")?;
    } else {
        write!(w, "{year:+05}")?;
    }
    write!(
        w,
        "-{:02}-{:02}{sep}{:02}:{:02}:{:02}",
        civil.month(),
        civil.day(),
        civil.hour(),
        civil.minute(),
        civil.second()
    )
}

/// Integer readings of a [`SignedDuration`] in the shapes the chrono build
/// used (`num_milliseconds`, `num_microseconds`, `num_nanoseconds`,
/// `to_std`).
pub trait SignedDurationExt {
    /// Whole milliseconds, truncated toward zero and saturated to `i64`.
    fn as_millis_i64(&self) -> i64;
    /// Whole microseconds, `None` when they do not fit `i64`.
    fn as_micros_i64(&self) -> Option<i64>;
    /// Nanoseconds, `None` when they do not fit `i64`.
    fn as_nanos_i64(&self) -> Option<i64>;
    /// The unsigned std duration; `Err` when negative, as chrono's `to_std`.
    fn to_std(&self) -> Result<std::time::Duration, jiff::Error>;
}

impl SignedDurationExt for SignedDuration {
    fn as_millis_i64(&self) -> i64 {
        let ms = self.as_millis();
        i64::try_from(ms).unwrap_or(if ms < 0 { i64::MIN } else { i64::MAX })
    }
    fn as_micros_i64(&self) -> Option<i64> {
        i64::try_from(self.as_micros()).ok()
    }
    fn as_nanos_i64(&self) -> Option<i64> {
        i64::try_from(self.as_nanos()).ok()
    }
    fn to_std(&self) -> Result<std::time::Duration, jiff::Error> {
        std::time::Duration::try_from(*self)
    }
}

impl fmt::Display for UtcTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Width and precision flags are ignored, as chrono's Display did.
        self.write_text(f, TextForm::Display)
    }
}

impl fmt::Debug for UtcTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_text(f, TextForm::Zulu)
    }
}

/// Error from parsing a [`UtcTimestamp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UtcTimestampParseError(String);

impl fmt::Display for UtcTimestampParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid UTC timestamp: {}", self.0)
    }
}

impl std::error::Error for UtcTimestampParseError {}

/// Drop fraction digits past the ninth, as chrono's reader did.
fn truncate_fraction(s: &str) -> std::borrow::Cow<'_, str> {
    let bytes = s.as_bytes();
    // The fraction starts after the time's seconds; search past the date.
    let Some(dot) = bytes
        .iter()
        .enumerate()
        .skip(10)
        .find(|(_, b)| **b == b'.' || **b == b',')
        .map(|(i, _)| i)
    else {
        return std::borrow::Cow::Borrowed(s);
    };
    let digits = bytes[dot + 1..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    if digits <= 9 {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..dot + 1 + 9]);
    out.push_str(&s[dot + 1 + digits..]);
    std::borrow::Cow::Owned(out)
}

/// Undo the relaxations chrono's reader allowed and jiff's does not: a
/// leading `+` on a four-digit year, a four-digit year below 0 (`-YYYY`),
/// spaces before the offset, a trailing `UTC` (chrono's own `Display`
/// suffix) in place of `Z`, and trailing whitespace.
fn relax(s: &str) -> std::borrow::Cow<'_, str> {
    let mut t = s.trim();
    if t.len() > 5 && t.starts_with('+') && t.as_bytes()[5] == b'-' {
        t = &t[1..];
    }
    let mut out = String::with_capacity(t.len() + 1);
    let mut rest = t;
    // `get` rather than a byte slice: `len - 3` can fall inside a multi-byte
    // char, and parsing untrusted text must return an error, never panic.
    let utc_suffix = rest
        .len()
        .checked_sub(3)
        .and_then(|i| rest.get(i..))
        .is_some_and(|tail| tail.eq_ignore_ascii_case("utc"));
    if utc_suffix {
        rest = rest[..rest.len() - 3].trim_end();
    }
    // The offset starts at the first `+`, `-`, `Z` or `z` after the date.
    let offset_at = rest
        .char_indices()
        .skip(10)
        .find(|(_, c)| matches!(c, '+' | '-' | 'Z' | 'z'))
        .map(|(i, _)| i);
    match offset_at {
        Some(i) => {
            out.push_str(rest[..i].trim_end());
            out.push_str(&rest[i..]);
        }
        None => out.push_str(rest),
    }
    if utc_suffix {
        out.push('Z');
    }
    // chrono wrote a year below 0 as `-YYYY` (as `write_date_time` does);
    // jiff reads a signed year only as six digits, `-00YYYY`.
    let bytes = out.as_bytes();
    if bytes.len() > 5
        && bytes[0] == b'-'
        && bytes[1..5].iter().all(u8::is_ascii_digit)
        && bytes[5] == b'-'
    {
        out.insert_str(1, "00");
    }
    if out == s {
        std::borrow::Cow::Borrowed(s)
    } else {
        std::borrow::Cow::Owned(out)
    }
}

/// Reads RFC 3339 text the way chrono did, with one narrowing: chrono
/// skipped whitespace before each number inside the date, the time and the
/// offset (`2026 -11-04T14:30:00Z`), and this reader rejects it. Leading and
/// trailing whitespace, a space before the offset and a space in place of
/// `T` still read.
impl FromStr for UtcTimestamp {
    type Err = UtcTimestampParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let relaxed = relax(s);
        let text = truncate_fraction(&relaxed);
        Timestamp::from_str(&text)
            .map(Self)
            .map_err(|e| UtcTimestampParseError(format!("{s:?}: {e}")))
    }
}

impl Serialize for UtcTimestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text(TextForm::Zulu))
    }
}

impl<'de> Deserialize<'de> for UtcTimestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = UtcTimestamp;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an RFC 3339 timestamp string")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<UtcTimestamp, E> {
                v.parse().map_err(E::custom)
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

impl From<Timestamp> for UtcTimestamp {
    fn from(t: Timestamp) -> Self {
        Self(t)
    }
}

impl From<UtcTimestamp> for Timestamp {
    fn from(t: UtcTimestamp) -> Self {
        t.0
    }
}

impl TryFrom<SystemTime> for UtcTimestamp {
    type Error = jiff::Error;
    fn try_from(t: SystemTime) -> Result<Self, Self::Error> {
        Timestamp::try_from(t).map(Self)
    }
}

impl From<UtcTimestamp> for SystemTime {
    fn from(t: UtcTimestamp) -> Self {
        SystemTime::from(t.0)
    }
}

impl std::ops::Add<SignedDuration> for UtcTimestamp {
    type Output = Self;
    fn add(self, d: SignedDuration) -> Self {
        Self(self.0 + d)
    }
}

impl std::ops::Sub<SignedDuration> for UtcTimestamp {
    type Output = Self;
    fn sub(self, d: SignedDuration) -> Self {
        Self(self.0 - d)
    }
}

impl std::ops::AddAssign<SignedDuration> for UtcTimestamp {
    fn add_assign(&mut self, d: SignedDuration) {
        *self = *self + d;
    }
}

impl std::ops::SubAssign<SignedDuration> for UtcTimestamp {
    fn sub_assign(&mut self, d: SignedDuration) {
        *self = *self - d;
    }
}

impl std::ops::Sub<UtcTimestamp> for UtcTimestamp {
    type Output = SignedDuration;
    fn sub(self, earlier: UtcTimestamp) -> SignedDuration {
        self.duration_since(earlier)
    }
}

#[cfg(test)]
#[path = "time/tests/chrono_text_compat_tests.rs"]
mod chrono_text_compat_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn system_utc_clock_advances() {
        let clock = system_utc_clock();
        let first = clock();
        std::thread::sleep(std::time::Duration::from_millis(1));
        assert!(clock() > first);
    }
    #[test]
    fn slot_grid_origin_is_phase_zero() {
        let origin = slot_grid_origin();
        assert_eq!(origin.as_millisecond().rem_euclid(30_000), 0);
        assert_eq!(
            crate::slot::SlotParity::of(origin),
            crate::slot::SlotParity::Even
        );
    }
}
