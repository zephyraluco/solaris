//! Component model.

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

/// What happened after a component handled a key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyResult {
    /// Consumed; nothing else should see the key.
    Handled,
    /// Not interested. The caller may fall through to a lower layer.
    Ignored,
    /// The component closed with an affirmative result.
    Confirmed,
    /// The component closed without action.
    Cancelled,
}

impl KeyResult {
    pub fn is_handled(self) -> bool {
        !matches!(self, KeyResult::Ignored)
    }

    /// `true` when the component asked to close.
    pub fn is_close(self) -> bool {
        matches!(self, KeyResult::Confirmed | KeyResult::Cancelled)
    }
}

/// What happened after a component handled a mouse event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseResult {
    Handled,
    Ignored,
}

impl MouseResult {
    pub fn is_handled(self) -> bool {
        matches!(self, MouseResult::Handled)
    }
}

/// A retained UI element that renders into a rectangular region.
pub trait Component {
    /// Draw into `buf` within `area`.
    fn render(&mut self, buf: &mut Buffer, area: Rect);

    /// Handle a key event when this component owns focus.
    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        let _ = key;
        KeyResult::Ignored
    }

    /// Handle a mouse event targeted at this component.
    fn handle_mouse(&mut self, event: MouseEvent) -> MouseResult {
        let _ = event;
        MouseResult::Ignored
    }

    /// Handle a bracketed-paste payload.
    ///
    /// On Windows bracketed paste is disabled, so pasted text arrives as key
    /// events instead and this is never called.
    fn handle_paste(&mut self, text: &str) -> KeyResult {
        let _ = text;
        KeyResult::Ignored
    }

    /// Advance per-frame state (animations, background drains).
    ///
    /// Returns `true` when the component changed and a redraw is needed.
    fn tick(&mut self) -> bool {
        false
    }

    /// Natural height for the given width, when the component can report it.
    ///
    /// [`ScrollView`](crate::components::ScrollView) uses this to size its
    /// offscreen canvas; components that return `None` are assumed to fit.
    fn desired_height(&mut self, width: u16) -> Option<u16> {
        let _ = width;
        None
    }

    /// Monotonic content revision, used by render caches.
    ///
    /// `0` means "unknown", which forces caches to rebuild every frame.
    fn version(&self) -> u64 {
        0
    }

    /// Drop cached render state.
    fn invalidate(&mut self) {}
}
