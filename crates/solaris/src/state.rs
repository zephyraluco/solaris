//! Application state: routes, the session transcript, and notifications.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use solaris_core::Mode;

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
    /// Tokens reported for this turn.
    pub tokens: u32,
    /// Cost reported for this turn.
    pub cost_usd: f64,
    /// Whether streamed events for this turn finished.
    pub complete: bool,
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

    /// Total tokens reported across all turns.
    pub fn total_tokens(&self) -> u32 {
        self.turns.iter().map(|turn| turn.tokens).sum()
    }

    /// Total cost reported across all turns.
    pub fn total_cost(&self) -> f64 {
        // `Iterator::sum` seeds floats with `-0.0`, which would render as
        // `$-0.0000`; adding zero normalises the sign.
        self.turns.iter().map(|turn| turn.cost_usd).sum::<f64>() + 0.0
    }

    /// Completed turns flattened into backend history.
    pub fn history(&self, mode: Mode) -> Vec<solaris_core::Message> {
        let mut history = vec![solaris_core::Message::system(format!(
            "You are solaris, a terminal assistant running in {} mode.",
            mode.label().to_lowercase()
        ))];
        for turn in self.turns.iter().filter(|turn| turn.complete) {
            history.push(solaris_core::Message::user(turn.prompt.clone()));
            history.push(solaris_core::Message::assistant(turn.reply.clone()));
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
        assert_eq!(history[1].content, "one");
        assert_eq!(history[2].content, "1");
    }

    #[test]
    fn totals_sum_across_turns() {
        let mut session = SessionState::new();
        session.turns.push(Turn {
            tokens: 10,
            cost_usd: 0.5,
            complete: true,
            ..Default::default()
        });
        session.turns.push(Turn {
            tokens: 5,
            cost_usd: 0.25,
            complete: true,
            ..Default::default()
        });
        assert_eq!(session.total_tokens(), 15);
        assert!((session.total_cost() - 0.75).abs() < f64::EPSILON);
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
