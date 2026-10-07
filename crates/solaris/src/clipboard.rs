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
    /// Writes text, reporting whether it landed.
    pub write: ClipboardWriter,
    /// Reads text, or `None` when there is nothing to read.
    pub read: ClipboardReader,
}

impl Clipboard {
    /// The platform clipboard, through `arboard`.
    pub fn system() -> Self {
        let handle: Rc<RefCell<Option<arboard::Clipboard>>> = Rc::new(RefCell::new(None));

        let writer = Rc::clone(&handle);
        let write = Rc::new(move |text: &str| {
            with(&writer, |clipboard| clipboard.set_text(text.to_string()))
                .is_some_and(|written| written.is_ok())
        });

        let reader = Rc::clone(&handle);
        let read = Rc::new(move || {
            with(&reader, |clipboard| clipboard.get_text())
                .and_then(Result::ok)
                .filter(|text| !text.is_empty())
        });

        Self { write, read }
    }
}

/// Run `use_it` against the platform clipboard, opening it on first use.
///
/// The handle is kept open because some platforms tie a clipboard connection to
/// the process that opened it.
fn with<T>(
    handle: &Rc<RefCell<Option<arboard::Clipboard>>>,
    use_it: impl FnOnce(&mut arboard::Clipboard) -> T,
) -> Option<T> {
    let mut handle = handle.borrow_mut();
    if handle.is_none() {
        *handle = arboard::Clipboard::new().ok();
    }
    handle.as_mut().map(use_it)
}
