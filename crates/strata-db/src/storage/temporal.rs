//! Calendar-date conversions for [`Value::Date`](crate::storage::types::Value::Date).
//!
//! A `Value::Date` is a count of days since the Unix epoch
//! (`1970-01-01`, UTC) — no time, no timezone. We keep the on-disk form
//! a plain `i32` day offset and lean on [`chrono::NaiveDate`] for the
//! calendar math: parsing, leap-year-aware validation, and formatting.

use chrono::NaiveDate;

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
}
