//! Application state: routes, the session transcript, and notifications.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use solaris_core::{Mode, ToolCall, ToolResult, Usage, tokens_for_characters};

/// One tool call the model made, and what running it produced.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolStep {
    /// What the model asked for.
    pub call: ToolCall,
    /// What running it produced, once it has run.
    ///
    /// `None` while the call is in flight, and also when the turn ended before
    /// it could run — a cancelled turn leaves a step like that behind.
    pub result: Option<ToolResult>,
    /// How long the call took, once it has run.
    pub duration_ms: Option<u64>,
}

impl ToolStep {
    /// A step whose call has not run yet.
    pub fn pending(call: ToolCall) -> Self {
        Self {
            call,
            result: None,
            duration_ms: None,
        }
    }

    /// Record what the call produced.
    pub fn finish(&mut self, result: ToolResult, duration_ms: u64) {
        self.result = Some(result);
        self.duration_ms = Some(duration_ms);
    }

    /// Whether the call has run.
    pub fn is_done(&self) -> bool {
        self.result.is_some()
    }

    /// Whether it ran and failed.
    pub fn is_error(&self) -> bool {
        self.result.as_ref().is_some_and(|result| result.is_error)
    }

    /// How long it took, when it has run.
    pub fn duration_ms(&self) -> Option<u64> {
        self.duration_ms
    }
}

/// One prompt/response exchange.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Turn {
    /// The user's prompt.
    pub prompt: String,
    /// The assistant's visible reply.
    pub reply: String,
    /// The assistant's thinking trace.
    pub thinking: String,
    /// Whether the thinking block is expanded.
    pub thinking_expanded: bool,
    /// The tool calls this turn made, in the order they were made.
    pub steps: Vec<ToolStep>,
    /// Tokens the turn consumed, as the backend reported or estimated them.
    pub usage: Usage,
    /// Cost reported for this turn.
    pub cost_usd: f64,
    /// Whether streamed events for this turn finished.
    pub complete: bool,
}

impl Turn {
    /// Every token the turn touched.
    pub fn tokens(&self) -> u32 {
        self.usage.total()
    }
}

/// The conversation plus its streaming state.
#[derive(Debug, Default)]
pub struct SessionState {
    /// Completed and in-flight turns, oldest first.
    pub turns: Vec<Turn>,
    /// Transient status line shown while working.
    pub status: Option<String>,
    /// Scroll offset in transcript lines.
    pub scroll: u16,
    /// Whether the transcript should stay pinned to the newest line.
    pub follow_end: bool,
    /// Monotonic revision, used to invalidate the rendered transcript.
    pub version: u64,
}

impl SessionState {
    /// An empty session.
    pub fn new() -> Self {
        Self {
            follow_end: true,
            ..Default::default()
        }
    }

