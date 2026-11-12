//! What one model costs on one channel.
//!
//! Prices belong to the channel, not to the model: the same model name can
//! cost differently behind different upstreams. Every dimension of a price
//! entry is an integer number of micro-USD per 1M tokens, so `2_500_000`
//! means 2.50 USD per 1M tokens. No floating point value takes part in
//! storage, conversion or arithmetic: the admin-facing shape carries
//! decimal USD as strings or numbers, and [`parse_usd`] converts it with
//! exact decimal arithmetic, rejecting amounts that have no micro-USD
//! representation instead of rounding them.
//!
//! A [`ModelPrices`] value can only be built through a validating
//! constructor, so an instance in hand is always well formed: five
//! dimensions per model, every amount an exact non-negative integer.
//!
//! Five dimensions, four required and one optional. `input`, `output`,
//! `cached_input` and `cached_output` are the OpenAI bill: prompt tokens at
//! the full rate, completions, prompt tokens served from the upstream cache
//! and cached completions. `cache_write_input` is the rate for Anthropic
//! cache WRITES (micro-USD per 1M tokens for
//! `cache_creation_input_tokens`), the dimension the write tokens are billed
//! at: Anthropic asks more for a write than for a plain input token, and
//! that difference lives in the number the admin enters here rather than in
//! a hard-coded multiple of `input`, so the table can carry the provider's
//! real figure. A model that reports no cache writes never charges this
//! dimension, which is why it may be omitted: an absent (or explicit `null`)
//! `cache_write_input` means "no write price set" and contributes zero,
//! exactly like an unpriced model costs zero. Entries stored before the
//! dimension existed carry the other four and keep loading, with the write
//! dimension reading as zero.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The name of the one optional price dimension: prompt tokens the upstream
/// wrote into its cache. Absent or null means no write price is set, which
/// contributes nothing to a cost rather than inventing a rate.
pub const CACHE_WRITE_INPUT: &str = "cache_write_input";

/// Every price dimension, in the order the admin UI asks for them. All but
/// [`CACHE_WRITE_INPUT`] are required on a stored entry.
pub const DIMENSIONS: [&str; 5] = [
    "input",
    "output",
    "cached_input",
    "cached_output",
    CACHE_WRITE_INPUT,
];

/// `true` for the one dimension an entry may leave out.
fn is_optional_dimension(dimension: &str) -> bool {
    dimension == CACHE_WRITE_INPUT
}

/// Micro-USD per 1M tokens for one model on one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    pub input: u64,
    pub output: u64,
    pub cached_input: u64,
    pub cached_output: u64,
    /// The rate Anthropic cache-write tokens (`cache_creation_input_tokens`)
    /// are billed at, independent of `input`: a write costs more than a
    /// plain input token, and the real figure comes from here. Zero when the
    /// entry names no write rate, which is also what a stored entry written
    /// before the dimension existed reads as, so a four-dimension table
    /// keeps loading unchanged.
    #[serde(default)]
    pub cache_write_input: u64,
}

/// The price table of one channel, keyed by model name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelPrices(BTreeMap<String, ModelPrice>);

impl ModelPrices {
    /// Reads the stored column shape: model name to integer micro-USD
    /// dimensions, the write dimension optional. Corrupt or non-integer
    /// content is an error, never a silently repaired table.
    pub fn from_stored_json(raw: &str) -> Result<Self, ModelPricesError> {
        let table: BTreeMap<String, ModelPrice> =
            serde_json::from_str(raw).map_err(|error| ModelPricesError::Stored {
                detail: error.to_string(),
            })?;
        Self::from_table(table)
    }

