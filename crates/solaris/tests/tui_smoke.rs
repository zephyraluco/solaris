//! End-to-end smoke tests.
//!
//! These drive the real stack — `Tui` event routing, the `App` component, the
//! framework components and a scripted test backend — and render into a ratatui
//! `TestBackend`, so the whole UI path is exercised without needing a terminal.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use solaris::{App, AppOptions, Clipboard};
use solaris_backend::{AgentBackend, AgentEventStream};
use solaris_core::{AgentEvent, BackendError, Config, ToolCall, TurnRequest, Usage};
use solaris_provider::BackendOptions;
use solaris_tui::{Theme, Tui};

const WIDTH: u16 = 90;
const HEIGHT: u16 = 26;
/// Row of the transcript's last visible line (above the editor and footer).
const BOTTOM: usize = 21;
/// The status footer, the last row of the frame.
const FOOTER: u16 = HEIGHT - 1;

/// A backend that streams a prompt-echoing markdown reply, so the whole UI path
/// can be driven without a provider and without a network.
struct FakeBackend;

#[async_trait::async_trait]
impl AgentBackend for FakeBackend {
    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
        let reply = format!(
            "You said: {}\n\n## Reply\n\n- streamed chunk by chunk\n",
            request.prompt.trim()
        );
        let events = vec![
            AgentEvent::ThinkingDelta("thinking ".to_string()),
            AgentEvent::TextDelta(reply),
            AgentEvent::TurnComplete {
                usage: Usage::new(4, 2),
                cost_usd: 0.0,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn label(&self) -> &str {
        "fake"
    }
}

/// A backend that asks for a tool this session does not have, then answers.
///
/// It proves the loop is wired in without running anything: a call for a tool
/// that was never declared is refused inside the loop, so no file and no command
/// is touched.
#[derive(Default)]
struct ToolBackend {
    round: AtomicUsize,
}

#[async_trait::async_trait]
impl AgentBackend for ToolBackend {
    async fn run_turn(&self, _request: TurnRequest) -> Result<AgentEventStream, BackendError> {
        let round = self.round.fetch_add(1, Ordering::Relaxed);
        let events = if round == 0 {
            vec![
                AgentEvent::ToolCall(ToolCall::new(
                    "call-1",
                    "teleport",
                    serde_json::json!({ "to": "mars" }),
                )),
                AgentEvent::TurnComplete {
                    usage: Usage::new(2, 1),
                    cost_usd: 0.0,
                },
            ]
        } else {
            vec![
                AgentEvent::TextDelta("nowhere to go".to_string()),
                AgentEvent::TurnComplete {
                    usage: Usage::new(3, 1),
                    cost_usd: 0.0,
                },
            ]
        };
        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn label(&self) -> &str {
        "tool"
    }
}

struct Harness {
    tui: Tui,
    terminal: Terminal<TestBackend>,
}

/// A clipboard that records whatever is copied to it and hands back `paste`
/// when asked, so tests never touch the machine's real one.
fn clipboard_for(paste: Option<&str>) -> (Clipboard, Rc<RefCell<Vec<String>>>) {
    let copied: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let log = Rc::clone(&copied);
    let pasted = paste.map(str::to_string);

    let clipboard = Clipboard {
        write: Rc::new(move |text: &str| {
            log.borrow_mut().push(text.to_string());
            true
        }),
        read: Rc::new(move || pasted.clone()),
    };
    (clipboard, copied)
}

impl Harness {
    /// A harness whose clipboard starts empty and drops whatever is copied to
    /// it, so tests never touch the machine's real one.
    fn new() -> Self {
        let (clipboard, _) = clipboard_for(None);
        Self::with_clipboard(clipboard)
    }

    fn with_clipboard(clipboard: Clipboard) -> Self {
        Self::with_backend_and_clipboard(Arc::new(FakeBackend), clipboard)
    }

    /// A harness driven by `backend` rather than the echoing fake.
    fn with_backend(backend: Arc<dyn AgentBackend>) -> Self {
        let (clipboard, _) = clipboard_for(None);
        Self::with_backend_and_clipboard(backend, clipboard)
    }

    fn with_backend_and_clipboard(backend: Arc<dyn AgentBackend>, clipboard: Clipboard) -> Self {
        let fallback = Arc::clone(&backend);
        let mut tui = Tui::new();
        let mut options = AppOptions::new(
            backend,
            Config::default(),
            tui.quit_flag(),
            tui.overlay_queue(),
            tui.overlay_flag(),
        );
        // The app only sees a selection when it shares the driver's handle,
        // which is exactly what `main` wires up.
        options.selection = tui.selection();
        options.clipboard = clipboard;
        // Resolve backends from the credentials the way the binary does, so the
        // footer reports what a real session would. No test has credentials and
        // the network is off limits, so an unresolved choice is filled in with
        // the fake — the UI path under test is the same either way.
        options.backend_factory = Arc::new(move |auth, model| {
            let mut choice = solaris_provider::choose_backend(
                auth,
                model,
                BackendOptions {
                    environment: solaris_provider::empty_environment(),
                },
            );
            if choice.provider_id.is_none() {
                choice.backend = Arc::clone(&fallback);
            }
            choice
        });
        tui.set_root(Box::new(App::new(options)));

        let terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("test terminal");
        Self { tui, terminal }
    }

    /// Draw one frame and return the visible text.
    fn draw(&mut self) -> String {
        let Self { tui, terminal } = self;
        terminal
            .draw(|frame| {
                let area = frame.area();
                tui.render(frame.buffer_mut(), area);
            })
            .expect("draw");
        buffer_text(terminal)
    }

    fn key(&mut self, code: KeyCode) {
        self.tui
            .handle_key(KeyEvent::new(code, KeyModifiers::empty()));
    }

    fn ctrl(&mut self, c: char) {
        self.tui
            .handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    /// Send a mouse event through the driver, the way the event loop does.
    fn mouse(&mut self, kind: MouseEventKind, column: u16, row: u16) {
        self.tui.handle_mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        });
    }

    fn type_str(&mut self, text: &str) {
        for ch in text.chars() {
            self.key(KeyCode::Char(ch));
        }
    }

    /// Tick and redraw until `needle` appears, or give up.
    async fn settle_on(&mut self, needle: &str) -> String {
        let mut text = self.draw();
        for _ in 0..500 {
            self.tui.tick();
            tokio::time::sleep(Duration::from_millis(1)).await;
            text = self.draw();
            if text.contains(needle) {
                break;
            }
        }
        text
    }
}

fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn the_first_frame_is_the_terminal_ready_for_input() {
    // No welcome screen: the app opens on the transcript with the editor and
    // status footer already in place, and the welcome box leading the
    // transcript until the first turn arrives.
    let mut harness = Harness::new();
    let text = harness.draw();

    assert!(!text.contains("press enter to start"), "{text}");
    assert!(
        text.contains("Welcome back"),
        "welcome box missing:\n{text}"
    );
    assert!(
        text.contains("Tips for getting started"),
        "tips missing:\n{text}"
    );
    assert!(
        text.contains("No recent activity"),
        "activity placeholder missing:\n{text}"
    );
    assert!(text.contains("Ask anything"), "{text}");
    assert!(text.contains("BUILD"), "{text}");
    // The footer names the backend that would answer, which under test is the
    // stand-in rather than a provider.
    assert!(text.contains("fake"), "{text}");

    // Typing works immediately, with no key press needed to "enter".
    harness.type_str("hi");
    let text = harness.draw();
    assert!(text.contains("hi"), "{text}");
}

#[test]
fn enter_on_the_slash_menu_runs_the_highlighted_command() {
    let mut harness = Harness::new();
    harness.type_str("/he");
    harness.key(KeyCode::Enter);

    // `/help` opened instead of the two characters being sent as a prompt.
    let text = harness.draw();
    assert!(text.contains("Keyboard"), "help did not open:\n{text}");
    assert!(!text.contains("You said: /he"), "{text}");

    // Escape closes it, leaving an empty prompt behind.
    harness.key(KeyCode::Esc);
    let text = harness.draw();
    assert!(!text.contains("Keyboard"), "{text}");
}

#[test]
fn tab_on_the_slash_menu_fills_the_command_in() {
    let mut harness = Harness::new();
    harness.type_str("/th");
    harness.key(KeyCode::Tab);

    // The command is typed in but not run, so arguments can follow.
    let text = harness.draw();
    assert!(text.contains("/theme"), "{text}");
    assert!(!text.contains("Select a theme:"), "Tab ran it:\n{text}");

    harness.type_str(" light");
    harness.key(KeyCode::Enter);
    harness.tui.tick();
    let text = harness.draw();
    assert!(text.contains("theme: light"), "{text}");
}

#[tokio::test]
async fn the_welcome_box_scrolls_away_with_the_first_turn() {
    let mut harness = Harness::new();
    assert!(harness.draw().contains("Welcome back"));

    harness.type_str("hello");
    harness.key(KeyCode::Enter);
    let text = harness.settle_on("You said: hello").await;

    assert!(!text.contains("Welcome back"), "box still up:\n{text}");
    assert!(!text.contains("Tips for getting started"), "{text}");
}

#[test]
fn the_buddy_command_opens_the_card() {
    let mut harness = Harness::new();
    harness.type_str("/buddy");
    harness.key(KeyCode::Enter);

    let text = harness.draw();
    assert!(text.contains("Buddy"), "{text}");
    assert!(text.contains("species:"), "{text}");
    assert!(text.contains("stats:"), "{text}");
}

#[test]
fn slash_completion_popup_lists_commands() {
    let mut harness = Harness::new();
    harness.key(KeyCode::Char('/'));
    let text = harness.draw();

    // `/theme` is only ever rendered by the completion popup.
    assert!(text.contains("/theme"), "{text}");
    assert!(text.contains("/model"), "{text}");
}

#[test]
fn filtering_the_slash_completion_popup_renders_the_single_match() {
    // Regression: typing `/c` left one match at hint index 4, which the popup
    // used to index the filtered list with, panicking on `cargo run`.
    let mut harness = Harness::new();
    harness.type_str("/c");
    let text = harness.draw();

    assert!(text.contains("/clear"), "{text}");
    assert!(!text.contains("/theme"), "popup not filtered:\n{text}");
}

#[test]
fn command_palette_renders_the_claude_picker_and_captures_input() {
    let mut harness = Harness::new();
    harness.ctrl('k');

    let text = harness.draw();
    assert!(text.contains("Commands"), "{text}");
    assert!(text.contains("Select a command:"), "{text}");
    assert!(
        text.contains("❯ 1. /help · Show keyboard shortcuts"),
        "{text}"
    );
    assert!(text.contains("Enter to select · Esc to cancel"), "{text}");

    // While the overlay is modal, typing filters the palette, not the editor.
    harness.type_str("mode");
    let text = harness.draw();
    assert!(text.contains("filter: mode"), "{text}");
    assert!(text.contains("/mode"), "{text}");

    // Escape closes it.
    harness.key(KeyCode::Esc);
    let text = harness.draw();
    assert!(!text.contains("Select a command:"), "{text}");
}

#[test]
fn theme_picker_applies_the_highlighted_theme() {
    let mut harness = Harness::new();
    harness.type_str("/theme");
    harness.key(KeyCode::Enter);

    let text = harness.draw();
    assert!(text.contains("Select a theme:"), "{text}");
    assert!(text.contains("❯ 1. dark · dark background"), "{text}");
    assert!(text.contains("2. light · light background"), "{text}");
    assert!(text.contains("Enter to select · Esc to cancel"), "{text}");

    // Move to "light" and confirm: the dialog closes and the theme is applied.
    harness.key(KeyCode::Down);
    harness.key(KeyCode::Enter);
    harness.tui.tick();
    let text = harness.draw();

    assert!(!text.contains("Select a theme:"), "{text}");
    assert!(text.contains("theme: light"), "{text}");
}

#[test]
fn help_overlay_opens_and_closes() {
    let mut harness = Harness::new();
    harness.key(KeyCode::F(1));

    let text = harness.draw();
    assert!(text.contains("Keyboard"), "{text}");
    assert!(text.contains("ctrl+k"), "{text}");

    harness.key(KeyCode::Esc);
    let text = harness.draw();
    assert!(!text.contains("Keyboard"), "{text}");
}

#[test]
fn theme_command_applies_immediately() {
    let mut harness = Harness::new();

    // `/theme light` applies without opening a dialog.
    harness.type_str("/theme light");
    harness.key(KeyCode::Enter);
    let text = harness.draw();
    assert!(text.contains("light"), "{text}");
}

#[tokio::test]
async fn prompt_streams_a_reply_rendered_as_markdown() {
    let mut harness = Harness::new();
    harness.type_str("hello");
    harness.key(KeyCode::Enter);

    let text = harness.settle_on("You said: hello").await;

    assert!(text.contains("hello"), "prompt missing:\n{text}");
    // Markdown headings lose their `##` marker when rendered.
    assert!(text.contains("Reply"), "reply missing:\n{text}");
    assert!(!text.contains("## Reply"), "raw markdown leaked:\n{text}");
    assert!(!text.contains("generating"), "spinner stuck:\n{text}");
    // The footer reports the tokens the backend reported.
    assert!(text.contains("tok"), "token counter missing:\n{text}");
}

#[tokio::test]
async fn a_tool_call_and_its_answer_are_shown_in_the_transcript() {
    let mut harness = Harness::with_backend(Arc::new(ToolBackend::default()));
    harness.type_str("go");
    harness.key(KeyCode::Enter);

    let text = harness.settle_on("nowhere to go").await;

    assert!(text.contains("teleport"), "the call is shown:\n{text}");
    assert!(text.contains("mars"), "its arguments are shown:\n{text}");
    assert!(
        text.contains('✗'),
        "a call that did not run is marked as failed:\n{text}"
    );
    assert!(
        text.contains("nowhere to go"),
        "the model's answer follows:\n{text}"
    );
}

#[tokio::test]
async fn mouse_wheel_scrolls_a_transcript_that_overflows() {
    let mut harness = Harness::new();

    // Three turns is more than the viewport can show.
    for prompt in ["hello", "again", "third"] {
        harness.type_str(prompt);
        harness.key(KeyCode::Enter);
    }
    let _ = harness.settle_on("You said: third").await;
    let at_bottom = harness.draw();
    assert!(at_bottom.contains("You said: third"), "{at_bottom}");

    // Wheel up detaches the view from the end. One turn already fills the
    // viewport, so three turns guarantee the transcript overflows.
    for _ in 0..5 {
        harness.tui.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 10,
            row: 3,
            modifiers: KeyModifiers::empty(),
        });
    }
    let scrolled = harness.draw();
    assert_ne!(
        at_bottom, scrolled,
        "scrolling up should change the visible window"
    );

