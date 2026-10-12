//! When a conversation is summarized, and how much of it is kept.
//!
//! The policy is here and the driving of it is in [`crate::session`], because the
//! decisions are pure — a token count against a window, an index into a list —
//! and being able to test them without a backend is worth the split.
//!
//! Two numbers decide everything, and both are deliberately absolute rather than
//! fractions of the window:
//!
//! - [`reserve_tokens`](Compaction::reserve_tokens) is room held back for the
//!   reply. A reply does not get bigger because the window did, so a share of the
//!   window is the wrong unit: on a 1M-token window a tenth of it is 100k of room
//!   nothing will ever use, and on a small window a tenth may be less than one
//!   answer.
//! - [`keep_recent_tokens`](Compaction::keep_recent_tokens) is how much of the
//!   most recent conversation is never summarized. It is a token budget and not a
//!   number of messages for the same reason: eight messages can be a paragraph or
//!   a whole file.

use solaris_backend::{Message, tokens_for_characters};

/// Tokens held back from the window for the reply itself.
pub const DEFAULT_RESERVE_TOKENS: u32 = 16_384;

/// Tokens of the most recent conversation that are never summarized.
pub const DEFAULT_KEEP_RECENT_TOKENS: u32 = 20_000;

/// When a conversation is summarized to stay inside the model's window.
///
/// Summarizing costs a model call, so it happens only when there is a reason —
/// the conversation has reached the point where the next reply might not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compaction {
    /// Whether an over-budget conversation is summarized at all.
    pub enabled: bool,
    /// Room left in the window for the reply.
    pub reserve_tokens: u32,
    /// How much of the newest conversation is always kept verbatim.
    pub keep_recent_tokens: u32,
}

impl Default for Compaction {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: DEFAULT_RESERVE_TOKENS,
            keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
        }
    }
}

impl Compaction {
    /// A session that never summarizes, and sends whatever it has.
    ///
    /// The right choice for a caller that does its own context management, and
    /// the one a test wants: no turn then makes a second model call.
    pub const OFF: Self = Self {
        enabled: false,
        reserve_tokens: 0,
        keep_recent_tokens: 0,
    };

    /// Whether the conversation has reached the point of needing a summary.
    ///
    /// A window of zero means nothing reported a size, and then nothing is ever
    /// over budget: summarizing against a window nobody stated would throw the
    /// conversation away for no reason.
    pub fn should_compact(self, context_tokens: u32, context_window: u32) -> bool {
        if !self.enabled || context_window == 0 {
            return false;
        }
        context_tokens > context_window.saturating_sub(self.reserve_tokens)
    }

    /// Where the kept tail starts, so `messages[..cut]` is what gets summarized.
    ///
    /// Returns `0` when there is nothing worth summarizing, and never leaves a
    /// tool result at the start of the tail: a result with no request ahead of it
    /// is rejected by every wire, so the cut moves forward until the tail can
    /// begin a conversation on its own.
    pub fn cut_point(self, messages: &[Message]) -> usize {
        let mut kept = 0u32;
        let mut index = messages.len();

        while index > 0 {
            let cost = tokens_for_characters(messages[index - 1].characters());
            // At least one message is always kept: a summary of everything would
            // leave the model with no idea what was just said.
            if kept > 0 && kept.saturating_add(cost) > self.keep_recent_tokens {
                break;
            }
            kept = kept.saturating_add(cost);
            index -= 1;
        }

        while index < messages.len() && messages[index].is_tool_results() {
            index += 1;
        }
        index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use solaris_backend::{ToolCall, ToolResult};

    fn call(id: &str) -> ToolCall {
        ToolCall::new(id, "read", json!({}))
    }

    #[test]
    fn a_conversation_under_its_budget_is_left_alone() {
        let compaction = Compaction::default();

        // A 200k window with 16k held back: compaction starts past 184k.
        assert!(!compaction.should_compact(180_000, 200_000));
        assert!(compaction.should_compact(190_000, 200_000));
        assert!(
            !compaction.should_compact(190_000, 0),
            "no window was reported"
        );
    }

    #[test]
    fn turning_it_off_means_it_never_triggers() {
        assert!(!Compaction::OFF.should_compact(u32::MAX, 100));
    }

    #[test]
    fn the_tail_is_kept_by_token_budget_not_by_message_count() {
        let compaction = Compaction {
            enabled: true,
            reserve_tokens: 0,
            keep_recent_tokens: 100,
        };
        // Each message is 400 characters, so about 100 tokens: one of them
        // nearly fills the budget and two cannot.
        let messages: Vec<Message> = (0..5)
            .map(|index| Message::user(format!("{}{index}", "x".repeat(397))))
            .collect();

        let cut = compaction.cut_point(&messages);
        assert_eq!(cut, 4, "only the last one fits in 100 tokens");
        assert!(cut > 0, "something was left to summarize");
    }

    #[test]
    fn at_least_one_message_is_always_kept() {
        let compaction = Compaction {
            enabled: true,
            reserve_tokens: 0,
            keep_recent_tokens: 1,
        };
        let messages = vec![
            Message::user("x".repeat(4000)),
            Message::user("y".repeat(4000)),
        ];

        assert_eq!(
            compaction.cut_point(&messages),
            1,
            "a summary of everything would leave nothing to continue from"
        );
    }

    #[test]
    fn the_tail_never_begins_with_an_answer_whose_question_was_summarized() {
        let compaction = Compaction {
            enabled: true,
            reserve_tokens: 0,
            keep_recent_tokens: 1,
        };
        let messages = vec![
            Message::user("read it"),
            Message::tool_use(call("call-1")),
            Message::tool_result(ToolResult::ok(&call("call-1"), "contents")),
        ];

        // The natural cut would be at the tool result, which no wire accepts
        // without the call it answers; the cut moves past it instead.
        let cut = compaction.cut_point(&messages);
        assert_eq!(cut, 3);
        assert!(
            !messages[cut..]
                .first()
                .is_some_and(Message::is_tool_results),
            "the tail must be able to start a conversation"
        );
    }

    #[test]
    fn a_conversation_shorter_than_the_budget_has_nothing_to_cut() {
        let compaction = Compaction::default();
        let messages = vec![Message::user("hello")];
        assert_eq!(compaction.cut_point(&messages), 0);
        assert_eq!(compaction.cut_point(&[]), 0);
    }
}
