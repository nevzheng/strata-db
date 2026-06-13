//! Temporal conversions for [`Value::Date`](crate::storage::types::Value::Date)
//! and [`Value::Timestamp`](crate::storage::types::Value::Timestamp).
//!
//! A `Value::Date` is a count of days since the Unix epoch
//! (`1970-01-01`, UTC) — no time, no timezone. A `Value::Timestamp` is
//! an absolute instant: microseconds since that same epoch, UTC,
//! following SQL `TIMESTAMP WITH TIME ZONE` semantics — the offset in a
//! literal is honored on input and normalized to UTC, a literal with no
//! offset is taken as UTC (we have no session time zone), and output
//! renders as UTC (`+00:00`).
//!
//! Both keep a plain integer on-disk form (`i32` days / `i64` micros) and
//! lean on `chrono` for the calendar math: parsing, leap-year-aware
//! validation, and formatting.

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};

/// The Unix epoch as a civil date — the zero point for `Value::Date`.
fn epoch() -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).expect("epoch is a valid date")
}

/// Parse a SQL `DATE` body — `YYYY-MM-DD` — into days since the epoch.
/// `chrono` validates the shape and the calendar (months, day-of-month,
/// leap years), so an invalid literal fails here at bind time rather
/// than wrapping silently.
pub fn parse_date(s: &str) -> Result<i32, String> {
    let date = s
        .parse::<NaiveDate>()
        .map_err(|_| format!("invalid DATE literal: {s:?}"))?;
    Ok((date - epoch()).num_days() as i32)
}

/// Render a day count as `YYYY-MM-DD`.
pub fn format_date(days: i32) -> String {
    let date = epoch() + chrono::Duration::days(days as i64);
    date.format("%Y-%m-%d").to_string()
}

/// Parse a SQL `TIMESTAMP WITH TIME ZONE` body into microseconds since
/// the epoch, UTC. An explicit offset (`+02`, `+02:00`, `Z`) is honored
/// and normalized to UTC; a literal with no offset is interpreted as UTC.
/// Both space- and `T`-separated forms are accepted, with optional
/// fractional seconds.
pub fn parse_timestamptz(s: &str) -> Result<i64, String> {
    let s = s.trim();
    let invalid = || format!("invalid TIMESTAMP literal: {s:?}");

    // Offset-aware forms first: honor the offset, normalize to UTC.
    // `%#z` accepts `+02`, `+0200`, and `+02:00`; RFC 3339 covers `Z`.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.timestamp_micros());
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f%#z",
        "%Y-%m-%dT%H:%M:%S%.f%#z",
        "%Y-%m-%d %H:%M:%S%#z",
        "%Y-%m-%dT%H:%M:%S%#z",
    ] {
        if let Ok(dt) = DateTime::parse_from_str(s, fmt) {
            return Ok(dt.timestamp_micros());
        }
    }
    // No offset: interpret the civil time as UTC.
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(ndt.and_utc().timestamp_micros());
        }
    }
    Err(invalid())
}

/// Render microseconds-since-epoch as a UTC timestamp,
/// `YYYY-MM-DD HH:MM:SS+00:00`. Sub-second precision is preserved in
/// storage but not shown.
pub fn format_timestamptz(micros: i64) -> String {
    DateTime::<Utc>::from_timestamp_micros(micros)
        .expect("timestamp micros in range")
        .format("%Y-%m-%d %H:%M:%S%:z")
        .to_string()
}

/// Parse a SQL `TIME` body — `HH:MM:SS[.ffffff]` — into microseconds
/// since midnight (`TIME WITHOUT TIME ZONE`).
pub fn parse_time(s: &str) -> Result<i64, String> {
    let s = s.trim();
    let invalid = || format!("invalid TIME literal: {s:?}");
    let t = NaiveTime::parse_from_str(s, "%H:%M:%S%.f")
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M:%S"))
        .map_err(|_| invalid())?;
    Ok(t.num_seconds_from_midnight() as i64 * 1_000_000 + t.nanosecond() as i64 / 1_000)
}