    // The window moved up, so the newest turn's opening line is no longer the
    // last thing on screen; the bottom-most content differs.
    let bottom_last_line = at_bottom.lines().nth(BOTTOM).unwrap_or("");
    let scrolled_last_line = scrolled.lines().nth(BOTTOM).unwrap_or("");
    assert_ne!(
        bottom_last_line, scrolled_last_line,
        "the bottom row should show older content after scrolling up"
    );

    // Wheeling back past the end re-arms following and reproduces the bottom.
    for _ in 0..60 {
        harness.tui.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 10,
            row: 3,
            modifiers: KeyModifiers::empty(),
        });
    }
    assert_eq!(
        at_bottom,
        harness.draw(),
        "scrolling back to the end should restore the followed view"
    );
}

#[tokio::test]
async fn the_model_command_needs_a_provider_then_lists_its_models() {
    let mut harness = Harness::new();

    // With nothing connected there is no model list to choose from, so the
    // command explains itself instead of opening an empty picker.
    harness.type_str("/model");
    harness.key(KeyCode::Enter);
    let frame = harness.draw();
    assert!(!frame.contains("Select a model:"), "{frame}");
    assert!(frame.contains("/connect"), "no advice was shown:\n{frame}");

    // Connect a provider by API key; the wizard's own model step takes the
    // first model it offers.
    harness.type_str("/connect");
    harness.key(KeyCode::Enter);
    harness.key(KeyCode::Enter); // pick Anthropic
    harness.type_str("sk-live-1234");
    harness.key(KeyCode::Enter); // confirm the key
    harness.key(KeyCode::Enter); // take the model the wizard offers

    // Now the picker lists that provider's models, in the prompt region.
    harness.type_str("/model");
    harness.key(KeyCode::Enter);
    assert!(harness.tui.overlay_queue().borrow().is_empty());
    let frame = harness.draw();
    assert!(frame.contains("Select a model:"), "{frame}");
    assert!(frame.contains("claude-opus-4-1"), "{frame}");

    harness.key(KeyCode::Down);
    harness.key(KeyCode::Enter);

    let after = harness.draw();
    assert!(!after.contains("Select a model:"), "the picker stayed up");
    assert!(after.contains("model:"), "the choice was not announced");
}

