//! Builds the styled transcript lines from session state.
//!
//! The transcript is rebuilt only when the session revision, terminal size,
//! theme, or spinner frame changes, so scrolling and idle frames are free —
//! except while it is empty, when the welcome box's animated companion needs a
//! fresh frame every tick.

use std::collections::BTreeMap;

use crossterm::event::MouseEvent;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use solaris_tui::components::markdown::{MarkdownStyle, render_markdown};
use solaris_tui::components::welcome::{WelcomeData, WelcomeStyles, render_welcome};
use solaris_tui::components::{
    Collapsible, CollapsibleStatus, CollapsibleStyles, SubagentSummary, SubagentSummaryStatus,
};
use solaris_tui::theme::Theme;
use solaris_tui::util::{truncate_to_width, wrap_text};

use crate::state::{SessionState, SteeringNote, ToolStep, Turn};

/// Spinner glyphs shared with the reusable TUI activity indicators.
pub use solaris_tui::components::loader::SPINNER;

/// A tool disclosure header; the target identifies its application record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollapsibleHeader {
    pub row: usize,
    pub target: (usize, usize),
    pub marker_offset: u16,
    pub body_rows: usize,
    pub detail: bool,
}

struct SubagentHeader {
    row: usize,
    summary: SubagentSummary,
}

#[derive(Default)]
struct TranscriptHeaders {
    tools: Vec<CollapsibleHeader>,
    subagents: BTreeMap<(usize, usize), SubagentHeader>,
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
    /// Tool disclosures and retained child terminal links have separate handlers.
    headers: TranscriptHeaders,
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
            self.lines = build_with_headers(
                session,
                area,
                theme,
                spinner_frame,
                welcome,
                &mut self.headers,
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
        self.headers
            .tools
            .iter()
            .find(|header| header.row == row)
            .copied()
    }

    /// Deliver pointer events to child links independently of tool disclosures.
    pub fn handle_subagent_mouse(
        &mut self,
        event: MouseEvent,
        viewport: Rect,
        scroll: u16,
        selection_active: bool,
    ) -> (bool, Option<(usize, usize)>) {
        let mut handled = false;
        let mut activated = None;
        for (&target, header) in &mut self.headers.subagents {
            let visible_row = header
                .row
                .checked_sub(usize::from(scroll))
                .filter(|row| *row < usize::from(viewport.height));
            let area = visible_row.map_or(Rect::default(), |row| {
                Rect::new(viewport.x, viewport.y + row as u16, viewport.width, 1)
            });
            handled |= header
                .summary
                .on_mouse(event, area, selection_active)
                .is_handled();
            if header.summary.take_activated() {
                activated = Some(target);
            }
        }
        (handled, activated)
    }

    pub fn cancel_subagent_interaction(&mut self) {
        for header in self.headers.subagents.values_mut() {
            header.summary.cancel_interaction();
        }
    }

    /// Force a rebuild on the next call.
    pub fn invalidate(&mut self) {
        self.initialized = false;
        self.cancel_subagent_interaction();
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
        &mut TranscriptHeaders::default(),
    )
}

fn build_with_headers(
    session: &SessionState,
    area: Rect,
    theme: &Theme,
    spinner_frame: Option<u16>,
    welcome: &WelcomeData,
    headers: &mut TranscriptHeaders,
) -> Vec<Line<'static>> {
    headers.tools.clear();
    headers.subagents.retain(|&(turn, step), _| {
        session
            .turns
            .get(turn)
            .and_then(|turn| turn.steps.get(step))
            .is_some_and(|step| step.subagent.is_some())
    });
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
        push_steps(
            &mut out,
            turn.into(),
            width,
            theme,
            spinner_frame,
            index,
            headers,
        );
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

/// Consecutive ordinary calls share a disclosure; shell commands own one and
/// child agents link to a terminal. Steering notes stay outside these groups.
struct ToolTranscript<'a> {
    steps: &'a [ToolStep],
    steering: &'a [SteeringNote],
    complete: bool,
}

impl<'a> From<&'a Turn> for ToolTranscript<'a> {
    fn from(turn: &'a Turn) -> Self {
        Self {
            steps: &turn.steps,
            steering: &turn.steering,
            complete: turn.complete,
        }
    }
}

