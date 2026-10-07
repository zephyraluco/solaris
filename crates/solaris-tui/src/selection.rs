//! Screen-wide mouse text selection.
//!
//! The framework owns selection the way it owns the overlay stack: pointer
//! events move an anchor and a focus, and a post-render pass inverts every cell
//! between them and keeps their text. The design follows claurst's transcript
//! selection with the area restriction lifted — every cell of the frame is fair
//! game, so dragging over any component selects what it drew.
//!
//! A selection is not a transient gesture. Releasing the button leaves the
//! range on screen until something replaces it, and the framework never copies
//! anything on its own: the application calls [`Selection::selected_text`] when
//! the user asks for the text.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::util::rect_contains;

/// A screen cell: column, row.
pub type Point = (u16, u16);

/// Shared selection state, handed to the application by [`Tui::selection`].
///
/// [`Tui::selection`]: crate::Tui::selection
pub type SelectionHandle = Rc<RefCell<Selection>>;

/// Clicks in the same cell within this window escalate: twice selects a word,
/// three times selects the paragraph under the pointer.
const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(500);

/// Drag-selection state over the whole rendered frame.
pub struct Selection {
    anchor: Option<Point>,
    focus: Option<Point>,
    /// The frame the pointer coordinates refer to, set by the render pass.
    area: Rect,
    /// Style painted over selected cells.
    style: Style,
    /// Rendered text of every row in `area`, snapshotted after each frame.
    rows: Vec<String>,
    click_cell: Option<Point>,
    click_at: Option<Instant>,
    click_count: u8,
}

impl Default for Selection {
    fn default() -> Self {
        Self::new()
    }
}

impl Selection {
    /// An empty selection with a visible default highlight.
    pub fn new() -> Self {
        Self {
            anchor: None,
            focus: None,
            area: Rect::default(),
            style: Style::default().add_modifier(Modifier::REVERSED),
            rows: Vec::new(),
            click_cell: None,
            click_at: None,
            click_count: 0,
        }
    }

    /// A selection ready to be shared between the driver and the application.
    pub fn handle() -> SelectionHandle {
        Rc::new(RefCell::new(Self::new()))
    }

    /// Colours used to highlight selected cells.
    pub fn set_style(&mut self, fg: Color, bg: Color) {
        self.style = Style::default().fg(fg).bg(bg);
    }

    /// The style painted over selected cells.
    pub fn style(&self) -> Style {
        self.style
    }

    /// Tell the selection which frame the pointer coordinates refer to.
    ///
    /// A resize invalidates every stored coordinate, so the selection resets
    /// rather than highlighting an unrelated region of the new layout.
    pub fn set_area(&mut self, area: Rect) {
        if area != self.area {
            self.area = area;
            self.clear();
        }
    }

    /// Drop the highlight.
    ///
    /// The click sequence survives, so the second press of a double click still
    /// resolves to a word even though the first one cleared the range.
    pub fn clear(&mut self) {
        self.anchor = None;
        self.focus = None;
    }

    /// Whether a range is currently highlighted.
    pub fn is_active(&self) -> bool {
        self.bounds().is_some_and(|(start, end)| start != end)
    }

    /// The text of the current selection, read from the last rendered frame.
    pub fn selected_text(&self) -> String {
        let Some((start, end)) = self.bounds() else {
            return String::new();
        };
        if start == end {
            return String::new();
        }

        let mut out = String::new();
        for row in start.1..=end.1 {
            if row > start.1 {
                out.push('\n');
            }
            let from = if row == start.1 { start.0 } else { self.area.x };
            let to = if row == end.1 { end.0 } else { self.right() };
            out.push_str(self.row_slice(row, from, to).trim_end());
        }

        out.trim_end().to_string()
    }