#[tokio::test]
async fn ctrl_v_pastes_the_clipboard_into_the_prompt() {
    let (clipboard, _) = clipboard_for(Some("pasted into the prompt"));
    let mut harness = Harness::with_clipboard(clipboard);
    let _ = harness.draw();

    harness.ctrl('v');

    let text = harness.draw();
    assert!(text.contains("pasted into the prompt"), "{text}");
}

#[tokio::test]
async fn dragging_the_footer_then_ctrl_c_copies_what_it_shows() {
    let (clipboard, copied) = clipboard_for(None);
    let mut harness = Harness::with_clipboard(clipboard);

    // The footer names the backend and the mode, so it has known content
    // whatever the transcript above it happens to be showing.
    let frame = harness.draw();
    let footer: Vec<char> = frame
        .lines()
        .nth(FOOTER as usize)
        .expect("a footer row")
        .chars()
        .collect();
    let expected: String = footer[1..=14].iter().collect();
    assert!(
        expected.contains("fake") && expected.contains("BUILD"),
        "unexpected footer: {expected:?}"
    );

    harness.mouse(MouseEventKind::Down(MouseButton::Left), 1, FOOTER);
    harness.mouse(MouseEventKind::Drag(MouseButton::Left), 14, FOOTER);
    let _ = harness.draw();
    harness.mouse(MouseEventKind::Up(MouseButton::Left), 14, FOOTER);
    harness.tui.tick();

    assert!(
        copied.borrow().is_empty(),
        "dragging copied to the clipboard by itself"
    );

    let quit = harness.tui.quit_flag();
    harness.ctrl('c');
    harness.tui.tick();

    assert_eq!(copied.borrow().as_slice(), [expected]);
    assert!(!quit.get(), "copying a selection quit the app");
}