    /// Reads the external admin shape: model name to USD amounts per 1M
    /// tokens, each an exact decimal string or number. The four OpenAI
    /// dimensions are required; `cache_write_input` may be omitted or null,
    /// which leaves the write price unset.
    pub fn from_usd_json(value: &Value) -> Result<Self, ModelPricesError> {
        let Value::Object(entries) = value else {
            return Err(ModelPricesError::NotAnObject);
        };
        let mut table = BTreeMap::new();
        for (model, entry) in entries {
            if model.trim().is_empty() {
                return Err(ModelPricesError::EmptyModelName);
            }
            let Value::Object(dimensions) = entry else {
                return Err(ModelPricesError::BadEntry {
                    model: model.clone(),
                });
            };
            for dimension in dimensions.keys() {
                if !DIMENSIONS.contains(&dimension.as_str()) {
                    return Err(ModelPricesError::UnknownDimension {
                        model: model.clone(),
                        dimension: dimension.clone(),
                    });
                }
            }
            let mut micro = [0u64; DIMENSIONS.len()];
            for (index, dimension) in DIMENSIONS.iter().enumerate() {
                let amount = match dimensions.get(*dimension) {
                    Some(amount) => amount,
                    // Only the write dimension may be left out: an entry
                    // without it prices no cache writes.
                    None if is_optional_dimension(dimension) => continue,
                    None => {
                        return Err(ModelPricesError::MissingDimension {
                            model: model.clone(),
                            dimension,
                        })
                    }
                };
                // The same dimension also takes an explicit null as "no
                // price set", so a client can clear it without dropping the
                // key from the object.
                if is_optional_dimension(dimension) && amount.is_null() {
                    continue;
                }
                micro[index] = parse_usd_amount(amount, model, dimension)?;
            }
            table.insert(
                model.clone(),
                ModelPrice {
                    input: micro[0],
                    output: micro[1],
                    cached_input: micro[2],
                    cached_output: micro[3],
                    cache_write_input: micro[4],
                },
            );
        }
        Self::from_table(table)
    }

    /// Renders the stored column shape, the same integer JSON the API
    /// returns.
    pub fn to_stored_json(&self) -> String {
        serde_json::to_string(&self.0)
            .expect("map of integer price entries serializes without failure")
    }

    /// The price entry of one model.
    pub fn get(&self, model: &str) -> Option<&ModelPrice> {
        self.0.get(model)
    }

    /// Every model with its price entry, ordered by model name.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &ModelPrice)> {
        self.0.iter()
    }

    /// Number of priced models.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// `true` when no model is priced.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every priced model must be part of the channel's whitelist; a
    /// channel without a whitelist serves every upstream model, so there is
    /// nothing to check against.
    pub fn validate_against_allowed(
        &self,
        allowed: Option<&[String]>,
    ) -> Result<(), ModelPricesError> {
        let Some(allowed) = allowed.filter(|models| !models.is_empty()) else {
            return Ok(());
        };
        let outside: Vec<&str> = self
            .0
            .keys()
            .filter(|model| !allowed.iter().any(|allowed_model| &allowed_model == model))
            .map(String::as_str)
            .collect();
        if outside.is_empty() {
            Ok(())
        } else {
            Err(ModelPricesError::OutsideAllowedModels {
                models: outside.join(", "),
            })
        }
    }

    fn from_table(table: BTreeMap<String, ModelPrice>) -> Result<Self, ModelPricesError> {
        Self::from_entries(table)
    }

    /// Builds a table from already-parsed entries, rejecting blank model
    /// names. Callers that read a human-written line format convert each
    /// amount with [`parse_usd`] first.
    pub fn from_entries(
        entries: impl IntoIterator<Item = (String, ModelPrice)>,
    ) -> Result<Self, ModelPricesError> {
        let mut table = BTreeMap::new();
        for (model, price) in entries {
            if model.trim().is_empty() {
                return Err(ModelPricesError::EmptyModelName);
            }
            table.insert(model, price);
        }
        Ok(Self(table))
    }
}

/// `true` when the whitelist resolves to a finite model set and every model
/// in it carries a complete price entry. This is the precondition for
/// monthly key limits: a channel serving every upstream model has no fixed
/// model set to price.
pub fn all_models_priced(allowed: Option<&[String]>, prices: Option<&ModelPrices>) -> bool {
    let Some(allowed) = allowed.filter(|models| !models.is_empty()) else {
        return false;
    };
    let Some(prices) = prices else {
        return false;
    };
    allowed
        .iter()
        .all(|model| prices.get(model.as_str()).is_some())
}

