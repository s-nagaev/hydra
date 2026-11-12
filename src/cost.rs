//! What usage costs, in integer micro-USD.
//!
//! Money stays fixed point end to end: a price is an integer number of
//! micro-USD per 1M tokens (see [`crate::prices`]) and a cost is the
//! token-weighted sum of those prices, divided by 1_000_000 exactly once.
//! No float takes part in the arithmetic, and the division floor loses at
//! most one micro-USD per computed figure.
//!
//! The convention is OpenAI's: `prompt_tokens` already includes the tokens
//! the upstream served from its cache (and, for Anthropic, the tokens it
//! wrote into it), so the base-priced share of the prompt is what remains
//! once the cache dimensions are taken out of it. Each token carries exactly
//! one rate, whichever dimension it belongs to:
//!
//! ```text
//! cost = (prompt - cached_input - cache_write_input) * input_price
//!      + cached_input * cached_input_price
//!      + cache_write_input * cache_write_input_price
//!      + completion * output_price
//!      + cached_output * cached_output_price
//! ```
//!
//! Cache READS are charged at the cached rate instead of the input rate for
//! those tokens, as they always were. Cache WRITES (Anthropic
//! `cache_creation_input_tokens`) get their own dimension at the rate the
//! model's provider asks for, which is why it is an independently settable
//! price rather than a multiple of `input`: the write rate is the whole
//! price of those tokens, not a surcharge on top of the input rate, so no
//! token is ever billed twice. A model whose entry names no write rate
//! charges nothing for its write tokens, the same way a model the channel
//! does not price costs nothing; the dimensions are independent, and neither
//! an invented rate nor a silent second billing is acceptable.
//!
//! With no cache writes reported (every Chat and Responses request, and any
//! Anthropic request that wrote no cache) the write term is zero and the
//! formula is exactly the four-dimension one it replaces.
//!
//! Every product and the sum are held in `i128` and saturate at each step,
//! so no input can wrap silently: the worst case (every token count at its
//! type's maximum against a maximum-priced model) saturates to [`i64::MAX`]
//! micro-USD instead of producing a negative or wrapped number.
//!
//! NULL semantics: a NULL token count means the upstream reported nothing
//! for that dimension, and [`TokenTotals::from_row`] reads it as zero.
//! Read that way a row with no token counts at all costs nothing, and a row
//! whose cached-input count is NULL charges the whole prompt at the full
//! input price, which is exactly the OpenAI reading of a missing cache
//! report.

use crate::prices::{ModelPrice, ModelPrices};

/// Tokens per price unit: the stored prices are micro-USD per 1M tokens.
const TOKENS_PER_UNIT: i128 = 1_000_000;

/// Token counts of one request or of one aggregate group.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenTotals {
    pub prompt: i64,
    pub completion: i64,
    pub cached_input: i64,
    pub cached_output: i64,
    /// Prompt tokens the upstream wrote into its cache (Anthropic
    /// `cache_creation_input_tokens`), a subset of `prompt`.
    pub cache_write_input: i64,
}

impl TokenTotals {
    /// Reads the five counts of one row, with every NULL dimension counted
    /// as zero. A negative count (only reachable from a hand-edited
    /// database) is clamped to zero so it cannot reduce a cost.
    pub fn from_row(
        prompt: Option<i64>,
        completion: Option<i64>,
        cached_input: Option<i64>,
        cached_output: Option<i64>,
        cache_write_input: Option<i64>,
    ) -> Self {
        TokenTotals {
            prompt: non_negative(prompt),
            completion: non_negative(completion),
            cached_input: non_negative(cached_input),
            cached_output: non_negative(cached_output),
            cache_write_input: non_negative(cache_write_input),
        }
    }