#[tokio::test]
async fn the_dragged_range_stays_highlighted_after_the_release() {
    let mut harness = Harness::new();
    let _ = harness.draw();

    harness.mouse(MouseEventKind::Down(MouseButton::Left), 1, FOOTER);
    harness.mouse(MouseEventKind::Drag(MouseButton::Left), 6, FOOTER);
    harness.mouse(MouseEventKind::Up(MouseButton::Left), 6, FOOTER);
    let _ = harness.draw();

    let highlight = Theme::dark().selection_bg;
    let buffer = harness.terminal.backend().buffer();
    for column in 1..=6 {
        assert_eq!(buffer[(column, FOOTER)].bg, highlight, "column {column}");
    }
    assert_ne!(buffer[(0, FOOTER)].bg, highlight, "before the anchor");
    assert_ne!(buffer[(7, FOOTER)].bg, highlight, "after the focus");

    // A press somewhere else starts a new range, so the old one goes away.
    harness.mouse(MouseEventKind::Down(MouseButton::Left), 20, FOOTER);
    let _ = harness.draw();
    let buffer = harness.terminal.backend().buffer();
    assert_ne!(buffer[(1, FOOTER)].bg, highlight);
}

#[tokio::test]
async fn cells_inside_a_dialog_are_selectable_too() {
    let mut harness = Harness::new();
    harness.key(KeyCode::F(1));

    let frame = harness.draw();
    let row = frame
        .lines()
        .position(|line| line.contains("Keyboard"))
        .expect("the help dialog never opened") as u16;

    // "Keyboard" is the dialog's first content line, one cell in from its left
    // edge — so this drag has to take the dialog's own cells, not the welcome
    // box showing through beside it.
    harness.mouse(MouseEventKind::Down(MouseButton::Left), 13, row);
    harness.mouse(MouseEventKind::Drag(MouseButton::Left), 30, row);
    harness.mouse(MouseEventKind::Up(MouseButton::Left), 30, row);
    let _ = harness.draw();

    let highlight = Theme::dark().selection_bg;
    let buffer = harness.terminal.backend().buffer();
    assert_eq!(buffer[(13, row)].bg, highlight, "inside the dialog");
    assert_eq!(buffer[(20, row)].bg, highlight, "inside the dialog");
    assert_ne!(buffer[(12, row)].bg, highlight, "outside the dialog");

    // Escape closes the dialog without taking the selection with it.
    harness.key(KeyCode::Esc);
    let _ = harness.draw();
    assert_eq!(harness.terminal.backend().buffer()[(20, row)].bg, highlight);
}

