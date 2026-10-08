//! Builds the styled transcript lines from session state.
//!
//! The transcript is rebuilt only when the session revision, terminal size,
//! theme, or spinner frame changes, so scrolling and idle frames are free —
//! except while it is empty, when the welcome box's animated companion needs a
//! fresh frame every tick.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use solaris_tui::components::markdown::{MarkdownStyle, render_markdown};
use solaris_tui::components::welcome::{WelcomeData, WelcomeStyles, render_welcome};
use solaris_tui::theme::Theme;
use solaris_tui::util::{truncate_to_width, wrap_text};

use crate::state::{SessionState, Turn};

/// Spinner glyphs shown while a response streams.
pub const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

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
            self.lines = build(session, area, theme, spinner_frame, welcome);
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
        push_steps(&mut out, turn, width, theme);
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
fn push_steps(out: &mut Vec<Line<'static>>, turn: &Turn, width: usize, theme: &Theme) {
    for step in &turn.steps {
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
    use solaris_core::{ToolCall, ToolResult};

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
        assert!(text.contains(r#"{"path":"a.txt"}"#), "arguments are shown: {text}");
        assert!(text.contains("7ms"), "timing is shown: {text}");

        assert!(text.contains("✗ bash"), "{text}");
        assert!(
            text.contains("exit code 1"),
            "a failure says why, not just that it failed: {text}"
        );

        assert!(text.contains("⋯ grep"), "a call in flight: {text}");
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
