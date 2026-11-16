use crate::state::{NoticeKind, ToolStep, Turn};
use solaris_agent::{AgentEvent, ContextUsage, Usage};
/// The conversation plus its streaming state.
#[derive(Debug, Default)]
pub struct SessionProjection {
    pub session_id: String,
    pub sequence: u64,
    pub runs: std::collections::HashMap<u64, usize>,
    target: Option<usize>,
    pub billing: Option<solaris_agent::UsageTotals>,
    pub context: ContextUsage,
    /// Completed and in-flight turns, oldest first.
    pub turns: Vec<Turn>,
    /// Transient status line shown while working.
    pub status: Option<String>,
    /// Monotonic revision, used to invalidate the rendered transcript.
    pub version: u64,
}

impl SessionProjection {
    /// An empty session.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bump the revision so the transcript re-renders.
    pub fn bump(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    /// The turn currently being streamed, if any.
    pub fn active_turn(&self) -> Option<&Turn> {
        self.target
            .and_then(|index| self.turns.get(index))
            .or_else(|| self.turns.last())
            .filter(|turn| !turn.complete)
    }

    /// Mutable access to the turn being streamed.
    pub fn active_turn_mut(&mut self) -> Option<&mut Turn> {
        let index = self
            .target
            .unwrap_or_else(|| self.turns.len().saturating_sub(1));
        let turn = self.turns.get_mut(index)?;
        if turn.complete {
            return None;
        }
        turn.layout_revision = turn.layout_revision.wrapping_add(1);
        Some(turn)
    }

    /// Whether a response is currently streaming.
    pub fn is_streaming(&self) -> bool {
        self.active_turn().is_some()
    }

    /// Total tokens across all turns.
    ///
    /// This is a session total, not a measure of the context window; see
    /// [`Self::context_tokens`] for that.
    pub fn total_tokens(&self) -> u32 {
        self.billing.map_or_else(
            || {
                self.turns
                    .iter()
                    .fold(0u32, |sum, turn| sum.saturating_add(turn.tokens()))
            },
            |total| total.usage.total(),
        )
    }

    /// Occupancy reported by Agent, never inferred from cumulative task usage.
    pub fn context_tokens(&self) -> Option<u32> {
        self.context.tokens
    }

    /// Every bucket summed across all turns, for the stats dialog.
    ///
    /// The result is flagged as an estimate when any turn in it was one.
    pub fn total_usage(&self) -> Usage {
        if let Some(totals) = self.billing {
            return totals.usage;
        }
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
        if let Some(totals) = self.billing {
            return totals.cost_usd;
        }
        // `Iterator::sum` seeds floats with `-0.0`, which would render as
        // `$-0.0000`; adding zero normalises the sign.
        self.turns.iter().map(|turn| turn.cost_usd).sum::<f64>() + 0.0
    }
}

#[derive(Debug, Default)]
pub struct Effects {
    pub notices: Vec<(NoticeKind, String)>,
    pub follow_end: bool,
}
impl Effects {
    fn warning(&mut self, text: impl Into<String>) {
        self.notices.push((NoticeKind::Warning, text.into()));
    }
    fn info(&mut self, text: impl Into<String>) {
        self.notices.push((NoticeKind::Info, text.into()));
    }
    fn error(&mut self, text: impl Into<String>) {
        self.notices.push((NoticeKind::Error, text.into()));
    }
}
impl SessionProjection {
    pub fn restore(
        session_id: String,
        records: Vec<solaris_agent::SessionRecord>,
    ) -> (Self, Effects) {
        let mut state = Self {
            session_id: session_id.clone(),
            ..Default::default()
        };
        let mut notices = Effects::default();
        for record in records {
            match record {
                solaris_agent::SessionRecord::Prompt { text } => state.turns.push(Turn {
                    prompt: text,
                    ..Default::default()
                }),
                solaris_agent::SessionRecord::RuntimeEvent { mut envelope } => {
                    envelope.session_id = session_id.clone();
                    if let Err(error) = state.apply_runtime(envelope) {
                        notices.error(error);
                    }
                }
                solaris_agent::SessionRecord::Event { event } => {
                    state.apply(event);
                }
                solaris_agent::SessionRecord::Cleared => {
                    state.turns.clear();
                    state.runs.clear();
                    state.context = Default::default();
                }
                solaris_agent::SessionRecord::Interrupted => {
                    state.apply(AgentEvent::Error(
                        "session interrupted; verify unfinished tool outcomes before retrying"
                            .into(),
                    ));
                    notices.warning(
                        "session interrupted; verify unfinished tool outcomes before retrying",
                    );
                }
                _ => {}
            }
        }
        state.bump();
        (state, notices)
    }