fn push_steps(
    out: &mut Vec<Line<'static>>,
    turn: ToolTranscript<'_>,
    width: usize,
    theme: &Theme,
    spinner_frame: Option<u16>,
    turn_index: usize,
    headers: &mut TranscriptHeaders,
) {
    // Messages the user steered in are drawn where they arrived, among the
    // calls they followed: where the correction landed is why the model's next
    // move changed, so collecting them at the end would lose the point of them.
    let mut notes = turn.steering.iter().peekable();
    while let Some(note) = notes.next_if(|note| note.after_steps == 0) {
        push_steering(out, &note.text, width, theme);
    }

    let mut finished = 0usize;
    let mut step_index = 0;
    while step_index < turn.steps.len() {
        let step = &turn.steps[step_index];
        let mut end = step_index + 1;
        if let Some((title, style, status)) = subagent_summary(step, turn.complete, theme) {
            let header = headers
                .subagents
                .entry((turn_index, step_index))
                .or_insert_with(|| SubagentHeader {
                    row: out.len(),
                    summary: SubagentSummary::default(),
                });
            header.row = out.len();
            header.summary.set_title(title, style);
            header.summary.set_status(status);
            header.summary.set_spinner_frame(spinner_frame.unwrap_or(0));
            out.push(header.summary.line(width as u16));
        } else {
            let (view, body_rows) = if is_shell(step) {
                let body_rows = step
                    .result
                    .as_ref()
                    .map_or(1, |result| tool_display_text(result).lines().count().max(1));
                (shell_view(step, turn.complete, theme), body_rows)
            } else {
                let mut group_finished = finished + usize::from(step.is_done());
                while end < turn.steps.len()
                    && turn.steps[end].subagent.is_none()
                    && !is_shell(&turn.steps[end])
                    && !notes
                        .peek()
                        .is_some_and(|note| note.after_steps <= group_finished)
                {
                    group_finished += usize::from(turn.steps[end].is_done());
                    end += 1;
                }
                (
                    tool_group_view(
                        &turn.steps[step_index..end],
                        turn.complete,
                        theme,
                        spinner_frame,
                    ),
                    end - step_index,
                )
            };
            let layout = view.layout(width as u16);
            for header in layout.headers {
                let detail = !header.path.is_empty();
                let target_step = step_index + header.path.first().copied().unwrap_or(0);
                headers.tools.push(CollapsibleHeader {
                    row: out.len() + header.row,
                    target: (turn_index, target_step),
                    marker_offset: header.marker_offset,
                    body_rows: if detail {
                        turn.steps[target_step]
                            .result
                            .as_ref()
                            .map_or(1, |result| tool_display_text(result).lines().count().max(1))
                    } else {
                        body_rows
                    },
                    detail,
                });
            }
            out.extend(layout.lines);
        }

        finished += turn.steps[step_index..end]
            .iter()
            .filter(|step| step.is_done())
            .count();
        while let Some(note) = notes.next_if(|note| note.after_steps <= finished) {
            push_steering(out, &note.text, width, theme);
        }
        step_index = end;
    }

    // A note that arrived after the last call — while the answer streamed, say.
    for note in notes {
        push_steering(out, &note.text, width, theme);
    }
}

fn is_shell(step: &ToolStep) -> bool {
    matches!(step.call.name.as_str(), "shell" | "bash" | "powershell")
}

fn tool_group_view(
    steps: &[ToolStep],
    turn_complete: bool,
    theme: &Theme,
    _spinner_frame: Option<u16>,
) -> Collapsible {
    let pending = steps.iter().any(|step| !step.is_done());
    let running = pending && !turn_complete;
    let interrupted = pending && turn_complete;
    let failed = !running && (interrupted || steps.iter().any(ToolStep::is_error));
    let label = if running { "Exploring" } else { "Explored" };
    let title = format!("{label}: {}", tool_counts(steps, running));
    // The first call anchors the group, so appending calls or receiving results
    // cannot replace the user's disclosure state.
    let mut view = step_view(&steps[0], title, running, failed, theme);
    if steps[0].disclosure.is_expanded() {
        let mut children = Vec::new();
        for step in steps {
            let mut child = Collapsible::new(tool_step_title(step))
                .with_state(step.detail_disclosure)
                .styles(CollapsibleStyles::from_theme(theme))
                .indentation(0, 2);
            child.set_status(if running && !step.is_done() {
                CollapsibleStatus::Running
            } else if step.is_error() || (!step.is_done() && turn_complete) {
                CollapsibleStatus::Failed
            } else {
                CollapsibleStatus::Idle
            });
            let output = match &step.result {
                Some(result) if result.output.is_empty() => "No output.".to_string(),
                Some(result) => tool_display_text(result),
                None if running => "Running…".to_string(),
                None => "No output.".to_string(),
            };
            child.set_content(
                output
                    .lines()
                    .map(|line| {
                        Line::styled(
                            single_line(line),
                            Style::default().fg(if step.is_error() {
                                theme.error
                            } else {
                                theme.dim
                            }),
                        )
                    })
                    .collect(),
            );
            children.push(child);
        }
        view.set_children(children);
    }
    view
}

fn shell_view(step: &ToolStep, turn_complete: bool, theme: &Theme) -> Collapsible {
    let running = !step.is_done() && !turn_complete;
    let interrupted = !step.is_done() && turn_complete;
    let failed = !running && (interrupted || step.is_error());
    let command = step.call.input["command"]
        .as_str()
        .map(single_line)
        .unwrap_or_else(|| step.call.input.to_string());
    let mut title = format!("Shell: {command}");
    if let Some(elapsed) = step.duration_ms() {
        title.push_str(&format!("  ·  {elapsed}ms"));
    }
    let mut view = step_view(step, title, running, failed, theme);
    if step.disclosure.is_expanded() {
        let output = match &step.result {
            Some(result) if result.output.is_empty() => "No output.".to_string(),
            Some(result) => tool_display_text(result),
            None if running => "Running…".to_string(),
            None => "No output.".to_string(),
        };
        let style = Style::default().fg(if failed { theme.error } else { theme.muted });
        view.set_content(
            output
                .lines()
                .map(|line| Line::styled(single_line(line), style))
                .collect(),
        );
    }
    view
}

