//! Multi-line prompt editor.
//!
//! Ported from pi-tui's `Editor`: multi-line editing, word navigation, a
//! kill-ring, an undo stack, history, and slash-command completion that is
//! rendered inside the input frame.
//!
//! `Ctrl+K` is deliberately *not* bound here: the application uses it for the
//! command palette. `Ctrl+U` / `Ctrl+W` still delete backwards.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, Widget};
use unicode_width::UnicodeWidthChar;

use crate::component::{Component, KeyResult, MouseResult};
use crate::util::{display_width, rect_contains, truncate_to_width};

/// Maximum text rows before the editor starts scrolling.
const MAX_TEXT_ROWS: u16 = 8;
/// Maximum completion rows drawn above the text.
const MAX_COMPLETION_ROWS: u16 = 5;
/// Prompt marker drawn before the first display row (2 cells wide).
const PROMPT: &str = "› ";
/// Indent for every other display row — wrapped continuations and additional
/// logical lines alike (2 cells wide).
const CONTINUATION: &str = "  ";
/// Undo depth.
const MAX_UNDO: usize = 200;

/// A slash command offered by the editor's completion popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandHint {
    pub name: String,
    pub description: String,
    pub args: String,
}

impl CommandHint {
    /// A hint for `/name`.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        args: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            args: args.into(),
        }
    }
}

/// Styles used while drawing the editor.
#[derive(Debug, Clone, Copy)]
pub struct EditorStyles {
    pub border: Style,
    pub border_focused: Style,
    pub prompt: Style,
    pub text: Style,
    pub placeholder: Style,
    pub completion: Style,
    pub completion_selected: Style,
    pub cursor: Style,
}

impl Default for EditorStyles {
    fn default() -> Self {
        Self {
            border: Style::default().add_modifier(Modifier::DIM),
            border_focused: Style::default(),
            prompt: Style::default().add_modifier(Modifier::BOLD),
            text: Style::default(),
            placeholder: Style::default().add_modifier(Modifier::DIM),
            completion: Style::default(),
            completion_selected: Style::default().add_modifier(Modifier::BOLD),
            cursor: Style::default().add_modifier(Modifier::REVERSED),
        }
    }
}

type Snapshot = (Vec<Vec<char>>, usize, usize);

struct DisplayLine {
    text: String,
    logical: usize,
    start: usize,
}

/// The prompt input widget.
pub struct Editor {
    lines: Vec<Vec<char>>,
    cursor_row: usize,
    cursor_col: usize,
    preferred_col: Option<usize>,
    history: Vec<String>,
    history_cursor: Option<usize>,
    history_stash: Option<String>,
    kill_ring: Vec<String>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    submitted: Option<String>,
    placeholder: String,
    hints: Vec<CommandHint>,
    completion_index: usize,
    completion_dismissed: bool,
    focused: bool,
    scroll_row: u16,
    styles: EditorStyles,
    version: u64,
    last_area: Rect,
    last_text_area: Rect,
    last_display: Vec<DisplayLine>,
    last_scroll: u16,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    /// An empty editor with a default placeholder.
    pub fn new() -> Self {
        Self {
            lines: vec![Vec::new()],
            cursor_row: 0,
            cursor_col: 0,
            preferred_col: None,
            history: Vec::new(),
            history_cursor: None,
            history_stash: None,
            kill_ring: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            submitted: None,
            placeholder: "Ask anything…".to_string(),
            hints: Vec::new(),
            completion_index: 0,
            completion_dismissed: false,
            focused: true,
            scroll_row: 0,
            styles: EditorStyles::default(),
            version: 0,
            last_area: Rect::default(),
            last_text_area: Rect::default(),
            last_display: Vec::new(),
            last_scroll: 0,
        }
    }

    /// Editor with a custom placeholder.
    pub fn with_placeholder(placeholder: impl Into<String>) -> Self {
        let mut editor = Self::new();
        editor.placeholder = placeholder.into();
        editor
    }

    /// Replace the placeholder text.
    pub fn set_placeholder(&mut self, placeholder: impl Into<String>) {
        self.placeholder = placeholder.into();
    }

    /// Provide the slash commands offered by completion.
    pub fn set_hints(&mut self, hints: Vec<CommandHint>) {
        self.hints = hints;
        self.bump();
    }

    /// Apply drawing styles.
    pub fn set_styles(&mut self, styles: EditorStyles) {
        self.styles = styles;
        self.bump();
    }

    /// Mark the editor as focused (draws the cursor).
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Whether the editor is focused.
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    /// The current text, with lines joined by `\n`.
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|line| line.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Replace the text and move the cursor to the end.
    pub fn set_text(&mut self, text: &str) {
        self.record_undo();
        self.set_text_untracked(text);
    }

    /// Whether the editor is empty.
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// Take the submitted text, if the user pressed Enter since the last call.
    pub fn take_submitted(&mut self) -> Option<String> {
        self.submitted.take()
    }

    /// Number of history entries.
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// Add a submitted prompt to the history.
    pub fn push_history(&mut self, entry: impl Into<String>) {
        let entry = entry.into();
        if entry.trim().is_empty() {
            return;
        }
        if self.history.last().map(String::as_str) != Some(entry.as_str()) {
            self.history.push(entry);
        }
        self.history_cursor = None;
        self.history_stash = None;
    }