/// Converts a decimal USD amount per 1M tokens into integer micro-USD.
///
/// The text is accepted when it has an exact micro-USD representation, so
/// `2.5` becomes `2_500_000` and `0.0000005` is rejected rather than
/// rounded to `0` or `1`.
pub fn parse_usd(raw: &str) -> Result<u64, UsdAmountError> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(UsdAmountError::Format);
    }
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (text, ""),
    };
    if whole.is_empty() && fraction.is_empty() {
        return Err(UsdAmountError::Format);
    }
    let digits = |part: &str| part.chars().all(|c| c.is_ascii_digit());
    if !digits(whole) || !digits(fraction) {
        return Err(UsdAmountError::Format);
    }
    // Digits past the sixth cannot be represented in micro-USD unless they
    // are all zero.
    let (kept, dropped) = fraction.split_at(fraction.len().min(6));
    if dropped.chars().any(|c| c != '0') {
        return Err(UsdAmountError::NotExact);
    }
    let whole_value: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().map_err(|_| UsdAmountError::Overflow)?
    };
    let micro_value: u64 = format!("{kept:0<6}")
        .parse()
        .map_err(|_| UsdAmountError::Format)?;
    whole_value
        .checked_mul(1_000_000)
        .and_then(|value| value.checked_add(micro_value))
        .ok_or(UsdAmountError::Overflow)
}

/// Renders micro-USD as decimal USD: `2_500_000` becomes `2.50`, `1`
/// becomes `0.000001`. Shorter amounts keep at least two decimals; longer
/// ones keep every significant digit, so the round trip through
/// [`parse_usd`] is exact.
pub fn format_usd(micro: u64) -> String {
    let whole = micro / 1_000_000;
    let fraction = micro % 1_000_000;
    if fraction == 0 {
        return format!("{whole}.00");
    }
    let digits = format!("{fraction:06}");
    let trimmed = digits.trim_end_matches('0');
    let padded = if trimmed.len() < 2 {
        format!("{trimmed:0<2}")
    } else {
        trimmed.to_string()
    };
    format!("{whole}.{padded}")
}

/// Why a decimal USD amount cannot be stored as micro-USD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsdAmountError {
    /// Not a plain non-negative decimal (sign, exponent or stray characters).
    Format,
    /// More than six decimal places of significance: no exact micro-USD.
    NotExact,
    /// Too large for a u64 micro-USD value.
    Overflow,
}

/// Everything that can be wrong with a price table.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelPricesError {
    #[error("stored model_prices is not valid price JSON: {detail}")]
    Stored { detail: String },
    #[error("model_prices must be an object mapping model names to prices")]
    NotAnObject,
    #[error(
        "model_prices entry for \"{model}\" must be an object with input, output, cached_input \
         and cached_output, plus an optional cache_write_input"
    )]
    BadEntry { model: String },
    #[error("model_prices entry for \"{model}\" is missing \"{dimension}\"")]
    MissingDimension {
        model: String,
        dimension: &'static str,
    },
    #[error("model_prices entry for \"{model}\" has unknown dimension \"{dimension}\"")]
    UnknownDimension { model: String, dimension: String },
    #[error("model_prices contains an empty model name")]
    EmptyModelName,
    #[error(
        "model_prices[{model}].{dimension} must be a non-negative decimal USD amount, \
         got \"{value}\""
    )]
    BadUsd {
        model: String,
        dimension: &'static str,
        value: String,
    },
    #[error(
        "model_prices[{model}].{dimension} \"{value}\" is finer than 1e-6 USD and cannot be \
         stored exactly"
    )]
    NotExactUsd {
        model: String,
        dimension: &'static str,
        value: String,
    },
    #[error("model_prices prices models outside allowed_models: {models}")]
    OutsideAllowedModels { models: String },
}

