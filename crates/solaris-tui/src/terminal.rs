//! Terminal lifecycle: raw mode, alternate screen, mouse capture, and a panic
//! hook that always restores the terminal.

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(not(target_os = "windows"))]
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

/// Terminal type used by [`crate::Tui::run`].
pub type PiTerminal = Terminal<CrosstermBackend<Stdout>>;

static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Enter raw mode + alternate screen + mouse capture and build a terminal.
pub fn setup_terminal() -> io::Result<PiTerminal> {
    enable_raw_mode()?;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    // Bracketed paste stays off on Windows: Windows Terminal wraps pasted text
    // in VT sequences that the console backend does not decode into paste
    // events, so the payload would arrive as individual key presses instead.
    #[cfg(not(target_os = "windows"))]
    execute!(stdout, EnableBracketedPaste)?;

    TERMINAL_ACTIVE.store(true, Ordering::SeqCst);

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;
    Ok(terminal)
}

/// Leave the alternate screen and disable raw mode. Safe to call twice.
pub fn restore_terminal() -> io::Result<()> {
    if !TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        return Ok(());
    }

    let mut stdout = io::stdout();
    execute!(stdout, DisableMouseCapture)?;
    #[cfg(not(target_os = "windows"))]
    execute!(stdout, DisableBracketedPaste)?;
    execute!(stdout, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
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
