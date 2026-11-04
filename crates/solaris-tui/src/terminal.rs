//! Terminal lifecycle: raw mode, alternate screen, mouse capture, and a panic
//! hook that always restores the terminal.

use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::cursor::{Hide, Show};
#[cfg(not(target_os = "windows"))]
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::{Frame, Terminal};

/// Terminal type used by [`crate::Tui::run`].
pub type PiTerminal = Terminal<CrosstermBackend<Stdout>>;

static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Enter raw mode + alternate screen + mouse capture and build a terminal.
pub fn setup_terminal() -> io::Result<PiTerminal> {
    enable_raw_mode()?;

    TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
    let result = (|| {
        let mut stdout = io::stdout();
        execute!(stdout, Hide, EnterAlternateScreen, Hide, EnableMouseCapture)?;
        // Windows console input does not decode bracketed-paste VT sequences.
        #[cfg(not(target_os = "windows"))]
        execute!(stdout, EnableBracketedPaste)?;

        let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        terminal.hide_cursor()?;
        terminal.clear()?;
        Ok(terminal)
    })();
    if result.is_err() {
        let _ = restore_terminal();
    }
    result
}

/// Draw with a hidden hardware cursor serving only as the IME anchor.
pub(crate) fn draw_frame<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    render: impl FnOnce(&mut Frame<'_>) -> Option<(u16, u16)>,
) -> io::Result<()> {
    let mut update = SynchronizedFrame {
        terminal,
        ended: false,
    };
    let result = (|| {
        execute!(update.terminal.backend_mut(), BeginSynchronizedUpdate)?;
        update.terminal.hide_cursor()?;
        let mut cursor = None;
        update.terminal.draw(|frame| cursor = render(frame))?;
        if let Some(position) = cursor {
            update.terminal.set_cursor_position(position)?;
        }
        Ok(())
    })();
    let end = update.end();
    result.and(end)
}

// End synchronized output on I/O errors and unwinding as well as success.
struct SynchronizedFrame<'a, W: Write> {
    terminal: &'a mut Terminal<CrosstermBackend<W>>,
    ended: bool,
}

impl<W: Write> SynchronizedFrame<'_, W> {
    fn end(&mut self) -> io::Result<()> {
        let result = execute!(self.terminal.backend_mut(), EndSynchronizedUpdate);
        self.ended = result.is_ok();
        result
    }
}

impl<W: Write> Drop for SynchronizedFrame<'_, W> {
    fn drop(&mut self) {
        if !self.ended {
            let _ = self.end();
        }
    }
}

/// Leave the alternate screen and disable raw mode. Safe to call twice.
pub fn restore_terminal() -> io::Result<()> {
    if !TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        return Ok(());
    }
    restore_output(&mut io::stdout(), disable_raw_mode)
}

fn restore_output(
    writer: &mut impl Write,
    disable_raw: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    // Evaluate every operation even if an earlier write fails. Show comes after
    // leaving the alternate screen so the shell's cursor is visible too.
    let mut result = execute!(writer, EndSynchronizedUpdate);
    result = result.and(execute!(writer, DisableMouseCapture));
    #[cfg(not(target_os = "windows"))]
    {
        result = result.and(execute!(writer, DisableBracketedPaste));
    }
    result = result.and(execute!(writer, LeaveAlternateScreen));
    result = result.and(execute!(writer, Show));
    result.and(disable_raw())
}

/// Install a panic hook that restores the terminal before the previous hook
/// runs, so a panic never leaves the user in raw mode.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        previous(info);
    }));
}

// Windows may use console APIs instead of writing ANSI to the capture writer.
#[cfg(all(test, not(target_os = "windows")))]
pub(crate) mod tests {
    use super::*;
    use ratatui::{TerminalOptions, Viewport, layout::Rect};

    #[derive(Default)]
    pub(crate) struct CaptureState {
        pub bytes: Vec<u8>,
        pub fail_next: bool,
        pub fail_marker: Option<u8>,
    }