    /// Observe a pointer event, returning whether the highlight changed.
    ///
    /// The selection sees every event, whether or not a component underneath
    /// reacts to it too: a press both moves the editor's caret and starts a
    /// range, and only a drag turns the range into a selection.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> bool {
        if self.area.width == 0 || self.area.height == 0 {
            return false;
        }
        let point = (event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if !rect_contains(self.area, point.0, point.1) {
                    let had = self.anchor.is_some();
                    self.clear();
                    return had;
                }
                match self.escalate(point) {
                    2 => self.select_word(point),
                    3 => self.select_paragraph(point),
                    _ => {
                        self.anchor = Some(point);
                        self.focus = Some(point);
                    }
                }
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.anchor.is_none() {
                    return false;
                }
                let point = self.clamp(point);
                if self.focus == Some(point) {
                    return false;
                }
                self.focus = Some(point);
                // A drag is no longer part of a click sequence.
                self.click_count = 0;
                true
            }
            // Releasing keeps the range: the highlight stays on screen until a
            // new press replaces it, the frame resizes, or the application
            // clears it. Nothing is copied here — that is the user's move.
            MouseEventKind::Up(MouseButton::Left) => false,
            _ => false,
        }
    }

    /// Post-render pass: remember what each row says, then highlight the
    /// selected cells. It runs last, so overlay cells are selectable too.
    pub fn highlight(&mut self, buf: &mut Buffer) {
        self.snapshot(buf);

        let Some((start, end)) = self.bounds() else {
            return;
        };
        if start == end {
            return;
        }

        for row in start.1..=end.1 {
            let from = if row == start.1 { start.0 } else { self.area.x };
            let to = if row == end.1 { end.0 } else { self.right() };
            // Snap the ends onto the glyphs that cover them, so a range that
            // starts or ends inside a wide glyph takes all of it.
            let from = self.glyph_span(row, from).map_or(from, |(col, _)| col);
            let to = self
                .glyph_span(row, to)
                .map_or(to, |(col, width)| col + width - 1)
                .min(self.right());

            for col in from..=to {
                if let Some(cell) = buf.cell_mut((col, row)) {
                    cell.set_style(self.style);
                }
            }
        }
    }

    /// Normalised, clamped selection bounds, or `None` when nothing is
    /// selected. Ordering is row-major, so `start` is never after `end`.
    fn bounds(&self) -> Option<(Point, Point)> {
        if self.area.width == 0 || self.area.height == 0 {
            return None;
        }
        let anchor = self.clamp(self.anchor?);
        let focus = self.clamp(self.focus?);
        Some(if (anchor.1, anchor.0) <= (focus.1, focus.0) {
            (anchor, focus)
        } else {
            (focus, anchor)
        })
    }

    fn clamp(&self, point: Point) -> Point {
        let bottom = self.area.bottom().saturating_sub(1);
        let column = point.0.clamp(self.area.x, self.right());
        let row = point.1.clamp(self.area.y, bottom);
        (column, row)
    }

    /// The last column of the frame.
    fn right(&self) -> u16 {
        self.area.right().saturating_sub(1)
    }

    /// Count this press in the current click sequence: 1 for a plain click,
    /// 2 for a word, 3 for a paragraph. A fourth click starts over.
    fn escalate(&mut self, point: Point) -> u8 {
        let now = Instant::now();
        let repeat = self.click_cell == Some(point)
            && self
                .click_at
                .is_some_and(|last| now.duration_since(last) <= MULTI_CLICK_WINDOW);
        self.click_count = if repeat { self.click_count % 3 + 1 } else { 1 };
        self.click_cell = Some(point);
        self.click_at = Some(now);
        self.click_count
    }

    fn select_word(&mut self, point: Point) {
        match self.word_bounds(point) {
            Some((start, end)) => {
                self.anchor = Some((start, point.1));
                self.focus = Some((end, point.1));
            }
            // Double-clicking blanks selects nothing, like a single click.
            None => {
                self.anchor = Some(point);
                self.focus = Some(point);
            }
        }
    }

    /// The word under `point`, as an inclusive column range.
    ///
    /// Boundaries follow Unicode word segmentation — the same UAX #29 rules
    /// `Intl.Segmenter` gives the library this borrows from — so punctuation
    /// ends a word rather than only whitespace. Adjacent runs of the same kind
    /// are taken together, which is what makes a run of ideographs (each its
    /// own segment under UAX #29) one word.
    fn word_bounds(&self, point: Point) -> Option<(u16, u16)> {
        const BLANK: u8 = 0;
        const PUNCTUATION: u8 = 1;
        const WORD: u8 = 2;

        let text = self.row(point.1)?;
        let mut col = self.area.x;
        let mut segments: Vec<(u16, u16, u8)> = Vec::new();
        for segment in text.split_word_bounds() {
            let width = UnicodeWidthStr::width(segment) as u16;
            let kind = if segment.chars().all(char::is_whitespace) {
                BLANK
            } else if segment.chars().any(char::is_alphanumeric) {
                WORD
            } else {
                PUNCTUATION
            };
            segments.push((col, width.max(1), kind));
            col = col.saturating_add(width);
        }

        let index = segments.iter().position(|(start, width, _)| {
            point.0 >= *start && point.0 < start.saturating_add(*width)
        })?;
        let kind = segments[index].2;
        if kind == BLANK {
            return None;
        }

        let mut first = index;
        while first > 0 && segments[first - 1].2 == kind {
            first -= 1;
        }
        let mut last = index;
        while last + 1 < segments.len() && segments[last + 1].2 == kind {
            last += 1;
        }

        let (last_start, last_width, _) = segments[last];
        let start = segments[first].0;
        let end = last_start.saturating_add(last_width).saturating_sub(1);
        Some((start, end))
    }

    /// Select the run of non-blank rows containing `point` — a paragraph.
    fn select_paragraph(&mut self, point: Point) {
        let mut first = point.1;
        while !self.is_blank(first) && first > self.area.y && !self.is_blank(first - 1) {
            first -= 1;
        }
        let mut last = point.1;
        while !self.is_blank(last) && last + 1 < self.area.bottom() && !self.is_blank(last + 1) {
            last += 1;
        }

        self.anchor = Some((self.area.x, first));
        self.focus = Some((self.right(), last));
    }

    /// Whether `row` shows nothing but whitespace.
    fn is_blank(&self, row: u16) -> bool {
        self.row(row).is_none_or(|text| text.trim().is_empty())
    }

    /// The rendered text of `row`, or `None` outside the frame.
    fn row(&self, row: u16) -> Option<&str> {
        let index = row.checked_sub(self.area.y)? as usize;
        self.rows.get(index).map(String::as_str)
    }

    /// The glyph covering `col` in `row`, as `(start column, cell width)`.
    fn glyph_span(&self, row: u16, col: u16) -> Option<(u16, u16)> {
        let text = self.row(row)?;
        let mut start = self.area.x;
        for ch in text.chars() {
            let cells = ch.width().unwrap_or(0) as u16;
            let width = cells.max(1);
            if col >= start && col < start.saturating_add(width) {
                return Some((start, width));
            }
            start = start.saturating_add(cells);
        }
        None
    }

    /// The text of `row` between two inclusive columns.
    fn row_slice(&self, row: u16, from: u16, to: u16) -> String {
        let Some(text) = self.row(row) else {
            return String::new();
        };
        let from = self.glyph_span(row, from).map_or(from, |(col, _)| col);
        let to = self
            .glyph_span(row, to)
            .map_or(to, |(col, width)| col + width - 1);

        let mut out = String::new();
        let mut col = self.area.x;
        for ch in text.chars() {
            let width = ch.width().unwrap_or(0) as u16;
            if col >= from && col.saturating_add(width) <= to.saturating_add(1) {
                out.push(ch);
            }
            col = col.saturating_add(width);
        }
        out
    }

    /// Remember what every row currently displays, so word and paragraph
    /// boundaries can be found without reading the buffer again.
    fn snapshot(&mut self, buf: &Buffer) {
        self.rows.clear();
        if self.area.width == 0 || self.area.height == 0 {
            return;
        }

        for row in self.area.y..self.area.bottom() {
            let mut line = String::with_capacity(self.area.width as usize);
            // A wide grapheme hides the cells it spans, and ratatui leaves them
            // reset — which reads back as a space. Skip them, so a column in
            // `rows` still lines up with the column it came from.
            let mut hidden_until = self.area.x;
            for col in self.area.x..self.area.right() {
                if col < hidden_until {
                    continue;
                }
                let symbol = buf.cell((col, row)).map_or(" ", |cell| cell.symbol());
                line.push_str(symbol);
                hidden_until = col.saturating_add(UnicodeWidthStr::width(symbol) as u16);
            }
            self.rows.push(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 20,
        height: 4,
    };

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    fn down(column: u16, row: u16) -> MouseEvent {
        mouse(MouseEventKind::Down(MouseButton::Left), column, row)
    }

    fn up(column: u16, row: u16) -> MouseEvent {
        mouse(MouseEventKind::Up(MouseButton::Left), column, row)
    }

    fn drag_to(column: u16, row: u16) -> MouseEvent {
        mouse(MouseEventKind::Drag(MouseButton::Left), column, row)
    }

    /// A selection over a freshly painted buffer whose rows are `rows`.
    fn selection_with(rows: &[&str]) -> (Selection, Buffer) {
        let mut selection = Selection::new();
        selection.set_area(AREA);
        selection.set_style(Color::Black, Color::White);

        let mut buf = Buffer::empty(AREA);
        for (row, text) in rows.iter().enumerate() {
            buf.set_string(0, row as u16, text, Style::default());
        }
        selection.highlight(&mut buf);
        (selection, buf)
    }

    /// Press, drag, release — the three events of a mouse selection.
    fn swipe(selection: &mut Selection, from: Point, to: Point) {
        selection.handle_mouse(down(from.0, from.1));
        selection.handle_mouse(drag_to(to.0, to.1));
        selection.handle_mouse(up(to.0, to.1));
    }

    #[test]
    fn a_drag_selects_across_rows() {
        let (mut selection, _) = selection_with(&["hello world", "second line", "third"]);
        selection.handle_mouse(down(0, 0));
        selection.handle_mouse(drag_to(4, 1));

        assert_eq!(selection.selected_text(), "hello world\nsecon");
    }

    #[test]
    fn the_selection_outlives_the_release() {
        let (mut selection, mut buf) = selection_with(&["hello world"]);
        swipe(&mut selection, (0, 0), (4, 0));
        selection.highlight(&mut buf);

        assert!(selection.is_active(), "releasing must not cancel it");
        assert_eq!(selection.selected_text(), "hello");
        assert_eq!(buf[(1, 0)].bg, Color::White, "still highlighted");
    }

    #[test]
    fn a_new_press_replaces_the_selection() {
        let (mut selection, _) = selection_with(&["hello world"]);
        swipe(&mut selection, (0, 0), (4, 0));
        assert!(selection.is_active());

        selection.handle_mouse(down(8, 0));
        assert_eq!(selection.selected_text(), "");
        selection.handle_mouse(up(8, 0));
        assert_eq!(selection.selected_text(), "");
    }

    #[test]
    fn trailing_spaces_are_trimmed_and_blank_rows_collapse() {
        let (mut selection, _) = selection_with(&["ab", "", "cd"]);
        swipe(&mut selection, (0, 0), (19, 2));

        assert_eq!(selection.selected_text(), "ab\n\ncd");
    }

    #[test]
    fn a_press_alone_selects_nothing() {
        let (mut selection, _) = selection_with(&["hello world"]);
        selection.handle_mouse(down(3, 0));
        assert!(!selection.is_active(), "a press is not yet a selection");

        selection.handle_mouse(up(3, 0));
        assert!(!selection.is_active());
        assert_eq!(selection.selected_text(), "");
    }

    #[test]
    fn a_drag_outside_the_frame_is_clamped_to_it() {
        let (mut selection, _) = selection_with(&["hello world", "second line"]);
        swipe(&mut selection, (0, 0), (500, 500));

        assert_eq!(selection.selected_text(), "hello world\nsecond line");
    }

    #[test]
    fn a_drag_starting_outside_the_frame_does_nothing() {
        let (mut selection, _) = selection_with(&["hello"]);
        swipe(&mut selection, (50, 50), (3, 0));

        assert!(!selection.is_active());
        assert_eq!(selection.selected_text(), "");
    }

    #[test]
    fn a_double_click_selects_the_word_under_the_pointer() {
        let (mut selection, _) = selection_with(&["hello world"]);
        for _ in 0..2 {
            selection.handle_mouse(down(7, 0));
            selection.handle_mouse(up(7, 0));
        }

        assert!(selection.is_active());
        assert_eq!(selection.selected_text(), "world");
    }

    #[test]
    fn a_triple_click_selects_the_paragraph() {
        let (mut selection, _) = selection_with(&["one two", "three four", "", "five"]);
        for _ in 0..3 {
            selection.handle_mouse(down(1, 1));
            selection.handle_mouse(up(1, 1));
        }

        assert_eq!(selection.selected_text(), "one two\nthree four");
    }

    #[test]
    fn a_click_sequence_wraps_back_to_a_plain_click() {
        let (mut selection, _) = selection_with(&["hello world"]);
        for _ in 0..3 {
            selection.handle_mouse(down(1, 0));
            selection.handle_mouse(up(1, 0));
        }
        assert!(selection.is_active());

        // The fourth press starts a fresh sequence.
        selection.handle_mouse(down(1, 0));
        assert!(!selection.is_active());
    }

    #[test]
    fn a_click_in_another_cell_starts_a_new_sequence() {
        let (mut selection, _) = selection_with(&["hello world"]);
        selection.handle_mouse(down(0, 0));
        selection.handle_mouse(up(0, 0));
        selection.handle_mouse(down(6, 0));

        assert!(!selection.is_active(), "a new cell is a plain click");
    }

    #[test]
    fn a_wheel_event_is_left_to_the_components() {
        let (mut selection, _) = selection_with(&["hello"]);
        assert!(!selection.handle_mouse(mouse(MouseEventKind::ScrollUp, 2, 0)));
    }

    #[test]
    fn selected_cells_are_repainted_and_the_rest_are_left_alone() {
        let (mut selection, mut buf) = selection_with(&["hello world", "second"]);
        selection.handle_mouse(down(2, 0));
        selection.handle_mouse(drag_to(1, 1));
        selection.highlight(&mut buf);

        assert_eq!(buf[(2, 0)].fg, Color::Black, "the anchor cell");
        assert_eq!(buf[(2, 0)].bg, Color::White);
        assert_eq!(buf[(19, 0)].bg, Color::White, "the rest of the first row");
        assert_eq!(buf[(1, 1)].bg, Color::White, "the focus cell");
        assert_ne!(buf[(1, 0)].bg, Color::White, "before the anchor");
        assert_ne!(buf[(4, 1)].bg, Color::White, "after the focus");
    }

    #[test]
    fn a_resize_drops_the_selection() {
        let (mut selection, _) = selection_with(&["hello world"]);
        swipe(&mut selection, (0, 0), (4, 0));
        assert!(selection.is_active());

        selection.set_area(Rect::new(0, 0, 40, 4));
        assert!(!selection.is_active());
    }

    #[test]
    fn wide_glyphs_are_extracted_once() {
        let (mut selection, _) = selection_with(&["中文字"]);
        swipe(&mut selection, (0, 0), (5, 0));

        assert_eq!(selection.selected_text(), "中文字");
    }

    #[test]
    fn a_double_click_stops_at_punctuation() {
        // Word boundaries follow Unicode segmentation, so punctuation is its
        // own unit rather than part of the word.
        let (mut selection, _) = selection_with(&["hello,world"]);
        for _ in 0..2 {
            selection.handle_mouse(down(1, 0));
            selection.handle_mouse(up(1, 0));
        }

        assert_eq!(selection.selected_text(), "hello");
    }

    #[test]
    fn a_double_click_inside_a_wide_word_takes_the_whole_word() {
        let (mut selection, _) = selection_with(&["中文 ab"]);
        for _ in 0..2 {
            // Column 3 is the trailing cell of 文.
            selection.handle_mouse(down(3, 0));
            selection.handle_mouse(up(3, 0));
        }

        assert_eq!(selection.selected_text(), "中文");
    }

    #[test]
    fn a_selection_of_blanks_has_no_text() {
        let (mut selection, _) = selection_with(&["hello", "", ""]);
        swipe(&mut selection, (0, 1), (5, 2));

        assert!(selection.is_active(), "the range is still there");
        assert_eq!(selection.selected_text(), "");
    }

    #[test]
    fn the_highlight_survives_until_it_is_cleared() {
        let (mut selection, _) = selection_with(&["hello world"]);
        swipe(&mut selection, (0, 0), (4, 0));

        assert!(selection.is_active());
        selection.clear();
        assert!(!selection.is_active());
    }
}