fn tool_display_text(result: &solaris_agent::ToolResult) -> String {
    let mut output = result.output.clone();
    if let Some(patch) = result.metadata.details["change"]["patch"].as_str() {
        output.push_str("\n\n");
        output.push_str(patch);
        if let Some(id) = result.metadata.details["change"]["id"].as_str() {
            output.push_str(&format!("\nChange: {id} (solaris --undo {id})"));
        }
    }
    output
}

fn step_view(
    step: &ToolStep,
    title: String,
    running: bool,
    failed: bool,
    theme: &Theme,
) -> Collapsible {
    let mut view = Collapsible::new(title)
        .with_state(step.disclosure)
        .styles(CollapsibleStyles::from_theme(theme))
        .indentation(2, 4);
    // Failure only changes the palette after execution has stopped. It never
    // adds a message or changes the user's disclosure state.
    view.set_status(if running {
        CollapsibleStatus::Running
    } else if failed {
        CollapsibleStatus::Failed
    } else {
        CollapsibleStatus::Idle
    });
    view
}

fn single_line(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

fn spinner(frame: Option<u16>) -> &'static str {
    SPINNER[usize::from(frame.unwrap_or(0)) % SPINNER.len()]
}

fn subagent_summary(
    step: &ToolStep,
    turn_complete: bool,
    theme: &Theme,
) -> Option<(String, Style, SubagentSummaryStatus)> {
    let child = step.subagent.as_ref()?;
    let running = child.result.is_none() && !turn_complete;
    let colour = if running { theme.accent } else { theme.muted };
    Some((
        format!("SubAgent: {}", subagent_task_name(step)),
        Style::default().fg(colour),
        if running {
            SubagentSummaryStatus::Running
        } else {
            SubagentSummaryStatus::Complete
        },
    ))
}

fn subagent_task_name(step: &ToolStep) -> String {
    ["name", "prompt"]
        .into_iter()
        .filter_map(|key| step.call.input[key].as_str())
        .find_map(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
        .map(single_line)
        .unwrap_or_else(|| step.call.id.clone())
}

/// The dedicated child terminal contains its task, live calls, and final report.
pub fn subagent_lines(
    step: &ToolStep,
    turn_complete: bool,
    width: u16,
    theme: &Theme,
    spinner_frame: Option<u16>,
) -> Vec<Line<'static>> {
    subagent_layout(step, turn_complete, width, theme, spinner_frame).lines
}

/// Child tools use the same disclosures and nested hit targets as the main view.
#[derive(Default)]
pub struct SubagentTranscript {
    pub lines: Vec<Line<'static>>,
    pub headers: Vec<CollapsibleHeader>,
}

pub fn subagent_layout(
    step: &ToolStep,
    turn_complete: bool,
    width: u16,
    theme: &Theme,
    spinner_frame: Option<u16>,
) -> SubagentTranscript {
    let mut out = Vec::new();
    let Some(child) = step.subagent.as_ref() else {
        return SubagentTranscript::default();
    };
    let running = child.result.is_none() && !turn_complete;
    let markdown = MarkdownStyle::from_theme(theme);
    if let Some(prompt) = step.call.input["prompt"].as_str() {
        push_prompt(&mut out, prompt, usize::from(width), theme);
        out.push(Line::default());
    }
    let mut headers = TranscriptHeaders::default();
    push_steps(
        &mut out,
        ToolTranscript {
            steps: &child.steps,
            steering: &[],
            complete: !running,
        },
        usize::from(width),
        theme,
        spinner_frame,
        0,
        &mut headers,
    );
    out.push(Line::default());
    if let Some(result) = &child.result {
        out.extend(render_markdown(&result.output, width, &markdown));
    } else {
        out.push(Line::styled(
            if running {
                format!("{} working…", spinner(spinner_frame))
            } else {
                "Interrupted".into()
            },
            Style::default().fg(if running { theme.accent } else { theme.error }),
        ));
    }
    SubagentTranscript {
        lines: out,
        headers: headers.tools,
    }
}

