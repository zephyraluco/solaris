//! Solaris terminal layout and input hit targets; no execution lifecycle.
use super::*;

impl App {
    /// Render the main session page (`TranscriptContainer` + `BottomDock`).
    pub(super) fn render_session(&mut self, buf: &mut Buffer, area: Rect) {
        let snapshot = self.sessions.agent.snapshot();
        let context = if self.session.context.window > 0 {
            self.session.context
        } else {
            snapshot.context(self.config.mode)
        };
        let status_state = StatusBarState {
            label: snapshot.label,
            model: snapshot.model,
            mode: self.config.mode,
            context,
        };
        self.session_page
            .bottom
            .sync_prompt(status_state, !self.overlay_open());

        let spinner = self.session.is_streaming().then_some(self.spinner_frame);
        let welcome = self.welcome_data();
        self.session_page.render(
            buf,
            area,
            &mut self.session,
            &self.theme,
            spinner,
            &welcome,
            &self.selection,
        );
    }

    pub(super) fn handle_collapsible_mouse(&mut self, event: MouseEvent) -> bool {
        let transcript_area = self.session_page.transcript.area();
        let over_transcript = rect_contains(transcript_area, event.column, event.row);
        let scroll = self
            .subagent_terminal
            .as_ref()
            .map_or(self.session.scroll, |view| view.scroll);
        let row = scroll + usize::from(event.row.saturating_sub(transcript_area.y));
        let header = over_transcript
            .then(|| match &self.subagent_terminal {
                Some(view) => view
                    .headers
                    .iter()
                    .find(|header| header.row == row)
                    .copied(),
                None => self.session_page.transcript.collapsible_at(row),
            })
            .flatten();
        let hit = header.map(|header| {
            CollapsibleHit::new(
                (header.target.0, header.target.1, header.detail),
                Rect::new(transcript_area.x, event.row, transcript_area.width, 1),
                header.marker_offset,
            )
        });
        let selected = self.selection.borrow().is_active();
        match self
            .session_page
            .transcript
            .collapsible_mouse_mut()
            .handle_mouse(event, hit, selected)
        {
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
                            .min(usize::from(transcript_area.height.saturating_sub(1)));
                        let end = usize::from(event.row - transcript_area.y) + 1 + reveal;
                        let shift = end.saturating_sub(usize::from(transcript_area.height));
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
        self.session_page.transcript.cancel_interactions();
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
        let page = i32::from(self.session_page.transcript.area().height.max(1));
        match key.code {
            KeyCode::Esc => {
                self.subagent_terminal = None;
                self.session_page.transcript.cancel_collapsible();
                self.session_page.transcript.set_area(Rect::default());
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
        self.session_page.transcript.set_area(content);
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
