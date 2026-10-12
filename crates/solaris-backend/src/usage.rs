//! Token accounting: what one turn used, and what that cost.
//!
//! Providers report usage under different names and disagree about what "input
//! tokens" includes, so [`Usage`] normalises everything into four additive
//! buckets. [`Price`] turns those buckets into dollars using the bundled
//! snapshot of list prices, which is why a turn's cost is an estimate rather
//! than a bill.

use serde::{Deserialize, Serialize};

/// Tokens a turn consumed, split the way providers bill them.
///
/// The four buckets are additive and never overlap: a provider that folds
/// cached input into its plain input count — OpenAI does — has it subtracted
/// out again, so [`Usage::total`] is always the real number of tokens the turn
/// touched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens billed at the full input rate.
    #[serde(default)]
    pub input_tokens: u32,
    /// Tokens the model generated.
    #[serde(default)]
    pub output_tokens: u32,
    /// Input tokens served from the provider's prompt cache.
    #[serde(default)]
    pub cache_read_tokens: u32,
    /// Input tokens written to the provider's prompt cache.
    #[serde(default)]
    pub cache_write_tokens: u32,
    /// Set when the provider reported nothing and these numbers are a size
    /// estimate rather than a measurement.
    #[serde(default)]
    pub estimated: bool,
}

impl Usage {
    /// Measured usage, with the two counts every provider reports.
    pub fn new(input_tokens: u32, output_tokens: u32) -> Self {
        Self {
            input_tokens,
            output_tokens,
            ..Default::default()
        }
    }

    /// The same usage with cache traffic filled in.
    pub fn with_cache(self, read_tokens: u32, write_tokens: u32) -> Self {
        Self {
            cache_read_tokens: read_tokens,
            cache_write_tokens: write_tokens,
            ..self
        }
    }

    /// A size estimate for a turn whose provider reported nothing at all.
    pub fn estimate(input_characters: usize, output_characters: usize) -> Self {
        Self {
            input_tokens: tokens_for_characters(input_characters),
            output_tokens: tokens_for_characters(output_characters),
            estimated: true,
            ..Default::default()
        }
    }

    /// Every token the turn touched, which is what the context gauge counts.
    pub fn total(&self) -> u32 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    /// This usage plus `other`, bucket by bucket.
    ///
    /// One turn can be several model calls — that is what a tool round trip is —
    /// and what the turn cost is the sum of them. `estimated` sticks if either
    /// side was an estimate, because the total is then partly a guess.
    pub fn merge(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_read_tokens: self
                .cache_read_tokens
                .saturating_add(other.cache_read_tokens),
            cache_write_tokens: self
                .cache_write_tokens
                .saturating_add(other.cache_write_tokens),
            estimated: self.estimated || other.estimated,
        }
    }

    /// Whether the provider reported nothing and nothing could be estimated.
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// Rough token count for `characters` characters of text.
///
/// Four characters per token is the usual rule of thumb for English prose and
/// source code; it is close enough to keep the context gauge honest.
pub fn tokens_for_characters(characters: usize) -> u32 {
    (characters as u32).div_ceil(4)
}

/// List price for one model, in USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    /// Rate for input tokens.
    pub input: f64,
    /// Rate for generated tokens.
    pub output: f64,
    /// Rate for input served from the prompt cache.
    pub cache_read: f64,
    /// Rate for input written to the prompt cache.
    pub cache_write: f64,
}

impl Price {
    /// A price from its four rates, each in USD per million tokens.
    pub const fn per_million(input: f64, output: f64, cache_read: f64, cache_write: f64) -> Self {
        Self {
            input,
            output,
            cache_read,
            cache_write,
        }
    }

    /// A model running on this machine, which costs nothing to call.
    pub const FREE: Price = Price::per_million(0.0, 0.0, 0.0, 0.0);

    /// What `usage` costs at this price.
    pub fn cost(&self, usage: &Usage) -> f64 {
        let billed = |tokens: u32, rate: f64| f64::from(tokens) * rate / 1_000_000.0;

        billed(usage.input_tokens, self.input)
            + billed(usage.output_tokens, self.output)
            + billed(usage.cache_read_tokens, self.cache_read)
            + billed(usage.cache_write_tokens, self.cache_write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_add_up() {
        let usage = Usage::new(100, 40).with_cache(1_000, 200);
        assert_eq!(usage.total(), 1_340);
        assert!(!usage.is_empty());
        assert!(Usage::default().is_empty());
    }

    #[test]
    fn a_tool_round_trip_sums_its_model_calls() {
        let first = Usage::new(100, 20).with_cache(10, 1);
        let second = Usage::new(300, 5);

        let total = first.merge(second);
        assert_eq!(total.input_tokens, 400);
        assert_eq!(total.output_tokens, 25);
        assert_eq!(total.cache_read_tokens, 10);
        assert_eq!(total.cache_write_tokens, 1);
        assert!(!total.estimated);

        // One estimated half makes the total partly a guess.
        assert!(first.merge(Usage::estimate(4, 4)).estimated);
        assert!(Usage::estimate(4, 4).merge(first).estimated);
    }

    #[test]
    fn cost_charges_each_bucket_at_its_own_rate() {
        // Sonnet-shaped rates: a million plain input tokens, a million output
        // tokens, and a million cached reads.
        let price = Price::per_million(3.0, 15.0, 0.3, 3.75);
        let usage = Usage::new(1_000_000, 1_000_000).with_cache(1_000_000, 1_000_000);

        let expected = 3.0 + 15.0 + 0.3 + 3.75;
        assert!((price.cost(&usage) - expected).abs() < 1e-9);
    }

    #[test]
    fn a_free_model_costs_nothing() {
        let usage = Usage::new(500_000, 500_000);
        assert_eq!(Price::FREE.cost(&usage), 0.0);
    }

    #[test]
    fn estimation_is_flagged_and_rounded_up() {
        let usage = Usage::estimate(5, 0);
        assert!(usage.estimated);
        assert_eq!(usage.input_tokens, 2);
        assert_eq!(usage.output_tokens, 0);
        assert_eq!(tokens_for_characters(0), 0);
        assert_eq!(tokens_for_characters(4), 1);
        assert_eq!(tokens_for_characters(5), 2);
    }

    #[test]
    fn usage_survives_a_json_round_trip() {
        let usage = Usage::new(1, 2).with_cache(3, 4);
        let json = serde_json::to_string(&usage).expect("encode");
        assert_eq!(serde_json::from_str::<Usage>(&json).expect("decode"), usage);

        // A record written before a field existed still decodes.
        let legacy = r#"{"input_tokens":7,"output_tokens":8}"#;
        assert_eq!(
            serde_json::from_str::<Usage>(legacy).expect("decode"),
            Usage::new(7, 8)
        );
    }
}
