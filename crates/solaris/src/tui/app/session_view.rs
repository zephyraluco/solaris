//! Solaris terminal layout and input hit targets; no execution lifecycle.
use super::*;
impl App {
    /// Render the transcript, the prompt region, and the status footer.
    pub(super) fn render_session(&mut self, buf: &mut Buffer, area: Rect) {
        if area.width < 12 || area.height < 4 {
            self.last_transcript = Rect::default();
            self.collapsible_mouse.cancel();
            self.transcript.cancel_subagent_interaction();
            return;
        }

        // Active questions take over both the prompt region and the bottom
        // status footer so the panel's hint line sits flush at the bottom edge.
        if self.inline.is_none()
            && let Some(question) = self.question.as_mut()
        {
            let max_panel = area.height.saturating_sub(1).max(3);
            let panel_height = question
                .desired_height(area.width)
                .unwrap_or(8)
                .clamp(3, max_panel);
            let rows = layout::split(
                area,
                Axis::Vertical,
                &[Entry::auto().grow(1).min(1), Entry::px(panel_height)],
            );

            self.render_transcript(buf, rows[0]);
            if let Some(question) = self.question.as_mut() {
                question.render(buf, rows[1]);
            }
            return;
        }

        let max_editor = area.height.saturating_sub(2).max(3);
        // Inline flows (`/connect`, `/model`) take over the prompt region
        // instead of opening an overlay, replacing the input box.
        let prompt_height = match self.inline.as_mut() {
            Some(flow) => flow.desired_height(area.width),
            None => self.editor.desired_height(area.width).unwrap_or(3),
        };
        let prompt_height = prompt_height.clamp(3, max_editor);

        let rows = layout::split(
            area,
            Axis::Vertical,
            &[
                Entry::auto().grow(1).min(1),
                Entry::px(prompt_height),
                Entry::px(1),
            ],
        );

        self.render_transcript(buf, rows[0]);
        match self.inline.as_mut() {
            Some(flow) => flow.render(buf, rows[1]),
            None => {
                self.editor.set_focused(!self.overlay_open());
                self.editor.render(buf, rows[1]);
            }
        }
        self.render_footer(buf, rows[2]);
    }

    pub(super) fn render_transcript(&mut self, buf: &mut Buffer, area: Rect) {
        self.last_transcript = area;
        if area.width == 0 || area.height == 0 {
            return;
        }

        let spinner = self.session.is_streaming().then_some(self.spinner_frame);
        let welcome = self.welcome_data();
        let lines = self
            .transcript
            .lines(&self.session, area, &self.theme, spinner, &welcome);

        let visible = area.height as usize;
        let max_scroll = lines.len().saturating_sub(visible);

        if self.session.follow_end {
            self.session.scroll = max_scroll;
        }
        self.session.scroll = self.session.scroll.min(max_scroll);
        // Re-arm following once the view is back at the bottom.
        if self.session.scroll >= max_scroll {
            self.session.follow_end = true;
        }

        for (row, line) in lines
            .iter()
            .skip(self.session.scroll)
            .take(visible)
            .enumerate()
        {
            buf.set_line(area.x, area.y + row as u16, line, area.width);
        }
        self.selection.borrow_mut().set_document(
            area,
            self.session.scroll,
            self.transcript.document(),
        );
    }