    fn event_turn_mut(&mut self) -> Option<&mut Turn> {
        if let Some(index) = self.target {
            let turn = self.turns.get_mut(index)?;
            turn.layout_revision = turn.layout_revision.wrapping_add(1);
            Some(turn)
        } else {
            self.active_turn_mut()
        }
    }
    pub fn bind_run(&mut self, id: u64) {
        if !self.runs.contains_key(&id) {
            let index = if self.active_turn().is_some() {
                self.turns.len() - 1
            } else {
                self.turns.push(Turn {
                    prompt: "Background task results".into(),
                    ..Default::default()
                });
                self.turns.len() - 1
            };
            self.runs.insert(id, index);
        }
    }
    pub fn apply_runtime(&mut self, event: solaris_agent::RuntimeEvent) -> Result<Effects, String> {
        if self.session_id.is_empty() {
            self.session_id = event.session_id.clone();
        }
        if !self.session_id.is_empty() && self.session_id != event.session_id {
            return Ok(Effects::default());
        }
        if let Some(sequence) = event.sequence {
            if sequence <= self.sequence {
                return Ok(Effects::default());
            }
            if self.sequence != 0 && sequence != self.sequence + 1 {
                return Err("non-contiguous session events".into());
            }
            self.sequence = sequence;
        }
        if let Some(id) = event.task_id {
            if let Some(prompt) = &event.prompt
                && !self.runs.contains_key(&id)
                && self.active_turn().is_none()
            {
                self.turns.push(Turn {
                    prompt: prompt.clone(),
                    ..Default::default()
                });
            }
            if event.prompt.is_some()
                || matches!(
                    event.event,
                    AgentEvent::TextDelta(_)
                        | AgentEvent::ThinkingDelta(_)
                        | AgentEvent::ToolCall(_)
                )
            {
                self.bind_run(id);
            }
            self.target = self.runs.get(&id).copied();
        } else if matches!(event.event, AgentEvent::Error(_)) {
            let mut effects = Effects::default();
            if let AgentEvent::Error(error) = event.event {
                effects.error(error);
            }
            return Ok(effects);
        }
        let result = self.apply(event.event);
        if let Some(id) = event.task_id
            && let Some(index) = self.target
        {
            self.runs.insert(id, index);
        }
        self.target = None;
        Ok(result)
    }
    pub fn apply(&mut self, event: AgentEvent) -> Effects {
        let mut effects = Effects::default();
        match event {
            AgentEvent::ContextUsageUpdated { context } => self.context = context,
            AgentEvent::ThinkingDelta(chunk) => {
                if let Some(turn) = self.event_turn_mut() {
                    turn.push_thinking(&chunk);
                }
            }
            AgentEvent::TextDelta(chunk) => {
                if let Some(turn) = self.event_turn_mut() {
                    turn.push_text(&chunk);
                }
            }
            AgentEvent::Status(text) => self.status = Some(text),
            AgentEvent::ToolCall(call) => {
                if let Some(turn) = self.event_turn_mut() {
                    turn.push_tool(call);
                }
            }
            AgentEvent::ToolResult {
                result,
                duration_ms,
            } => {
                if let Some(turn) = self.event_turn_mut() {
                    // The newest call with this id that has not been answered
                    // yet, since a model may call the same tool twice.
                    let step = turn
                        .steps
                        .iter_mut()
                        .rev()
                        .find(|step| step.call.id == result.id && !step.is_done());
                    if let Some(step) = step {
                        step.finish(result, duration_ms);
                    }
                }
            }
            AgentEvent::SubagentUsage {
                task_id,
                usage,
                cost_usd,
            } => {
                let index = if task_id.is_empty() {
                    self.target.or_else(|| self.turns.len().checked_sub(1))
                } else {
                    self.turns.iter().rposition(|turn| {
                        turn.steps.iter().any(|step| {
                            step.subagent.as_ref().is_some_and(|child| {
                                child.task_id.as_deref() == Some(task_id.as_str())
                            })
                        })
                    })
                };
                if let Some(turn) = index.and_then(|index| self.turns.get_mut(index)) {
                    turn.subagent_tokens = turn.subagent_tokens.saturating_add(usage.total());
                    // Preserve already consumed child usage even if the parent
                    // fails. A successful TurnComplete replaces this with totals.
                    turn.usage = turn.usage.merge(usage);
                    turn.cost_usd += cost_usd;
                }
            }
            AgentEvent::SubagentSession {
                task_id,
                session_id,
            } => {
                if let Some(step) = self
                    .turns
                    .iter_mut()
                    .rev()
                    .find_map(|turn| turn.subagent_mut(&task_id))
                    && let Some(child) = &mut step.subagent
                {
                    child.session_id = Some(session_id);
                }
            }
            AgentEvent::SubagentStarted { call_id, task_id } => {
                if let Some(turn) = self.event_turn_mut()
                    && let Some(child) = turn
                        .steps
                        .iter_mut()
                        .rev()
                        .find(|step| step.call.id == call_id)
                        .and_then(|step| step.subagent.as_mut())
                {
                    child.task_id = Some(task_id);
                }
            }
            AgentEvent::SubagentToolCall { task_id, call } => {
                if let Some(child) = self
                    .turns
                    .iter_mut()
                    .rev()
                    .find_map(|turn| turn.subagent_mut(&task_id))
                    .and_then(|step| step.subagent.as_mut())
                {
                    child.steps.push(ToolStep::pending(call));
                }
            }
            AgentEvent::SubagentToolResult {
                task_id,
                result,
                duration_ms,
            } => {
                if let Some(child) = self
                    .turns
                    .iter_mut()
                    .rev()
                    .find_map(|turn| turn.subagent_mut(&task_id))
                    .and_then(|step| step.subagent.as_mut())
                    && let Some(step) = child
                        .steps
                        .iter_mut()
                        .rev()
                        .find(|step| step.call.id == result.id && !step.is_done())
                {
                    step.finish(result, duration_ms);
                }
            }
            AgentEvent::SubagentComplete {
                task_id,
                output,
                is_error,
                duration_ms,
            } => {
                let status = if is_error { "failed" } else { "complete" };
                let notice = format!("background subagent {task_id}: {status}");
                if is_error {
                    effects.warning(notice);
                } else {
                    effects.info(notice);
                }
                if let Some(step) = self
                    .turns
                    .iter_mut()
                    .rev()
                    .find_map(|turn| turn.subagent_mut(&task_id))
                {
                    let result = if is_error {
                        solaris_agent::ToolResult::error(&step.call, output)
                    } else {
                        solaris_agent::ToolResult::ok(&step.call, output)
                    };
                    if let Some(child) = &mut step.subagent {
                        child.finish(result, duration_ms);
                    }
                }
            }
            AgentEvent::TurnComplete { usage, cost_usd } => {
                if let Some(turn) = self.event_turn_mut() {
                    turn.complete = true;
                    turn.usage = usage;
                    turn.cost_usd = cost_usd;
                }
                self.status = None;
            }
            // The model ran out of room, so anything it asked for in the same
            // message was refused. Saying so explains why the next thing it does
            // may look like a repetition.
            AgentEvent::OutputTruncated => {
                effects.warning("the reply hit the output limit; its tool calls were refused");
            }
            // The conversation was summarized to make room. It is worth a
            // banner because the model suddenly remembers less, and worth a line
            // in the transcript because a session that forgot things should say
            // so where it can be read back later.
            AgentEvent::Compacted { dropped } => {
                if let Some(turn) = self.event_turn_mut() {
                    turn.compacted += dropped;
                }
                effects.warning(format!(
                    "summarized {dropped} earlier messages to make room in the context window"
                ));
            }
            // A message the user steered in has been read. Recorded here rather
            // than when it was typed, so it lands in the turn that read it.
            AgentEvent::Steered { text } => {
                if let Some(turn) = self.event_turn_mut() {
                    turn.push_steering(text);
                }
            }
            // A queued prompt is now being run as a turn of its own. The turn it
            // arrived during is over, so it is closed and a new one opened with
            // this prompt: everything that streams from here belongs to it.
            AgentEvent::FollowUp { prompt } => {
                if let Some(turn) = self.event_turn_mut() {
                    turn.complete = true;
                    if turn.reply.is_empty() {
                        turn.push_text("_(superseded by the queued prompt)_");
                    }
                }
                self.turns.push(Turn {
                    prompt,
                    ..Default::default()
                });
                if self.target.is_some() {
                    self.target = Some(self.turns.len() - 1);
                }
                effects.follow_end = true;
            }
            AgentEvent::Error(message) => {
                if let Some(turn) = self.event_turn_mut() {
                    turn.complete = true;
                    if turn.reply.trim().is_empty() {
                        turn.push_text(&format!("**Error:** {message}"));
                    }
                }
                self.status = None;
                effects.error(message);
            }
        }
        self.bump();
        effects
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn envelope(
        sequence: u64,
        run: u64,
        prompt: Option<&str>,
        event: AgentEvent,
    ) -> solaris_agent::RuntimeEvent {
        solaris_agent::RuntimeEvent {
            session_id: "session".into(),
            sequence: Some(sequence),
            task_id: Some(run),
            prompt: prompt.map(str::to_owned),
            event,
        }
    }
    #[test]
    fn live_and_restored_projection_share_identity_and_followup_routing() {
        let events = vec![
            envelope(
                1,
                10,
                Some("question"),
                AgentEvent::Status("working".into()),
            ),
            envelope(2, 10, None, AgentEvent::TextDelta("first".into())),
            envelope(
                3,
                10,
                None,
                AgentEvent::FollowUp {
                    prompt: "next".into(),
                },
            ),
            envelope(4, 10, None, AgentEvent::TextDelta("second".into())),
            envelope(
                5,
                10,
                None,
                AgentEvent::TurnComplete {
                    usage: Default::default(),
                    cost_usd: 0.0,
                },
            ),
        ];
        let mut live = SessionProjection::default();
        for event in &events {
            live.apply_runtime(event.clone()).unwrap();
        }
        let records = events
            .into_iter()
            .map(|envelope| solaris_agent::SessionRecord::RuntimeEvent { envelope })
            .collect();
        let (restored, _) = SessionProjection::restore("session".into(), records);
        assert_eq!(live.turns, restored.turns);
        assert_eq!(live.sequence, restored.sequence);
        assert_eq!(live.turns[0].prompt, "question");
        assert_eq!(live.turns[1].reply, "second");
    }
    #[test]
    fn late_terminal_event_updates_its_run_instead_of_the_active_turn() {
        let mut state = SessionProjection::default();
        state
            .apply_runtime(envelope(
                1,
                1,
                Some("first"),
                AgentEvent::TextDelta("first answer".into()),
            ))
            .unwrap();
        state
            .apply_runtime(envelope(
                2,
                1,
                None,
                AgentEvent::TurnComplete {
                    usage: Default::default(),
                    cost_usd: 0.0,
                },
            ))
            .unwrap();
        state
            .apply_runtime(envelope(
                3,
                2,
                Some("second"),
                AgentEvent::TextDelta("second answer".into()),
            ))
            .unwrap();
        state
            .apply_runtime(envelope(4, 1, None, AgentEvent::Error("late error".into())))
            .unwrap();
        assert!(!state.turns[1].complete);
        assert_eq!(state.turns[1].reply, "second answer");
        assert_eq!(state.sequence, 4);
    }
    #[test]
    fn late_background_usage_is_attributed_to_its_job_owner() {
        let mut state = SessionProjection::default();
        let mut step = ToolStep::pending(solaris_agent::ToolCall::new(
            "call",
            "subagent",
            serde_json::json!({"agent":"explore","prompt":"inspect"}),
        ));
        step.subagent.as_mut().unwrap().task_id = Some("job-old".into());
        state.turns.push(Turn {
            prompt: "old".into(),
            steps: vec![step],
            complete: true,
            ..Default::default()
        });
        state.turns.push(Turn {
            prompt: "current".into(),
            ..Default::default()
        });
        state.runs.insert(2, 1);
        state
            .apply_runtime(envelope(
                1,
                2,
                None,
                AgentEvent::SubagentUsage {
                    task_id: "job-old".into(),
                    usage: Usage::new(2, 1),
                    cost_usd: 0.5,
                },
            ))
            .unwrap();
        assert_eq!(state.turns[0].usage.total(), 3);
        assert_eq!(state.turns[0].cost_usd, 0.5);
        assert_eq!(state.turns[1].usage.total(), 0);
        assert!(!state.turns[1].complete);
    }

    #[test]
    fn duplicate_and_foreign_events_do_not_advance_or_corrupt_the_projection() {
        let mut state = SessionProjection::default();
        let event = envelope(
            1,
            1,
            Some("question"),
            AgentEvent::TextDelta("answer".into()),
        );
        state.apply_runtime(event.clone()).unwrap();
        state.apply_runtime(event.clone()).unwrap();
        let mut foreign = event;
        foreign.session_id = "different".into();
        foreign.sequence = Some(2);
        state.apply_runtime(foreign).unwrap();
        assert_eq!(state.sequence, 1);
        assert_eq!(state.turns.len(), 1);
        assert_eq!(state.turns[0].reply, "answer");
    }
}
