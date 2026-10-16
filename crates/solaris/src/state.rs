//! Application state: routes, the session transcript, and notifications.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use solaris_agent::{ToolCall, ToolResult, Usage, tokens_for_characters};
use solaris_tui::components::CollapsibleState;

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
    /// Reusable disclosure state; record updates never reset the user's choice.
    pub disclosure: CollapsibleState,
    /// Child activity belongs to one collapsible row, not top-level tool steps.
    pub subagent: Option<Box<SubagentStep>>,
}

/// Display state for one child, independent of its launch acknowledgement.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubagentStep {
    /// Runtime identity used to route concurrent child tool events.
    pub task_id: Option<String>,
    /// Tool calls inside the child, never mixed into the parent's step list.
    pub steps: Vec<ToolStep>,
    /// The child's final report or failure, not the background launch receipt.
    pub result: Option<ToolResult>,
    /// Total child duration, including its wait for an execution slot.
    pub duration_ms: Option<u64>,
}

impl SubagentStep {
    /// Finish without changing whether the user has expanded the row.
    pub fn finish(&mut self, result: ToolResult, duration_ms: u64) {
        self.result = Some(result);
        self.duration_ms = Some(duration_ms);
    }
}

impl ToolStep {
    /// A step whose call has not run yet.
    pub fn pending(call: ToolCall) -> Self {
        let subagent = (call.name == "subagent").then(Box::default);
        Self {
            call,
            result: None,
            duration_ms: None,
            disclosure: CollapsibleState::default(),
            subagent,
        }
    }

    /// Record what the call produced.
    pub fn finish(&mut self, result: ToolResult, duration_ms: u64) {
        if let Some(child) = &mut self.subagent {
            let background = self.call.input["background"].as_bool() == Some(true);
            if background && !result.is_error {
                // Also accept a launch acknowledgement without a Started event
                // for callers replaying older transcripts. It is not completion.
                if child.task_id.is_none() {
                    child.task_id = serde_json::from_str::<serde_json::Value>(&result.output)
                        .ok()
                        .and_then(|value| value["task_id"].as_str().map(str::to_string));
                }
            } else {
                child.finish(result.clone(), duration_ms);
            }
        }
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

/// A message the user added while a turn was already running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteeringNote {
    /// What the user typed.
    pub text: String,
    /// How many of the turn's calls had finished when it arrived.
    ///
    /// It is recorded because where the message landed is the whole point of
    /// steering: a correction that arrived after the third call is not the same
    /// as one that arrived after the first, and the transcript has to show which
    /// it was.
    pub after_steps: usize,
}

/// One prompt/response exchange.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Turn {
    /// The user's prompt.
    pub prompt: String,
    /// Messages the user steered in while this turn was running.
    pub steering: Vec<SteeringNote>,
    /// The assistant's visible reply.
    pub reply: String,
    /// The assistant's thinking trace.
    pub thinking: String,
    /// Whether the thinking block is expanded.
    pub thinking_expanded: bool,
    /// The tool calls this turn made, in the order they were made.
    pub steps: Vec<ToolStep>,
    /// How many earlier messages were summarized away to make room, if any.
    pub compacted: usize,
    /// Tokens the turn consumed, as the backend reported or estimated them.
    pub usage: Usage,
    /// Tokens spent in child contexts, already included in `usage` on completion.
    pub subagent_tokens: u32,
    /// Cost reported for this turn.
    pub cost_usd: f64,
    /// Whether streamed events for this turn finished.
    pub complete: bool,
}

impl Turn {
    /// Locate child activity by its runtime ID, even after its launch returned.
    pub fn subagent_mut(&mut self, task_id: &str) -> Option<&mut ToolStep> {
        self.steps.iter_mut().rev().find(|step| {
            step.subagent
                .as_ref()
                .is_some_and(|child| child.task_id.as_deref() == Some(task_id))
        })
    }

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
            .rposition(|turn| turn.complete && turn.tokens() > turn.subagent_tokens)
        {
            Some(index) => {
                let trailing: usize = self.turns[index + 1..]
                    .iter()
                    .map(|turn| turn.prompt.chars().count())
                    .sum();
                self.turns[index]
                    .tokens()
                    .saturating_sub(self.turns[index].subagent_tokens)
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
    fn child_usage_is_billed_but_does_not_fill_the_parent_context_window() {
        let mut session = SessionState::new();
        session.turns.push(Turn {
            usage: Usage::new(120, 30),
            subagent_tokens: 100,
            cost_usd: 1.5,
            complete: true,
            ..Default::default()
        });
        assert_eq!(session.total_tokens(), 150);
        assert_eq!(session.total_cost(), 1.5);
        assert_eq!(session.context_tokens(), 50);
    }

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
