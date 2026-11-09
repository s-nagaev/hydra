//! Monthly spending limits on client keys, denominated in money.
//!
//! The stored unit is an integer number of micro-USD per key row
//! (`api_keys.monthly_limit_micro_usd`); NULL means unlimited, which is the
//! same state as a key that was never given a limit. The admin-facing shape
//! is decimal USD as a JSON string or number, converted by the exact
//! decimal rules the channel prices use (see [`crate::prices`]): an amount
//! with significance finer than 1e-6 USD has no micro-USD representation and
//! is rejected rather than rounded, and an amount above [`MAX_MICRO_USD`]
//! (one billion USD) is outside the range a limit may occupy. No float
//! takes part in any conversion.
//!
//! Between the two bounds the value is a plain non-negative integer, so a
//! stored limit is always meaningful and never needs re-validation when it
//! is read back.

use serde_json::Value;

use crate::prices::{self, UsdAmountError};

/// Upper bound of a stored limit: 1e15 micro-USD, one billion USD. Far
/// above any budget a self-hosted router plausibly carries, and far below
/// the i64 range the column decodes into.
pub const MAX_MICRO_USD: i64 = 1_000_000_000_000_000;

/// Why a monthly limit cannot be stored.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LimitError {
    #[error("monthly_limit_usd must be a non-negative decimal USD amount, got \"{value}\"")]
    BadUsd { value: String },
    #[error("monthly_limit_usd \"{value}\" is finer than 1e-6 USD and cannot be stored exactly")]
    NotExact { value: String },
    #[error("monthly_limit_usd \"{value}\" is above the $1B maximum")]
    TooLarge { value: String },
    #[error("monthly_limit_micro_usd {value} is outside 0..={max}")]
    OutOfRange { value: i64, max: i64 },
}

/// Converts the external JSON shape of a limit into integer micro-USD. A
/// string or a number is read as decimal USD; any other JSON type is a bad
/// amount, never a coerced one.
pub fn parse_usd_value(value: &Value) -> Result<i64, LimitError> {
    let raw = match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    };
    parse_usd_str(&raw)
}

/// Converts a decimal USD text, as typed into an admin form field, into
/// integer micro-USD.
pub fn parse_usd_str(raw: &str) -> Result<i64, LimitError> {
    let text = raw.trim().to_string();
    let micro = prices::parse_usd(raw).map_err(|error| match error {
        UsdAmountError::NotExact => LimitError::NotExact {
            value: text.clone(),
        },
        // Only an amount too large to hold as micro-USD in a u64 reaches
        // this branch, which is far past the limit's own bound.
        UsdAmountError::Overflow => LimitError::TooLarge {
            value: text.clone(),
        },
        UsdAmountError::Format => LimitError::BadUsd {
            value: text.clone(),
        },
    })?;
    let micro = i64::try_from(micro).map_err(|_| LimitError::TooLarge {
        value: text.clone(),
    })?;
    match validate_micro(micro) {
        Ok(()) => Ok(micro),
        Err(_) => Err(LimitError::TooLarge { value: text }),
    }
}

/// Checks a limit that is already an integer micro-USD value: non-negative
/// and inside [`MAX_MICRO_USD`]. This is the domain-layer guard, so a limit
/// that reaches storage has passed it exactly once.
pub fn validate_micro(micro: i64) -> Result<(), LimitError> {
    if !(0..=MAX_MICRO_USD).contains(&micro) {
        return Err(LimitError::OutOfRange {
            value: micro,
            max: MAX_MICRO_USD,
        });
    }
    Ok(())
}