#[tokio::test]
async fn mode_toggle_repaints_the_footer() {
    let mut harness = Harness::new();
    assert!(harness.draw().contains("BUILD"));

    harness.key(KeyCode::Tab);
    let text = harness.draw();
    assert!(text.contains("PLAN"), "{text}");
    assert!(!text.contains("BUILD"), "{text}");
}

#[tokio::test]
async fn a_single_cell_drag_copies_that_cell() {
    let (clipboard, copied) = clipboard_for(None);
    let mut harness = Harness::with_clipboard(clipboard);

    // The footer opens with a space, so its second cell holds the first
    // character of the model name whatever else is on screen.
    let frame = harness.draw();
    let expected: String = frame
        .lines()
        .nth(FOOTER as usize)
        .expect("a footer row")
        .chars()
        .nth(1)
        .expect("a footer cell")
        .to_string();
    assert_ne!(expected, " ", "the footer moved");

    harness.mouse(MouseEventKind::Down(MouseButton::Left), 1, FOOTER);
    harness.mouse(MouseEventKind::Drag(MouseButton::Left), 1, FOOTER);
    harness.mouse(MouseEventKind::Up(MouseButton::Left), 1, FOOTER);
    let _ = harness.draw();

    let quit = harness.tui.quit_flag();
    harness.ctrl('c');
    harness.tui.tick();

    assert_eq!(copied.borrow().as_slice(), [expected]);
    assert!(!quit.get(), "a one-cell selection asked to quit");
}

