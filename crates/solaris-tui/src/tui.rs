//! The TUI driver: a root component, an overlay stack, and the event loop.
//!
//! This crate owns the event loop (unlike a CLI-owned loop), so an application
//! only has to implement [`Component`] and hand it to [`Tui::set_root`].
//!
//! Input routing is deliberately single-path: while any overlay is visible it
//! captures every key and mouse event, and only the root sees input otherwise.
//!
//! Selection cuts across that path rather than joining it: it observes every
//! pointer event on the way through and paints itself over the finished frame,
//! so any cell the app or an overlay drew can be dragged over and copied.

use std::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;
use std::time::Duration;

use crossterm::event::{
    self, Event, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Clear, Widget};

use crate::component::Component;
use crate::overlay::{self, OverlayOptions};
use crate::selection::{Selection, SelectionHandle};
use crate::terminal::PiTerminal;
use crate::util::rect_contains;

/// Queue of overlays the root component wants opened.
pub type OverlayQueue = Rc<RefCell<Vec<(Box<dyn Component>, OverlayOptions)>>>;

/// Flag the root component raises to leave the event loop.
pub type QuitFlag = Rc<Cell<bool>>;

struct OverlayEntry {
    component: Box<dyn Component>,
    options: OverlayOptions,
    rect: Rect,
}

/// Owns the component tree and drives rendering/input.
pub struct Tui {
    root: Option<Box<dyn Component>>,
    overlays: Vec<OverlayEntry>,
    queue: OverlayQueue,
    quit: QuitFlag,
    overlay_flag: Rc<Cell<bool>>,
    selection: SelectionHandle,
    frame: u64,
    poll_interval: Duration,
}

impl Default for Tui {
    fn default() -> Self {
        Self::new()
    }
}

impl Tui {
    /// A TUI with no root component yet.
    pub fn new() -> Self {
        Self {
            root: None,
            overlays: Vec::new(),
            queue: Rc::new(RefCell::new(Vec::new())),
            quit: Rc::new(Cell::new(false)),
            overlay_flag: Rc::new(Cell::new(false)),
            selection: Selection::handle(),
            frame: 0,
            poll_interval: Duration::from_millis(16),
        }
    }

    /// Set the root component, which fills the whole terminal.
    pub fn set_root(&mut self, root: Box<dyn Component>) {
        self.root = Some(root);
    }

    /// Handle used by the root component to open overlays.
    pub fn overlay_queue(&self) -> OverlayQueue {
        Rc::clone(&self.queue)
    }

    /// Handle used by the root component to request exit.
    pub fn quit_flag(&self) -> QuitFlag {
        Rc::clone(&self.quit)
    }

    /// Handle reporting whether an overlay is currently visible.
    ///
    /// The root component reads this to drop focus while a dialog is modal,
    /// even when the dialog is dismissed by a click outside of it.
    pub fn overlay_flag(&self) -> Rc<Cell<bool>> {
        Rc::clone(&self.overlay_flag)
    }

    /// Handle giving the application access to the screen selection.
    ///
    /// The driver tracks the drag and extracts the text; the application sets
    /// the highlight colours and decides what to do with a finished selection.
    pub fn selection(&self) -> SelectionHandle {
        Rc::clone(&self.selection)
    }

    /// Ask the loop to exit after the current frame.
    pub fn request_quit(&self) {
        self.quit.set(true);
    }

    /// Whether any overlay is currently visible.
    pub fn has_overlay(&self) -> bool {
        !self.overlays.is_empty()
    }

    /// Close the topmost overlay.
    pub fn close_top_overlay(&mut self) {
        self.overlays.pop();
    }

    /// Frames rendered so far.
    pub fn frame_count(&self) -> u64 {
        self.frame
    }

