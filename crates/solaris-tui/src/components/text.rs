//! Plain text block with optional word wrapping.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::component::Component;
use crate::util::{truncate_to_width, wrap_text};

/// A block of text drawn line by line.
pub struct Text {
    text: String,
    style: Style,
    wrap: bool,
    version: u64,
}

impl Text {
    /// New wrapping text block.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
            wrap: true,
            version: 0,
        }
    }

    /// Apply a style to every line.
    pub fn styled(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// Disable wrapping; long lines are truncated instead.
    pub fn no_wrap(mut self) -> Self {
        self.wrap = false;
        self
    }

    /// Replace the content.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.version = self.version.wrapping_add(1);
    }

    /// Current content.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Content split into display lines at `width`.
    pub fn lines(&self, width: u16) -> Vec<String> {
        self.wrapped(width)
    }

    fn wrapped(&self, width: u16) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        if self.wrap {
            wrap_text(&self.text, width as usize)
        } else {
            self.text.lines().map(str::to_string).collect()
        }
    }
}

impl Component for Text {
    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        for (index, line) in self
            .wrapped(area.width)
            .iter()
            .take(area.height as usize)
            .enumerate()
        {
            let line = truncate_to_width(line, area.width as usize, "");
            buf.set_string(area.x, area.y + index as u16, line, self.style);
        }
    }

    fn desired_height(&mut self, width: u16) -> Option<u16> {
        Some(self.wrapped(width).len() as u16)
    }

    fn version(&self) -> u64 {
        self.version
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_wrapped_height() {
        let mut text = Text::new("the quick brown fox");
        // "the quick" and "brown fox" both fit in 9 cells.
        assert_eq!(text.desired_height(9), Some(2));
    }

    #[test]
    fn renders_first_lines_only() {
        let mut text = Text::new("a\nb\nc");
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        text.render(&mut buf, area);
        assert_eq!(buf[(0, 0)].symbol(), "a");
        assert_eq!(buf[(0, 1)].symbol(), "b");
    }

    #[test]
    fn version_changes_with_content() {
        let mut text = Text::new("a");
        let before = text.version();
        text.set_text("b");
        assert_ne!(before, text.version());
    }
}
