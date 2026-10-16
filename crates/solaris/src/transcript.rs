//! Builds the styled transcript lines from session state.
//!
//! The transcript is rebuilt only when the session revision, terminal size,
//! theme, or spinner frame changes, so scrolling and idle frames are free —
//! except while it is empty, when the welcome box's animated companion needs a
//! fresh frame every tick.

use std::collections::BTreeMap;

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use solaris_tui::components::markdown::{MarkdownStyle, render_markdown};
use solaris_tui::components::welcome::{WelcomeData, WelcomeStyles, render_welcome};
use solaris_tui::components::{Collapsible, CollapsibleStatus, CollapsibleStyles};
use solaris_tui::theme::Theme;
use solaris_tui::util::{truncate_to_width, wrap_text};

use crate::state::{SessionState, ToolStep, Turn};

/// Spinner glyphs shared with the reusable TUI activity indicators.
pub use solaris_tui::components::loader::SPINNER;

/// A virtual disclosure header; the target identifies an application record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollapsibleHeader {
    pub row: usize,
    pub target: (usize, usize),
    pub marker_offset: u16,
    pub body_rows: usize,
}

/// Cached transcript lines.
#[derive(Default)]
pub struct TranscriptView {
    cache_width: u16,
    cache_height: u16,
    cache_version: u64,
    cache_theme: String,
    cache_spinner: u16,
    /// Whether the cached lines were the welcome box, so the first turn
    /// invalidates them even if the session revision was not bumped.
    cache_empty: bool,
    lines: Vec<Line<'static>>,
    /// Header geometry for any collapsible record, not just child agents.
    collapsible_headers: Vec<CollapsibleHeader>,
    initialized: bool,
}

impl TranscriptView {
    /// A view with no cached lines.
    pub fn new() -> Self {
        Self {
            cache_version: u64::MAX,
            cache_spinner: u16::MAX,
            ..Default::default()
        }
    }

    /// Rendered transcript lines, rebuilt only when the inputs change.
    ///
    /// While the transcript is empty it is rebuilt every frame instead: the
    /// welcome box carries the animated companion and is only a screenful.
    pub fn lines(
        &mut self,
        session: &SessionState,
        area: Rect,
        theme: &Theme,
        spinner_frame: Option<u16>,
        welcome: &WelcomeData,
    ) -> &[Line<'static>] {
        let spinner_key = spinner_frame.unwrap_or(u16::MAX);
        let empty = session.turns.is_empty();
        let stale = !self.initialized
            || empty
            || self.cache_empty != empty
            || self.cache_width != area.width
            || self.cache_height != area.height
            || self.cache_version != session.version
            || self.cache_theme != theme.name
            || self.cache_spinner != spinner_key;

        if stale {
            self.collapsible_headers.clear();
            self.lines = build_with_headers(
                session,
                area,
                theme,
                spinner_frame,
                welcome,
                &mut self.collapsible_headers,
            );
            self.cache_width = area.width;
            self.cache_height = area.height;
            self.cache_version = session.version;
            self.cache_theme = theme.name.clone();
            self.cache_spinner = spinner_key;
            self.cache_empty = empty;
            self.initialized = true;
        }

        &self.lines
    }

    /// Resolve a title in the rendered transcript, before the viewport scroll.
    pub fn collapsible_at(&self, row: usize) -> Option<CollapsibleHeader> {
        self.collapsible_headers
            .iter()
            .find(|header| header.row == row)
            .copied()
    }

    /// Force a rebuild on the next call.
    pub fn invalidate(&mut self) {
        self.initialized = false;
    }
}

/// Build every transcript line for `session`.
///
/// An empty session renders the welcome box in the space the conversation will
/// take, so it scrolls away with the first turn.
pub fn build(
    session: &SessionState,
    area: Rect,
    theme: &Theme,
    spinner_frame: Option<u16>,
    welcome: &WelcomeData,
) -> Vec<Line<'static>> {
    build_with_headers(
        session,
        area,
        theme,
        spinner_frame,
        welcome,
        &mut Vec::new(),
    )
}