    #[derive(Clone, Default)]
    pub(crate) struct Capture(pub std::rc::Rc<std::cell::RefCell<CaptureState>>);

    impl Capture {
        pub fn output(&self) -> String {
            String::from_utf8(self.0.borrow().bytes.clone()).unwrap()
        }
        pub fn clear(&self) {
            self.0.borrow_mut().bytes.clear();
        }
    }

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let mut state = self.0.borrow_mut();
            let marker = state
                .fail_marker
                .is_some_and(|marker| bytes.contains(&marker));
            if marker {
                state.fail_marker = None;
            }
            if std::mem::take(&mut state.fail_next) || marker {
                return Err(io::Error::other("injected write failure"));
            }
            state.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    pub(crate) fn terminal() -> (Terminal<CrosstermBackend<Capture>>, Capture) {
        terminal_with_area(Rect::new(0, 0, 40, 10))
    }

    pub(crate) fn terminal_with_area(area: Rect) -> (Terminal<CrosstermBackend<Capture>>, Capture) {
        let capture = Capture::default();
        let terminal = Terminal::with_options(
            CrosstermBackend::new(capture.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(area),
            },
        )
        .unwrap();
        (terminal, capture)
    }

    #[test]
    fn frames_keep_cursor_hidden_and_move_before_ending_update() {
        let (mut terminal, capture) = terminal();
        for symbol in ["a", "b", "中"] {
            capture.clear();
            draw_frame(&mut terminal, |frame| {
                frame.buffer_mut()[(0, 9)].set_symbol(symbol);
                Some((7, 3))
            })
            .unwrap();
            let bytes = capture.output();
            assert!(bytes.starts_with("\x1b[?2026h\x1b[?25l"));
            assert!(!bytes.contains("\x1b[?25h"));
            assert!(bytes.ends_with("\x1b[4;8H\x1b[?2026l"));
        }
        capture.clear();
        draw_frame(&mut terminal, |_| None).unwrap();
        let bytes = capture.output();
        assert!(!bytes.contains("\x1b[4;8H"));
        assert!(!bytes.contains("\x1b[?25h"));
        assert!(bytes.ends_with("\x1b[?2026l"));
    }

    #[test]
    fn draw_failure_still_ends_update_and_preserves_error() {
        let (mut terminal, capture) = terminal();
        for fail_begin in [true, false] {
            capture.clear();
            if fail_begin {
                capture.0.borrow_mut().fail_next = true;
            } else {
                capture.0.borrow_mut().fail_marker = Some(b'!');
            }
            let error = draw_frame(&mut terminal, |frame| {
                frame.buffer_mut()[(0, 0)].set_symbol("!");
                Some((7, 3))
            })
            .unwrap_err();
            assert_eq!(error.to_string(), "injected write failure");
            assert!(capture.output().ends_with("\x1b[?2026l"));
            assert!(!capture.output().contains("\x1b[?25h"));
        }
    }

    #[test]
    fn unwinding_ends_synchronized_update() {
        let (mut terminal, capture) = terminal();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = draw_frame(&mut terminal, |_| panic!("render failure"));
        }));
        assert!(panic.is_err());
        assert!(capture.output().ends_with("\x1b[?2026l"));
    }

    #[test]
    fn restoration_shows_cursor_and_resets_raw_mode_despite_write_failure() {
        for fail_next in [false, true] {
            let mut writer = Capture::default();
            writer.0.borrow_mut().fail_next = fail_next;
            let mut raw_disabled = false;
            let result = restore_output(&mut writer, || {
                raw_disabled = true;
                Ok(())
            });
            assert_eq!(result.is_err(), fail_next);
            assert!(raw_disabled);
            let bytes = writer.output();
            assert!(bytes.ends_with("\x1b[?1049l\x1b[?25h"));
        }
    }
}