/// Render microseconds-since-midnight as `HH:MM:SS`. Sub-second precision
/// is preserved in storage but not shown.
pub fn format_time(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_zero() {
        assert_eq!(parse_date("1970-01-01").unwrap(), 0);
        assert_eq!(format_date(0), "1970-01-01");
    }

    #[test]
    fn known_anchor_date() {
        // 30 years (7 leap days) from the epoch to 2000-01-01.
        assert_eq!(parse_date("2000-01-01").unwrap(), 10957);
        assert_eq!(format_date(10957), "2000-01-01");
    }

    #[test]
    fn parse_and_format_roundtrip() {
        for s in ["1970-01-01", "2000-01-01", "2026-06-13", "2024-02-29"] {
            assert_eq!(format_date(parse_date(s).unwrap()), s);
        }
    }

    #[test]
    fn parse_rejects_invalid() {
        for s in [
            "2026-13-01", // month out of range
            "2026-00-10", // month zero
            "2026-06-31", // June has 30 days
            "2023-02-29", // not a leap year
            "2026/06/13", // wrong separator
            "2026-06",    // too few parts
            "garbage",
        ] {
            assert!(parse_date(s).is_err(), "expected {s:?} to be rejected");
        }
    }

    #[test]
    fn leap_day_validity() {
        assert!(parse_date("2024-02-29").is_ok());
        assert!(parse_date("2000-02-29").is_ok()); // divisible by 400
        assert!(parse_date("1900-02-29").is_err()); // divisible by 100, not 400
    }

    #[test]
    fn timestamptz_epoch_is_zero() {
        assert_eq!(parse_timestamptz("1970-01-01 00:00:00").unwrap(), 0);
        assert_eq!(format_timestamptz(0), "1970-01-01 00:00:00+00:00");
    }

    #[test]
    fn timestamptz_offset_normalizes_to_utc() {
        // +02:00 means the same instant is two hours earlier in UTC.
        let with_offset = parse_timestamptz("2026-06-13 14:30:00+02:00").unwrap();
        let in_utc = parse_timestamptz("2026-06-13 12:30:00+00:00").unwrap();
        assert_eq!(with_offset, in_utc);
    }

    #[test]
    fn timestamptz_accepts_short_offset_and_z() {
        let short = parse_timestamptz("2026-06-13 14:30:00+02").unwrap();
        let utc = parse_timestamptz("2026-06-13 12:30:00+00").unwrap();
        let zulu = parse_timestamptz("2026-06-13T12:30:00Z").unwrap();
        assert_eq!(short, utc);
        assert_eq!(utc, zulu);
    }

    #[test]
    fn timestamptz_no_offset_is_utc() {
        let naive = parse_timestamptz("2026-06-13 12:30:00").unwrap();
        let utc = parse_timestamptz("2026-06-13 12:30:00+00").unwrap();
        assert_eq!(naive, utc);
    }

    #[test]
    fn timestamptz_roundtrip_renders_utc() {
        let micros = parse_timestamptz("2026-06-13 14:30:00+00").unwrap();
        assert_eq!(format_timestamptz(micros), "2026-06-13 14:30:00+00:00");
    }

    #[test]
    fn timestamptz_rejects_invalid() {
        assert!(parse_timestamptz("not a timestamp").is_err());
        assert!(parse_timestamptz("2026-13-01 00:00:00+00").is_err());
    }

    #[test]
    fn time_parse_and_format() {
        assert_eq!(parse_time("00:00:00").unwrap(), 0);
        assert_eq!(parse_time("14:30:00").unwrap(), 52_200_000_000);
        assert_eq!(format_time(parse_time("23:59:59").unwrap()), "23:59:59");
        assert!(parse_time("nope").is_err());
    }
}
