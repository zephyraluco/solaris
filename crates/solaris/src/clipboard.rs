//! Writing selected text to the system clipboard.
//!
//! The framework extracts the text and the application decides what to do with
//! it; this module is the one place that knows about the platform. The writer
//! is a value rather than a call so tests can record copies instead of
//! touching the real clipboard.

use std::cell::RefCell;
use std::rc::Rc;

/// Writes text to the system clipboard, reporting whether it landed.
pub type ClipboardWriter = Rc<dyn Fn(&str) -> bool>;

/// A [`ClipboardWriter`] backed by the platform clipboard.
///
/// The handle is opened once and kept, because some platforms tie a clipboard
/// connection to the player it was opened for.
pub fn system_writer() -> ClipboardWriter {
    let clipboard: Rc<RefCell<Option<arboard::Clipboard>>> = Rc::new(RefCell::new(None));

    Rc::new(move |text: &str| {
        let mut clipboard = clipboard.borrow_mut();
        if clipboard.is_none() {
            *clipboard = arboard::Clipboard::new().ok();
        }
        match clipboard.as_mut() {
            Some(clipboard) => clipboard.set_text(text.to_string()).is_ok(),
            None => false,
        }
    })
}