    /// Bump the revision so the transcript re-renders.
    pub fn bump(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    /// The turn currently being streamed, if any.
    pub fn active_turn(&self) -> Option<&Turn> {
        self.turns.last().filter(|turn| !turn.complete)
    }

    /// Mutable access to the turn being streamed.
    pub fn active_turn_mut(&mut self) -> Option<&mut Turn> {
        self.turns.last_mut().filter(|turn| !turn.complete)
    }

    /// Whether a response is currently streaming.
    pub fn is_streaming(&self) -> bool {
        self.active_turn().is_some()
    }

    /// Total tokens across all turns.
    ///
    /// This is a session total, not a measure of the context window; see
    /// [`SessionState::context_tokens`] for that.
    pub fn total_tokens(&self) -> u32 {
        self.turns.iter().map(Turn::tokens).sum()
    }

    /// Tokens the conversation currently occupies in the model's context.
    ///
    /// Only the last measured turn can say this: its usage counts everything
    /// the provider had read up to and including that turn, and the next turn
    /// sends the same prefix again. Summing a session's turns instead grows
    /// without bound and says nothing about how full the window is. Prompts
    /// submitted since that turn are added as a size estimate.
    pub fn context_tokens(&self) -> u32 {
        match self
            .turns
            .iter()
            .rposition(|turn| turn.complete && turn.tokens() > 0)
        {
            Some(index) => {
                let trailing: usize = self.turns[index + 1..]
                    .iter()
                    .map(|turn| turn.prompt.chars().count())
                    .sum();
                self.turns[index]
                    .tokens()
                    .saturating_add(tokens_for_characters(trailing))
            }
            // Nothing has been measured yet, so estimate the prompts rather
            // than report an empty context.
            None => tokens_for_characters(
                self.turns
                    .iter()
                    .map(|turn| turn.prompt.chars().count())
                    .sum(),
            ),
        }
    }

    /// Every bucket summed across all turns, for the stats dialog.
    ///
    /// The result is flagged as an estimate when any turn in it was one.
    pub fn total_usage(&self) -> Usage {
        self.turns
            .iter()
            .fold(Usage::default(), |total, turn| Usage {
                input_tokens: total.input_tokens.saturating_add(turn.usage.input_tokens),
                output_tokens: total.output_tokens.saturating_add(turn.usage.output_tokens),
                cache_read_tokens: total
                    .cache_read_tokens
                    .saturating_add(turn.usage.cache_read_tokens),
                cache_write_tokens: total
                    .cache_write_tokens
                    .saturating_add(turn.usage.cache_write_tokens),
                estimated: total.estimated || turn.usage.estimated,
            })
    }

    /// Total cost reported across all turns.
    pub fn total_cost(&self) -> f64 {
        // `Iterator::sum` seeds floats with `-0.0`, which would render as
        // `$-0.0000`; adding zero normalises the sign.
        self.turns.iter().map(|turn| turn.cost_usd).sum::<f64>() + 0.0
    }

    /// Completed turns flattened into backend history.
    ///
    /// Tool calls ride along, so the next turn's model can see what it already
    /// looked at instead of asking again. A call whose result never arrived —
    /// the turn was cancelled while it ran — is left out rather than sent as a
    /// call with no answer, which every wire rejects.
    pub fn history(&self, mode: Mode) -> Vec<solaris_core::Message> {
        let mut history = vec![solaris_core::Message::system(format!(
            "You are solaris, a terminal assistant running in {} mode.",
            mode.label().to_lowercase()
        ))];

        for turn in self.turns.iter().filter(|turn| turn.complete) {
            history.push(solaris_core::Message::user(turn.prompt.clone()));

            for step in turn.steps.iter().filter(|step| step.result.is_some()) {
                history.push(solaris_core::Message::tool_use(step.call.clone()));
                if let Some(result) = &step.result {
                    history.push(solaris_core::Message::tool_result(result.clone()));
                }
            }

            // An empty reply adds nothing, and some wires reject a blank text
            // block outright.
            if !turn.reply.is_empty() {
                history.push(solaris_core::Message::assistant(turn.reply.clone()));
            }
        }
        history
    }
}

/// Severity of a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Warning,
    Error,
}

impl NoticeKind {
    /// Single-character marker drawn before the text.
    pub fn marker(self) -> char {
        match self {
            NoticeKind::Info => 'i',
            NoticeKind::Warning => '!',
            NoticeKind::Error => 'x',
        }
    }
}

struct Notice {
    kind: NoticeKind,
    text: String,
    created: Instant,
    persistent: bool,
}

/// Small queue of transient notices, mirroring claurst's error/warning slot.
#[derive(Default)]
pub struct NotificationQueue {
    entries: VecDeque<Notice>,
}

impl NotificationQueue {
    /// An empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue an informational notice (expires after 3 s).
    pub fn info(&mut self, text: impl Into<String>) {
        self.push(NoticeKind::Info, text, false, Duration::from_secs(3));
    }

    /// Queue a warning (expires after 5 s).
    pub fn warning(&mut self, text: impl Into<String>) {
        self.push(NoticeKind::Warning, text, false, Duration::from_secs(5));
    }

    /// Queue an error. Errors persist until cleared.
    pub fn error(&mut self, text: impl Into<String>) {
        self.entries.push_back(Notice {
            kind: NoticeKind::Error,
            text: text.into(),
            created: Instant::now(),
            persistent: true,
        });
    }

