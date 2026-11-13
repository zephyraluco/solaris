//! Token accounting: what one turn used, and what that cost.
//!
//! Providers report usage under different names and disagree about what "input
//! tokens" includes, so [`Usage`] normalises everything into four additive
//! buckets. Official catalog pricing turns those buckets into reference dollars.

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

impl From<rig_core::completion::Usage> for Usage {
    fn from(value: rig_core::completion::Usage) -> Self {
        let read = value.cached_input_tokens.unwrap_or(0);
        let write = value.cache_creation_input_tokens.unwrap_or(0);
        let bounded = |value: u64| u32::try_from(value).unwrap_or(u32::MAX);
        Usage::new(
            bounded(
                value
                    .input_tokens
                    .unwrap_or(0)
                    .saturating_sub(read.saturating_add(write)),
            ),
            bounded(value.output_tokens.unwrap_or(0)),
        )
        .with_cache(bounded(read), bounded(write))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_input_is_not_counted_twice() {
        let mut measured = rig_core::completion::Usage::new();
        measured.input_tokens = Some(1000);
        measured.output_tokens = Some(50);
        measured.cached_input_tokens = Some(700);
        measured.cache_creation_input_tokens = Some(100);
        let converted = Usage::from(measured);
        assert_eq!(converted, Usage::new(200, 50).with_cache(700, 100));
        assert_eq!(converted.total(), 1050);
    }
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