#[tokio::test]
async fn a_drag_over_blank_cells_copies_the_blanks() {
    let (clipboard, copied) = clipboard_for(None);
    let mut harness = Harness::with_clipboard(clipboard);

    // Take a run of blank cells from wherever this layout keeps them, so the
    // drag does not depend on any one row's wording.
    let frame = harness.draw();
    let (column, row) = frame
        .lines()
        .enumerate()
        .find_map(|(row, line)| {
            let cells: Vec<char> = line.chars().collect();
            (0..cells.len().saturating_sub(10))
                .find(|start| cells[*start..*start + 10].iter().all(|cell| *cell == ' '))
                .map(|column| (column as u16, row as u16))
        })
        .expect("no row has a blank run");
    let last = column + 9;

    harness.mouse(MouseEventKind::Down(MouseButton::Left), column, row);
    harness.mouse(MouseEventKind::Drag(MouseButton::Left), last, row);
    harness.mouse(MouseEventKind::Up(MouseButton::Left), last, row);
    let _ = harness.draw();

    let quit = harness.tui.quit_flag();
    harness.ctrl('c');
    harness.tui.tick();

    assert_eq!(copied.borrow().as_slice(), [" ".repeat(10)]);
    assert!(!quit.get(), "a blank selection asked to quit");
}