    /// Micro-USD these counts cost at `price`.
    ///
    /// The cache dimensions are taken out of the prompt in that order, each
    /// capped by what is left, because both are subsets of it: a cached
    /// count larger than the prompt (impossible from a well-behaved
    /// upstream, reachable by hand-editing a row) can only account for the
    /// tokens that exist, and the base-priced share never goes negative
    /// however corrupt the row is.
    pub fn cost(&self, price: &ModelPrice) -> i64 {
        let prompt = i128::from(self.prompt);
        let cached_input = i128::from(self.cached_input).min(prompt);
        let cache_write_input = i128::from(self.cache_write_input).min(prompt - cached_input);
        let base_input = prompt - cached_input - cache_write_input;
        let total = product(base_input, price.input)
            .saturating_add(product(cached_input, price.cached_input))
            .saturating_add(product(cache_write_input, price.cache_write_input))
            .saturating_add(product(i128::from(self.completion), price.output))
            .saturating_add(product(i128::from(self.cached_output), price.cached_output));
        let micro = total / TOKENS_PER_UNIT;
        i64::try_from(micro).unwrap_or(i64::MAX)
    }
}

/// Cost of one model's usage in micro-USD: zero when the channel prices no
/// such model, because a model outside the price table has no rate to charge
/// and inventing one would be worse than billing nothing.
pub fn model_cost(model: Option<&str>, totals: &TokenTotals, prices: Option<&ModelPrices>) -> i64 {
    let Some(price) = model.and_then(|name| prices.and_then(|table| table.get(name))) else {
        return 0;
    };
    totals.cost(price)
}

/// One token count times its per-1M-token price, in `i128` so the product of
/// two maximum values cannot wrap, and saturating so it cannot panic either.
fn product(tokens: i128, price: u64) -> i128 {
    tokens.saturating_mul(i128::from(price))
}

