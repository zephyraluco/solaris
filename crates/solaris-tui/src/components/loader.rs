//! Animated spinner with a message.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::component::Component;
use crate::util::truncate_to_width;

const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Frames a glyph is held before advancing (~10 fps at a 16 ms tick).
const FRAMES_PER_STEP: u32 = 6;

/// Braille spinner plus a message.
pub struct Loader {
    index: usize,
    ticks: u32,
    message: String,
    spinner_style: Style,
    message_style: Style,
    running: bool,
}

impl Loader {
    /// A running loader showing `message`.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            index: 0,
            ticks: 0,
            message: message.into(),
            spinner_style: Style::default(),
            message_style: Style::default(),
            running: true,
        }
    }

    /// Style the spinner glyph.
    pub fn spinner_style(mut self, style: Style) -> Self {
        self.spinner_style = style;
        self
    }

    /// Style the message text.
    pub fn message_style(mut self, style: Style) -> Self {
        self.message_style = style;
        self
    }

    /// Replace the message.
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = message.into();
    }

    /// Stop animating; the render shows a static glyph.
    pub fn stop(&mut self) {
        self.running = false;
    }

    /// Resume animating.
    pub fn start(&mut self) {
        self.running = true;
    }

    /// Whether the spinner is animating.
    pub fn is_running(&self) -> bool {
        self.running
    }
}

impl Component for Loader {
    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let glyph = if self.running {
            FRAMES[self.index]
        } else {
            "·"
        };
        buf.set_string(area.x, area.y, glyph, self.spinner_style);

        if area.width <= 2 {
            return;
        }
        let message = truncate_to_width(&self.message, area.width as usize - 2, "...");
        buf.set_string(area.x + 2, area.y, message, self.message_style);
    }

    fn tick(&mut self) -> bool {
        if !self.running {
            return false;
        }
        self.ticks += 1;
        if self.ticks >= FRAMES_PER_STEP {
            self.ticks = 0;
            self.index = (self.index + 1) % FRAMES.len();
            return true;
        }
        false
    }

    fn desired_height(&mut self, _width: u16) -> Option<u16> {
        Some(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advances_one_step_per_frames_per_step_ticks() {
        let mut loader = Loader::new("working");
        let mut redraws = 0;
        for _ in 0..FRAMES_PER_STEP {
            if loader.tick() {
                redraws += 1;
            }
        }
        assert_eq!(redraws, 1);
        assert_eq!(loader.index, 1);
    }

    #[test]
    fn stopped_loader_never_requests_redraw() {
        let mut loader = Loader::new("done");
        loader.stop();
        for _ in 0..100 {
            assert!(!loader.tick());
        }
    }
}