fn build_with_headers(
    session: &SessionState,
    area: Rect,
    theme: &Theme,
    spinner_frame: Option<u16>,
    welcome: &WelcomeData,
    headers: &mut Vec<CollapsibleHeader>,
) -> Vec<Line<'static>> {
    let width = area.width.max(8) as usize;
    let markdown = MarkdownStyle::from_theme(theme);
    let mut out: Vec<Line<'static>> = Vec::new();

    if session.turns.is_empty() {
        let styles = WelcomeStyles::from_theme(theme);
        out.extend(render_welcome(welcome, area, &styles));
        return out;
    }

    for (index, turn) in session.turns.iter().enumerate() {
        if index > 0 {
            out.push(Line::default());
        }
        push_prompt(&mut out, &turn.prompt, width, theme);
        push_compaction(&mut out, turn, theme);
        push_steps(&mut out, turn, width, theme, spinner_frame, index, headers);
        push_thinking(&mut out, turn, width, theme);
        if !turn.reply.trim().is_empty() {
            out.push(Line::default());
            out.extend(render_markdown(&turn.reply, width as u16, &markdown));
        }

        if !turn.complete {
            let glyph = spinner_frame
                .map(|frame| SPINNER[frame as usize % SPINNER.len()])
                .unwrap_or(SPINNER[0]);
            out.push(Line::default());
            out.push(Line::from(vec![
                Span::styled(format!("{glyph} "), Style::default().fg(theme.accent)),
                Span::styled("generating…", Style::default().fg(theme.muted)),
            ]));
        }
    }

    out
}

fn push_prompt(out: &mut Vec<Line<'static>>, prompt: &str, width: usize, theme: &Theme) {
    let wrap = width.saturating_sub(2).max(4);
    let style = Style::default().fg(theme.user).add_modifier(Modifier::BOLD);
    for (index, segment) in wrap_text(prompt, wrap).into_iter().enumerate() {
        let marker = if index == 0 { "› " } else { "  " };
        out.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(theme.user)),
            Span::styled(segment, style),
        ]));
    }
}

fn push_thinking(out: &mut Vec<Line<'static>>, turn: &Turn, width: usize, theme: &Theme) {
    if turn.thinking.trim().is_empty() {
        return;
    }

    let dim = Style::default().fg(theme.dim);
    let italic = dim.add_modifier(Modifier::ITALIC);
    let marker = if turn.thinking_expanded { "▾" } else { "▸" };

    out.push(Line::from(vec![
        Span::styled(format!("{marker} thinking "), dim),
        Span::styled(summarize(&turn.thinking), italic),
        Span::styled("  ctrl+o", Style::default().fg(theme.border)),
    ]));

    if turn.thinking_expanded {
        let wrap = width.saturating_sub(2).max(4);
        for segment in wrap_text(&turn.thinking, wrap) {
            out.push(Line::from(vec![
                Span::styled("  ", dim),
                Span::styled(segment, italic),
            ]));
        }
    }
}

/// One line per tool call, so a turn says what it did rather than only what it
/// concluded.
///
/// The line is the tool, its arguments and how long it took. A failed call also
/// shows the first line of what it said, because that is the part a person
/// needs; the full text went to the model either way.
fn push_steps(
    out: &mut Vec<Line<'static>>,
    turn: &Turn,
    width: usize,
    theme: &Theme,
    spinner_frame: Option<u16>,
    turn_index: usize,
    headers: &mut Vec<CollapsibleHeader>,
) {
    // Messages the user steered in are drawn where they arrived, among the
    // calls they followed: where the correction landed is why the model's next
    // move changed, so collecting them at the end would lose the point of them.
    let mut notes = turn.steering.iter().peekable();
    while let Some(note) = notes.next_if(|note| note.after_steps == 0) {
        push_steering(out, &note.text, width, theme);
    }

    let mut finished = 0usize;
    for (step_index, step) in turn.steps.iter().enumerate() {
        if let Some(view) = subagent_view(step, turn.complete, theme, spinner_frame) {
            headers.push(CollapsibleHeader {
                row: out.len(),
                target: (turn_index, step_index),
                marker_offset: view.marker_offset(),
                body_rows: step
                    .subagent
                    .as_ref()
                    .map_or(0, |child| child.steps.len().max(1)),
            });
            out.extend(view.lines(width as u16));
        } else {
            push_tool_step(out, step, width, theme);
        }

        // A note that followed this call is drawn after it, once the call is
        // known to have finished.
        if step.is_done() {
            finished += 1;
            while let Some(note) = notes.next_if(|note| note.after_steps <= finished) {
                push_steering(out, &note.text, width, theme);
            }
        }
    }

    // A note that arrived after the last call — while the answer streamed, say.
    for note in notes {
        push_steering(out, &note.text, width, theme);
    }
}