    fn push(&mut self, kind: NoticeKind, text: impl Into<String>, persistent: bool, ttl: Duration) {
        let _ = ttl;
        self.entries.push_back(Notice {
            kind,
            text: text.into(),
            created: Instant::now(),
            persistent,
        });
    }

    /// Drop expired entries. Returns `true` when something changed.
    pub fn tick(&mut self) -> bool {
        let before = self.entries.len();
        self.entries.retain(|notice| {
            if notice.persistent {
                return true;
            }
            let ttl = match notice.kind {
                NoticeKind::Info => Duration::from_secs(3),
                NoticeKind::Warning => Duration::from_secs(5),
                NoticeKind::Error => Duration::from_secs(10),
            };
            notice.created.elapsed() < ttl
        });
        before != self.entries.len()
    }

    /// The notice currently shown, if any.
    pub fn current(&self) -> Option<(NoticeKind, &str)> {
        self.entries
            .iter()
            .rev()
            .find(|notice| notice.kind != NoticeKind::Info)
            .or_else(|| self.entries.back())
            .map(|notice| (notice.kind, notice.text.as_str()))
    }

    /// Remove everything.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Number of queued notices.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_turn_is_the_last_incomplete_one() {
        let mut session = SessionState::new();
        assert!(!session.is_streaming());

        session.turns.push(Turn {
            prompt: "hi".into(),
            ..Default::default()
        });
        assert!(session.is_streaming());

