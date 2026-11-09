//! UTC calendar months as the monthly-limit feature labels them.
//!
//! A month is both the budget period of a key's monthly limit and the
//! grouping label of the usage aggregates. Its seven-byte `YYYY-MM` bucket
//! is the only representation that crosses a module boundary, so the
//! in-memory flag store compares months without parsing dates and the
//! aggregate queries build their `created_at` range from the same value the
//! flag store keys on. Day and time arithmetic is UTC throughout, matching
//! the canonical `YYYY-MM-DD HH:MM:SS` text the `created_at` column stores:
//! the stored string compares correctly against the bounds below, which is
//! what lets the range predicate use the time-ordered index.

use time::OffsetDateTime;

/// Length of the `YYYY-MM` label every month renders to.
pub const LABEL_LEN: usize = 7;

/// A UTC calendar month.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Month {
    year: i32,
    /// 1 through 12.
    number: u8,
}

impl Month {
    /// The month the current UTC instant falls into.
    pub fn current() -> Self {
        let now = OffsetDateTime::now_utc();
        Month {
            year: now.year(),
            number: u8::from(now.month()),
        }
    }

    /// A month from its calendar parts; `None` when `number` is outside
    /// 1 through 12.
    pub fn from_parts(year: i32, number: u8) -> Option<Self> {
        (1..=12).contains(&number).then_some(Month { year, number })
    }

    /// The seven-byte `YYYY-MM` label, the value the in-memory flag store
    /// keys its entries on.
    pub fn bucket(&self) -> [u8; LABEL_LEN] {
        // `{:04}` always yields at least four characters, so the 7-byte
        // slice below is in range for every possible year.
        let label = format!("{:04}-{:02}", self.year, self.number);
        let mut bucket = [0u8; LABEL_LEN];
        bucket.copy_from_slice(&label.as_bytes()[..LABEL_LEN]);
        bucket
    }

    /// Inclusive lower bound of the month, in the canonical stored
    /// timestamp format.
    pub fn start(&self) -> String {
        format!("{:04}-{:02}-01 00:00:00", self.year, self.number)
    }

    /// Exclusive upper bound: the first instant of the following month.
    pub fn end(&self) -> String {
        let next = if self.number == 12 {
            Month {
                year: self.year.saturating_add(1),
                number: 1,
            }
        } else {
            Month {
                year: self.year,
                number: self.number + 1,
            }
        };
        next.start()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_month_bounds_the_current_instant() {
        let month = Month::current();
        let now = crate::db::timestamp::now_utc();
        assert!(
            month.start() <= now && now < month.end(),
            "{now} must fall inside [{}, {})",
            month.start(),
            month.end()
        );
    }

    #[test]
    fn buckets_are_seven_byte_year_month_labels() {
        assert_eq!(&Month::from_parts(2026, 11).unwrap().bucket(), b"2026-11");
        assert_eq!(&Month::from_parts(2026, 1).unwrap().bucket(), b"2026-01");
        assert_eq!(&Month::from_parts(999, 12).unwrap().bucket(), b"0999-12");
        assert_eq!(
            Month::from_parts(2026, 11).unwrap().bucket().len(),
            LABEL_LEN
        );
    }

    #[test]
    fn a_december_month_ends_in_the_next_january() {
        let december = Month::from_parts(2026, 12).unwrap();
        assert_eq!(december.start(), "2026-12-01 00:00:00");
        assert_eq!(december.end(), "2027-01-01 00:00:00");

        let november = Month::from_parts(2026, 11).unwrap();
        assert_eq!(november.start(), "2026-11-01 00:00:00");
        assert_eq!(november.end(), "2026-12-01 00:00:00");
    }

    #[test]
    fn parts_outside_the_calendar_are_rejected() {
        assert!(Month::from_parts(2026, 0).is_none());
        assert!(Month::from_parts(2026, 13).is_none());
        assert!(Month::from_parts(2026, 12).is_some());
        assert!(Month::from_parts(2026, 1).is_some());
    }
}