    /// Insert pasted text, honouring embedded newlines.
    pub fn insert_paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.record_undo();
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let mut parts = normalized.split('\n');
        if let Some(first) = parts.next() {
            self.insert_str_raw(first);
        }
        for part in parts {
            self.insert_newline_raw();
            self.insert_str_raw(part);
        }
        self.bump();
    }

    /// Clear the text and the cursor.
    pub fn clear(&mut self) {
        self.record_undo();
        self.set_text_untracked("");
    }

    // ---------------------------------------------------------------- internals

    fn bump(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    fn record_undo(&mut self) {
        self.undo
            .push((self.lines.clone(), self.cursor_row, self.cursor_col));
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.completion_dismissed = false;
        self.bump();
    }

    fn set_text_untracked(&mut self, text: &str) {
        self.lines = text
            .split('\n')
            .map(|line| line.chars().collect::<Vec<char>>())
            .collect();
        if self.lines.is_empty() {
            self.lines.push(Vec::new());
        }
        self.cursor_row = self.lines.len() - 1;
        self.cursor_col = self.lines[self.cursor_row].len();
        self.preferred_col = None;
        self.scroll_row = 0;
        self.completion_dismissed = false;
        self.bump();
    }

    fn insert_str_raw(&mut self, text: &str) {
        for ch in text.chars() {
            self.lines[self.cursor_row].insert(self.cursor_col, ch);
            self.cursor_col += 1;
        }
        self.preferred_col = None;
    }

    fn insert_newline_raw(&mut self) {
        let tail = self.lines[self.cursor_row].split_off(self.cursor_col);
        self.lines.insert(self.cursor_row + 1, tail);
        self.cursor_row += 1;
        self.cursor_col = 0;
        self.preferred_col = None;
    }

    fn current_line(&self) -> &Vec<char> {
        &self.lines[self.cursor_row]
    }

    fn slash_query(&self) -> Option<String> {
        if self.lines.len() != 1 || self.cursor_row != 0 {
            return None;
        }
        if self.cursor_col != self.lines[0].len() {
            return None;
        }
        let text: String = self.lines[0].iter().collect();
        let trimmed = text.trim_start();
        let body = trimmed.strip_prefix('/')?;
        // An empty body means "show every command".
        if body.chars().any(char::is_whitespace) {
            return None;
        }
        Some(body.to_string())
    }

    fn completions(&self) -> Vec<usize> {
        if self.completion_dismissed {
            return Vec::new();
        }
        let Some(query) = self.slash_query() else {
            return Vec::new();
        };
        let query = query.to_lowercase();
        self.hints
            .iter()
            .enumerate()
            .filter(|(_, hint)| hint.name.to_lowercase().starts_with(&query))
            .map(|(index, _)| index)
            .collect()
    }

    fn completion_rows(&self) -> u16 {
        self.completions().len().min(MAX_COMPLETION_ROWS as usize) as u16
    }

    fn accept_completion(&mut self) -> bool {
        let completions = self.completions();
        if completions.is_empty() {
            return false;
        }
        // `completions` holds hint indices, so index it by the selected *position*.
        let hint_index = completions[self.completion_index.min(completions.len() - 1)];
        let hint = self.hints[hint_index].clone();
        let mut text = format!("/{}", hint.name);
        if !hint.args.is_empty() {
            text.push(' ');
        }
        self.record_undo();
        self.set_text_untracked(&text);
        self.completion_index = 0;
        true
    }

    fn wrap_width(&self, width: u16) -> usize {
        (width.saturating_sub(2 + PROMPT.chars().count() as u16) as usize).max(1)
    }

    fn display_rows(&self, wrap: usize) -> usize {
        self.lines
            .iter()
            .map(|line| wrap_chars(line, wrap).len())
            .sum()
    }

    fn build_display(&self, wrap: usize) -> Vec<DisplayLine> {
        let mut display = Vec::new();
        for (row, line) in self.lines.iter().enumerate() {
            for (start, _end, text) in wrap_chars(line, wrap) {
                display.push(DisplayLine {
                    text,
                    logical: row,
                    start,
                });
            }
        }
        display
    }

    fn cursor_display(&self, display: &[DisplayLine]) -> Option<(usize, usize)> {
        let mut index = None;
        for (i, line) in display.iter().enumerate() {
            if line.logical == self.cursor_row && line.start <= self.cursor_col {
                index = Some(i);
            }
        }
        let i = index?;
        let line = &display[i];
        let take = self
            .cursor_col
            .saturating_sub(line.start)
            .min(line.text.chars().count());
        let prefix: String = line.text.chars().take(take).collect();
        Some((i, display_width(&prefix)))
    }

    // ------------------------------------------------------------- edit actions

    fn insert_char(&mut self, ch: char) {
        self.record_undo();
        self.lines[self.cursor_row].insert(self.cursor_col, ch);
        self.cursor_col += 1;
        self.preferred_col = None;
    }

    fn insert_newline(&mut self) {
        self.record_undo();
        self.insert_newline_raw();
    }

    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.record_undo();
            self.cursor_col -= 1;
            self.lines[self.cursor_row].remove(self.cursor_col);
        } else if self.cursor_row > 0 {
            self.record_undo();
            let current = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
            self.lines[self.cursor_row].extend(current);
        } else {
            return;
        }
        self.preferred_col = None;
    }

    fn delete_forward(&mut self) {
        if self.cursor_col < self.lines[self.cursor_row].len() {
            self.record_undo();
            self.lines[self.cursor_row].remove(self.cursor_col);
        } else if self.cursor_row + 1 < self.lines.len() {
            self.record_undo();
            let next = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].extend(next);
        } else {
            return;
        }
        self.preferred_col = None;
    }

    fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
        }
        self.preferred_col = None;
    }

    fn move_right(&mut self) {
        if self.cursor_col < self.lines[self.cursor_row].len() {
            self.cursor_col += 1;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
        self.preferred_col = None;
    }

    fn move_vertical(&mut self, delta: i32) {
        let target_row = self.cursor_row as i32 + delta;
        if target_row < 0 || target_row as usize >= self.lines.len() {
            return;
        }
        let preferred = *self.preferred_col.get_or_insert(self.cursor_col);
        self.cursor_row = target_row as usize;
        self.cursor_col = preferred.min(self.lines[self.cursor_row].len());
    }

    fn word_left(&mut self) {
        if self.cursor_col == 0 {
            if self.cursor_row > 0 {
                self.cursor_row -= 1;
                self.cursor_col = self.lines[self.cursor_row].len();
            }
            self.preferred_col = None;
            return;
        }
        let line = &self.lines[self.cursor_row];
        let mut index = self.cursor_col;
        while index > 0 && line[index - 1].is_whitespace() {
            index -= 1;
        }
        while index > 0 && !line[index - 1].is_whitespace() {
            index -= 1;
        }
        self.cursor_col = index;
        self.preferred_col = None;
    }

    fn word_right(&mut self) {
        let line = &self.lines[self.cursor_row];
        let len = line.len();
        if self.cursor_col >= len {
            if self.cursor_row + 1 < self.lines.len() {
                self.cursor_row += 1;
                self.cursor_col = 0;
            }
            self.preferred_col = None;
            return;
        }
        let mut index = self.cursor_col;
        while index < len && line[index].is_whitespace() {
            index += 1;
        }
        while index < len && !line[index].is_whitespace() {
            index += 1;
        }
        self.cursor_col = index;
        self.preferred_col = None;
    }

    fn kill_to_line_start(&mut self) {
        if self.cursor_col == 0 {
            return;
        }
        self.record_undo();
        let removed: String = self.lines[self.cursor_row]
            .drain(..self.cursor_col)
            .collect();
        self.kill_ring.push(removed);
        self.cursor_col = 0;
        self.preferred_col = None;
    }

    fn kill_word_back(&mut self) {
        if self.cursor_col == 0 {
            return;
        }
        let line = &self.lines[self.cursor_row];
        let mut index = self.cursor_col;
        while index > 0 && line[index - 1].is_whitespace() {
            index -= 1;
        }
        while index > 0 && !line[index - 1].is_whitespace() {
            index -= 1;
        }
        let start = index;
        self.record_undo();
        let removed: String = self.lines[self.cursor_row]
            .drain(start..self.cursor_col)
            .collect();
        self.kill_ring.push(removed);
        self.cursor_col = start;
        self.preferred_col = None;
    }

    fn kill_word_forward(&mut self) {
        let line = &self.lines[self.cursor_row];
        let len = line.len();
        if self.cursor_col >= len {
            return;
        }
        let mut index = self.cursor_col;
        while index < len && line[index].is_whitespace() {
            index += 1;
        }
        while index < len && !line[index].is_whitespace() {
            index += 1;
        }
        let end = index;
        self.record_undo();
        let removed: String = self.lines[self.cursor_row]
            .drain(self.cursor_col..end)
            .collect();
        self.kill_ring.push(removed);
        self.preferred_col = None;
    }

    fn undo(&mut self) {
        if let Some((lines, row, col)) = self.undo.pop() {
            self.redo
                .push((self.lines.clone(), self.cursor_row, self.cursor_col));
            self.lines = lines;
            self.cursor_row = row.min(self.lines.len().saturating_sub(1));
            self.cursor_col = col.min(self.lines[self.cursor_row].len());
            self.bump();
        }
    }

    fn redo(&mut self) {
        if let Some((lines, row, col)) = self.redo.pop() {
            self.undo
                .push((self.lines.clone(), self.cursor_row, self.cursor_col));
            self.lines = lines;
            self.cursor_row = row.min(self.lines.len().saturating_sub(1));
            self.cursor_col = col.min(self.lines[self.cursor_row].len());
            self.bump();
        }
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_cursor {
            Some(0) => 0,
            Some(i) => i - 1,
            None => {
                self.history_stash = Some(self.text());
                self.history.len() - 1
            }
        };
        self.history_cursor = Some(index);
        let entry = self.history[index].clone();
        self.set_text_untracked(&entry);
    }

    fn history_next(&mut self) {
        let Some(current) = self.history_cursor else {
            return;
        };
        if current + 1 < self.history.len() {
            self.history_cursor = Some(current + 1);
            let entry = self.history[current + 1].clone();
            self.set_text_untracked(&entry);
        } else {
            self.history_cursor = None;
            let stash = self.history_stash.take().unwrap_or_default();
            self.set_text_untracked(&stash);
        }
    }

    fn submit(&mut self) -> KeyResult {
        let text = self.text();
        if text.trim().is_empty() {
            return KeyResult::Ignored;
        }
        self.push_history(text.clone());
        self.submitted = Some(text);
        self.set_text_untracked("");
        KeyResult::Handled
    }

    /// The slash-command list, drawn under the input frame rather than inside
    /// it: the highlighted row is marked with `›`, then the name, then a dimmed
    /// description.
    fn render_completions(&self, buf: &mut Buffer, area: Rect, completions: &[usize]) {
        if area.height == 0 || area.width < 6 {
            return;
        }

        for (row, hint_index) in completions.iter().take(area.height as usize).enumerate() {
            let y = area.y + row as u16;
            let selected = row
                == self
                    .completion_index
                    .min(completions.len().saturating_sub(1));
            let style = if selected {
                self.styles.completion_selected
            } else {
                self.styles.completion
            };
            let marker = if selected { "› " } else { "  " };
            buf.set_string(area.x, y, marker, style);

            // `hint_index` is an index into `self.hints`; `row` is the position
            // in the filtered list. Indexing `completions` with the former was
            // the bug that panicked whenever a filter left fewer matches than
            // the matched command's position.
            let hint = &self.hints[*hint_index];
            let name_width = area.width.saturating_sub(4) as usize;
            let label = truncate_to_width(&format!("/{}", hint.name), name_width, "…");
            let label_cells = display_width(&label) as u16;
            buf.set_string(area.x + 2, y, &label, style);
            let remaining = area.width.saturating_sub(2 + label_cells + 1);
            if remaining >= 4 {
                let description = truncate_to_width(&hint.description, remaining as usize, "…");
                buf.set_string(
                    area.x + 3 + label_cells,
                    y,
                    description,
                    self.styles.placeholder,
                );
            }
        }
    }

    fn chars_for_cells(text: &str, cells: usize) -> usize {
        let mut used = 0usize;
        let mut count = 0usize;
        for ch in text.chars() {
            let ch_width = ch.width().unwrap_or(0);
            if used + ch_width > cells {
                break;
            }
            used += ch_width;
            count += 1;
        }
        count
    }
}