        session.turns.last_mut().unwrap().complete = true;
        assert!(!session.is_streaming());
        assert!(session.active_turn().is_none());
    }

    #[test]
    fn history_includes_only_completed_turns() {
        let mut session = SessionState::new();
        session.turns.push(Turn {
            prompt: "one".into(),
            reply: "1".into(),
            complete: true,
            ..Default::default()
        });
        session.turns.push(Turn {
            prompt: "two".into(),
            reply: "partial".into(),
            complete: false,
            ..Default::default()
        });

        let history = session.history(Mode::Build);
        // system + one user/assistant pair.
        assert_eq!(history.len(), 3);
        assert_eq!(history[1].text(), "one");
        assert_eq!(history[2].text(), "1");
    }

    #[test]
    fn history_carries_what_a_turn_did_with_tools() {
        let call = ToolCall::new("call-1", "read", serde_json::json!({ "path": "a.txt" }));
        let result = ToolResult::ok(&call, "hello");
        let mut session = SessionState::new();
        session.turns.push(Turn {
            prompt: "read it".into(),
            reply: "it says hello".into(),
            steps: vec![ToolStep {
                call,
                result: Some(result),
                duration_ms: None,
            }],
            complete: true,
            ..Default::default()
        });

        let history = session.history(Mode::Build);
        assert_eq!(
            history.len(),
            5,
            "system, prompt, the call, its result, and the reply"
        );
        assert_eq!(history[2].tool_calls().count(), 1);
        assert_eq!(history[3].tool_results().count(), 1);
        assert_eq!(
            history[3].tool_results().next().expect("a result").output,
            "hello"
        );
        assert_eq!(history[4].text(), "it says hello");
    }

    #[test]
    fn a_call_whose_result_never_arrived_is_left_out_of_history() {
        let call = ToolCall::new("call-1", "read", serde_json::json!({}));
        let mut session = SessionState::new();
        session.turns.push(Turn {
            prompt: "read it".into(),
            reply: "hmm".into(),
            steps: vec![ToolStep::pending(call)],
            complete: true,
            ..Default::default()
        });

        let history = session.history(Mode::Build);
        assert_eq!(history.len(), 3, "a call with no answer is not sent");
        assert_eq!(history[2].tool_calls().count(), 0);
    }

    #[test]
    fn an_empty_reply_is_not_sent_as_a_blank_message() {
        let mut session = SessionState::new();
        session.turns.push(Turn {
            prompt: "hi".into(),
            reply: String::new(),
            complete: true,
            ..Default::default()
        });

        let history = session.history(Mode::Build);
        assert_eq!(history.len(), 2, "system and the prompt");
    }

    #[test]
    fn totals_sum_across_turns() {
        let mut session = SessionState::new();
        session.turns.push(Turn {
            usage: Usage::new(6, 4).with_cache(0, 0),
            cost_usd: 0.5,
            complete: true,
            ..Default::default()
        });
        session.turns.push(Turn {
            usage: Usage::new(5, 0),
            cost_usd: 0.25,
            complete: true,
            ..Default::default()
        });
        assert_eq!(session.total_tokens(), 15);
        assert!((session.total_cost() - 0.75).abs() < f64::EPSILON);

        let usage = session.total_usage();
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 4);
        assert!(!usage.estimated, "nothing in this session was estimated");
    }

    #[test]
    fn the_context_is_measured_by_the_last_reported_turn() {
        let mut session = SessionState::new();
        session.turns.push(Turn {
            prompt: "a".into(),
            usage: Usage::new(10, 5),
            complete: true,
            ..Default::default()
        });
        session.turns.push(Turn {
            prompt: "b".into(),
            usage: Usage::new(100, 50),
            complete: true,
            ..Default::default()
        });

        assert_eq!(
            session.total_tokens(),
            165,
            "the session total keeps summing"
        );
        assert_eq!(
            session.context_tokens(),
            150,
            "only the newest turn says what is in the window"
        );
    }

    #[test]
    fn a_prompt_that_has_not_been_answered_yet_is_estimated() {
        let mut session = SessionState::new();
        session.turns.push(Turn {
            prompt: "a".into(),
            usage: Usage::new(100, 0),
            complete: true,
            ..Default::default()
        });
        // 41 characters is 11 tokens at the usual four-per-token.
        session.turns.push(Turn {
            prompt: "x".repeat(41),
            ..Default::default()
        });

        assert_eq!(session.context_tokens(), 111);
    }

    #[test]
    fn an_unmeasured_session_estimates_its_prompts() {
        let mut session = SessionState::new();
        assert_eq!(session.context_tokens(), 0);

        session.turns.push(Turn {
            prompt: "x".repeat(8),
            ..Default::default()
        });
        assert_eq!(session.context_tokens(), 2);
    }

    #[test]
    fn a_turn_that_reported_nothing_does_not_reset_the_gauge() {
        let mut session = SessionState::new();
        session.turns.push(Turn {
            prompt: "a".into(),
            usage: Usage::new(100, 0),
            complete: true,
            ..Default::default()
        });
        // A turn that failed before reporting usage leaves the last real
        // measurement in place; only its own prompt is added as an estimate.
        session.turns.push(Turn {
            prompt: "b".into(),
            complete: true,
            ..Default::default()
        });

        assert_eq!(session.context_tokens(), 101);
    }

    #[test]
    fn a_session_with_an_estimated_turn_says_so() {
        let mut session = SessionState::new();
        session.turns.push(Turn {
            usage: Usage::new(10, 20),
            ..Default::default()
        });
        session.turns.push(Turn {
            usage: Usage::estimate(40, 80),
            ..Default::default()
        });

        let usage = session.total_usage();
        assert_eq!(usage.total(), 60);
        assert!(usage.estimated, "the sum must admit one guess");
    }

    #[test]
    fn an_empty_session_costs_a_positive_zero() {
        // `Iterator::sum` seeds floats with `-0.0`, which the prompt panel would
        // render as `$-0.0000`.
        let session = SessionState::new();
        assert_eq!(format!("{:.4}", session.total_cost()), "0.0000");
    }

    #[test]
    fn errors_persist_and_have_priority() {
        let mut queue = NotificationQueue::new();
        queue.info("info");
        queue.error("boom");

        // The error wins over the newer-looking info entry.
        assert_eq!(
            queue.current().map(|(kind, _)| kind),
            Some(NoticeKind::Error)
        );
        assert_eq!(queue.len(), 2);

        // Errors never expire on tick.
        queue.tick();
        assert_eq!(queue.current().map(|(_, text)| text), Some("boom"));
        assert_eq!(queue.len(), 2);

        queue.clear();
        assert!(queue.is_empty());
        assert!(queue.current().is_none());
    }

    #[test]
    fn revision_bumps_monotonically() {
        let mut session = SessionState::new();
        let first = session.version;
        session.bump();
        assert_ne!(first, session.version);
    }
}