fn parse_usd_amount(
    value: &Value,
    model: &str,
    dimension: &'static str,
) -> Result<u64, ModelPricesError> {
    // serde_json keeps the literal text of a number, so a JSON number
    // reaches the decimal parser with the digits the client wrote.
    let raw = match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    };
    match parse_usd(&raw) {
        Ok(micro) => Ok(micro),
        Err(UsdAmountError::NotExact) => Err(ModelPricesError::NotExactUsd {
            model: model.to_string(),
            dimension,
            value: raw,
        }),
        Err(_) => Err(ModelPricesError::BadUsd {
            model: model.to_string(),
            dimension,
            value: raw,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usd(prices: Value) -> Result<ModelPrices, ModelPricesError> {
        ModelPrices::from_usd_json(&prices)
    }

    #[test]
    fn usd_json_converts_decimal_strings_and_numbers_to_micro_usd() {
        let prices = usd(json!({
            "gpt-4o": {
                "input": "2.5",
                "output": 10,
                "cached_input": "1.25",
                "cached_output": "5.00"
            }
        }))
        .unwrap();
        let entry = prices.get("gpt-4o").unwrap();
        assert_eq!(entry.input, 2_500_000);
        assert_eq!(entry.output, 10_000_000);
        assert_eq!(entry.cached_input, 1_250_000);
        assert_eq!(entry.cached_output, 5_000_000);
        assert_eq!(
            entry.cache_write_input, 0,
            "an entry that names no write price prices no writes"
        );
        assert_eq!(prices.len(), 1);
        assert!(!prices.is_empty());
    }

    #[test]
    fn the_cache_write_dimension_is_read_from_the_usd_shape() {
        let prices = usd(json!({
            "claude-opus-4": {
                "input": "15.00",
                "output": 75,
                "cached_input": "1.50",
                "cached_output": "0.00",
                "cache_write_input": "18.75"
            }
        }))
        .unwrap();
        let entry = prices.get("claude-opus-4").unwrap();
        assert_eq!(entry.input, 15_000_000);
        assert_eq!(entry.output, 75_000_000);
        assert_eq!(entry.cached_input, 1_500_000);
        assert_eq!(entry.cached_output, 0);
        assert_eq!(
            entry.cache_write_input, 18_750_000,
            "the write price is set independently, never derived from input"
        );
    }

    #[test]
    fn an_absent_or_null_cache_write_price_leaves_the_dimension_unpriced() {
        let base = json!({
            "input": "1.00",
            "output": "2.00",
            "cached_input": "0.50",
            "cached_output": "1.00"
        });
        let mut absent = base.clone();
        absent["cache_write_input"] = json!(null);
        for entry in [base, absent] {
            let prices = usd(json!({ "m": entry })).unwrap();
            assert_eq!(
                prices.get("m").unwrap().cache_write_input,
                0,
                "no write price means this dimension contributes nothing"
            );
        }
        // A null where a dimension is required is still malformed, not a
        // silent zero.
        let error = usd(json!({
            "m": {
                "input": null,
                "output": "2.00",
                "cached_input": "0.50",
                "cached_output": "1.00"
            }
        }))
        .unwrap_err();
        assert!(matches!(error, ModelPricesError::BadUsd { .. }), "{error}");
    }

    #[test]
    fn usd_json_keeps_sub_cent_amounts_exact() {
        let prices = usd(json!({
            "tiny": {
                "input": "0.000001",
                "output": "0.00001",
                "cached_input": "0",
                "cached_output": 0.25
            }
        }))
        .unwrap();
        let entry = prices.get("tiny").unwrap();
        assert_eq!(entry.input, 1);
        assert_eq!(entry.output, 10);
        assert_eq!(entry.cached_input, 0);
        assert_eq!(entry.cached_output, 250_000);
    }

    #[test]
    fn amounts_finer_than_micro_usd_are_rejected_not_rounded() {
        let error = usd(json!({
            "m": { "input": "0.0000005", "output": "1", "cached_input": "1", "cached_output": "1" }
        }))
        .unwrap_err();
        assert!(matches!(error, ModelPricesError::NotExactUsd { .. }));

        // Padding past the sixth place with zeros keeps the amount exact.
        let prices = usd(json!({
            "m": { "input": "2.5000000", "output": "1", "cached_input": "1", "cached_output": "1" }
        }))
        .unwrap();
        assert_eq!(prices.get("m").unwrap().input, 2_500_000);
    }

    #[test]
    fn negative_and_malformed_amounts_are_rejected() {
        for amount in [
            json!("-1"),
            json!(-1),
            json!("1e3"),
            json!(""),
            json!("2.5.1"),
        ] {
            let error = usd(json!({
                "m": { "input": amount, "output": "1", "cached_input": "1", "cached_output": "1" }
            }))
            .unwrap_err();
            assert!(
                matches!(error, ModelPricesError::BadUsd { .. }),
                "amount {amount} must be rejected as malformed"
            );
        }
    }

    #[test]
    fn missing_and_unknown_dimensions_are_rejected() {
        let error = usd(json!({
            "m": { "input": "1", "output": "1", "cached_input": "1" }
        }))
        .unwrap_err();
        assert!(matches!(
            error,
            ModelPricesError::MissingDimension {
                dimension: "cached_output",
                ..
            }
        ));

        // The write dimension is the one an entry may leave out; every other
        // one missing is still an error.
        let error = usd(json!({
            "m": { "output": "1", "cached_input": "1", "cached_output": "1" }
        }))
        .unwrap_err();
        assert!(matches!(
            error,
            ModelPricesError::MissingDimension {
                dimension: "input",
                ..
            }
        ));
        assert!(usd(json!({
            "m": {
                "input": "1",
                "output": "1",
                "cached_input": "1",
                "cached_output": "1"
            }
        }))
        .is_ok());

        let error = usd(json!({
            "m": { "input": "1", "output": "1", "cached_input": "1", "cached_output": "1", "x": "1" }
        }))
        .unwrap_err();
        assert!(matches!(error, ModelPricesError::UnknownDimension { .. }));
    }

    #[test]
    fn non_object_entries_and_empty_model_names_are_rejected() {
        assert!(matches!(
            usd(json!(["gpt-4o"])).unwrap_err(),
            ModelPricesError::NotAnObject
        ));
        assert!(matches!(
            usd(json!({ "gpt-4o": "2.5" })).unwrap_err(),
            ModelPricesError::BadEntry { .. }
        ));
        assert!(matches!(
            usd(json!({ "  ": { "input": "1", "output": "1", "cached_input": "1", "cached_output": "1" } }))
                .unwrap_err(),
            ModelPricesError::EmptyModelName
        ));
    }

    #[test]
    fn stored_json_round_trips_through_the_integer_shape() {
        let prices = usd(json!({
            "gpt-4o": {
                "input": "2.5",
                "output": "10",
                "cached_input": "1.25",
                "cached_output": "5",
                "cache_write_input": "3.125"
            }
        }))
        .unwrap();
        let stored = prices.to_stored_json();
        assert_eq!(
            stored,
            "{\"gpt-4o\":{\"input\":2500000,\"output\":10000000,\"cached_input\":1250000,\
             \"cached_output\":5000000,\"cache_write_input\":3125000}}"
        );
        assert_eq!(ModelPrices::from_stored_json(&stored).unwrap(), prices);
    }

    #[test]
    fn a_stored_table_without_the_write_dimension_keeps_loading() {
        // Entries written before the dimension existed carry four keys and
        // must read back as "no write price", never as a decode failure.
        let stored = r#"{"gpt-4o":{"input":2500000,"output":10000000,"cached_input":1250000,"cached_output":5000000}}"#;
        let prices = ModelPrices::from_stored_json(stored).unwrap();
        let entry = prices.get("gpt-4o").unwrap();
        assert_eq!(entry.input, 2_500_000);
        assert_eq!(entry.cache_write_input, 0);

        // The write dimension is written back on the next store, so the
        // table converges on the five-dimension shape.
        assert!(prices.to_stored_json().contains("\"cache_write_input\":0"));
        // A null in the stored shape is a corrupt entry, not a zero: the
        // integer column is the only thing the parser accepts.
        assert!(matches!(
            ModelPrices::from_stored_json(
                r#"{"gpt-4o":{"input":1,"output":1,"cached_input":1,"cached_output":1,"cache_write_input":null}}"#
            )
            .unwrap_err(),
            ModelPricesError::Stored { .. }
        ));
    }

    #[test]
    fn stored_json_rejects_non_integers_negatives_and_missing_dimensions() {
        assert!(matches!(
            ModelPrices::from_stored_json("not json").unwrap_err(),
            ModelPricesError::Stored { .. }
        ));
        assert!(matches!(
            ModelPrices::from_stored_json(
                r#"{"gpt-4o":{"input":2.5,"output":1,"cached_input":1,"cached_output":1}}"#
            )
            .unwrap_err(),
            ModelPricesError::Stored { .. }
        ));
        assert!(matches!(
            ModelPrices::from_stored_json(
                r#"{"gpt-4o":{"input":-1,"output":1,"cached_input":1,"cached_output":1}}"#
            )
            .unwrap_err(),
            ModelPricesError::Stored { .. }
        ));
        assert!(matches!(
            ModelPrices::from_stored_json(r#"{"gpt-4o":{"input":1,"output":1,"cached_input":1}}"#)
                .unwrap_err(),
            ModelPricesError::Stored { .. }
        ));
    }

    #[test]
    fn prices_are_a_subset_of_the_whitelist_only_when_one_is_set() {
        let prices = usd(json!({
            "gpt-4o": { "input": "1", "output": "1", "cached_input": "1", "cached_output": "1" }
        }))
        .unwrap();
        assert!(prices.validate_against_allowed(None).is_ok());
        assert!(prices.validate_against_allowed(Some(&[])).is_ok());
        assert!(prices
            .validate_against_allowed(Some(&["gpt-4o".to_string()]))
            .is_ok());
        let error = prices
            .validate_against_allowed(Some(&["other".to_string()]))
            .unwrap_err();
        assert!(matches!(
            error,
            ModelPricesError::OutsideAllowedModels { ref models } if models == "gpt-4o"
        ));
    }

    #[test]
    fn limitable_requires_a_finite_whitelist_and_complete_prices() {
        let prices = usd(json!({
            "gpt-4o": { "input": "1", "output": "1", "cached_input": "1", "cached_output": "1" }
        }))
        .unwrap();
        let full = vec!["gpt-4o".to_string()];
        assert!(all_models_priced(Some(&full), Some(&prices)));
        assert!(!all_models_priced(None, Some(&prices)));
        assert!(!all_models_priced(Some(&[]), Some(&prices)));
        assert!(!all_models_priced(Some(&full), None));
        let partial = vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()];
        assert!(!all_models_priced(Some(&partial), Some(&prices)));
    }

    #[test]
    fn usd_parsing_and_formatting_round_trip_exactly() {
        for micro in [
            0u64,
            1,
            10,
            100,
            1_250_000,
            2_500_000,
            10_000_000,
            999_999_999,
        ] {
            let rendered = format_usd(micro);
            assert_eq!(
                parse_usd(&rendered).unwrap(),
                micro,
                "round trip {rendered}"
            );
        }
        assert_eq!(format_usd(2_500_000), "2.50");
        assert_eq!(format_usd(10_000_000), "10.00");
        assert_eq!(format_usd(1_250_000), "1.25");
        assert_eq!(format_usd(1), "0.000001");
        assert_eq!(format_usd(0), "0.00");
        assert_eq!(parse_usd("  2.50  ").unwrap(), 2_500_000);
        assert_eq!(parse_usd(".5").unwrap(), 500_000);
        assert_eq!(parse_usd("7.").unwrap(), 7_000_000);
    }
}