fn push_tool_step(out: &mut Vec<Line<'static>>, step: &ToolStep, width: usize, theme: &Theme) {
    let (glyph, colour) = match step.result.as_ref() {
        None => ("⋯", theme.muted),
        Some(result) if result.is_error => ("✗", theme.error),
        Some(_) => ("✓", theme.success),
    };

    // Arguments are what makes a call recognisable at a glance. Values are
    // printed as compact JSON, so this is always one line.
    let mut detail = format!("{}", step.call.input);
    if let Some(result) = step.result.as_ref().filter(|result| result.is_error) {
        detail.push_str(" — ");
        detail.push_str(&summarize(&result.output));
    }
    if let Some(elapsed) = step.duration_ms() {
        detail.push_str(&format!("  ·  {elapsed}ms"));
    }

    // Indent, glyph, space, the tool name, then two spaces before the detail.
    let used = 6 + step.call.name.chars().count();
    out.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(format!("{glyph} "), Style::default().fg(colour)),
        Span::styled(
            step.call.name.clone(),
            Style::default()
                .fg(theme.assistant)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  {}",
                truncate_to_width(&detail, width.saturating_sub(used).max(8), "…")
            ),
            Style::default().fg(theme.muted),
        ),
    ]));
}

fn spinner(frame: Option<u16>) -> &'static str {
    SPINNER[usize::from(frame.unwrap_or(0)) % SPINNER.len()]
}

fn subagent_view(
    step: &ToolStep,
    turn_complete: bool,
    theme: &Theme,
    spinner_frame: Option<u16>,
) -> Option<Collapsible> {
    let child = step.subagent.as_ref()?;
    let running = child.result.is_none() && !turn_complete;
    let interrupted = child.result.is_none() && turn_complete;
    let failed = interrupted || child.result.as_ref().is_some_and(|result| result.is_error);
    let label = match step.call.input["agent"].as_str() {
        Some("explore") if running => "Exploring",
        Some("explore") => "Explored",
        Some("general") => "General",
        _ => "Subagent",
    };
    let mut detail = format!("{label}: {}", subagent_counts(&child.steps, running));
    if interrupted {
        detail.push_str(" — interrupted");
    } else if let Some(result) = child.result.as_ref().filter(|result| result.is_error) {
        detail.push_str(" — ");
        detail.push_str(&summarize(&result.output));
    }
    let mut view = Collapsible::new(detail)
        .with_state(step.disclosure)
        .styles(CollapsibleStyles::from_theme(theme))
        .indentation(2, 4);
    view.set_status(if failed {
        CollapsibleStatus::Failed
    } else if running {
        CollapsibleStatus::Running
    } else {
        CollapsibleStatus::Idle
    });
    view.set_spinner_frame(spinner_frame.unwrap_or(0));
    if step.disclosure.is_expanded() {
        let mut body = Vec::new();
        for tool in &child.steps {
            push_child_step(&mut body, tool, running, theme, spinner_frame);
        }
        if child.steps.is_empty() {
            body.push(Line::styled(
                "No tool calls.",
                Style::default().fg(theme.dim),
            ));
        }
        view.set_content(body);
    }
    Some(view)
}

