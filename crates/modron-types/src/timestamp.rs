//! The instant type for persisted and wire timestamps (DEP-1: chrono was
//! replaced by jiff).
//!
//! The serde shape is pinned to what chrono's `DateTime<Utc>` wrote before the
//! swap, so no file or frame changes: RFC 3339 with `Z` for UTC, and the
//! fraction at the shortest exact width of 0, 3, 6 or 9 digits (chrono's
//! `SecondsFormat::AutoSi`). Deserialization accepts any RFC 3339 offset and
//! any fraction width, as chrono's did. `to_rfc3339` keeps chrono's `+00:00`
//! spelling for the string fields that code writes by hand (`last_used`,
//! `checked_at`, `saved_at`, `last_run`).
//! tests/timestamp_wire.rs pins every one of these shapes.

use std::ops::{Add, Deref, Sub};

use jiff::tz::TimeZone;
use jiff::SignedDuration;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A UTC instant. Wraps `jiff::Timestamp` so the serde and string shapes
/// cannot drift when a new field is added.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(jiff::Timestamp);

impl Timestamp {
    /// The current instant (chrono's `Utc::now`).
    pub fn now() -> Self {
        Self(jiff::Timestamp::now())
    }

    /// Wraps an existing jiff instant.
    pub fn from_jiff(ts: jiff::Timestamp) -> Self {
        Self(ts)
    }

    /// The underlying jiff instant.
    pub fn as_jiff(self) -> jiff::Timestamp {
        self.0
    }

    /// RFC 3339 with `+00:00` for UTC, the shape chrono's `to_rfc3339` wrote.
    pub fn to_rfc3339(self) -> String {
        format_rfc3339(self.0, false)
    }

    /// Parses any RFC 3339 instant (`Z` or an explicit offset). `None` on a
    /// malformed string, as chrono's `parse_from_rfc3339(..).ok()` returned.
    pub fn parse_rfc3339(text: &str) -> Option<Self> {
        parse_instant(text).map(Self)
    }

    /// strftime text of the instant in UTC, for the date labels and id
    /// stamps the code writes (chrono's `format(pattern)` on a UTC value).
    pub fn format(self, pattern: &str) -> String {
        self.0.to_zoned(TimeZone::UTC).strftime(pattern).to_string()
    }
}

impl Deref for Timestamp {
    type Target = jiff::Timestamp;

    fn deref(&self) -> &jiff::Timestamp {
        &self.0
    }
}

impl From<jiff::Timestamp> for Timestamp {
    fn from(ts: jiff::Timestamp) -> Self {
        Self(ts)
    }
}

impl Add<SignedDuration> for Timestamp {
    type Output = Timestamp;

    fn add(self, rhs: SignedDuration) -> Timestamp {
        Self(self.0 + rhs)
    }
}

impl Sub<SignedDuration> for Timestamp {
    type Output = Timestamp;

    fn sub(self, rhs: SignedDuration) -> Timestamp {
        Self(self.0 - rhs)
    }
}

impl Sub<Timestamp> for Timestamp {
    type Output = SignedDuration;

    /// `self - other`, as chrono's `signed_duration_since`.
    fn sub(self, rhs: Timestamp) -> SignedDuration {
        self.0.duration_since(rhs.0)
    }
}

/// Whole days in `duration`, truncated toward zero: chrono's `num_days()`.
pub fn whole_days(duration: SignedDuration) -> i64 {
    duration.as_secs() / 86_400
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format_rfc3339(self.0, true))
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_instant(&text)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid timestamp {text:?}")))
    }
}

/// Parses an RFC 3339 instant. jiff reads at most nine fraction digits; chrono
/// truncated longer fractions, so the excess is cut before the retry. chrono
/// never writes more than nine, so this only matters for hand-edited or
/// foreign text.
fn parse_instant(text: &str) -> Option<jiff::Timestamp> {
    if let Ok(ts) = text.parse::<jiff::Timestamp>() {
        return Some(ts);
    }
    let dot = text.find('.')?;
    let frac_start = dot + 1;
    let frac_end = text[frac_start..]
        .find(|c: char| !c.is_ascii_digit())
        .map_or(text.len(), |i| frac_start + i);
    if frac_end - frac_start <= 9 {
        return None;
    }
    let cut = format!(
        "{}{}{}",
        &text[..frac_start],
        &text[frac_start..frac_start + 9],
        &text[frac_end..]
    );
    cut.parse::<jiff::Timestamp>().ok()
}

/// Writes `ts` the way chrono's `write_rfc3339` with `SecondsFormat::AutoSi`
/// did for a UTC instant. `use_z` picks `Z` (serde) or `+00:00` (to_rfc3339).
fn format_rfc3339(ts: jiff::Timestamp, use_z: bool) -> String {
    let dt = ts.to_zoned(TimeZone::UTC).datetime();
    let year = dt.year() as i32;
    let nano = dt.subsec_nanosecond() as u32;
    let year_text = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!("{year:+05}")
    };
    let fraction = if nano == 0 {
        String::new()
    } else if nano.is_multiple_of(1_000_000) {
        format!(".{:03}", nano / 1_000_000)
    } else if nano.is_multiple_of(1_000) {
        format!(".{:06}", nano / 1_000)
    } else {
        format!(".{nano:09}")
    };
    let zone = if use_z { "Z" } else { "+00:00" };
    format!(
        "{year_text}-{:02}-{:02}T{:02}:{:02}:{:02}{fraction}{zone}",
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
    )
}