    pub(super) fn handle_collapsible_mouse(&mut self, event: MouseEvent) -> bool {
        let over_transcript = rect_contains(self.last_transcript, event.column, event.row);
        let scroll = self
            .subagent_terminal
            .as_ref()
            .map_or(self.session.scroll, |view| view.scroll);
        let row = scroll + usize::from(event.row.saturating_sub(self.last_transcript.y));
        let header = over_transcript
            .then(|| match &self.subagent_terminal {
                Some(view) => view
                    .headers
                    .iter()
                    .find(|header| header.row == row)
                    .copied(),
                None => self.transcript.collapsible_at(row),
            })
            .flatten();
        let hit = header.map(|header| {
            CollapsibleHit::new(
                (header.target.0, header.target.1, header.detail),
                Rect::new(
                    self.last_transcript.x,
                    event.row,
                    self.last_transcript.width,
                    1,
                ),
                header.marker_offset,
            )
        });
        let selected = self.selection.borrow().is_active();
        match self.collapsible_mouse.handle_mouse(event, hit, selected) {
            CollapsibleAction::Ignored => false,
            CollapsibleAction::Handled => true,
            CollapsibleAction::Toggle((turn, step, detail)) => {
                let tool = if let Some(view) = &self.subagent_terminal {
                    self.session
                        .turns
                        .get_mut(view.target.0)
                        .and_then(|turn| turn.steps.get_mut(view.target.1))
                        .and_then(|step| step.subagent.as_mut())
                        .and_then(|child| child.steps.get_mut(step))
                } else {
                    self.session
                        .turns
                        .get_mut(turn)
                        .and_then(|turn| turn.steps.get_mut(step))
                };
                if let Some(step) = tool {
                    let state = if detail {
                        &mut step.detail_disclosure
                    } else {
                        &mut step.disclosure
                    };
                    if state.toggle() {
                        // A title on the viewport's bottom row must reveal
                        // its details, not expand entirely below the screen.
                        let reveal = header
                            .map_or(0, |header| header.body_rows)
                            .min(usize::from(self.last_transcript.height.saturating_sub(1)));
                        let end = usize::from(event.row - self.last_transcript.y) + 1 + reveal;
                        let shift = end.saturating_sub(usize::from(self.last_transcript.height));
                        if let Some(view) = &mut self.subagent_terminal {
                            view.scroll = view.scroll.saturating_add(shift);
                        } else {
                            self.session.scroll = self.session.scroll.saturating_add(shift);
                        }
                    }
                    // Keep the title in view instead of jumping to the tail
                    // when expansion adds more content below the viewport.
                    if let Some(view) = &mut self.subagent_terminal {
                        view.follow_end = false;
                    } else {
                        self.session.follow_end = false;
                    }
                    if let Some(view) = &mut self.subagent_terminal {
                        view.layout_key = None;
                    }
                    self.session.bump();
                }
                true
            }
        }
    }

    pub(super) fn render_footer(&mut self, buf: &mut Buffer, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let notice = self.notifications.current();
        let (right, style) = if let Some((kind, text)) = notice {
            let style = match kind {
                NoticeKind::Error => Style::default()
                    .fg(self.theme.error)
                    .add_modifier(Modifier::BOLD),
                NoticeKind::Warning => Style::default().fg(self.theme.warning),
                NoticeKind::Info => Style::default().fg(self.theme.muted),
            };
            (format!("{} {} ", kind.marker(), text), style)
        } else if self.session.is_streaming() {
            let glyph = SPINNER[self.spinner_frame as usize % SPINNER.len()];
            let status = self.session.status.as_deref().unwrap_or("working");
            // Child activity is shown by its summary component.
            if status.starts_with("subagent ") {
                (String::new(), Style::default())
            } else {
                (
                    format!("{glyph} {status} "),
                    Style::default().fg(self.theme.accent),
                )
            }
        } else {
            (
                format!("{} ", keymap::FOOTER_HINT),
                Style::default().fg(self.theme.dim),
            )
        };

        // The right slot wins the space it needs; the left one is truncated to
        // whatever remains so the two never overlap. A notice is shown even
        // when it is wider than the footer — truncated, because dropping it
        // would hide the one message that says what just went wrong — while the
        // standing hint and the spinner give way to the facts beside them.
        let right_width = display_width(&right) as u16;
        let show_right = notice.is_some() || right_width + 4 < area.width;
        let left_budget = if show_right {
            area.width.saturating_sub(right_width.min(area.width))
        } else {
            area.width
        };

        // The model slot disappears while no model is named, so the footer
        // never shows an empty gap between two separators.
        let snapshot = self.sessions.agent.snapshot();
        let muted = Style::default().fg(self.theme.muted);
        let context = if self.session.context.window > 0 {
            self.session.context
        } else {
            snapshot.context(self.config.mode)
        };
        let (gauge, gauge_style) = context_gauge(context, &self.theme);
        let mut prefix_facts = vec![snapshot.label];
        if !snapshot.model.is_empty() {
            prefix_facts.push(snapshot.model);
        }
        prefix_facts.push(self.config.mode.label().to_string());
        let segments = [
            (format!(" {} · 上下文 ", prefix_facts.join(" · ")), muted),
            (gauge, gauge_style),
            (" ".to_string(), muted),
        ];
        draw_footer_segments(buf, area.x, area.y, left_budget as usize, &segments, muted);

        if show_right {
            let width = right_width.min(area.width);
            buf.set_string(
                area.x + area.width - width,
                area.y,
                truncate_to_width(&right, width as usize, "…"),
                style,
            );
        }
    }
}