/// Renders a stored limit as decimal USD: `50_000_000` becomes `50.00`,
/// `1_500_000` becomes `1.50` and `1` becomes `0.000001`. Negative values
/// only exist if the column was edited out of band; they render as `0.00`
/// rather than as a nonsensical amount.
pub fn format_usd(micro: i64) -> String {
    prices::format_usd(u64::try_from(micro).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decimal_strings_and_numbers_convert_to_micro_usd() {
        assert_eq!(parse_usd_value(&json!("50.00")).unwrap(), 50_000_000);
        assert_eq!(parse_usd_value(&json!("0.01")).unwrap(), 10_000);
        assert_eq!(parse_usd_value(&json!(5)).unwrap(), 5_000_000);
        assert_eq!(parse_usd_value(&json!(0.25)).unwrap(), 250_000);
        assert_eq!(parse_usd_value(&json!("0")).unwrap(), 0);
        assert_eq!(parse_usd_value(&json!(0)).unwrap(), 0);
        assert_eq!(parse_usd_value(&json!("  12.5  ")).unwrap(), 12_500_000);
    }

    #[test]
    fn the_boundary_amounts_round_trip_exactly() {
        assert_eq!(parse_usd_str("0.000001").unwrap(), 1);
        let max = parse_usd_value(&json!("1000000000")).unwrap();
        assert_eq!(max, MAX_MICRO_USD);
        assert_eq!(validate_micro(max), Ok(()));
        assert_eq!(format_usd(max), "1000000000.00");
        assert_eq!(parse_usd_str(&format_usd(max)).unwrap(), max);
    }

    #[test]
    fn finer_than_a_micro_usd_is_rejected_not_rounded() {
        let error = parse_usd_value(&json!("0.0000005")).unwrap_err();
        assert!(matches!(error, LimitError::NotExact { .. }), "{error}");
        // A JSON number arrives through serde_json's canonical rendering, so
        // an amount this small reaches the parser in exponent form; it is
        // refused as malformed rather than rounded. Refused either way is
        // the property that matters.
        let error = parse_usd_value(&json!(0.0000005)).unwrap_err();
        assert!(matches!(error, LimitError::BadUsd { .. }), "{error}");
        // Zero padding past the sixth place keeps the amount exact.
        assert_eq!(parse_usd_value(&json!("2.5000000")).unwrap(), 2_500_000);
    }

    #[test]
    fn negative_and_malformed_amounts_are_rejected() {
        for amount in [
            json!(-1),
            json!("-1"),
            json!(""),
            json!(null),
            json!(true),
            json!([1]),
            json!({ "usd": 1 }),
            json!("1e3"),
            json!("2.5.1"),
        ] {
            let error = parse_usd_value(&amount).unwrap_err();
            assert!(
                matches!(error, LimitError::BadUsd { .. }),
                "{amount} must be rejected as a bad amount, got {error}"
            );
        }
    }

    #[test]
    fn amounts_above_the_bound_are_rejected() {
        let error = parse_usd_value(&json!("1000000001")).unwrap_err();
        assert!(matches!(error, LimitError::TooLarge { .. }), "{error}");
        let error = parse_usd_value(&json!("999999999999999999999")).unwrap_err();
        assert!(matches!(error, LimitError::TooLarge { .. }), "{error}");
    }

    #[test]
    fn validate_micro_rejects_negative_and_oversized_integers() {
        let error = validate_micro(-1).unwrap_err();
        assert!(
            matches!(
                error,
                LimitError::OutOfRange {
                    value: -1,
                    max: MAX_MICRO_USD
                }
            ),
            "{error}"
        );
        let error = validate_micro(MAX_MICRO_USD + 1).unwrap_err();
        assert!(matches!(error, LimitError::OutOfRange { .. }), "{error}");
        assert_eq!(validate_micro(0), Ok(()));
        assert_eq!(validate_micro(MAX_MICRO_USD), Ok(()));
    }

    #[test]
    fn formatting_keeps_at_least_two_decimals_and_stays_exact() {
        assert_eq!(format_usd(50_000_000), "50.00");
        assert_eq!(format_usd(0), "0.00");
        assert_eq!(format_usd(1_500_000), "1.50");
        assert_eq!(format_usd(1), "0.000001");
        assert_eq!(format_usd(-5), "0.00");
    }
}
