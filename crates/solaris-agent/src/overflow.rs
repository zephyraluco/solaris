//! Recognising the one error a turn can recover from.
//!
//! A context overflow is not a failure to report and stop on: the conversation
//! can be summarized and the round tried again. But every platform words it
//! differently, and some do not report it as an error at all — they accept the
//! request and answer, or truncate the input and stop early. So there are three
//! ways to notice, and all three are needed.
//!
//! This is guesswork about text, which is why the list below is long and
//! documented per phrasing: a missed phrasing costs a retry the user would have
//! wanted, and a false positive costs a summary and a wasted call. Where the two
//! risks differ, this errs towards missing it — a spurious compaction throws away
//! conversation for no reason.
//!
//! Matching is plain substring work on the lowercased message rather than
//! regular expressions: the parts that actually identify these errors are fixed
//! phrases, and the numbers around them are not what makes a message an overflow.

use solaris_backend::Usage;

/// Phrases that mean the input was longer than the model's window.
///
/// Each is the fixed part of one platform's wording; the numbers and punctuation
/// around them vary, so only this much is matched.
const OVERFLOW_MARKERS: &[&str] = &[
    // Anthropic: "prompt is too long: 213462 tokens > 200000 maximum"
    "prompt is too long",
    // Anthropic, byte size rather than tokens (HTTP 413)
    "request_too_large",
    // z.ai, and its non-standard finish reason surfaced as error text
    "prompt exceeds max length",
    "model_context_window_exceeded",
    // Amazon Bedrock
    "input is too long for requested model",
    // OpenAI, both the Completions and the Responses API
    "exceeds the context window",
    // OpenAI-compatible proxies (LiteLLM and friends)
    "maximum context length",
    "exceeds the maximum allowed input length",
    // Google Gemini is in the pair list below rather than here: "input token
    // count" appears in ordinary validation errors on its own.
    // xAI
    "maximum prompt length is",
    // Groq
    "reduce the length of the messages",
    // OpenRouter and Together AI
    "is longer than the model's context length",
    // GitHub Copilot
    "exceeds the limit of",
    // A runtime on this machine: llama.cpp, LM Studio, Ollama, DS4
    "exceeds the available context size",
    "greater than the context length",
    "but the configured context size is",
    "exceeded max context length",
    // MiniMax, Kimi, Mistral
    "context window exceeds limit",
    "exceeded model token limit",
    "too large for model with",
    // DashScope / Qwen
    "range of input length should be",
    // Generic fallbacks, deliberately last: they are the ones most likely to
    // appear in an unrelated error.
    "context_length_exceeded",
    "context length exceeded",
    "token limit exceeded",
];

/// Phrases that mean something else even when an overflow phrase also appears.
///
/// Bedrock, for one, reports throttling as "Too many tokens, please wait before
/// trying again", which would otherwise read as an overflow and throw the
/// conversation away in response to a rate limit.
const NOT_OVERFLOW_MARKERS: &[&str] = &[
    "throttling",
    "rate limit",
    "too many requests",
    "service unavailable",
];

/// Pairs where neither phrase means overflow on its own.
///
/// "input token count" appears in ordinary validation errors, and so does
/// "exceeds the maximum"; together they are Gemini's overflow message.
const OVERFLOW_PAIRS: &[(&str, &str)] = &[("input token count", "exceeds the maximum")];

/// Whether `message` is a platform complaining that the input was too long.
pub(crate) fn is_overflow(message: &str) -> bool {
    let text = message.to_ascii_lowercase();
    if NOT_OVERFLOW_MARKERS
        .iter()
        .any(|marker| text.contains(marker))
    {
        return false;
    }
    OVERFLOW_MARKERS.iter().any(|marker| text.contains(marker))
        || OVERFLOW_PAIRS
            .iter()
            .any(|(first, second)| text.contains(first) && text.contains(second))
}

/// Whether a call that reported `usage` already filled the window.
///
/// Some platforms accept an over-long request and answer anyway, so nothing in
/// the error path will ever say the window was full. The input side of the usage
/// does: `input_tokens` and `cache_read_tokens` are the four-bucket split's two
/// halves of "what was read", and neither overlaps the other.
pub(crate) fn filled_the_window(usage: &Usage, context_window: u32) -> bool {
    if context_window == 0 || usage.estimated {
        return false;
    }
    let read = usage.input_tokens.saturating_add(usage.cache_read_tokens);
    read > context_window
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One real phrasing per platform, as the reference implementation lists them.
    #[test]
    fn every_platforms_way_of_saying_it_is_recognised() {
        let phrasings = [
            "prompt is too long: 213462 tokens > 200000 maximum",
            "413 {\"error\":{\"type\":\"request_too_large\"}}",
            "prompt exceeds max length",
            "model_context_window_exceeded",
            "Input is too long for requested model",
            "This model's maximum context length is 131072 tokens",
            "This endpoint's maximum context length is 200000 tokens. However, you requested about 265330 tokens",
            "The input (265330 tokens) is longer than the model's context length (262144 tokens)",
            "input token count exceeds the maximum number of tokens allowed",
            "maximum prompt length is 131072",
            "reduce the length of the messages",
            "Request exceeds the limit of 200000 tokens",
            "exceeds the available context size",
            "tokens to keep from the initial prompt is greater than the context length",
            "prompt has 300000 tokens, but the configured context size is 262144 tokens",
            "prompt too long; exceeded max context length",
            "range of input length should be",
            "context_length_exceeded",
            "context length exceeded",
            "token limit exceeded",
            "You have exceeded model token limit",
        ];
        for phrasing in phrasings {
            assert!(is_overflow(phrasing), "not recognised: {phrasing}");
        }
    }

    #[test]
    fn a_rate_limit_is_not_an_overflow() {
        // Bedrock's throttling text would otherwise match an overflow phrase.
        assert!(!is_overflow(
            "ThrottlingException: Too many tokens, please wait"
        ));
        assert!(!is_overflow("Rate limit reached for gpt-4"));
        assert!(!is_overflow("429 Too Many Requests"));
        assert!(!is_overflow("Service unavailable, try again later"));
    }

    #[test]
    fn an_ordinary_failure_is_not_an_overflow() {
        assert!(!is_overflow("invalid api key"));
        assert!(!is_overflow("connection reset by peer"));
        assert!(!is_overflow("model not found"));
        assert!(!is_overflow("exceeds the maximum number of retries"));
        // Neither half of the Gemini pair means anything on its own.
        assert!(!is_overflow("input token count must be positive"));
    }

    #[test]
    fn a_window_filled_without_a_complaint_is_still_noticed() {
        let mut usage = Usage::new(250_000, 10);
        assert!(filled_the_window(&usage, 200_000));

        usage = Usage::new(150_000, 10).with_cache(60_000, 0);
        assert!(
            filled_the_window(&usage, 200_000),
            "cache reads count towards what was read"
        );

        usage = Usage::new(150_000, 10).with_cache(40_000, 0);
        assert!(!filled_the_window(&usage, 200_000));
    }

    #[test]
    fn an_unknown_window_or_an_estimate_proves_nothing() {
        let usage = Usage::new(250_000, 10);
        assert!(!filled_the_window(&usage, 0), "no window was reported");
        assert!(
            !filled_the_window(&Usage::estimate(400, 400), 100),
            "an estimate is not a measurement"
        );
    }
}
