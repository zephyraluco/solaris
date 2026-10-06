//! Blank vertical space.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::component::Component;

/// Reserves `height` empty rows.
pub struct Spacer {
    height: u16,
}

impl Spacer {
    /// A spacer of `height` rows.
    pub fn new(height: u16) -> Self {
        Self { height }
    }
}

impl Default for Spacer {
    fn default() -> Self {
        Self::new(1)
    }
}

impl Component for Spacer {
    fn render(&mut self, _buf: &mut Buffer, _area: Rect) {}

    fn desired_height(&mut self, _width: u16) -> Option<u16> {
        Some(self.height)
    }
}
