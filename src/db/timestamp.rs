//! Canonical UTC timestamp text shared by both database backends.
//!
//! Timestamps are stored as TEXT in the `YYYY-MM-DD HH:MM:SS` UTC format on
//! SQLite and PostgreSQL alike. The admin log filters string-compare their
//! normalized bounds against `created_at`, so the format must stay exactly
//! this wide on both backends.

use time::OffsetDateTime;

/// Current UTC time in the canonical stored format.
pub fn now_utc() -> String {
    format_utc(OffsetDateTime::now_utc())
}

/// Formats an instant in the canonical stored format. Shared by the filter
/// normalization in the admin logs handler, so user-supplied bounds and
/// stored values can never drift into different shapes.
pub fn format_utc(moment: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        moment.year(),
        u8::from(moment.month()),
        moment.day(),
        moment.hour(),
        moment.minute(),
        moment.second()
    )
}

#[cfg(test)]
mod tests {
    use time::{Date, Month, OffsetDateTime};

    use super::*;

    fn at(year: i32, month: Month, day: u8, hour: u8, minute: u8, second: u8) -> OffsetDateTime {
        Date::from_calendar_date(year, month, day)
            .unwrap()
            .with_hms(hour, minute, second)
            .unwrap()
            .assume_utc()
    }

    #[test]
    fn format_produces_the_canonical_stored_shape() {
        let moment = at(2026, Month::November, 8, 9, 7, 5);
        assert_eq!(format_utc(moment), "2026-11-08 09:07:05");
    }

    #[test]
    fn format_pads_single_digit_components() {
        let moment = at(2026, Month::January, 2, 3, 4, 5);
        assert_eq!(format_utc(moment), "2026-01-02 03:04:05");
    }

    #[test]
    fn now_utc_matches_the_stored_shape() {
        let raw = now_utc();
        assert_eq!(raw.len(), 19);
        for (index, expected) in [(4, b'-'), (7, b'-'), (10, b' '), (13, b':'), (16, b':')] {
            assert_eq!(raw.as_bytes()[index], expected, "in {raw}");
        }
        for (index, byte) in raw.bytes().enumerate() {
            if !matches!(index, 4 | 7 | 10 | 13 | 16) {
                assert!(byte.is_ascii_digit(), "in {raw}");
            }
        }
    }
}