const CONTEXT_BAR_WIDTH: usize = 10;

/// Format the context usage gauge (`░░░░░░░░░░ 0%`) and pick its status colour.
fn context_gauge(context: solaris_agent::ContextUsage, theme: &Theme) -> (String, Style) {
    let ratio = if context.window == 0 {
        0.0
    } else {
        f64::from(context.tokens.unwrap_or(0)) / f64::from(context.window)
    };
    let percent = (ratio * 100.0).round().max(0.0) as u32;
    let filled = ((ratio.clamp(0.0, 1.0) * CONTEXT_BAR_WIDTH as f64).round() as usize)
        .min(CONTEXT_BAR_WIDTH);
    let empty = CONTEXT_BAR_WIDTH - filled;
    let mut bar = String::with_capacity(CONTEXT_BAR_WIDTH * 3 + 6);
    for _ in 0..filled {
        bar.push('█');
    }
    for _ in 0..empty {
        bar.push('░');
    }
    bar.push(' ');
    bar.push_str(&format!("{percent}%"));

    let color = if percent >= 90 {
        theme.error
    } else if percent >= 70 {
        theme.warning
    } else {
        theme.success
    };
    (bar, Style::default().fg(color))
}

fn draw_footer_segments(
    buf: &mut Buffer,
    start_x: u16,
    y: u16,
    budget: usize,
    segments: &[(String, Style)],
    ellipsis_style: Style,
) {
    if budget == 0 {
        return;
    }
    let total: usize = segments.iter().map(|(text, _)| display_width(text)).sum();
    if total <= budget {
        let mut x = start_x;
        for (text, style) in segments {
            buf.set_string(x, y, text, *style);
            x = x.saturating_add(display_width(text) as u16);
        }
        return;
    }

    let content_budget = budget.saturating_sub(1);
    let mut used = 0usize;
    'outer: for (text, style) in segments {
        let seg_start = used;
        let mut slice_end = 0usize;
        for (idx, ch) in text.char_indices() {
            let ch_width = display_width(&text[idx..idx + ch.len_utf8()]);
            if used + ch_width > content_budget {
                if slice_end > 0 {
                    buf.set_string(start_x + seg_start as u16, y, &text[..slice_end], *style);
                }
                break 'outer;
            }
            used += ch_width;
            slice_end = idx + ch.len_utf8();
        }
        if slice_end > 0 {
            buf.set_string(start_x + seg_start as u16, y, &text[..slice_end], *style);
        }
    }
    buf.set_string(start_x + used as u16, y, "…", ellipsis_style);
}

#[derive(Default)]
pub(super) struct SubagentTerminal {
    pub(super) target: (usize, usize),
    pub(super) scroll: usize,
    pub(super) follow_end: bool,
    pub(super) headers: Vec<crate::tui::view::transcript::CollapsibleHeader>,
    pub(super) layout: Option<crate::tui::view::transcript::SubagentTranscript>,
    pub(super) layout_key: Option<(crate::tui::view::transcript::TurnKey, u16, String)>,
}

impl App {
    pub(super) fn subagent_targets(&self) -> Vec<(usize, usize)> {
        self.session
            .turns
            .iter()
            .enumerate()
            .flat_map(|(turn, state)| {
                state
                    .steps
                    .iter()
                    .enumerate()
                    .filter_map(move |(step, state)| state.subagent.as_ref().map(|_| (turn, step)))
            })
            .collect()
    }

    pub(super) fn open_subagent(&mut self, target: (usize, usize)) {
        self.subagent_terminal = Some(SubagentTerminal {
            target,
            follow_end: true,
            ..Default::default()
        });
        self.collapsible_mouse.cancel();
        self.transcript.cancel_subagent_interaction();
        self.selection.borrow_mut().clear();
    }

    pub(super) fn switch_subagent(&mut self, next: bool) {
        let targets = self.subagent_targets();
        if targets.is_empty() {
            return;
        }
        let current = self
            .subagent_terminal
            .as_ref()
            .and_then(|view| targets.iter().position(|target| *target == view.target));
        let index = match current {
            Some(index) if next => (index + 1) % targets.len(),
            Some(index) => (index + targets.len() - 1) % targets.len(),
            None if next => 0,
            None => targets.len() - 1,
        };
        self.open_subagent(targets[index]);
    }