#[tokio::test]
async fn two_ctrl_c_presses_quit_while_one_only_asks() {
    let mut harness = Harness::new();
    let quit = harness.tui.quit_flag();
    let _ = harness.draw();

    harness.ctrl('c');
    let asked = harness.draw();
    assert!(!quit.get(), "one press must not quit");
    assert!(asked.contains("press ctrl+c again to quit"), "{asked}");

    harness.ctrl('c');
    assert!(quit.get());
}

#[tokio::test]
async fn ctrl_c_stops_the_turn_that_is_streaming() {
    let mut harness = Harness::new();
    harness.type_str("hello");
    harness.key(KeyCode::Enter);
    let streaming = harness.draw();
    assert!(streaming.contains("generating"), "{streaming}");

    harness.ctrl('c');
    let stopped = harness.draw();

    assert!(!stopped.contains("generating"), "{stopped}");
    assert!(stopped.contains("cancelled"), "{stopped}");
    assert!(stopped.contains("hello"), "the prompt stays:\n{stopped}");
}

// --------------------------------------------------------------- /connect

#[test]
fn connect_wizard_renders_inline_in_the_prompt_region() {
    let mut harness = Harness::new();
    harness.type_str("/connect");
    harness.key(KeyCode::Enter);

    let text = harness.draw();
    assert!(text.contains("Connect"), "{text}");
    assert!(text.contains("Select a provider:"), "{text}");
    assert!(
        text.contains("❯ 1. Anthropic · Claude models — API key"),
        "{text}"
    );
    assert!(text.contains("esc cancel"), "{text}");
    // It is inline, not an overlay: it replaces the input line.
    assert!(!text.contains("Ask anything"), "{text}");
}

#[test]
fn connect_wizard_steps_through_the_custom_endpoint_fields() {
    let mut harness = Harness::new();
    harness.type_str("/connect");
    harness.key(KeyCode::Enter);
    harness.type_str("8"); // the custom endpoint row

    let text = harness.draw();
    assert!(text.contains("Connect Custom endpoint"), "{text}");
    assert!(text.contains("Endpoint URL:"), "{text}");
    assert!(text.contains("API key (optional):"), "{text}");
    assert!(text.contains("tab switch field"), "{text}");
}

#[test]
fn connecting_an_api_key_flows_into_the_model_picker() {
    let mut harness = Harness::new();
    harness.type_str("/connect");
    harness.key(KeyCode::Enter);
    harness.key(KeyCode::Enter); // pick Anthropic
    harness.type_str("sk-live-1234");
    harness.key(KeyCode::Enter); // confirm the key

    let text = harness.draw();
    assert!(text.contains("Select a model:"), "{text}");
    assert!(text.contains("claude-sonnet-4-5"), "{text}");

    harness.key(KeyCode::Enter); // pick the first model
    let text = harness.draw();
    assert!(!text.contains("Select a model:"), "{text}");
    assert!(
        text.contains("anthropic") && text.contains("claude-sonnet-4-5"),
        "the footer should report the connection:\n{text}"
    );
}

#[tokio::test]
async fn device_auth_reports_that_sign_in_is_unavailable() {
    let mut harness = Harness::new();
    harness.type_str("/connect");
    harness.key(KeyCode::Enter);
    harness.key(KeyCode::Down); // the subscription provider
    harness.key(KeyCode::Enter);

    harness.tui.tick();
    let text = harness.draw();

    // The sign-in flow is not built, and saying so is better than walking the
    // user through a code that mints a token nothing can use.
    assert!(text.contains("not implemented"), "{text}");
    assert!(text.contains("Press any key to dismiss"), "{text}");
    assert!(!text.contains("Enter this code in the browser:"), "{text}");
}