/// A NULL or negative token count reads as zero.
fn non_negative(value: Option<i64>) -> i64 {
    value.unwrap_or(0).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prices;

    /// A price entry with every dimension, the write one included.
    fn price(
        input: u64,
        output: u64,
        cached_input: u64,
        cached_output: u64,
        cache_write_input: u64,
    ) -> ModelPrice {
        ModelPrice {
            input,
            output,
            cached_input,
            cached_output,
            cache_write_input,
        }
    }

    /// The four OpenAI dimensions at 2.50/10.00/1.25/5.00 USD per 1M tokens,
    /// with no write rate: the shape every pre-existing test and price table
    /// uses.
    fn four_dimension_price() -> ModelPrice {
        price(2_500_000, 10_000_000, 1_250_000, 5_000_000, 0)
    }

    fn prices_with(model: &str, entry: ModelPrice) -> ModelPrices {
        ModelPrices::from_entries([(model.to_string(), entry)]).unwrap()
    }

    #[test]
    fn the_pinned_formula_charges_the_uncached_prompt_share_at_the_full_rate() {
        let rate = four_dimension_price();
        let totals = TokenTotals::from_row(Some(1000), Some(200), Some(400), Some(50), None);
        // (600 * 2.50 + 400 * 1.25 + 200 * 10.00 + 50 * 5.00) / 1e6 USD,
        // in micro-USD: 1_500_000_000 + 500_000_000 + 2_000_000_000 +
        // 250_000_000 = 4_250_000_000; divided by 1_000_000 gives 4250.
        assert_eq!(totals.cost(&rate), 4250);
    }

    #[test]
    fn a_missing_cached_count_charges_the_whole_prompt_and_missing_tokens_cost_nothing() {
        let rate = four_dimension_price();
        // No cached-input report: the prompt is entirely uncached.
        let totals = TokenTotals::from_row(Some(1000), None, None, None, None);
        assert_eq!(totals.cost(&rate), 2500);
        // No token counts at all: a row without usage contributes nothing.
        let empty = TokenTotals::from_row(None, None, None, None, None);
        assert_eq!(empty.cost(&rate), 0);
        // A fully cached prompt pays only the cached rate.
        let cached = TokenTotals::from_row(Some(1000), None, Some(1000), None, None);
        assert_eq!(cached.cost(&rate), 1250);
        // A cached count above the prompt is capped, never subtracted.
        let impossible = TokenTotals::from_row(Some(100), None, Some(500), None, None);
        assert_eq!(impossible.cost(&rate), 125);
    }

    #[test]
    fn cache_writes_are_charged_at_their_own_rate_and_leave_the_base_share() {
        // Anthropic reports the prompt additively: 1000 uncached input
        // tokens, 250 read from the cache and 50 written to it, so the
        // stored prompt is 1300. The write rate is 1.25x the input rate.
        let rate = price(2_500_000, 10_000_000, 1_250_000, 5_000_000, 3_125_000);
        let totals = TokenTotals::from_row(Some(1300), Some(200), Some(250), None, Some(50));
        // 1000 * 2.50 + 250 * 1.25 + 50 * 3.125 + 200 * 10.00 USD per 1M
        // tokens, in micro-USD: 2_500_000_000 + 312_500_000 + 156_250_000 +
        // 2_000_000_000 = 4_968_750_000; divided by 1_000_000 floors to 4968.
        assert_eq!(totals.cost(&rate), 4968);
        // Which rate applies to those 50 tokens is pinned from both sides.
        // An entry that names no write rate bills them at zero: 2_500_000_000
        // + 312_500_000 + 2_000_000_000 = 4_812_500_000, so 4812.
        let no_write_rate = price(2_500_000, 10_000_000, 1_250_000, 5_000_000, 0);
        assert_eq!(totals.cost(&no_write_rate), 4812);
        // Charging them at the base input rate instead gives 1050 * 2.50 +
        // 250 * 1.25 + 200 * 10.00, so 4937: the implemented formula is
        // neither of those, it is the write rate for the write tokens.
        let write_at_base = TokenTotals::from_row(Some(1300), Some(200), Some(250), None, None);
        assert_eq!(write_at_base.cost(&four_dimension_price()), 4937);
    }

    #[test]
    fn a_model_without_a_write_rate_charges_nothing_for_its_cache_writes() {
        // The dimension is independent: an entry that names no write rate
        // bills its write tokens at zero rather than inventing a multiple of
        // `input`. Those tokens are still out of the base-priced share,
        // because the base share is the prompt minus the cache dimensions.
        let rate = four_dimension_price();
        let writes = TokenTotals::from_row(Some(1000), None, None, None, Some(400));
        assert_eq!(
            writes.cost(&rate),
            // 600 uncached at 2.50 per 1M, in micro-USD.
            1500
        );
    }

    #[test]
    fn the_zero_cache_write_case_is_the_four_dimension_formula() {
        // The regression pin: with no write count reported, the cost of a
        // row is exactly what the four-dimension formula produced before the
        // write dimension existed, whatever the other counts are.
        let rate = four_dimension_price();
        for (prompt, completion, cached_input, cached_output) in [
            (0, 0, 0, 0),
            (1000, 0, 0, 0),
            (1000, 200, 400, 50),
            (1000, 200, 1000, 50),
            (7777, 333, 1, 2),
        ] {
            let totals = TokenTotals::from_row(
                Some(prompt),
                Some(completion),
                Some(cached_input),
                Some(cached_output),
                None,
            );
            let base = i128::from(prompt.max(0) - cached_input.max(0).min(prompt.max(0)));
            let expected = (base * i128::from(rate.input)
                + i128::from(cached_input) * i128::from(rate.cached_input)
                + i128::from(completion) * i128::from(rate.output)
                + i128::from(cached_output) * i128::from(rate.cached_output))
                / 1_000_000;
            assert_eq!(
                i128::from(totals.cost(&rate)),
                expected,
                "prompt {prompt}, completion {completion}, cached {cached_input}/{cached_output}"
            );
        }
    }

    #[test]
    fn a_write_count_above_the_prompt_is_capped_after_the_read_share() {
        // Both cache dimensions are subsets of the prompt, and the reads are
        // taken out first, so a corrupt pair can never push the base share
        // negative.
        let rate = price(1_000_000, 0, 2_000_000, 0, 3_000_000);
        let totals = TokenTotals::from_row(Some(100), None, Some(80), None, Some(500));
        // 80 read tokens at 2.00 USD per 1M plus the remaining 20 tokens at
        // the 3.00 write rate: 160 + 60 micro-USD.
        assert_eq!(totals.cost(&rate), 220);
        // Reads alone consume the prompt: nothing is left for the writes.
        let all_read = TokenTotals::from_row(Some(100), None, Some(100), None, Some(500));
        assert_eq!(all_read.cost(&rate), 200);
    }

    #[test]
    fn negative_counts_are_clamped_to_zero() {
        let rate = price(1_000_000, 1_000_000, 1_000_000, 1_000_000, 1_000_000);
        let totals = TokenTotals::from_row(Some(-5), Some(-5), Some(-5), Some(-5), Some(-5));
        assert_eq!(totals, TokenTotals::default());
        assert_eq!(totals.cost(&rate), 0);
    }

    #[test]
    fn the_division_floors_at_the_micro_usd() {
        let rate = price(1, 1, 1, 1, 1);
        let totals = TokenTotals::from_row(Some(999_999), None, None, None, None);
        assert_eq!(totals.cost(&rate), 0, "just under one micro-USD floors");
        let totals = TokenTotals::from_row(Some(1_000_000), None, None, None, None);
        assert_eq!(totals.cost(&rate), 1, "exactly one micro-USD stays");
    }

    #[test]
    fn a_zero_priced_dimension_charges_nothing_for_that_term() {
        let totals = TokenTotals::from_row(Some(2000), Some(1000), Some(1000), Some(1000), None);
        // Only the uncached prompt share is priced: 1000 tokens at 2.50 USD
        // per 1M, in micro-USD.
        let input_only = price(2_500_000, 0, 0, 0, 0);
        assert_eq!(totals.cost(&input_only), 2500);
        // Only completions are priced: 1000 tokens at 10.00 USD per 1M.
        let output_only = price(0, 10_000_000, 0, 0, 0);
        assert_eq!(totals.cost(&output_only), 10_000);
        // Only the cached-input share is priced: 1000 tokens at 1.25 USD.
        let cached_only = price(0, 0, 1_250_000, 0, 0);
        assert_eq!(totals.cost(&cached_only), 1250);
        // Every dimension at zero costs nothing whatever the token counts.
        assert_eq!(totals.cost(&price(0, 0, 0, 0, 0)), 0);
    }

    #[test]
    fn extreme_tokens_and_prices_saturate_instead_of_wrapping() {
        let rate = price(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        let totals = TokenTotals::from_row(
            Some(i64::MAX),
            Some(i64::MAX),
            Some(i64::MAX),
            Some(i64::MAX),
            Some(i64::MAX),
        );
        assert_eq!(totals.cost(&rate), i64::MAX);
    }

    #[test]
    fn an_unpriced_model_costs_nothing() {
        let rate = four_dimension_price();
        let table = prices_with("priced", rate);
        let totals = TokenTotals::from_row(Some(1000), Some(1000), None, None, None);
        // 1000 * 2.50 + 1000 * 10.00 USD per 1M tokens, in micro-USD.
        assert_eq!(model_cost(Some("priced"), &totals, Some(&table)), 12_500);
        assert_eq!(
            model_cost(Some("other"), &totals, Some(&table)),
            0,
            "a model the channel does not price costs nothing"
        );
        assert_eq!(model_cost(None, &totals, Some(&table)), 0);
        assert_eq!(model_cost(Some("priced"), &totals, None), 0);
    }

    #[test]
    fn prices_from_decimal_usd_produce_the_same_cost_as_the_stored_shape() {
        let table = prices::ModelPrices::from_usd_json(&serde_json::json!({
            "gpt-4o": {
                "input": "2.50",
                "output": "10.00",
                "cached_input": "1.25",
                "cached_output": "5.00",
                "cache_write_input": "3.125"
            }
        }))
        .unwrap();
        // A 1600-token prompt (1000 uncached, 400 read, 200 written), plus
        // 200 completions and 50 cached completions.
        let totals = TokenTotals::from_row(Some(1600), Some(200), Some(400), Some(50), Some(200));
        // 1000 * 2.50 + 400 * 1.25 + 200 * 3.125 + 200 * 10.00 + 50 * 5.00,
        // in micro-USD: 2_500_000_000 + 500_000_000 + 625_000_000 +
        // 2_000_000_000 + 250_000_000 = 5_875_000_000, so 5875.
        assert_eq!(model_cost(Some("gpt-4o"), &totals, Some(&table)), 5875);
    }
}