    pub(super) fn scroll_subagent(&mut self, delta: i32) {
        if let Some(view) = &mut self.subagent_terminal {
            view.scroll = view.scroll.saturating_add_signed(delta as isize);
            view.follow_end = false;
        }
    }

    pub(super) fn handle_subagent_key(&mut self, key: KeyEvent) -> KeyResult {
        let page = i32::from(self.last_transcript.height.max(1));
        match key.code {
            KeyCode::Esc => {
                self.subagent_terminal = None;
                self.collapsible_mouse.cancel();
                self.last_transcript = Rect::default();
                self.selection.borrow_mut().clear();
            }
            KeyCode::Left => self.switch_subagent(false),
            KeyCode::Right => self.switch_subagent(true),
            KeyCode::Up => self.scroll_subagent(-1),
            KeyCode::Down => self.scroll_subagent(1),
            KeyCode::PageUp => self.scroll_subagent(-page),
            KeyCode::PageDown => self.scroll_subagent(page),
            KeyCode::Home => {
                if let Some(view) = &mut self.subagent_terminal {
                    view.scroll = 0;
                    view.follow_end = false;
                }
            }
            KeyCode::End => {
                if let Some(view) = &mut self.subagent_terminal {
                    view.follow_end = true;
                }
            }
            _ => {}
        }
        KeyResult::Handled
    }

    pub(super) fn render_subagent(&mut self, buf: &mut Buffer, area: Rect) {
        let targets = self.subagent_targets();
        let Some(view) = &mut self.subagent_terminal else {
            return;
        };
        let Some(turn) = self.session.turns.get(view.target.0) else {
            self.subagent_terminal = None;
            return;
        };
        let Some(step) = turn.steps.get(view.target.1) else {
            self.subagent_terminal = None;
            return;
        };
        if area.is_empty() {
            return;
        }
        let index = targets
            .iter()
            .position(|target| *target == view.target)
            .unwrap_or(0)
            + 1;
        let name = step.call.input["agent"].as_str().unwrap_or("subagent");
        let identity = step
            .subagent
            .as_ref()
            .and_then(|child| child.task_id.as_deref())
            .unwrap_or(&step.call.id);
        let title = format!(" Subagent {index}/{} · {name} · {identity}", targets.len());
        buf.set_string(
            area.x,
            area.y,
            truncate_to_width(&title, usize::from(area.width), "…"),
            Style::default()
                .fg(self.theme.accent)
                .add_modifier(Modifier::BOLD),
        );
        let content = Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(2),
        );
        self.last_transcript = content;
        let key = (
            crate::tui::view::transcript::TurnKey::from(turn),
            area.width,
            self.theme.name.clone(),
        );
        if view.layout_key.as_ref() != Some(&key) {
            let layout = crate::tui::view::transcript::subagent_layout(
                step,
                turn.complete,
                area.width,
                &self.theme,
                Some(self.spinner_frame),
            );
            view.headers = layout.headers.clone();
            view.layout = Some(layout);
            view.layout_key = Some(key);
        }
        let lines = &mut view.layout.as_mut().expect("child layout prepared").lines;
        if let Some(line) = lines.last_mut()
            && line.to_string().ends_with(" working…")
        {
            *line = Line::styled(
                format!(
                    "{} working…",
                    SPINNER[self.spinner_frame as usize % SPINNER.len()]
                ),
                Style::default().fg(self.theme.accent),
            );
        }
        let max_scroll = lines.len().saturating_sub(usize::from(content.height));
        if view.follow_end {
            view.scroll = max_scroll;
        }
        view.scroll = view.scroll.min(max_scroll);
        if view.scroll == max_scroll {
            view.follow_end = true;
        }
        for (row, line) in lines
            .iter()
            .skip(view.scroll)
            .take(usize::from(content.height))
            .enumerate()
        {
            buf.set_line(content.x, content.y + row as u16, line, content.width);
        }
        if area.height > 1 {
            buf.set_string(
                area.x,
                area.bottom() - 1,
                truncate_to_width(
                    " Esc back · ←/→ switch agents · ↑/↓ scroll · PgUp/PgDn",
                    usize::from(area.width),
                    "…",
                ),
                Style::default().fg(self.theme.muted),
            );
        }
    }
}
