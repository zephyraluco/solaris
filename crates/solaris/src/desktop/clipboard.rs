//! The system clipboard, in both directions.
//!
//! The framework never touches the clipboard; the application holds one of
//! these, so tests can swap either direction for a recorder instead of driving
//! the machine's real one.

use std::cell::RefCell;
use std::rc::Rc;

/// Writes text to the system clipboard, reporting whether it landed.
pub type ClipboardWriter = Rc<dyn Fn(&str) -> bool>;

/// Reads text from the system clipboard, or `None` when it holds none.
pub type ClipboardReader = Rc<dyn Fn() -> Option<String>>;

/// The clipboard as the application sees it.
pub struct Clipboard {
    /// System operations run on the application I/O worker; injected callbacks are immediate.
    pub background: bool,
    /// Writes text, reporting whether it landed.
    pub write: ClipboardWriter,
    /// Reads text, or `None` when there is nothing to read.
    pub read: ClipboardReader,
}

impl Clipboard {
    /// The worker retains its native clipboard connection between operations.
    pub fn system() -> Self {
        Self {
            background: true,
            write: Rc::new(system_write),
            read: Rc::new(system_read),
        }
    }
}

thread_local! {
    static SYSTEM: RefCell<Option<arboard::Clipboard>> = const { RefCell::new(None) };
}
pub(crate) fn system_read() -> Option<String> {
    SYSTEM.with(|handle| {
        let mut handle = handle.borrow_mut();
        if handle.is_none() {
            *handle = arboard::Clipboard::new().ok();
        }
        handle
            .as_mut()?
            .get_text()
            .ok()
            .filter(|text| !text.is_empty())
    })
}
pub(crate) fn system_write(text: &str) -> bool {
    SYSTEM.with(|handle| {
        let mut handle = handle.borrow_mut();
        if handle.is_none() {
            *handle = arboard::Clipboard::new().ok();
        }
        handle
            .as_mut()
            .is_some_and(|clipboard| clipboard.set_text(text).is_ok())
    })
}
