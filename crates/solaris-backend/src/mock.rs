//! Deterministic mock backend: produces a canned thinking trace and a markdown
//! reply, streamed chunk by chunk so the TUI's streaming path is exercised.

use std::time::Duration;

use futures::StreamExt;
use solaris_core::{AgentEvent, BackendError, TurnRequest};

use crate::{AgentBackend, AgentEventStream};

/// Builds scripted replies without touching the network.
pub struct MockBackend {
    /// Per-chunk delay; set to zero in tests.
    chunk_delay: Duration,
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MockBackend {
    /// Backend with a 24 ms per-chunk delay, which reads as steady streaming.
    pub fn new() -> Self {
        Self {
            chunk_delay: Duration::from_millis(24),
        }
    }

    /// Backend with an explicit per-chunk delay.
    pub fn with_delay(delay: Duration) -> Self {
        Self { chunk_delay: delay }
    }
}

#[async_trait::async_trait]
impl AgentBackend for MockBackend {
    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
        let mut events = Vec::new();

        for chunk in chunk_words(&thinking_for(&request), 4) {
            events.push(AgentEvent::ThinkingDelta(chunk));
        }
        events.push(AgentEvent::Status("composing reply".to_string()));
        for chunk in chunk_words(&reply_for(&request), 2) {
            events.push(AgentEvent::TextDelta(chunk));
        }

        let reply = reply_for(&request);
        events.push(AgentEvent::TurnComplete {
            tokens: estimate_tokens(&reply) as u32,
            cost_usd: estimate_cost(&reply),
        });

        let delay = self.chunk_delay;
        let stream = futures::stream::iter(events).then(move |event| async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            event
        });

        Ok(Box::pin(stream))
    }

    fn label(&self) -> &str {
        "mock"
    }
}

/// Split `text` into chunks of at most `size` whitespace-delimited words,
/// keeping the original whitespace so reassembly is lossless.
fn chunk_words(text: &str, size: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut words = 0usize;

    for word in text.split_inclusive(' ') {
        current.push_str(word);
        words += 1;
        if words >= size {
            chunks.push(std::mem::take(&mut current));
            words = 0;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn thinking_for(request: &TurnRequest) -> String {
    format!(
        "The user asked: {}. I should keep the reply short, show a heading, a bullet list and a fenced code block so every renderer path is exercised. Mode is {} so I will mention it.",
        summarize(&request.prompt),
        request.mode.label().to_lowercase()
    )
}

fn reply_for(request: &TurnRequest) -> String {
    if request.prompt.trim().is_empty() {
        return "I did not receive a prompt. Try typing something, or run `/help`.".to_string();
    }

    // Built with push_str so the fenced Rust block needs no brace escaping.
    let mut reply = String::new();
    reply.push_str(&format!("You said: **{}**\n", summarize(&request.prompt)));
    reply.push_str(
        "\nThis is a *mock* reply streamed chunk by chunk, so the transcript exercises \
         streaming text, `inline code` and collapsing thinking blocks.\n",
    );
    reply.push_str("\n## What you can try\n\n");
    reply.push_str("- `/help` — keyboard shortcuts and commands\n");
    reply.push_str("- `/theme light` — switch to the light theme\n");
    reply.push_str("- `/model solaris-mock-reason` — switch the active model\n");
    reply.push_str("- `Tab` — toggle between build and plan mode\n\n");
    reply.push_str("```rust\nfn main() {\n    println!(\"hello from solaris\");\n}\n```\n\n");
    reply.push_str(&format!(
        "> Mode: **{}** · {} message(s) of history.\n",
        request.mode.label(),
        request.history.len()
    ));
    reply
}

/// First line of the prompt, trimmed, for quoting back.
fn summarize(prompt: &str) -> String {
    let line = prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut summary = line.trim().to_string();
    if summary.chars().count() > 60 {
        summary = summary.chars().take(57).collect::<String>() + "...";
    }
    if summary.is_empty() {
        "(empty)".to_string()
    } else {
        summary
    }
}

/// Rough token count: ~4 characters per token.
fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4).max(1)
}

/// Rough cost estimate: $3 per million tokens.
fn estimate_cost(text: &str) -> f64 {
    estimate_tokens(text) as f64 * 3.0 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use solaris_core::Mode;

    #[test]
    fn chunking_is_lossless() {
        let text = "alpha beta gamma delta epsilon zeta";
        let joined: String = chunk_words(text, 2).concat();
        assert_eq!(joined, text);
    }

    #[test]
    fn chunking_respects_word_size() {
        let text = "a b c d e f g";
        let chunks = chunk_words(text, 3);
        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn summary_truncates_long_lines() {
        let long = "x".repeat(200);
        let summary = summarize(&long);
        assert!(summary.ends_with("..."));
        assert_eq!(summary.chars().count(), 60);
    }

    #[test]
    fn empty_prompt_is_reported() {
        assert_eq!(summarize("   \n  "), "(empty)");
    }

    #[tokio::test]
    async fn mock_stream_emits_thinking_text_and_completion() {
        let backend = MockBackend::with_delay(Duration::ZERO);
        let request = TurnRequest {
            history: Vec::new(),
            prompt: "hello there".to_string(),
            mode: Mode::Build,
        };

        let stream = backend.run_turn(request).await.expect("turn started");
        let events: Vec<_> = stream.collect().await;

        assert!(matches!(events.first(), Some(AgentEvent::ThinkingDelta(_))));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::TextDelta(_))));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Status(_))));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
    }

    #[tokio::test]
    async fn streamed_text_reassembles_into_the_reply() {
        let backend = MockBackend::with_delay(Duration::ZERO);
        let request = TurnRequest {
            history: Vec::new(),
            prompt: "streaming please".to_string(),
            mode: Mode::Plan,
        };

        let events: Vec<_> = backend
            .run_turn(request)
            .await
            .expect("turn started")
            .collect()
            .await;

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TextDelta(chunk) => Some(chunk.as_str()),
                _ => None,
            })
            .collect();

        assert!(text.contains("You said: **streaming please**"));
        assert!(text.contains("Mode: **PLAN**"));
    }

    #[tokio::test]
    async fn empty_prompt_produces_guidance_reply() {
        let backend = MockBackend::with_delay(Duration::ZERO);
        let request = TurnRequest {
            history: Vec::new(),
            prompt: "   ".to_string(),
            mode: Mode::Build,
        };

        let events: Vec<_> = backend
            .run_turn(request)
            .await
            .expect("turn started")
            .collect()
            .await;

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TextDelta(chunk) => Some(chunk.as_str()),
                _ => None,
            })
            .collect();

        assert!(text.contains("did not receive a prompt"));
    }
}