impl Component for Editor {
    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        self.last_area = area;
        if area.width < 6 || area.height < 3 {
            return;
        }

        let completion_rows = self.completion_rows();
        // The frame holds the input alone; the command list is drawn underneath
        // it. When the area is too short the list gives up rows first, then the
        // frame shrinks down to its borders and a single text row.
        let wrap = self.wrap_width(area.width);
        let display = self.build_display(wrap);
        let wanted_text = display.len().clamp(1, MAX_TEXT_ROWS as usize) as u16;
        let mut list_rows = completion_rows;
        let mut box_height = 2 + wanted_text;
        while box_height + list_rows > area.height {
            if list_rows > 0 {
                list_rows -= 1;
            } else if box_height > 3 {
                box_height -= 1;
            } else {
                break;
            }
        }

        let box_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: box_height,
        };
        let list_area = Rect {
            x: area.x,
            y: area.y + box_height,
            width: area.width,
            height: list_rows,
        };

        let border_style = if self.focused {
            self.styles.border_focused
        } else {
            self.styles.border
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border_style);
        let inner = block.inner(box_area);
        block.render(box_area, buf);

        if inner.width < 4 || inner.height == 0 {
            return;
        }

        let completions = self.completions();
        self.render_completions(buf, list_area, &completions);

        let text_area = inner;
        self.last_text_area = text_area;
        if text_area.width < 4 || text_area.height == 0 {
            return;
        }

        if self.is_empty() {
            buf.set_string(text_area.x, text_area.y, PROMPT, self.styles.prompt);
            let placeholder = truncate_to_width(
                &self.placeholder,
                text_area.width.saturating_sub(2) as usize,
                "…",
            );
            buf.set_string(
                text_area.x + 2,
                text_area.y,
                placeholder,
                self.styles.placeholder,
            );
        }

        let cursor = self.cursor_display(&display);

        let visible = text_area.height as usize;
        if let Some((index, _)) = cursor {
            if index < self.scroll_row as usize {
                self.scroll_row = index as u16;
            } else if index >= self.scroll_row as usize + visible {
                self.scroll_row = (index + 1 - visible) as u16;
            }
        }
        let max_scroll = display.len().saturating_sub(visible) as u16;
        self.scroll_row = self.scroll_row.min(max_scroll);
        self.last_scroll = self.scroll_row;

        for (index, line) in display
            .iter()
            .enumerate()
            .skip(self.scroll_row as usize)
            .take(visible)
        {
            let y = text_area.y + (index - self.scroll_row as usize) as u16;
            // Only the first row of the whole prompt carries the marker; every
            // continuation row — a wrap or a new line — is indented, the way
            // the transcript echoes a sent prompt.
            let marker = if index == 0 { PROMPT } else { CONTINUATION };
            buf.set_string(text_area.x, y, marker, self.styles.prompt);
            let text =
                truncate_to_width(&line.text, text_area.width.saturating_sub(2) as usize, "");
            buf.set_string(text_area.x + 2, y, text, self.styles.text);
        }

        if self.focused {
            if let Some((index, cell_col)) = cursor {
                if index >= self.scroll_row as usize && index < self.scroll_row as usize + visible {
                    let y = text_area.y + (index - self.scroll_row as usize) as u16;
                    let x = text_area.x + 2 + cell_col as u16;
                    if x < text_area.x + text_area.width && y < text_area.y + text_area.height {
                        let style = buf[(x, y)].style().patch(self.styles.cursor);
                        buf[(x, y)].set_style(style);
                    }
                }
            }
        }

        self.last_display = display;
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        use KeyCode::*;

        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt_mod = key.modifiers.contains(KeyModifiers::ALT);

        // Completion navigation takes precedence while the popup is open.
        let completions_open = !self.completions().is_empty();

        if completions_open {
            match key.code {
                Up => {
                    self.completion_index = self.completion_index.saturating_sub(1);
                    return KeyResult::Handled;
                }
                Down => {
                    let last = self.completions().len().saturating_sub(1);
                    self.completion_index = (self.completion_index + 1).min(last);
                    return KeyResult::Handled;
                }
                Tab => {
                    if self.accept_completion() {
                        return KeyResult::Handled;
                    }
                    return KeyResult::Ignored;
                }
                // Enter runs the highlighted command instead of sending the
                // partial text: `/` on its own is not a command, and the menu
                // already says what the highlighted one does. `Tab` still just
                // fills it in, so arguments can be typed first.
                Enter if !alt_mod => {
                    if self.accept_completion() {
                        return self.submit();
                    }
                    return KeyResult::Ignored;
                }
                Esc => {
                    self.completion_dismissed = true;
                    return KeyResult::Handled;
                }
                _ => {}
            }
        }

        if control {
            match key.code {
                Char('a') => {
                    self.cursor_col = 0;
                    self.preferred_col = None;
                    return KeyResult::Handled;
                }
                Char('e') => {
                    self.cursor_col = self.current_line().len();
                    self.preferred_col = None;
                    return KeyResult::Handled;
                }
                Char('u') => {
                    self.kill_to_line_start();
                    return KeyResult::Handled;
                }
                Char('w') => {
                    self.kill_word_back();
                    return KeyResult::Handled;
                }
                Char('p') | Up => {
                    self.history_prev();
                    return KeyResult::Handled;
                }
                Char('n') | Down => {
                    self.history_next();
                    return KeyResult::Handled;
                }
                Char('z') => {
                    self.undo();
                    return KeyResult::Handled;
                }
                Char('y') => {
                    self.redo();
                    return KeyResult::Handled;
                }
                Left => {
                    self.word_left();
                    return KeyResult::Handled;
                }
                Right => {
                    self.word_right();
                    return KeyResult::Handled;
                }
                _ => {}
            }
        }

        if alt_mod {
            match key.code {
                Char('d') | Delete => {
                    self.kill_word_forward();
                    return KeyResult::Handled;
                }
                Char('b') | Left => {
                    self.word_left();
                    return KeyResult::Handled;
                }
                Char('f') | Right => {
                    self.word_right();
                    return KeyResult::Handled;
                }
                _ => {}
            }
        }

        if crate::keys::is_newline(&key) {
            self.insert_newline();
            return KeyResult::Handled;
        }
        if crate::keys::is_submit(&key) {
            return self.submit();
        }

        match key.code {
            Backspace => {
                self.backspace();
                KeyResult::Handled
            }
            Delete => {
                self.delete_forward();
                KeyResult::Handled
            }
            Left => {
                self.move_left();
                KeyResult::Handled
            }
            Right => {
                self.move_right();
                KeyResult::Handled
            }
            Up => {
                if self.cursor_row == 0 && self.lines.len() == 1 {
                    self.history_prev();
                } else {
                    self.move_vertical(-1);
                }
                KeyResult::Handled
            }
            Down => {
                if self.cursor_row + 1 == self.lines.len() && self.lines.len() == 1 {
                    self.history_next();
                } else {
                    self.move_vertical(1);
                }
                KeyResult::Handled
            }
            Home => {
                self.cursor_col = 0;
                self.preferred_col = None;
                KeyResult::Handled
            }
            End => {
                self.cursor_col = self.current_line().len();
                self.preferred_col = None;
                KeyResult::Handled
            }
            Char(ch) if !control && !alt_mod => {
                self.insert_char(ch);
                KeyResult::Handled
            }
            _ => KeyResult::Ignored,
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> MouseResult {
        if !rect_contains(self.last_area, event.column, event.row) {
            return MouseResult::Ignored;
        }
        if matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
            && rect_contains(self.last_text_area, event.column, event.row)
        {
            let row_index =
                self.last_scroll as usize + (event.row - self.last_text_area.y) as usize;
            if let Some(line) = self.last_display.get(row_index) {
                let cells = event.column.saturating_sub(self.last_text_area.x + 2) as usize;
                let offset = Self::chars_for_cells(&line.text, cells);
                let logical = line.logical;
                let start = line.start;
                let line_len = self.lines[logical].len();
                self.cursor_row = logical;
                self.cursor_col = (start + offset).min(line_len);
                self.preferred_col = None;
                return MouseResult::Handled;
            }
        }
        MouseResult::Ignored
    }

    fn handle_paste(&mut self, text: &str) -> KeyResult {
        self.insert_paste(text);
        KeyResult::Handled
    }

    fn desired_height(&mut self, width: u16) -> Option<u16> {
        let wrap = self.wrap_width(width);
        let rows = self.display_rows(wrap).clamp(1, MAX_TEXT_ROWS as usize) as u16;
        Some(2 + self.completion_rows() + rows)
    }

    fn version(&self) -> u64 {
        self.version
    }
}