fn subagent_counts(steps: &[ToolStep], running: bool) -> String {
    let mut counts = BTreeMap::<(&str, &str), usize>::new();
    for step in steps {
        let label = match step.call.name.as_str() {
            "read" => ("read", "reads"),
            "webfetch" => ("fetch", "fetches"),
            "grep" | "find" | "glob" | "websearch" => ("search", "searches"),
            "ls" => ("listing", "listings"),
            "edit" | "patch" => ("edit", "edits"),
            "write" => ("write", "writes"),
            "shell" | "bash" | "powershell" => ("command", "commands"),
            _ => ("tool call", "tool calls"),
        };
        *counts.entry(label).or_default() += 1;
    }
    if counts.is_empty() {
        return if running {
            "starting…"
        } else {
            "no tool calls"
        }
        .to_string();
    }
    counts
        .into_iter()
        .map(|((single, plural), count)| {
            format!("{count} {}", if count == 1 { single } else { plural })
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn push_child_step(
    out: &mut Vec<Line<'static>>,
    step: &ToolStep,
    running: bool,
    theme: &Theme,
    spinner_frame: Option<u16>,
) {
    let glyph = if running && !step.is_done() {
        spinner(spinner_frame)
    } else {
        "→"
    };
    let label = match step.call.name.as_str() {
        "read" => "Read",
        "grep" => "Grep",
        "find" => "Find",
        "ls" => "List",
        "webfetch" => "WebFetch",
        "websearch" => "WebSearch",
        name => name,
    };
    let input = &step.call.input;
    let mut detail = ["path", "url", "command", "pattern"]
        .iter()
        .find_map(|key| input[*key].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| input.to_string());
    if step.call.name == "read" {
        let offset = input["offset"].as_u64();
        let limit = input["limit"].as_u64();
        if offset.is_some() || limit.is_some() {
            let start = offset.unwrap_or(1);
            detail.push_str(&format!(":{start}"));
            if let Some(limit) = limit {
                detail.push_str(&format!(
                    "-{}",
                    start.saturating_add(limit.saturating_sub(1))
                ));
            }
        }
    }
    if let Some(result) = step.result.as_ref().filter(|result| result.is_error) {
        detail.push_str(" — ");
        detail.push_str(&summarize(&result.output));
    }
    let detail: String = detail
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let colour = if step.is_error() {
        theme.error
    } else {
        theme.muted
    };
    out.push(Line::styled(
        format!("{glyph} {label} {detail}"),
        Style::default().fg(colour),
    ));
}

/// A message the user added while the turn was running.
fn push_steering(out: &mut Vec<Line<'static>>, text: &str, width: usize, theme: &Theme) {
    let label = "steered";
    let used = 6 + label.chars().count();
    out.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled("↳ ", Style::default().fg(theme.accent)),
        Span::styled(
            label.to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  {}",
                truncate_to_width(&summarize(text), width.saturating_sub(used).max(8), "…")
            ),
            Style::default().fg(theme.fg),
        ),
    ]));
}

/// A note that earlier messages were summarized away during this turn.
///
/// It is drawn before the calls because that is where it happened — the model
/// read less from the first round onwards — and because a turn that forgot
/// things has to say so somewhere it can be read back.
fn push_compaction(out: &mut Vec<Line<'static>>, turn: &Turn, theme: &Theme) {
    if turn.compacted == 0 {
        return;
    }
    out.push(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled("⌁ ", Style::default().fg(theme.muted)),
        Span::styled(
            format!(
                "{} earlier messages summarized to make room",
                turn.compacted
            ),
            Style::default().fg(theme.muted),
        ),
    ]));
}