    /// Run until the quit flag is raised.
    pub fn run(&mut self, terminal: &mut PiTerminal) -> io::Result<()> {
        let mut force_first_frame = true;

        while !self.quit.get() {
            let mut dirty = force_first_frame;
            force_first_frame = false;

            // Overlays requested by the root are promoted during render; a
            // non-empty queue therefore means this frame must be drawn.
            if !self.queue.borrow().is_empty() {
                dirty = true;
            }

            if event::poll(self.poll_interval)? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        self.handle_key(key);
                        dirty = true;
                    }
                    Event::Mouse(mouse) => {
                        self.handle_mouse(mouse);
                        dirty = true;
                    }
                    Event::Paste(text) => {
                        self.handle_paste(&text);
                        dirty = true;
                    }
                    Event::Resize(_, _) => dirty = true,
                    _ => {}
                }
            }

            if self.tick() {
                dirty = true;
            }

            if dirty {
                terminal.draw(|frame| {
                    let area = frame.area();
                    self.render(frame.buffer_mut(), area);
                })?;
            }
        }

        Ok(())
    }

    /// Render the root component and then every overlay, bottom to top.
    pub fn render(&mut self, buf: &mut Buffer, area: Rect) {
        self.frame = self.frame.wrapping_add(1);
        self.selection.borrow_mut().set_area(area);

        // Promote overlays queued by the root since the previous frame, then
        // refresh the visibility flag the root reads to drop focus.
        for (component, options) in self.queue.borrow_mut().drain(..) {
            self.overlays.push(OverlayEntry {
                component,
                options,
                rect: Rect::default(),
            });
        }
        self.overlay_flag.set(!self.overlays.is_empty());

        match self.root.as_mut() {
            Some(root) => root.render(buf, area),
            None => buf.set_style(area, Style::default()),
        }

        for entry in self.overlays.iter_mut() {
            let rect = overlay::resolve(area, &entry.options);
            entry.rect = rect;
            if rect.width > 0 && rect.height > 0 {
                // Opaque overlay: hide whatever is underneath.
                Clear.render(rect, buf);
                entry.component.render(buf, rect);
            }
        }

        // The selection paints over the finished frame, so it covers the root
        // and every overlay, and it records what each row now says.
        self.selection.borrow_mut().highlight(buf);
    }

    /// Route a key event. Overlays are modal and swallow everything.
    pub fn handle_key(&mut self, key: KeyEvent) {
        if let Some(top) = self.overlays.last_mut() {
            let result = top.component.handle_key(key);
            if result.is_close() {
                self.overlays.pop();
            }
            return;
        }

        if let Some(root) = self.root.as_mut() {
            root.handle_key(key);
        }
    }

    /// Route a mouse event. A left click outside the top overlay closes it.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        // The selection watches first: it has to see the whole drag even when a
        // component below consumes the press, and even when the press lands on
        // an overlay.
        self.selection.borrow_mut().handle_mouse(mouse);

        if let Some(top) = self.overlays.last_mut() {
            let inside = rect_contains(top.rect, mouse.column, mouse.row);
            if !inside {
                if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                    self.overlays.pop();
                    self.overlay_flag.set(false);
                }
                return;
            }
            top.component.handle_mouse(mouse);
            return;
        }

        if let Some(root) = self.root.as_mut() {
            root.handle_mouse(mouse);
        }
    }

    /// Route a paste payload to the overlay or the root.
    pub fn handle_paste(&mut self, text: &str) {
        if let Some(top) = self.overlays.last_mut() {
            let result = top.component.handle_paste(text);
            if result.is_close() {
                self.overlays.pop();
            }
            return;
        }

        if let Some(root) = self.root.as_mut() {
            root.handle_paste(text);
        }
    }

    /// Tick the tree; returns whether a redraw is needed.
    pub fn tick(&mut self) -> bool {
        let mut dirty = false;
        if let Some(root) = self.root.as_mut()
            && root.tick()
        {
            dirty = true;
        }
        for entry in self.overlays.iter_mut() {
            if entry.component.tick() {
                dirty = true;
            }
        }
        dirty
    }
}

/// Marker component used by tests and applications that need an inert root.
pub struct EmptyComponent;

impl Component for EmptyComponent {
    fn render(&mut self, _buf: &mut Buffer, _area: Rect) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_hit_testing_handles_edges() {
        let rect = Rect::new(2, 3, 4, 2);
        assert!(rect_contains(rect, 2, 3));
        assert!(rect_contains(rect, 5, 4));
        assert!(!rect_contains(rect, 6, 4));
        assert!(!rect_contains(rect, 2, 5));
        assert!(!rect_contains(Rect::new(0, 0, 0, 0), 0, 0));
    }
}