fn tool_counts(steps: &[ToolStep], running: bool) -> String {
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

fn tool_step_title(step: &ToolStep) -> String {
    let label = match step.call.name.as_str() {
        "read" => "Read",
        "grep" => "Grep",
        "find" => "Find",
        "ls" => "List",
        "glob" => "Glob",
        "edit" => "Edit",
        "patch" => "Patch",
        "write" => "Write",
        "shell" | "bash" | "powershell" => "Shell",
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
    if let Some(elapsed) = step.duration_ms() {
        detail.push_str(&format!("  ·  {elapsed}ms"));
    }
    format!("{} {}", single_line(label), single_line(&detail))
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
    fn ordinary_tool_calls_default_to_a_single_collapsed_count_summary() {
        let mut steps = Vec::new();
        for index in 0..8 {
            let call = ToolCall::new(
                format!("read-{index}"),
                "read",
                serde_json::json!({"path":format!("file-{index}.rs")}),
            );
            let mut step = ToolStep::pending(call.clone());
            step.finish(ToolResult::ok(&call, "private output"), 7);
            steps.push(step);
        }
        for name in ["grep", "find", "glob"] {
            let call = ToolCall::new(name, name, serde_json::json!({"pattern":"secret"}));
            let mut step = ToolStep::pending(call.clone());
            step.finish(ToolResult::ok(&call, "private output"), 3);
            steps.push(step);
        }
        let session = session_with(Turn {
            steps,
            complete: true,
            ..Default::default()
        });
        let lines = build(&session, area(80, 30), &Theme::dark(), None, &welcome());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert_eq!(
            text.matches("→ Explored: 8 reads, 3 searches").count(),
            1,
            "{text}"
        );
        assert!(
            !text.contains("file-") && !text.contains("secret"),
            "{text}"
        );
        assert!(!text.contains("private output"), "{text}");
        assert_eq!(lines.len(), 2, "{text}");
    }

    #[test]
    fn expanding_a_tool_group_shows_calls_and_marks_failures_without_messages() {
        let call = ToolCall::new("call-1", "read", serde_json::json!({ "path": "a.txt" }));
        let mut done = ToolStep::pending(call.clone());
        done.finish(ToolResult::ok(&call, "hello"), 7);
        done.disclosure.set_expanded(true);

        let failing = ToolCall::new("call-2", "grep", serde_json::json!({ "pattern": "x" }));
        let mut failed = ToolStep::pending(failing.clone());
        failed.finish(
            ToolResult::error(&failing, "search failed\nmore details"),
            3,
        );

        let running = ToolStep::pending(ToolCall::new(
            "call-3",
            "write",
            serde_json::json!({ "path": "b.txt", "content": "new text" }),
        ));

        let session = session_with(Turn {
            prompt: "do it".to_string(),
            reply: "done".to_string(),
            steps: vec![done, failed, running],
            ..Default::default()
        });

        let theme = Theme::dark();
        let lines = build(&session, area(80, 30), &theme, Some(1), &welcome());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(
            text.contains("↓ Exploring: 1 read, 1 search, 1 write"),
            "{text}"
        );
        assert_eq!(lines[1].style.fg, Some(theme.muted));
        assert_eq!(lines[1].spans[0].style.fg, Some(theme.muted));
        assert!(text.contains("    → Read a.txt"), "{text}");
        assert!(text.contains("7ms"), "timing is shown: {text}");
        let failure = lines
            .iter()
            .find(|line| line_text(line).contains("→ Grep"))
            .unwrap();
        assert_eq!(failure.style.fg, Some(theme.error));
        assert!(
            text.contains("    → Write b.txt"),
            "a call in flight: {text}"
        );
        assert!(
            !text.contains("hello")
                && !text.contains("search failed")
                && !text.contains("more details"),
            "{text}"
        );
    }

    #[test]
    fn tool_groups_only_turn_red_after_all_calls_finish_without_adding_messages() {
        for theme in [Theme::dark(), Theme::light()] {
            for expanded in [false, true] {
                let call = ToolCall::new("read", "read", serde_json::json!({"path":"a.rs"}));
                let mut failed = ToolStep::pending(call.clone());
                failed.disclosure.set_expanded(expanded);
                failed.finish(ToolResult::error(&call, "private error details"), 1);
                let call = ToolCall::new("search", "grep", serde_json::json!({"pattern":"needle"}));
                let mut steps = vec![failed, ToolStep::pending(call.clone())];

                let view = tool_group_view(&steps, false, &theme, Some(1));
                let lines = view.lines(90);
                let glyph = if expanded { "↓" } else { "→" };
                assert_eq!(
                    line_text(&lines[0]),
                    format!("  {glyph} Exploring: 1 read, 1 search")
                );
                assert_eq!(lines[0].style.fg, Some(theme.muted));
                assert_eq!(
                    lines[0].spans[0].style.fg,
                    Some(if expanded { theme.muted } else { theme.accent })
                );
                assert_eq!(view.state().is_expanded(), expanded);

                steps[1].finish(ToolResult::ok(&call, "matches"), 2);
                let view = tool_group_view(&steps, false, &theme, Some(1));
                let lines = view.lines(90);
                let glyph = if expanded { "↓" } else { "→" };
                assert_eq!(
                    line_text(&lines[0]),
                    format!("  {glyph} Explored: 1 read, 1 search")
                );
                assert_eq!(lines[0].style.fg, Some(theme.error));
                assert_eq!(lines[0].spans[0].style.fg, Some(theme.error));
                assert_eq!(view.state().is_expanded(), expanded);
                assert_eq!(lines.len(), if expanded { 3 } else { 1 });
                assert!(
                    !lines
                        .iter()
                        .any(|line| line_text(line).contains("private error details"))
                );
            }
        }
    }

    #[test]
    fn each_shell_command_has_its_own_disclosure_and_output() {
        for name in ["shell", "bash", "powershell"] {
            let call = ToolCall::new(name, name, serde_json::json!({"command":"cargo test"}));
            let mut step = ToolStep::pending(call.clone());
            step.finish(
                ToolResult::ok(&call, "running 2 tests\n\n  test result: ok"),
                12,
            );
            let mut session = session_with(Turn {
                steps: vec![step.clone(), step],
                complete: true,
                ..Default::default()
            });
            let lines = build(&session, area(80, 30), &Theme::dark(), None, &welcome());
            let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
            assert_eq!(text.matches("→ Shell: cargo test").count(), 2, "{text}");
            assert!(!text.contains("running 2 tests"), "{text}");
            session.turns[0].steps[0].disclosure.set_expanded(true);
            let lines = build(&session, area(80, 30), &Theme::dark(), None, &welcome());
            let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
            assert!(text.contains("↓ Shell: cargo test  ·  12ms"), "{text}");
            assert_eq!(text.matches("→ Shell: cargo test").count(), 1, "{text}");
            assert!(
                text.contains("    running 2 tests\n    \n      test result: ok"),
                "{text}"
            );
        }
    }

    #[test]
    fn shell_disclosures_preserve_expansion_when_results_arrive() {
        let call = ToolCall::new("test", "shell", serde_json::json!({"command":"cargo test"}));
        let mut session = session_with(Turn {
            steps: vec![ToolStep::pending(call.clone())],
            ..Default::default()
        });
        let mut view = TranscriptView::new();
        let theme = Theme::dark();
        for frame in [0, 1] {
            let lines = view.lines(&session, area(80, 30), &theme, Some(frame), &welcome());
            assert!(
                lines
                    .iter()
                    .any(|line| line_text(line).contains("→ Shell:"))
            );
        }
        session.turns[0].steps[0].disclosure.set_expanded(true);
        session.bump();
        let lines = view.lines(&session, area(80, 30), &theme, Some(1), &welcome());
        assert!(
            lines
                .iter()
                .any(|line| line_text(line).contains("↓ Shell:"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line_text(line).contains("Running…"))
        );

        session.turns[0].steps[0].finish(ToolResult::ok(&call, "tests passed"), 9);
        session.turns[0].complete = true;
        session.bump();
        let lines = view.lines(&session, area(80, 30), &theme, None, &welcome());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            text.contains("↓ Shell: cargo test") && text.contains("    tests passed"),
            "{text}"
        );
        assert!(!SPINNER.iter().any(|glyph| text.contains(glyph)), "{text}");
    }

    #[test]
    fn ordinary_groups_preserve_expansion_when_calls_are_appended_and_finished() {
        let call = ToolCall::new("read", "read", serde_json::json!({"path":"a.rs"}));
        let mut step = ToolStep::pending(call.clone());
        step.disclosure.set_expanded(true);
        let mut session = session_with(Turn {
            steps: vec![step],
            ..Default::default()
        });
        let mut view = TranscriptView::new();
        let theme = Theme::dark();
        let lines = view.lines(&session, area(80, 30), &theme, Some(0), &welcome());
        assert!(
            lines
                .iter()
                .any(|line| line_text(line).contains("↓ Exploring: 1 read"))
        );
        session.turns[0].steps.push(ToolStep::pending(call.clone()));
        for step in &mut session.turns[0].steps {
            step.finish(ToolResult::ok(&call, "data"), 2);
        }
        session.turns[0].complete = true;
        session.bump();
        let lines = view.lines(&session, area(80, 30), &theme, None, &welcome());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("↓ Explored: 2 reads"), "{text}");
        assert_eq!(text.matches("    → Read a.rs").count(), 2, "{text}");
    }

    #[test]
    fn shell_commands_child_agents_and_steering_notes_separate_ordinary_groups() {
        let mut steps = Vec::new();
        for (name, input) in [
            ("read", serde_json::json!({"path":"a.rs"})),
            ("grep", serde_json::json!({"pattern":"needle"})),
            ("shell", serde_json::json!({"command":"cargo test"})),
            ("read", serde_json::json!({"path":"b.rs"})),
            ("subagent", serde_json::json!({"agent":"general"})),
            ("read", serde_json::json!({"path":"c.rs"})),
        ] {
            let call = ToolCall::new(name, name, input);
            let mut step = ToolStep::pending(call.clone());
            step.finish(ToolResult::ok(&call, "done"), 1);
            steps.push(step);
        }
        let session = session_with(Turn {
            steps,
            steering: vec![crate::state::SteeringNote {
                text: "check this first".into(),
                after_steps: 1,
            }],
            complete: true,
            ..Default::default()
        });
        let mut view = TranscriptView::new();
        let lines = view.lines(&session, area(80, 30), &Theme::dark(), None, &welcome());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert_eq!(text.matches("→ Explored:").count(), 4, "{text}");
        let rows: Vec<_> = text.lines().collect();
        assert!(
            rows[1].contains("1 read") && rows[2].contains("steered"),
            "{text}"
        );
        assert!(
            rows[3].contains("1 search") && rows[4].contains("Shell:"),
            "{text}"
        );
        assert!(
            rows[6].contains("SubAgent:") && rows[7].contains("1 read"),
            "{text}"
        );
        assert_eq!(view.collapsible_at(2), None, "steering is not a disclosure");
        assert_eq!(
            view.collapsible_at(6),
            None,
            "a child summary is not a disclosure"
        );
        assert_eq!(view.headers.subagents[&(0, 4)].row, 6);
        for (row, step) in [(1, 0), (3, 1), (4, 2), (5, 3), (7, 5)] {
            assert_eq!(
                view.collapsible_at(row).map(|header| header.target),
                Some((0, step))
            );
        }
    }

    #[test]
    fn failed_shells_only_colour_the_title_and_keep_output_behind_the_disclosure() {
        let call = ToolCall::new("test", "shell", serde_json::json!({"command":"cargo test"}));
        let mut step = ToolStep::pending(call.clone());
        step.finish(
            ToolResult::error(&call, "exit code 1\nsecond error line"),
            4,
        );
        let mut session = session_with(Turn {
            steps: vec![step],
            complete: true,
            ..Default::default()
        });
        let theme = Theme::dark();
        let lines = build(&session, area(80, 30), &theme, None, &welcome());
        assert_eq!(line_text(&lines[1]), "  → Shell: cargo test  ·  4ms");
        assert_eq!(lines[1].style.fg, Some(theme.error));
        assert_eq!(lines[1].spans[0].style.fg, Some(theme.error));
        assert_eq!(lines.len(), 2);

        session.turns[0].steps[0].disclosure.set_expanded(true);
        let lines = build(&session, area(80, 30), &theme, None, &welcome());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert_eq!(line_text(&lines[1]), "  ↓ Shell: cargo test  ·  4ms");
        assert!(
            text.contains("    exit code 1\n    second error line"),
            "{text}"
        );
        assert_eq!(lines[1].spans[0].style.fg, Some(theme.error));
    }

    #[test]
    fn interrupted_ordinary_calls_and_shell_commands_stop_spinning() {
        let mut session = session_with(Turn {
            steps: vec![
                ToolStep::pending(ToolCall::new(
                    "read",
                    "read",
                    serde_json::json!({"path":"a.rs"}),
                )),
                ToolStep::pending(ToolCall::new(
                    "test",
                    "shell",
                    serde_json::json!({"command":"cargo test"}),
                )),
            ],
            complete: true,
            ..Default::default()
        });
        for step in &mut session.turns[0].steps {
            step.disclosure.set_expanded(true);
        }
        let lines = build(&session, area(80, 30), &Theme::dark(), Some(1), &welcome());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("↓ Explored: 1 read"), "{text}");
        assert!(text.contains("↓ Shell: cargo test"), "{text}");
        assert!(!text.to_lowercase().contains("interrupted"), "{text}");
        let theme = Theme::dark();
        for line in lines.iter().filter(|line| line_text(line).contains('↓')) {
            assert_eq!(line.spans[0].style.fg, Some(theme.error));
        }
        assert!(!SPINNER.iter().any(|glyph| text.contains(glyph)), "{text}");
    }

    #[test]
    fn an_empty_shell_result_has_a_body_and_accurate_header_geometry() {
        let call = ToolCall::new("test", "shell", serde_json::json!({"command":"true"}));
        let mut step = ToolStep::pending(call.clone());
        step.disclosure.set_expanded(true);
        step.finish(ToolResult::ok(&call, ""), 1);
        let session = session_with(Turn {
            steps: vec![step],
            complete: true,
            ..Default::default()
        });
        let mut view = TranscriptView::new();
        let lines = view.lines(&session, area(80, 30), &Theme::dark(), None, &welcome());
        assert!(line_text(&lines[2]).contains("No output."));
        assert_eq!(view.collapsible_at(1).unwrap().body_rows, 1);
        assert_eq!(view.collapsible_at(2), None);
    }

    #[test]
    fn collapsed_ordinary_groups_keep_the_right_arrow_while_running() {
        let call = ToolCall::new("read", "read", serde_json::json!({"path":"a.rs"}));
        let mut session = session_with(Turn {
            steps: vec![ToolStep::pending(call.clone())],
            ..Default::default()
        });
        for frame in [0, 1] {
            let lines = build(
                &session,
                area(80, 30),
                &Theme::dark(),
                Some(frame),
                &welcome(),
            );
            assert_eq!(line_text(&lines[1]), "  → Exploring: 1 read");
        }
        session.turns[0].steps[0].finish(ToolResult::ok(&call, "data"), 1);
        let lines = build(&session, area(80, 30), &Theme::dark(), Some(1), &welcome());
        assert_eq!(line_text(&lines[1]), "  → Explored: 1 read");
    }

    #[test]
    fn ordinary_groups_and_shell_outputs_fit_narrow_widths_without_control_characters() {
        for theme in [Theme::dark(), Theme::light()] {
            for expanded in [false, true] {
                let mut steps = Vec::new();
                for (name, input) in [
                    (
                        "read",
                        serde_json::json!({"path":"源代码👩‍💻e\u{301}".repeat(20)}),
                    ),
                    (
                        "powershell",
                        serde_json::json!({"command":"echo 中文👩‍💻\necho more\ttext"}),
                    ),
                ] {
                    let call = ToolCall::new(name, name, input);
                    let mut step = ToolStep::pending(call.clone());
                    step.disclosure.set_expanded(expanded);
                    step.finish(
                        ToolResult::ok(&call, "  中文👩‍💻e\u{301}\toutput\n\nsecond\u{1b}line"),
                        1,
                    );
                    steps.push(step);
                }
                let session = session_with(Turn {
                    steps,
                    complete: true,
                    ..Default::default()
                });
                for width in [8, 12, 24, 40] {
                    let lines = build(&session, area(width, 30), &theme, None, &welcome());
                    for line in &lines {
                        assert!(
                            line.width() <= usize::from(width),
                            "width={width}: {line:?}"
                        );
                        assert!(!line_text(line).chars().any(char::is_control), "{line:?}");
                    }
                }
            }
        }
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
    fn subagent_summaries_never_expand_in_the_main_transcript() {
        for expanded in [false, true] {
            let session = session_with(Turn {
                steps: vec![subagent_step(expanded, true)],
                complete: true,
                ..Default::default()
            });
            let lines = build(&session, area(90, 20), &Theme::dark(), None, &welcome());
            let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
            assert!(
                text.contains("🤖 SubAgent: Investigate the source"),
                "{text}"
            );
            assert!(
                !text.contains("src/agent.rs") && !text.contains("private tool data"),
                "{text}"
            );
            assert_eq!(lines.len(), 2);
        }
    }

    #[test]
    fn subagent_summaries_prefer_task_names_and_fall_back_to_the_prompt() {
        let theme = Theme::dark();
        let mut step = subagent_step(false, false);
        step.call.input["name"] = serde_json::json!("检查登录流程");
        assert_eq!(
            subagent_summary(&step, false, &theme).unwrap().0,
            "SubAgent: 检查登录流程"
        );
        step.finish(solaris_agent::ToolResult::ok(&step.call, "done"), 1);
        assert_eq!(
            subagent_summary(&step, true, &theme).unwrap().0,
            "SubAgent: 检查登录流程"
        );
        step.call.input["name"] = serde_json::json!("  \n ");
        step.call.input["prompt"] = serde_json::json!("\n 检查路由配置 \n完整任务说明");
        assert_eq!(subagent_task_name(&step), "检查路由配置");
        step.call.input["prompt"] = serde_json::Value::Null;
        assert_eq!(subagent_task_name(&step), "delegate");
    }

    #[test]
    fn child_terminal_shows_task_calls_outputs_and_final_report() {
        let mut step = subagent_step(false, true);
        let child = step.subagent.as_mut().unwrap();
        child.steps[0].disclosure.set_expanded(true);
        child.steps[1].detail_disclosure.set_expanded(true);
        let layout = subagent_layout(&step, true, 90, &Theme::dark(), None);
        assert!(
            layout
                .headers
                .iter()
                .any(|header| header.detail && header.target.1 == 1)
        );
        let lines = layout.lines;
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        for expected in [
            "Investigate the source",
            "Read src/agent.rs:175-429",
            "private tool data",
            "final report",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
    }

    #[test]
    fn child_terminal_animates_running_calls_and_stops_after_interruption() {
        let step = subagent_step(false, false);
        let theme = Theme::dark();
        let running = subagent_lines(&step, false, 90, &theme, Some(1));
        assert!(
            running
                .iter()
                .any(|line| line_text(line).contains("⠙ working…"))
        );
        let interrupted = subagent_lines(&step, true, 90, &theme, Some(1));
        let text = interrupted
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Interrupted"), "{text}");
        assert!(!SPINNER.iter().any(|glyph| text.contains(glyph)), "{text}");
    }

    #[test]
    fn child_terminal_collapses_tool_outputs_and_keeps_its_report_visible() {
        let mut step = subagent_step(false, true);
        let call = ToolCall::new(
            "shell",
            "shell",
            serde_json::json!({"command":"cargo test"}),
        );
        let mut shell = ToolStep::pending(call.clone());
        shell.finish(ToolResult::ok(&call, "shell output"), 1);
        step.subagent.as_mut().unwrap().steps.push(shell);
        let layout = subagent_layout(&step, true, 90, &Theme::dark(), None);
        let text = layout
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("→ Explored:") && text.contains("→ Shell: cargo test"),
            "{text}"
        );
        assert!(text.contains("final report"), "{text}");
        assert!(
            !text.contains("private tool data") && !text.contains("shell output"),
            "{text}"
        );
        assert_eq!(layout.headers.len(), 2);
        step.subagent.as_mut().unwrap().steps[3]
            .disclosure
            .set_expanded(true);
        let lines = subagent_lines(&step, true, 90, &Theme::dark(), None);
        assert!(
            lines
                .iter()
                .any(|line| line_text(line).contains("shell output"))
        );
    }

    #[test]
    fn subagent_summary_has_only_running_and_complete_states_regardless_of_outcome() {
        let theme = Theme::dark();
        let mut step = subagent_step(false, false);
        let tool = &mut step.subagent.as_mut().unwrap().steps[0];
        tool.finish(ToolResult::error(&tool.call, "nested failure"), 1);
        assert_eq!(
            subagent_summary(&step, false, &theme).unwrap().1.fg,
            Some(theme.accent)
        );
        assert_eq!(
            subagent_summary(&step, false, &theme).unwrap().2,
            SubagentSummaryStatus::Running
        );
        for failed in [false, true] {
            let result = if failed {
                ToolResult::error(&step.call, "failed report")
            } else {
                ToolResult::ok(&step.call, "report")
            };
            step.finish(result, 10);
            let (_, style, status) = subagent_summary(&step, false, &theme).unwrap();
            assert_eq!(style.fg, Some(theme.muted));
            assert_eq!(status, SubagentSummaryStatus::Complete);
        }
        let interrupted = subagent_step(false, false);
        let (_, style, status) = subagent_summary(&interrupted, true, &theme).unwrap();
        assert_eq!(style.fg, Some(theme.muted));
        assert_eq!(status, SubagentSummaryStatus::Complete);
    }

    #[test]
    fn subagent_summaries_and_terminals_fit_narrow_widths() {
        for width in [8, 12, 24, 40] {
            let step = subagent_step(true, true);
            let lines = subagent_lines(&step, true, width, &Theme::dark(), None);
            assert!(
                lines.iter().all(|line| line.width() <= usize::from(width)),
                "{lines:?}"
            );
            let session = session_with(Turn {
                steps: vec![step],
                complete: true,
                ..Default::default()
            });
            let lines = build(&session, area(width, 30), &Theme::dark(), None, &welcome());
            assert!(
                lines.iter().all(|line| line.width() <= usize::from(width)),
                "{lines:?}"
            );
        }
    }

    #[test]
    fn child_summary_hit_targets_do_not_depend_on_disclosure_state() {
        let mut view = TranscriptView::new();
        let mut session = session_with(Turn {
            prompt: "Inspect\nthe project".into(),
            steps: vec![subagent_step(false, true), subagent_step(false, true)],
            complete: true,
            ..Default::default()
        });
        for expanded in [false, true] {
            session.turns[0].steps[0].disclosure.set_expanded(expanded);
            session.bump();
            view.lines(&session, area(80, 20), &Theme::dark(), None, &welcome());
            assert_eq!(view.collapsible_at(2), None);
            assert_eq!(view.collapsible_at(3), None);
            assert_eq!(view.headers.subagents[&(0, 0)].row, 2);
            assert_eq!(view.headers.subagents[&(0, 1)].row, 3);
            assert_eq!(view.collapsible_at(4), None);
        }
    }

    #[test]
    fn child_component_keeps_its_click_through_rebuilds_and_is_removed_on_clear() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let mut view = TranscriptView::new();
        let mut session = session_with(Turn {
            steps: vec![subagent_step(false, false)],
            ..Default::default()
        });
        let viewport = area(80, 20);
        view.lines(&session, viewport, &Theme::dark(), Some(0), &welcome());
        let pointer = |kind| MouseEvent {
            kind,
            column: 2,
            row: 1,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(
            view.handle_subagent_mouse(
                pointer(MouseEventKind::Down(MouseButton::Left)),
                viewport,
                0,
                false
            ),
            (true, None)
        );
        session.bump();
        view.lines(&session, viewport, &Theme::dark(), Some(1), &welcome());
        assert_eq!(
            view.handle_subagent_mouse(
                pointer(MouseEventKind::Up(MouseButton::Left)),
                viewport,
                0,
                false
            ),
            (true, Some((0, 0)))
        );
        session.turns.clear();
        view.lines(&session, viewport, &Theme::dark(), None, &welcome());
        assert!(view.headers.subagents.is_empty());
    }
    #[test]
    fn a_step_line_longer_than_the_screen_is_cut() {
        let call = ToolCall::new(
            "call-1",
            "read",
            serde_json::json!({ "path": "x".repeat(200) }),
        );
        let mut step = ToolStep::pending(call);
        step.disclosure.set_expanded(true);
        let session = session_with(Turn {
            prompt: "p".to_string(),
            steps: vec![step],
            ..Default::default()
        });

        let lines = build(&session, area(40, 20), &Theme::dark(), None, &welcome());
        let step = lines
            .iter()
            .map(line_text)
            .find(|line| line.contains("Read"))
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