/// First line of `text`, trimmed and shortened for the collapsed thinking label.
pub fn summarize(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let trimmed = line.trim();
    let mut out: String = trimmed.chars().take(70).collect();
    if trimmed.chars().count() > 70 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ToolStep;
    use solaris_agent::{ToolCall, ToolResult};

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn session_with(turn: Turn) -> SessionState {
        let mut session = SessionState::new();
        session.turns.push(turn);
        session
    }

    /// A welcome box that only carries a greeting, so assertions stay readable.
    fn welcome() -> WelcomeData {
        WelcomeData {
            app_name: "solaris".to_string(),
            version: "0.1.0".to_string(),
            greeting: "Welcome back zeal!".to_string(),
            hint: "/help for commands".to_string(),
            mascot: vec!["  /\\_/\\".to_string()],
            tip: "Tab switches mode.".to_string(),
            recent: Vec::new(),
        }
    }

    fn area(width: u16, height: u16) -> Rect {
        Rect::new(0, 0, width, height)
    }

    #[test]
    fn empty_session_shows_the_welcome_box() {
        let session = SessionState::new();
        let lines = build(&session, area(80, 20), &Theme::dark(), None, &welcome());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(text.contains("solaris v0.1.0"), "{text}");
        assert!(text.contains("Welcome back zeal!"), "{text}");
        assert!(text.contains("/\\_/\\"), "the companion is missing: {text}");
        assert!(text.contains("Tips for getting started"), "{text}");
        assert!(text.contains("No recent activity"), "{text}");
    }

    #[test]
    fn tool_steps_are_rendered_beside_the_turn() {
        let call = ToolCall::new("call-1", "read", serde_json::json!({ "path": "a.txt" }));
        let mut done = ToolStep::pending(call.clone());
        done.finish(ToolResult::ok(&call, "hello"), 7);

        let failing = ToolCall::new("call-2", "bash", serde_json::json!({ "command": "false" }));
        let mut failed = ToolStep::pending(failing.clone());
        failed.finish(ToolResult::error(&failing, "exit code 1"), 3);

        let running = ToolStep::pending(ToolCall::new(
            "call-3",
            "grep",
            serde_json::json!({ "pattern": "x" }),
        ));

        let session = session_with(Turn {
            prompt: "do it".to_string(),
            reply: "done".to_string(),
            steps: vec![done, failed, running],
            ..Default::default()
        });

        let lines = build(&session, area(80, 30), &Theme::dark(), None, &welcome());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(text.contains("✓ read"), "{text}");
        assert!(
            text.contains(r#"{"path":"a.txt"}"#),
            "arguments are shown: {text}"
        );
        assert!(text.contains("7ms"), "timing is shown: {text}");

        assert!(text.contains("✗ bash"), "{text}");
        assert!(
            text.contains("exit code 1"),
            "a failure says why, not just that it failed: {text}"
        );

        assert!(text.contains("⋯ grep"), "a call in flight: {text}");
    }

    fn subagent_step(expanded: bool, complete: bool) -> ToolStep {
        let call = ToolCall::new(
            "delegate",
            "subagent",
            serde_json::json!({
                "agent": "explore", "prompt": "Investigate the source",
            }),
        );
        let mut step = ToolStep::pending(call.clone());
        step.disclosure.set_expanded(expanded);
        let child = step.subagent.as_mut().unwrap();
        for (name, input) in [
            (
                "webfetch",
                serde_json::json!({"url":"https://example.com/docs"}),
            ),
            (
                "read",
                serde_json::json!({"path":"src/agent.rs", "offset":175, "limit":255}),
            ),
            ("grep", serde_json::json!({"pattern":"SubagentRunner"})),
        ] {
            let call = ToolCall::new(name, name, input);
            let mut nested = ToolStep::pending(call.clone());
            if complete {
                nested.finish(ToolResult::ok(&call, "private tool data"), 1);
            }
            child.steps.push(nested);
        }
        if complete {
            step.finish(ToolResult::ok(&call, "final report"), 10);
        }
        step
    }

    #[test]
    fn subagents_default_to_one_collapsed_summary_with_a_right_arrow() {
        let session = session_with(Turn {
            steps: vec![subagent_step(false, true)],
            complete: true,
            ..Default::default()
        });
        let lines = build(&session, area(90, 20), &Theme::dark(), None, &welcome());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            text.contains("→ Explored: 1 fetch, 1 read, 1 search"),
            "{text}"
        );
        assert!(
            !text.contains("src/agent.rs") && !text.contains("example.com"),
            "{text}"
        );
        assert!(!text.contains("private tool data"), "{text}");
    }

    #[test]
    fn a_running_collapsed_subagent_animates_instead_of_showing_a_right_arrow() {
        let session = session_with(Turn {
            steps: vec![subagent_step(false, false)],
            ..Default::default()
        });
        for (frame, glyph) in [(0, "⠋"), (1, "⠙")] {
            let lines = build(
                &session,
                area(90, 20),
                &Theme::dark(),
                Some(frame),
                &welcome(),
            );
            let title = lines
                .iter()
                .map(line_text)
                .find(|line| line.contains("Exploring:"))
                .unwrap();
            assert!(
                title.starts_with(&format!("  {glyph} Exploring:")),
                "{title}"
            );
        }
    }

    #[test]
    fn expanded_subagents_show_a_down_arrow_and_indented_child_calls() {
        let session = session_with(Turn {
            steps: vec![subagent_step(true, true)],
            complete: true,
            ..Default::default()
        });
        let lines = build(&session, area(90, 20), &Theme::dark(), None, &welcome());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("↓ Explored:"), "{text}");
        assert!(
            text.contains("    → WebFetch https://example.com/docs"),
            "{text}"
        );
        assert!(text.contains("    → Read src/agent.rs:175-429"), "{text}");
        assert!(text.contains("    → Grep SubagentRunner"), "{text}");
    }

    #[test]
    fn expanding_a_running_subagent_keeps_the_down_arrow_and_animates_pending_tools() {
        let session = session_with(Turn {
            steps: vec![subagent_step(true, false)],
            ..Default::default()
        });
        let lines = build(&session, area(90, 20), &Theme::dark(), Some(1), &welcome());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("↓ Exploring:"), "{text}");
        assert!(text.contains("    ⠙ Read "), "{text}");
    }

    #[test]
    fn an_interrupted_turn_stops_all_child_spinners() {
        let session = session_with(Turn {
            steps: vec![subagent_step(true, false)],
            complete: true,
            ..Default::default()
        });
        let lines = build(&session, area(90, 20), &Theme::dark(), None, &welcome());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("interrupted"), "{text}");
        assert!(text.contains("    → Read "), "{text}");
        assert!(!SPINNER.iter().any(|glyph| text.contains(glyph)), "{text}");
    }

    #[test]
    fn failed_subagent_titles_keep_the_arrow_and_show_the_error_in_colour() {
        let mut step = subagent_step(false, false);
        step.finish(ToolResult::error(&step.call, "child timed out"), 10);
        let session = session_with(Turn {
            steps: vec![step],
            complete: true,
            ..Default::default()
        });
        let theme = Theme::dark();
        let lines = build(&session, area(90, 20), &theme, None, &welcome());
        let title = lines
            .iter()
            .find(|line| line_text(line).contains("Explored:"))
            .unwrap();
        assert!(line_text(title).contains("→ Explored:") && line_text(title).contains("timed out"));
        assert_eq!(title.spans[0].style.fg, Some(theme.error));
    }

    #[test]
    fn subagent_summaries_and_details_fit_narrow_terminal_widths() {
        for width in [8, 12, 24, 40] {
            let session = session_with(Turn {
                steps: vec![subagent_step(true, true)],
                complete: true,
                ..Default::default()
            });
            let lines = build(&session, area(width, 30), &Theme::dark(), None, &welcome());
            assert!(
                lines.iter().all(|line| line.width() <= usize::from(width)),
                "width={width}: {lines:?}"
            );
        }
    }

    #[test]
    fn cached_title_hit_targets_follow_expansion_and_never_include_nested_rows() {
        let mut view = TranscriptView::new();
        let mut session = session_with(Turn {
            prompt: "Inspect\nthe project".into(),
            steps: vec![subagent_step(false, true), subagent_step(false, true)],
            complete: true,
            ..Default::default()
        });
        view.lines(&session, area(80, 20), &Theme::dark(), None, &welcome());
        assert_eq!(
            view.collapsible_at(2).map(|header| header.target),
            Some((0, 0))
        );
        assert_eq!(
            view.collapsible_at(3).map(|header| header.target),
            Some((0, 1))
        );
        session.turns[0].steps[0].disclosure.set_expanded(true);
        session.bump();
        view.lines(&session, area(80, 20), &Theme::dark(), None, &welcome());
        assert_eq!(
            view.collapsible_at(2).map(|header| header.target),
            Some((0, 0))
        );
        assert_eq!(view.collapsible_at(3), None);
        assert_eq!(
            view.collapsible_at(6).map(|header| header.target),
            Some((0, 1))
        );
    }

    #[test]
    fn a_step_line_longer_than_the_screen_is_cut() {
        let call = ToolCall::new(
            "call-1",
            "read",
            serde_json::json!({ "path": "x".repeat(200) }),
        );
        let session = session_with(Turn {
            prompt: "p".to_string(),
            steps: vec![ToolStep::pending(call)],
            ..Default::default()
        });

        let lines = build(&session, area(40, 20), &Theme::dark(), None, &welcome());
        let step = lines
            .iter()
            .map(line_text)
            .find(|line| line.contains("read"))
            .expect("a step line");
        assert!(step.contains('…'), "{step}");
        assert!(step.chars().count() <= 40, "{step}");
    }

    #[test]
    fn a_narrow_transcript_falls_back_to_the_compact_banner() {
        let session = SessionState::new();
        let lines = build(&session, area(20, 20), &Theme::dark(), None, &welcome());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(text.contains("solaris v0.1.0"), "{text}");
        assert!(!text.contains('╭'), "{text}");
    }

    #[test]
    fn the_welcome_box_gives_way_to_the_conversation() {
        let session = session_with(Turn {
            prompt: "hi".into(),
            reply: "there".into(),
            complete: true,
            ..Default::default()
        });
        let lines = build(&session, area(80, 20), &Theme::dark(), None, &welcome());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(!text.contains("Welcome back"), "{text}");
        assert!(text.contains("there"), "{text}");
    }

    #[test]
    fn a_multi_line_prompt_marks_only_the_first_line() {
        // The editor indents continuation rows the same way, so the echo of a
        // sent prompt matches what was typed.
        let session = session_with(Turn {
            prompt: "one\ntwo".into(),
            complete: true,
            ..Default::default()
        });
        let lines = build(&session, area(40, 20), &Theme::dark(), None, &welcome());
        let text: Vec<String> = lines.iter().map(line_text).collect();

        assert!(text[0].starts_with("› one"), "{text:?}");
        assert!(text[1].starts_with("  two"), "{text:?}");
    }

    #[test]
    fn prompt_is_prefixed_and_wrapped() {
        let session = session_with(Turn {
            prompt: "one two three four five six".into(),
            ..Default::default()
        });
        let lines = build(&session, area(12, 20), &Theme::dark(), None, &welcome());
        assert!(line_text(&lines[0]).starts_with("› "));
        assert!(line_text(&lines[1]).starts_with("  "));
    }

    #[test]
    fn collapsed_thinking_shows_a_marker_and_can_expand() {
        let session = session_with(Turn {
            prompt: "hi".into(),
            thinking: "because reasons".into(),
            thinking_expanded: false,
            complete: true,
            ..Default::default()
        });
        let collapsed = build(&session, area(60, 20), &Theme::dark(), None, &welcome());
        assert!(
            collapsed
                .iter()
                .any(|l| line_text(l).starts_with("▸ thinking"))
        );

        let expanded = session_with(Turn {
            prompt: "hi".into(),
            thinking: "because reasons".into(),
            thinking_expanded: true,
            complete: true,
            ..Default::default()
        });
        let lines = build(&expanded, area(60, 20), &Theme::dark(), None, &welcome());
        assert!(lines.iter().any(|l| line_text(l).starts_with("▾ thinking")));
        assert!(
            lines
                .iter()
                .any(|l| line_text(l).contains("because reasons"))
        );
    }

    #[test]
    fn streaming_turn_appends_a_spinner() {
        let session = session_with(Turn {
            prompt: "hi".into(),
            reply: "partial".into(),
            complete: false,
            ..Default::default()
        });
        let lines = build(&session, area(40, 20), &Theme::dark(), Some(0), &welcome());
        let last = line_text(lines.last().unwrap());
        assert!(last.contains("generating"));
    }

    #[test]
    fn completed_turn_has_no_spinner() {
        let session = session_with(Turn {
            prompt: "hi".into(),
            reply: "done".into(),
            complete: true,
            ..Default::default()
        });
        let lines = build(&session, area(40, 20), &Theme::dark(), None, &welcome());
        assert!(!lines.iter().any(|l| line_text(l).contains("generating")));
    }

    #[test]
    fn cached_lines_are_reused_until_inputs_change() {
        let mut view = TranscriptView::new();
        let mut session = session_with(Turn {
            prompt: "hi".into(),
            reply: "there".into(),
            complete: true,
            ..Default::default()
        });
        let theme = Theme::dark();

        let first = view
            .lines(&session, area(40, 20), &theme, None, &welcome())
            .len();
        let second = view
            .lines(&session, area(40, 20), &theme, None, &welcome())
            .len();
        assert_eq!(first, second);

        session.bump();
        session.turns[0].reply.push_str(" more");
        let third = view
            .lines(&session, area(40, 20), &theme, None, &welcome())
            .len();
        assert!(third >= second);
    }
}