/// Wrap a logical line into `(start, end, text)` segments of at most `width` cells.
fn wrap_chars(chars: &[char], width: usize) -> Vec<(usize, usize, String)> {
    if width == 0 {
        return vec![(0, chars.len(), chars.iter().collect())];
    }
    if chars.is_empty() {
        return vec![(0, 0, String::new())];
    }

    let mut segments = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;
    let mut line_width = 0usize;
    let mut last_space: Option<usize> = None;

    while index < chars.len() {
        let ch = chars[index];
        let ch_width = ch.width().unwrap_or(0);

        if line_width + ch_width > width {
            let break_at = match last_space {
                Some(space) if space >= start => space + 1,
                _ => (index + 1).max(start + 1),
            }
            .min(chars.len());

            segments.push((start, break_at, chars[start..break_at].iter().collect()));
            start = break_at;
            index = break_at;
            line_width = 0;
            last_space = None;
            continue;
        }

        // Record the break opportunity only for characters that fit, so an
        // overflowing space never gets pulled onto the previous line.
        if ch == ' ' {
            last_space = Some(index);
        }
        line_width += ch_width;
        index += 1;
    }

    if start < chars.len() || segments.is_empty() {
        segments.push((start, chars.len(), chars[start..].iter().collect()));
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn type_text(editor: &mut Editor, text: &str) {
        for ch in text.chars() {
            editor.handle_key(key(KeyCode::Char(ch), KeyModifiers::empty()));
        }
    }

    fn buffer_text(buffer: &Buffer) -> String {
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Mirrors the real command table, where `clear` sits at index 4.
    fn hints() -> Vec<CommandHint> {
        vec![
            CommandHint::new("help", "show help", ""),
            CommandHint::new("theme", "switch theme", "[dark|light]"),
            CommandHint::new("model", "switch model", "[name]"),
            CommandHint::new("mode", "toggle mode", ""),
            CommandHint::new("clear", "clear transcript", ""),
            CommandHint::new("stats", "show stats", ""),
            CommandHint::new("quit", "exit", ""),
        ]
    }

    #[test]
    fn rendering_one_filtered_completion_shows_that_command() {
        // Regression: the popup indexed the filtered index list with a hint
        // index, so a filter leaving fewer matches than the match's position
        // panicked (`len is 1 but the index is 4` for `/c` → `clear`).
        let mut editor = Editor::new();
        editor.set_hints(hints());
        type_text(&mut editor, "/c");
        assert_eq!(editor.completions(), vec![4]);

        let area = Rect::new(0, 0, 60, 12);
        let mut buf = Buffer::empty(area);
        editor.render(&mut buf, area);

        let text = buffer_text(&buf);
        assert!(text.contains("/clear"), "{text}");
        assert!(!text.contains("/theme"), "popup not filtered:\n{text}");
    }

    #[test]
    fn rendering_several_filtered_completions_shows_each_command() {
        let mut editor = Editor::new();
        editor.set_hints(hints());
        type_text(&mut editor, "/m");
        assert_eq!(editor.completions(), vec![2, 3]);

        let area = Rect::new(0, 0, 60, 12);
        let mut buf = Buffer::empty(area);
        editor.render(&mut buf, area);

        let text = buffer_text(&buf);
        assert!(text.contains("/model"), "{text}");
        assert!(text.contains("/mode"), "{text}");
        assert!(!text.contains("/clear"), "popup not filtered:\n{text}");
    }

    #[test]
    fn completion_rows_separate_the_command_from_its_description() {
        let mut editor = Editor::new();
        editor.set_hints(hints());
        type_text(&mut editor, "/h");

        let area = Rect::new(0, 0, 60, 12);
        let mut buf = Buffer::empty(area);
        editor.render(&mut buf, area);

        // The description column used to start where `/help` ended, running the
        // two together as `/helpShow keyboard shortcuts`.
        let text = buffer_text(&buf);
        assert!(text.contains("/help show help"), "{text}");
        assert!(!text.contains("/helpShow"), "{text}");
    }

    #[test]
    fn the_completion_list_is_drawn_below_the_frame() {
        let mut editor = Editor::new();
        editor.set_hints(hints());
        type_text(&mut editor, "/");

        let area = Rect::new(0, 0, 60, 8);
        let mut buf = Buffer::empty(area);
        editor.render(&mut buf, area);

        let text = buffer_text(&buf);
        let lines: Vec<&str> = text.lines().collect();
        // Frame rows: top border, input, bottom border.
        assert!(lines[0].starts_with('╭'), "{text}");
        assert!(lines[1].starts_with("│› /"), "{text}");
        assert!(lines[2].starts_with('╰'), "{text}");
        // The list starts under the frame, outside the borders.
        assert!(lines[3].starts_with("› /help"), "{text}");
        assert!(lines[4].starts_with("  /theme"), "{text}");
        assert!(!text.contains("│› /help"), "{text}");
    }

    #[test]
    fn completion_popup_never_draws_over_the_input_line() {
        let mut editor = Editor::new();
        editor.set_hints(hints());
        type_text(&mut editor, "/");

        // The area is shorter than the popup wants; the frame keeps its borders
        // and one text row, and the list takes whatever row is left.
        let area = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(area);
        editor.render(&mut buf, area);

        let text = buffer_text(&buf);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with('╭'), "{text}");
        assert!(lines[1].starts_with("│› /"), "{text}");
        assert!(lines[2].starts_with('╰'), "{text}");
        assert!(lines[3].contains("/help"), "{text}");
    }

    #[test]
    fn enter_on_the_menu_runs_the_highlighted_command() {
        // `/c` leaves one match, and Enter must run it rather than sending the
        // two characters `/c` as a prompt.
        let mut editor = Editor::new();
        editor.set_hints(hints());
        type_text(&mut editor, "/c");

        assert_eq!(
            editor.handle_key(key(KeyCode::Enter, KeyModifiers::empty())),
            KeyResult::Handled
        );
        assert_eq!(editor.take_submitted().as_deref(), Some("/clear"));
        assert!(editor.is_empty(), "the input should be consumed");
    }

    #[test]
    fn enter_on_the_menu_runs_the_highlighted_match() {
        let mut editor = Editor::new();
        editor.set_hints(hints());
        type_text(&mut editor, "/m");
        assert_eq!(editor.completions(), vec![2, 3]);

        // The first match is `/model`; Down moves to `/mode`.
        editor.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
        editor.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
        assert_eq!(editor.take_submitted().as_deref(), Some("/mode"));
    }

    #[test]
    fn enter_with_arguments_typed_sends_them_as_written() {
        let mut editor = Editor::new();
        editor.set_hints(hints());
        // A space closes the menu, so Enter goes back to sending the text.
        type_text(&mut editor, "/theme light");
        assert!(editor.completions().is_empty());

        editor.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
        assert_eq!(editor.take_submitted().as_deref(), Some("/theme light"));
    }

    #[test]
    fn tab_fills_the_command_in_without_running_it() {
        let mut editor = Editor::new();
        editor.set_hints(hints());
        type_text(&mut editor, "/t");

        editor.handle_key(key(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(editor.text(), "/theme ", "arguments stay typeable");
        assert_eq!(editor.take_submitted(), None, "Tab must not run it");
    }

    #[test]
    fn alt_enter_still_inserts_a_newline_while_the_menu_is_open() {
        let mut editor = Editor::new();
        editor.set_hints(hints());
        type_text(&mut editor, "/t");

        editor.handle_key(key(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(editor.take_submitted(), None);
        assert!(editor.completions().is_empty(), "the menu closes");
    }

    #[test]
    fn only_the_first_row_carries_the_prompt_marker() {
        // Alt+Enter makes a second logical line, which must be indented rather
        // than marked, the way the transcript echoes a multi-line prompt.
        let mut editor = Editor::new();
        type_text(&mut editor, "one");
        editor.handle_key(key(KeyCode::Enter, KeyModifiers::ALT));
        type_text(&mut editor, "two");

        let area = Rect::new(0, 0, 40, 8);
        let mut buf = Buffer::empty(area);
        editor.render(&mut buf, area);

        let text = buffer_text(&buf);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].starts_with("│› one"), "{text}");
        assert!(lines[2].starts_with("│  two"), "{text}");
        assert!(!text.contains("› two"), "the second line is marked: {text}");

        // A wrapped row is indented the same way.
        let mut long = Editor::new();
        type_text(&mut long, &"word ".repeat(30));
        let area = Rect::new(0, 0, 30, 8);
        let mut buf = Buffer::empty(area);
        long.render(&mut buf, area);

        let text = buffer_text(&buf);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].starts_with("│› word"), "{text}");
        assert!(lines[2].starts_with("│  word"), "{text}");
        assert_eq!(text.matches('›').count(), 1, "{text}");
    }

    #[test]
    fn a_scrolled_prompt_drops_the_marker() {
        // The marker belongs to the first row of the input; once that row has
        // scrolled out of the frame, no visible row may claim it.
        let mut editor = Editor::new();
        for index in 0..12 {
            type_text(&mut editor, "line");
            if index < 11 {
                editor.handle_key(key(KeyCode::Enter, KeyModifiers::ALT));
            }
        }
        assert_eq!(editor.lines.len(), 12);

        let area = Rect::new(0, 0, 40, 6);
        let mut buf = Buffer::empty(area);
        editor.render(&mut buf, area);

        let text = buffer_text(&buf);
        assert!(text.contains("line"), "{text}");
        assert!(!text.contains('›'), "{text}");
    }

    #[test]
    fn typing_and_submitting() {
        let mut editor = Editor::new();
        type_text(&mut editor, "hello");
        assert_eq!(editor.text(), "hello");

        assert_eq!(
            editor.handle_key(key(KeyCode::Enter, KeyModifiers::empty())),
            KeyResult::Handled
        );
        assert_eq!(editor.take_submitted().as_deref(), Some("hello"));
        assert!(editor.is_empty());
        assert_eq!(editor.take_submitted(), None);
    }

    #[test]
    fn empty_submit_is_ignored() {
        let mut editor = Editor::new();
        assert_eq!(
            editor.handle_key(key(KeyCode::Enter, KeyModifiers::empty())),
            KeyResult::Ignored
        );
    }

    #[test]
    fn alt_enter_inserts_a_newline() {
        let mut editor = Editor::new();
        type_text(&mut editor, "a");
        editor.handle_key(key(KeyCode::Enter, KeyModifiers::ALT));
        type_text(&mut editor, "b");
        assert_eq!(editor.text(), "a\nb");
        assert_eq!(editor.lines.len(), 2);
    }

    #[test]
    fn backspace_joins_lines() {
        let mut editor = Editor::new();
        type_text(&mut editor, "a");
        editor.handle_key(key(KeyCode::Enter, KeyModifiers::ALT));
        type_text(&mut editor, "b");
        editor.handle_key(key(KeyCode::Home, KeyModifiers::empty()));
        editor.handle_key(key(KeyCode::Backspace, KeyModifiers::empty()));
        assert_eq!(editor.text(), "ab");
    }

    #[test]
    fn history_round_trip() {
        let mut editor = Editor::new();
        editor.push_history("first");
        editor.push_history("second");

        editor.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
        assert_eq!(editor.text(), "second");
        editor.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
        assert_eq!(editor.text(), "first");
        editor.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
        assert_eq!(editor.text(), "second");
        editor.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn undo_and_redo_step_through_edits() {
        let mut editor = Editor::new();
        type_text(&mut editor, "abc");
        editor.handle_key(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(editor.text(), "ab");
        editor.handle_key(key(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(editor.text(), "abc");
    }

    #[test]
    fn kill_word_back_removes_one_word() {
        let mut editor = Editor::new();
        type_text(&mut editor, "hello world");
        editor.handle_key(key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(editor.text(), "hello ");
    }

    #[test]
    fn kill_to_line_start_removes_prefix() {
        let mut editor = Editor::new();
        type_text(&mut editor, "hello");
        editor.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn completion_appears_and_tab_accepts() {
        let mut editor = Editor::new();
        editor.set_hints(vec![
            CommandHint::new("help", "show help", ""),
            CommandHint::new("theme", "switch theme", "[dark|light]"),
        ]);

        type_text(&mut editor, "/");
        assert_eq!(editor.completions().len(), 2);

        type_text(&mut editor, "th");
        assert_eq!(editor.completions().len(), 1);

        assert_eq!(
            editor.handle_key(key(KeyCode::Tab, KeyModifiers::empty())),
            KeyResult::Handled
        );
        assert_eq!(editor.text(), "/theme ");
    }

    #[test]
    fn escape_dismisses_completions() {
        let mut editor = Editor::new();
        editor.set_hints(vec![CommandHint::new("help", "show help", "")]);
        type_text(&mut editor, "/");
        assert_eq!(editor.completions().len(), 1);
        editor.handle_key(key(KeyCode::Esc, KeyModifiers::empty()));
        assert!(editor.completions().is_empty());
    }

    #[test]
    fn paste_splits_on_newlines() {
        let mut editor = Editor::new();
        editor.insert_paste("line one\nline two");
        assert_eq!(editor.text(), "line one\nline two");
        assert_eq!(editor.lines.len(), 2);
    }

    #[test]
    fn desired_height_grows_with_wrapping_then_caps() {
        let mut editor = Editor::new();
        type_text(&mut editor, "short");
        assert_eq!(editor.desired_height(40), Some(3));

        let mut long = Editor::new();
        type_text(&mut long, &"word ".repeat(60));
        let height = long.desired_height(40).unwrap();
        assert_eq!(height, 2 + MAX_TEXT_ROWS);
    }

    #[test]
    fn completion_rows_are_included_in_height() {
        let mut editor = Editor::new();
        editor.set_hints(vec![
            CommandHint::new("help", "a", ""),
            CommandHint::new("model", "b", ""),
        ]);
        type_text(&mut editor, "/");
        assert_eq!(editor.desired_height(40), Some(2 + 2 + 1));
    }

    #[test]
    fn wraps_long_logical_lines() {
        let chars: Vec<char> = "one two three four five".chars().collect();
        let segments = wrap_chars(&chars, 10);
        assert!(segments.len() >= 3);
        for (start, end, text) in &segments {
            assert!(start <= end);
            assert!(display_width(text) <= 10);
        }
    }

    #[test]
    fn wrap_covers_every_character_exactly_once() {
        let chars: Vec<char> = "alpha beta gamma".chars().collect();
        let segments = wrap_chars(&chars, 7);
        let rebuilt: String = segments.iter().map(|(_, _, text)| text.clone()).collect();
        assert_eq!(rebuilt, "alpha beta gamma");
    }
}
