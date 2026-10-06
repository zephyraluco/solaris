//! The inline `/connect` wizard.
//!
//! Modelled on claurst's `connect_flow`, including its deliberate difference
//! from every other dialog here: `/connect` is **not** a modal overlay. It is
//! drawn *inline* in the prompt region — accent title, short description, a
//! `Select a provider:` question, a `❯`-marked option list and a pinned hint
//! row — the way Claude Code's `/login` screen looks. Text-entry steps render
//! as plain inline fields with a `_` cursor instead of bordered inputs.
//!
//! [`ConnectFlow`] is a state machine over [`ConnectStep`] that owns key
//! handling, mouse hit-testing and rendering. The *effects* — storing the
//! credential, activating the provider, spawning the device-auth task — live in
//! [`crate::app`], because they touch application state rather than flow state.
//! [`ConnectOutcome`] is the single funnel out of this module.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use solaris_core::{PROVIDERS, ProviderSpec, mask_secret};
use solaris_tui::components::select_list::SelectItem;
use solaris_tui::theme::Theme;
use solaris_tui::util::{digit_count, display_width, truncate_to_width, wrap_text};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// Option rows shown at once.
const MAX_LIST_ROWS: u16 = 8;
/// Rows reserved for the pinned hint line.
const FOOTER_ROWS: u16 = 1;

/// Which panel of the wizard is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectStep {
    /// Step 1 — pick a provider from the catalogue.
    Provider,
    /// Step 2a — paste an API key.
    ApiKey,
    /// Step 2b — a custom OpenAI-compatible endpoint (URL plus optional key).
    CustomProvider,
    /// Step 2c — device-code OAuth, driven by a background task.
    DeviceAuth,
    /// Step 3 — pick a model from the provider that just connected.
    Model,
}

/// Work the application must perform when a step confirms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectSubmit {
    /// A runtime that needs no credential.
    Local {
        provider_id: String,
        provider_name: String,
    },
    /// Store one API key and activate the provider.
    ApiKey {
        provider_id: String,
        provider_name: String,
        key: String,
    },
    /// Persist the endpoint URL, store the key, activate the provider.
    CustomProvider {
        provider_id: String,
        provider_name: String,
        base_url: String,
        api_key: String,
    },
    /// Device auth finished — store the token and activate the provider.
    DeviceAuthToken {
        provider_id: String,
        provider_name: String,
        token: String,
    },
}

/// What a key or mouse event did to the flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectOutcome {
    /// Consumed; the flow stays on the same or a new step.
    Handled,
    /// The flow is finished — the application must drop it.
    Closed,
    /// A provider row was picked; the application decides which step follows.
    ProviderPicked { id: String, name: String },
    /// A credential was confirmed; the application applies it.
    Submit(ConnectSubmit),
    /// A model was chosen for the provider that just connected.
    ModelPicked { model_id: String },
}

/// Progress of the device-code OAuth step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceAuthStatus {
    /// Not started.
    Idle,
    /// Waiting for the authorization server to issue a code.
    WaitingForCode,
    /// The user code is on screen; waiting for the user to authorize.
    ShowingCode,
    /// A token was obtained.
    Success(String),
    /// Something went wrong.
    Error(String),
}

/// Messages the background device-auth task sends back to the event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceAuthEvent {
    /// A device code was issued — show it and the verification URI.
    GotCode {
        user_code: String,
        verification_uri: String,
    },
    /// The access token was obtained.
    TokenReceived(String),
    /// The attempt failed.
    Error(String),
}

/// Colours and weights used while drawing the wizard.
#[derive(Debug, Clone, Copy)]
pub struct ConnectStyles {
    pub title: Style,
    pub text: Style,
    pub muted: Style,
    pub dim: Style,
    pub tip: Style,
    pub ok: Style,
    pub warn: Style,
    pub error: Style,
}

impl ConnectStyles {
    /// Derive the palette from the active theme.
    pub fn from_theme(theme: &Theme) -> Self {
        Self {
            title: Style::default()
                .fg(theme.heading)
                .add_modifier(Modifier::BOLD),
            text: Style::default().fg(theme.fg),
            muted: Style::default().fg(theme.muted),
            dim: Style::default().fg(theme.dim),
            tip: Style::default()
                .fg(theme.success)
                .add_modifier(Modifier::BOLD),
            ok: Style::default().fg(theme.success),
            warn: Style::default().fg(theme.warning),
            error: Style::default().fg(theme.error),
        }
    }
}

/// Absolute or block-relative rows to `(row, entry index)` pairs, used for
/// mouse hit-testing.
type RowMap = Vec<(usize, usize)>;

/// One built panel: its lines plus where the picks and fields landed.
struct Block {
    lines: Vec<Line<'static>>,
    item_rows: RowMap,
    field_rows: RowMap,
}

/// The `/connect` wizard.
pub struct ConnectFlow {
    step: ConnectStep,
    styles: ConnectStyles,

    // provider / model pickers
    providers: Vec<SelectItem>,
    models: Vec<SelectItem>,
    selected: usize,
    scroll: usize,

    // the provider being set up
    provider_id: String,
    provider_name: String,

    // text entry: `input` is the API key or the endpoint URL, `input2` is the
    // custom endpoint's key, `field` selects between multi-field steps.
    input: String,
    input2: String,
    field: usize,

    // device auth
    device_status: DeviceAuthStatus,
    user_code: String,
    verification_uri: String,

    // render state
    last_area: Rect,
    item_rows: Vec<(u16, usize)>,
    field_rows: Vec<(u16, usize)>,
}

impl ConnectFlow {
    /// Step 1 — the provider picker, built from the catalogue.
    pub fn new(theme: &Theme) -> Self {
        let providers = PROVIDERS
            .iter()
            .map(|spec| {
                let item = SelectItem::new(spec.id, spec.name).description(spec.description);
                match spec.badge {
                    Some(badge) => item.badge(badge),
                    None => item,
                }
            })
            .collect();

        Self {
            step: ConnectStep::Provider,
            styles: ConnectStyles::from_theme(theme),
            providers,
            models: Vec::new(),
            selected: 0,
            scroll: 0,
            provider_id: String::new(),
            provider_name: String::new(),
            input: String::new(),
            input2: String::new(),
            field: 0,
            device_status: DeviceAuthStatus::Idle,
            user_code: String::new(),
            verification_uri: String::new(),
            last_area: Rect::default(),
            item_rows: Vec::new(),
            field_rows: Vec::new(),
        }
    }

    /// Restyle after a theme change.
    pub fn set_theme(&mut self, theme: &Theme) {
        self.styles = ConnectStyles::from_theme(theme);
    }

    /// Which panel is showing.
    pub fn step(&self) -> ConnectStep {
        self.step
    }

    /// The provider being set up.
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// The provider's display name.
    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    /// Device-auth progress.
    pub fn device_status(&self) -> &DeviceAuthStatus {
        &self.device_status
    }

    /// Area painted last frame (empty until the first render).
    pub fn area(&self) -> Rect {
        self.last_area
    }

    // -- step transitions --------------------------------------------------

    /// Step 2a — collect an API key for a provider.
    pub fn enter_api_key(&mut self, provider_id: String, provider_name: String) {
        self.enter_text_step(ConnectStep::ApiKey, provider_id, provider_name);
    }

    /// Step 2b — collect an endpoint URL and an optional key.
    pub fn enter_custom_provider(
        &mut self,
        provider_id: String,
        provider_name: String,
        current_url: Option<String>,
    ) {
        self.enter_text_step(ConnectStep::CustomProvider, provider_id, provider_name);
        self.input = current_url.unwrap_or_default();
    }

    /// Step 2c — device-code OAuth for a provider.
    pub fn enter_device_auth(&mut self, provider_id: String, provider_name: String) {
        self.enter_text_step(ConnectStep::DeviceAuth, provider_id, provider_name);
        self.device_status = DeviceAuthStatus::WaitingForCode;
    }

    /// Step 3 — pick a model from the provider that just connected.
    pub fn enter_models(&mut self, models: Vec<SelectItem>) {
        self.step = ConnectStep::Model;
        self.models = models;
        self.selected = 0;
        self.scroll = 0;
    }

    fn enter_text_step(&mut self, step: ConnectStep, provider_id: String, provider_name: String) {
        self.step = step;
        self.provider_id = provider_id;
        self.provider_name = provider_name;
        self.input.clear();
        self.input2.clear();
        self.field = 0;
        self.scroll = 0;
    }

    // -- device auth, driven from outside ----------------------------------

    /// A device code was issued.
    pub fn device_set_code(&mut self, user_code: String, verification_uri: String) {
        self.user_code = user_code;
        self.verification_uri = verification_uri;
        self.device_status = DeviceAuthStatus::ShowingCode;
    }

    /// The access token arrived.
    pub fn device_set_success(&mut self, token: String) {
        self.device_status = DeviceAuthStatus::Success(token);
    }

    /// The attempt failed.
    pub fn device_set_error(&mut self, message: String) {
        self.device_status = DeviceAuthStatus::Error(message);
    }

    /// The model highlighted on the model step.
    pub fn selected_model(&self) -> Option<&str> {
        self.models
            .get(self.selected)
            .map(|item| item.value.as_str())
    }

    // -- paste -------------------------------------------------------------

    /// Route pasted text into the active field.
    ///
    /// Returns `false` when the current step has no text field, so the caller
    /// drops the paste instead of leaking it into the prompt behind.
    pub fn insert_paste(&mut self, data: &str) -> bool {
        let target = match self.step {
            ConnectStep::ApiKey => &mut self.input,
            ConnectStep::CustomProvider => match self.field {
                0 => &mut self.input,
                _ => &mut self.input2,
            },
            _ => return false,
        };
        let cleaned: String = data.chars().filter(|c| !c.is_control()).collect();
        target.push_str(&cleaned);
        true
    }

    // -- navigation --------------------------------------------------------

    fn item_count(&self) -> usize {
        match self.step {
            ConnectStep::Provider => self.providers.len(),
            ConnectStep::Model => self.models.len(),
            _ => 0,
        }
    }

    fn items(&self) -> &[SelectItem] {
        match self.step {
            ConnectStep::Model => &self.models,
            _ => &self.providers,
        }
    }

    fn field_count(&self) -> usize {
        match self.step {
            ConnectStep::CustomProvider => 2,
            _ => 0,
        }
    }

    /// Move the highlight, wrapping around both ends like the pickers do.
    fn move_selection(&mut self, delta: isize) {
        let count = self.item_count();
        if count == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
    }

    /// Move between text fields, wrapping.
    fn move_field(&mut self, delta: isize) {
        let count = self.field_count();
        if count == 0 {
            return;
        }
        self.field = (self.field as isize + delta).rem_euclid(count as isize) as usize;
    }

    fn active_input_mut(&mut self) -> Option<&mut String> {
        match self.step {
            ConnectStep::ApiKey => Some(&mut self.input),
            ConnectStep::CustomProvider => {
                if self.field == 0 {
                    Some(&mut self.input)
                } else {
                    Some(&mut self.input2)
                }
            }
            _ => None,
        }
    }

    fn backspace(&mut self) {
        if let Some(input) = self.active_input_mut() {
            input.pop();
        }
    }

    // -- keys --------------------------------------------------------------

    /// Route one key press.
    ///
    /// Never ignores anything: while the wizard is up it owns the keyboard, so
    /// keys it does not understand are swallowed rather than leaking into the
    /// prompt editor underneath.
    pub fn on_key(&mut self, key: KeyEvent) -> ConnectOutcome {
        // Esc — and Ctrl+C, the instinctive "abort this prompt" — cancels the
        // whole wizard.
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return ConnectOutcome::Closed;
        }

        match self.step {
            ConnectStep::Provider => self.picker_key(key),
            ConnectStep::Model => self.model_key(key),
            ConnectStep::ApiKey => self.api_key_key(key),
            ConnectStep::CustomProvider => self.custom_provider_key(key),
            ConnectStep::DeviceAuth => self.device_auth_key(),
        }
    }

    fn picker_key(&mut self, key: KeyEvent) -> ConnectOutcome {
        match key.code {
            KeyCode::Up => {
                self.move_selection(-1);
                ConnectOutcome::Handled
            }
            KeyCode::Down => {
                self.move_selection(1);
                ConnectOutcome::Handled
            }
            KeyCode::PageUp => {
                self.move_selection(-(MAX_LIST_ROWS as isize));
                ConnectOutcome::Handled
            }
            KeyCode::PageDown => {
                self.move_selection(MAX_LIST_ROWS as isize);
                ConnectOutcome::Handled
            }
            KeyCode::Home => {
                self.selected = 0;
                ConnectOutcome::Handled
            }
            KeyCode::End => {
                self.selected = self.item_count().saturating_sub(1);
                ConnectOutcome::Handled
            }
            KeyCode::Enter => self.confirm_provider(),
            // Digits 1-9 jump straight to that row and confirm it, matching the
            // numbers on screen. Zero is not a shortcut.
            KeyCode::Char(c) if key.modifiers.is_empty() && c.is_ascii_digit() && c != '0' => {
                let index = (c as u8 - b'1') as usize;
                if index >= self.item_count() {
                    return ConnectOutcome::Handled;
                }
                self.selected = index;
                self.confirm_provider()
            }
            _ => ConnectOutcome::Handled,
        }
    }

    fn confirm_provider(&mut self) -> ConnectOutcome {
        let Some(item) = self.providers.get(self.selected) else {
            return ConnectOutcome::Handled;
        };
        ConnectOutcome::ProviderPicked {
            id: item.value.clone(),
            name: item.label.clone(),
        }
    }

    fn model_key(&mut self, key: KeyEvent) -> ConnectOutcome {
        match key.code {
            KeyCode::Up => {
                self.move_selection(-1);
                ConnectOutcome::Handled
            }
            KeyCode::Down => {
                self.move_selection(1);
                ConnectOutcome::Handled
            }
            KeyCode::Home => {
                self.selected = 0;
                ConnectOutcome::Handled
            }
            KeyCode::End => {
                self.selected = self.item_count().saturating_sub(1);
                ConnectOutcome::Handled
            }
            KeyCode::Enter => self.confirm_model(),
            KeyCode::Char(c) if key.modifiers.is_empty() && c.is_ascii_digit() && c != '0' => {
                let index = (c as u8 - b'1') as usize;
                if index >= self.item_count() {
                    return ConnectOutcome::Handled;
                }
                self.selected = index;
                self.confirm_model()
            }
            _ => ConnectOutcome::Handled,
        }
    }

    fn confirm_model(&mut self) -> ConnectOutcome {
        match self.selected_model() {
            Some(model_id) => ConnectOutcome::ModelPicked {
                model_id: model_id.to_string(),
            },
            None => ConnectOutcome::Handled,
        }
    }

    fn api_key_key(&mut self, key: KeyEvent) -> ConnectOutcome {
        match key.code {
            KeyCode::Enter => {
                let key_value = self.input.trim().to_string();
                if key_value.is_empty() {
                    ConnectOutcome::Handled
                } else {
                    ConnectOutcome::Submit(ConnectSubmit::ApiKey {
                        provider_id: self.provider_id.clone(),
                        provider_name: self.provider_name.clone(),
                        key: key_value,
                    })
                }
            }
            KeyCode::Backspace => {
                self.backspace();
                ConnectOutcome::Handled
            }
            KeyCode::Char(c) if plain_text_key(&key) => {
                if let Some(input) = self.active_input_mut() {
                    input.push(c);
                }
                ConnectOutcome::Handled
            }
            _ => ConnectOutcome::Handled,
        }
    }

    fn custom_provider_key(&mut self, key: KeyEvent) -> ConnectOutcome {
        match key.code {
            // Tab / Shift+Tab and ↓/↑ both switch fields.
            KeyCode::Tab | KeyCode::Down => {
                self.move_field(1);
                ConnectOutcome::Handled
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.move_field(-1);
                ConnectOutcome::Handled
            }
            KeyCode::Enter => {
                // The URL is mandatory; an empty one just moves the cursor to
                // the field that is still missing.
                if self.input.trim().is_empty() {
                    self.field = 0;
                    return ConnectOutcome::Handled;
                }
                ConnectOutcome::Submit(ConnectSubmit::CustomProvider {
                    provider_id: self.provider_id.clone(),
                    provider_name: self.provider_name.clone(),
                    base_url: self.input.trim().to_string(),
                    api_key: self.input2.trim().to_string(),
                })
            }
            KeyCode::Backspace => {
                self.backspace();
                ConnectOutcome::Handled
            }
            KeyCode::Char(c) if plain_text_key(&key) => {
                if let Some(input) = self.active_input_mut() {
                    input.push(c);
                }
                ConnectOutcome::Handled
            }
            _ => ConnectOutcome::Handled,
        }
    }

    fn device_auth_key(&mut self) -> ConnectOutcome {
        match &self.device_status {
            // Success: any key continues, and the app stores the credential.
            DeviceAuthStatus::Success(token) => {
                ConnectOutcome::Submit(ConnectSubmit::DeviceAuthToken {
                    provider_id: self.provider_id.clone(),
                    provider_name: self.provider_name.clone(),
                    token: token.clone(),
                })
            }
            // Error: any key dismisses.
            DeviceAuthStatus::Error(_) => ConnectOutcome::Closed,
            // While the background task works, every key is swallowed.
            _ => ConnectOutcome::Handled,
        }
    }

    // -- mouse -------------------------------------------------------------

    /// Route a mouse event.
    pub fn on_mouse(&mut self, mouse: MouseEvent) -> ConnectOutcome {
        if !rect_contains(self.last_area, mouse.column, mouse.row) {
            return ConnectOutcome::Handled;
        }

        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.move_selection(-1);
                ConnectOutcome::Handled
            }
            MouseEventKind::ScrollDown => {
                self.move_selection(1);
                ConnectOutcome::Handled
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = self.item_at_row(mouse.row) {
                    self.selected = index;
                    return match self.step {
                        ConnectStep::Model => self.confirm_model(),
                        _ => self.confirm_provider(),
                    };
                }
                if let Some(field) = self.field_at_row(mouse.row) {
                    self.field = field;
                }
                ConnectOutcome::Handled
            }
            _ => ConnectOutcome::Handled,
        }
    }

    fn item_at_row(&self, row: u16) -> Option<usize> {
        self.item_rows
            .iter()
            .find(|(r, _)| *r == row)
            .map(|(_, index)| *index)
    }

    fn field_at_row(&self, row: u16) -> Option<usize> {
        self.field_rows
            .iter()
            .find(|(r, _)| *r == row)
            .map(|(_, index)| *index)
    }

    // -- layout ------------------------------------------------------------

    /// Rows the fixed part of the block occupies at `width`.
    fn fixed_rows(&mut self, width: u16) -> u16 {
        self.body(width, 0).lines.len() as u16
    }

    /// Rows the option list would like to occupy.
    fn preferred_list_rows(&self) -> u16 {
        match self.step {
            ConnectStep::Provider | ConnectStep::Model => {
                (self.item_count() as u16).min(MAX_LIST_ROWS)
            }
            _ => 0,
        }
    }

    /// Total rows (block plus hint) the wizard wants at `width`.
    pub fn desired_height(&mut self, width: u16) -> u16 {
        self.fixed_rows(width) + self.preferred_list_rows() + FOOTER_ROWS
    }

    /// Smallest height the wizard can still be used in.
    pub fn min_height(&self) -> u16 {
        4
    }

    fn title(&self) -> String {
        match self.step {
            ConnectStep::Provider => "Connect".to_string(),
            _ => format!("Connect {}", self.provider_name),
        }
    }

    fn description(&self, width: u16) -> Vec<String> {
        match self.step {
            ConnectStep::Provider => wrap_text(
                "solaris can be used with a provider subscription or billed based on \
                 API usage through an API key.",
                width.saturating_sub(2).max(8) as usize,
            ),
            _ => Vec::new(),
        }
    }

    fn question(&self) -> String {
        match self.step {
            ConnectStep::Provider => "Select a provider:".to_string(),
            ConnectStep::ApiKey => "Paste your API key:".to_string(),
            ConnectStep::Model => "Select a model:".to_string(),
            _ => String::new(),
        }
    }

    fn device_lines(&self) -> Vec<Line<'static>> {
        match &self.device_status {
            DeviceAuthStatus::Idle | DeviceAuthStatus::WaitingForCode => vec![Line::from(
                Span::styled("Requesting device code…", self.styles.warn),
            )],
            DeviceAuthStatus::ShowingCode => vec![
                Line::from(Span::styled("Waiting for authorization…", self.styles.warn)),
                Line::from(""),
                Line::from(Span::styled(
                    "Enter this code in the browser:",
                    self.styles.muted,
                )),
                Line::from(Span::styled(
                    format!("    {}", self.user_code),
                    self.styles.text.add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    format!("  {}", self.verification_uri),
                    self.styles.muted,
                )),
            ],
            DeviceAuthStatus::Success(_) => vec![Line::from(Span::styled(
                "✓ Authorized — press any key to continue",
                self.styles.ok,
            ))],
            DeviceAuthStatus::Error(message) => vec![
                Line::from(Span::styled(format!("✗ {message}"), self.styles.error)),
                Line::from(""),
                Line::from(Span::styled("Press any key to dismiss.", self.styles.muted)),
            ],
        }
    }

    fn hint_line(&self) -> Line<'static> {
        let hints: Vec<&str> = match self.step {
            ConnectStep::Provider | ConnectStep::Model => {
                vec!["↑/↓ select", "enter confirm"]
            }
            ConnectStep::ApiKey => vec!["enter confirm"],
            ConnectStep::CustomProvider => vec!["tab switch field", "enter confirm"],
            ConnectStep::DeviceAuth => match self.device_status {
                DeviceAuthStatus::Success(_) | DeviceAuthStatus::Error(_) => {
                    vec!["any key continue"]
                }
                _ => Vec::new(),
            },
        };

        let mut spans: Vec<Span<'static>> = Vec::new();
        for hint in hints {
            if !spans.is_empty() {
                spans.push(Span::styled("   ", self.styles.dim));
            }
            spans.push(Span::styled(hint.to_string(), self.styles.dim));
        }

        // Esc always cancels — except once the flow is already done, where the
        // hint line says what the next key does instead.
        let finished = self.step == ConnectStep::DeviceAuth
            && matches!(self.device_status, DeviceAuthStatus::Success(_));
        if !finished {
            if !spans.is_empty() {
                spans.push(Span::styled("   ", self.styles.dim));
            }
            spans.push(Span::styled("esc cancel", self.styles.dim));
        }

        // Position counter for long lists.
        if matches!(self.step, ConnectStep::Provider | ConnectStep::Model) && self.item_count() > 0
        {
            spans.push(Span::styled(
                format!("   {}/{}", self.selected + 1, self.item_count()),
                self.styles.dim,
            ));
        }

        Line::from(spans)
    }

    /// Draw the wizard into `area` (the prompt region of the layout).
    pub fn render(&mut self, buf: &mut Buffer, area: Rect) {
        self.last_area = area;
        self.item_rows.clear();
        self.field_rows.clear();

        if area.width == 0 || area.height < 2 {
            return;
        }

        let width = area.width;
        // The hint row is pinned to the bottom so a squeezed layout still tells
        // the user how to get out.
        let body_area = Rect {
            height: area.height - 1,
            ..area
        };
        let hint_area = Rect {
            y: area.y + area.height - 1,
            height: 1,
            ..area
        };

        let list_rows = body_area.height.saturating_sub(self.fixed_rows(width)) as usize;
        let block = self.body(width, list_rows);

        for (row, line) in block
            .lines
            .iter()
            .take(body_area.height as usize)
            .enumerate()
        {
            buf.set_line(body_area.x, body_area.y + row as u16, line, body_area.width);
        }
        buf.set_line(hint_area.x, hint_area.y, &self.hint_line(), hint_area.width);

        // Hit-testing works on absolute rows, so translate what `body` recorded.
        self.item_rows = block
            .item_rows
            .into_iter()
            .map(|(row, index)| (body_area.y.saturating_add(row as u16), index))
            .collect();
        self.field_rows = block
            .field_rows
            .into_iter()
            .map(|(row, index)| (body_area.y.saturating_add(row as u16), index))
            .collect();
    }

    /// Build the block, with row indices relative to its first line.
    fn body(&mut self, width: u16, list_rows: usize) -> Block {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut item_rows: RowMap = Vec::new();
        let mut field_rows: RowMap = Vec::new();

        lines.push(Line::from(Span::styled(self.title(), self.styles.title)));

        let description = self.description(width);
        if !description.is_empty() {
            lines.push(Line::from(""));
            for line in description {
                lines.push(Line::from(Span::styled(line, self.styles.muted)));
            }
        }

        match self.step {
            ConnectStep::Provider | ConnectStep::Model => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(self.question(), self.styles.text)));
                lines.push(Line::from(""));
                self.push_item_rows(width, list_rows, &mut lines, &mut item_rows);
            }
            ConnectStep::ApiKey => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(self.question(), self.styles.text)));
                lines.push(Line::from(""));
                field_rows.push((lines.len(), 0));
                lines.push(self.secret_line(&self.input, true));
            }
            ConnectStep::CustomProvider => {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("Endpoint URL:", self.styles.text)));
                field_rows.push((lines.len(), 0));
                lines.push(self.plain_field_line(&self.input, self.field == 0));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "API key (optional):",
                    self.styles.text,
                )));
                field_rows.push((lines.len(), 1));
                lines.push(self.secret_line(&self.input2, self.field == 1));
            }
            ConnectStep::DeviceAuth => {
                lines.push(Line::from(""));
                for line in self.device_lines() {
                    lines.push(line);
                }
            }
        }

        Block {
            lines,
            item_rows,
            field_rows,
        }
    }

    /// The option list, windowed so the highlighted row is always visible.
    fn push_item_rows(
        &mut self,
        width: u16,
        list_rows: usize,
        lines: &mut Vec<Line<'static>>,
        item_rows: &mut RowMap,
    ) {
        let total = self.item_count();
        if total == 0 || list_rows == 0 {
            return;
        }

        let visible = list_rows.min(MAX_LIST_ROWS as usize).min(total);
        let mut start = self.scroll.min(total.saturating_sub(visible));
        if self.selected < start {
            start = self.selected;
        }
        if self.selected >= start + visible {
            start = self.selected + 1 - visible;
        }
        self.scroll = start;

        for index in start..start + visible {
            item_rows.push((lines.len(), index));
            lines.push(self.item_row(width, index));
        }
    }

    /// One option row: `❯ 3. Title · description   BADGE`.
    fn item_row(&self, width: u16, index: usize) -> Line<'static> {
        let item = &self.items()[index];
        let selected = index == self.selected;

        let marker = if selected { "❯ " } else { "  " };
        // Numbers are right-aligned across the list, matching the pickers.
        let number = format!(
            "{:>width$}. ",
            index + 1,
            width = digit_count(self.item_count())
        );
        let title_style = if selected {
            self.styles.text.add_modifier(Modifier::BOLD)
        } else {
            self.styles.muted
        };
        let desc_style = if selected {
            self.styles.text
        } else {
            self.styles.dim
        };

        let prefix_width = 2 + display_width(&number);
        let badge = item.badge.as_deref().filter(|text| !text.is_empty());
        let badge_width = badge.map_or(0, display_width);
        let badge_space = if badge_width > 0 && badge_width + 3 < width as usize {
            badge_width + 2
        } else {
            0
        };

        let mut text = item.label.clone();
        if !item.description.is_empty() {
            text.push_str(" · ");
            text.push_str(&item.description);
        }
        let text = truncate_to_width(
            &text,
            (width as usize)
                .saturating_sub(prefix_width)
                .saturating_sub(badge_space),
            "…",
        );

        // Split the truncated text back so title and description keep their own
        // colours.
        let (title, desc) = match text.split_once(" · ") {
            Some((title, desc)) => (title.to_string(), Some(desc.to_string())),
            None => (text, None),
        };

        let mut spans = vec![
            Span::styled(marker.to_string(), self.styles.title),
            Span::styled(number, self.styles.dim),
            Span::styled(title, title_style),
        ];
        if let Some(desc) = desc {
            spans.push(Span::styled(" · ", self.styles.dim));
            spans.push(Span::styled(desc, desc_style));
        }
        if let Some(badge) = badge.filter(|_| badge_space > 0) {
            let used: usize = spans.iter().map(|span| display_width(&span.content)).sum();
            let gap = (width as usize).saturating_sub(used + badge_width);
            spans.push(Span::styled(" ".repeat(gap), Style::default()));
            spans.push(Span::styled(badge.to_string(), self.styles.tip));
        }

        Line::from(spans)
    }

    /// A masked text field with a `_` cursor while it is active.
    fn secret_line(&self, value: &str, active: bool) -> Line<'static> {
        let (text, style) = if value.is_empty() {
            ("paste your API key here…".to_string(), self.styles.dim)
        } else {
            (
                mask_secret(value),
                if active {
                    self.styles.text.add_modifier(Modifier::BOLD)
                } else {
                    self.styles.text
                },
            )
        };
        let mut spans = vec![
            Span::styled("  ", Style::default()),
            Span::styled(text, style),
        ];
        if active {
            spans.push(Span::styled("_", self.styles.title));
        }
        Line::from(spans)
    }

    /// An unmasked text field with a `_` cursor while it is active.
    fn plain_field_line(&self, value: &str, active: bool) -> Line<'static> {
        let (text, style) = if value.is_empty() {
            (
                "https://your-openai-compatible-endpoint/v1".to_string(),
                self.styles.dim,
            )
        } else {
            (
                value.to_string(),
                if active {
                    self.styles.text.add_modifier(Modifier::BOLD)
                } else {
                    self.styles.text
                },
            )
        };
        let mut spans = vec![
            Span::styled("  ", Style::default()),
            Span::styled(text, style),
        ];
        if active {
            spans.push(Span::styled("_", self.styles.title));
        }
        Line::from(spans)
    }
}

/// Whether a key press is plain typing (Shift allowed) rather than a shortcut,
/// so Ctrl/Alt combinations are never inserted as literal characters.
fn plain_text_key(key: &KeyEvent) -> bool {
    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    solaris_tui::util::rect_contains(rect, x, y)
}

/// Look up the catalogue entry a provider id belongs to.
pub fn provider_spec(id: &str) -> Option<&'static ProviderSpec> {
    solaris_core::provider(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn new_flow() -> ConnectFlow {
        ConnectFlow::new(&Theme::dark())
    }

    fn rendered(flow: &mut ConnectFlow, width: u16, height: u16) -> String {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        flow.render(&mut buffer, area);
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    #[test]
    fn arrows_move_and_enter_picks_the_highlighted_row() {
        let mut flow = new_flow();
        assert_eq!(flow.on_key(key(KeyCode::Down)), ConnectOutcome::Handled);

        assert_eq!(
            flow.on_key(key(KeyCode::Enter)),
            ConnectOutcome::ProviderPicked {
                id: "claude-subscription".into(),
                name: "Claude subscription".into(),
            }
        );
    }

    #[test]
    fn selection_wraps_around_both_ends() {
        let mut flow = new_flow();
        flow.on_key(key(KeyCode::Up));
        assert_eq!(
            flow.on_key(key(KeyCode::Enter)),
            ConnectOutcome::ProviderPicked {
                id: "custom".into(),
                name: "Custom endpoint".into(),
            },
            "up from the first row should land on the last"
        );

        let mut flow = new_flow();
        flow.on_key(key(KeyCode::End));
        flow.on_key(key(KeyCode::Down));
        assert_eq!(flow.selected, 0, "down from the last row should wrap");
    }

    #[test]
    fn digit_keys_jump_and_confirm() {
        let mut flow = new_flow();
        assert_eq!(
            flow.on_key(key(KeyCode::Char('3'))),
            ConnectOutcome::ProviderPicked {
                id: "openai".into(),
                name: "OpenAI".into(),
            }
        );
    }

    #[test]
    fn zero_is_not_a_shortcut() {
        let mut flow = new_flow();
        assert_eq!(
            flow.on_key(key(KeyCode::Char('0'))),
            ConnectOutcome::Handled
        );
        assert_eq!(flow.selected, 0);
    }

    #[test]
    fn a_digit_beyond_the_list_is_ignored() {
        let mut flow = new_flow();
        let count = flow.item_count();
        let beyond = char::from_digit(count as u32 + 1, 10).expect("digit");
        assert_eq!(
            flow.on_key(key(KeyCode::Char(beyond))),
            ConnectOutcome::Handled
        );
        assert_eq!(flow.selected, 0);
    }

    #[test]
    fn esc_and_ctrl_c_cancel_the_flow() {
        let mut flow = new_flow();
        assert_eq!(flow.on_key(key(KeyCode::Esc)), ConnectOutcome::Closed);

        let mut flow = new_flow();
        assert_eq!(
            flow.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            ConnectOutcome::Closed
        );
    }

    #[test]
    fn the_provider_step_swallows_unbound_keys_instead_of_leaking_them() {
        let mut flow = new_flow();
        assert_eq!(
            flow.on_key(key(KeyCode::Char('x'))),
            ConnectOutcome::Handled
        );
        assert_eq!(flow.on_key(key(KeyCode::F(5))), ConnectOutcome::Handled);
        assert_eq!(flow.step(), ConnectStep::Provider);
    }

    #[test]
    fn the_api_key_step_submits_the_trimmed_value() {
        let mut flow = new_flow();
        flow.enter_api_key("anthropic".into(), "Anthropic".into());
        for ch in "  sk-abc  ".chars() {
            flow.on_key(key(KeyCode::Char(ch)));
        }

        assert_eq!(
            flow.on_key(key(KeyCode::Enter)),
            ConnectOutcome::Submit(ConnectSubmit::ApiKey {
                provider_id: "anthropic".into(),
                provider_name: "Anthropic".into(),
                key: "sk-abc".into(),
            })
        );
    }

    #[test]
    fn the_api_key_step_ignores_enter_while_empty() {
        let mut flow = new_flow();
        flow.enter_api_key("anthropic".into(), "Anthropic".into());
        assert_eq!(flow.on_key(key(KeyCode::Enter)), ConnectOutcome::Handled);
    }

    #[test]
    fn text_fields_ignore_control_characters() {
        let mut flow = new_flow();
        flow.enter_api_key("anthropic".into(), "Anthropic".into());

        // Ctrl+V must not be inserted as a literal 'v'.
        flow.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert!(flow.input.is_empty(), "input was {:?}", flow.input);

        flow.on_key(key(KeyCode::Char('s')));
        flow.on_key(key(KeyCode::Backspace));
        assert!(flow.input.is_empty());
    }

    #[test]
    fn the_custom_provider_requires_a_url_before_submitting() {
        let mut flow = new_flow();
        flow.enter_custom_provider("custom".into(), "Custom endpoint".into(), None);

        assert_eq!(flow.on_key(key(KeyCode::Enter)), ConnectOutcome::Handled);
        assert_eq!(flow.field, 0, "the cursor should sit on the missing URL");

        for ch in "https://example.test/v1".chars() {
            flow.on_key(key(KeyCode::Char(ch)));
        }
        flow.on_key(key(KeyCode::Tab));
        for ch in "sk-key".chars() {
            flow.on_key(key(KeyCode::Char(ch)));
        }

        assert_eq!(
            flow.on_key(key(KeyCode::Enter)),
            ConnectOutcome::Submit(ConnectSubmit::CustomProvider {
                provider_id: "custom".into(),
                provider_name: "Custom endpoint".into(),
                base_url: "https://example.test/v1".into(),
                api_key: "sk-key".into(),
            })
        );
    }

    #[test]
    fn the_custom_provider_prefills_the_stored_url() {
        let mut flow = new_flow();
        flow.enter_custom_provider(
            "custom".into(),
            "Custom endpoint".into(),
            Some("https://stored.test/v1".into()),
        );

        assert_eq!(flow.input, "https://stored.test/v1");
        assert_eq!(flow.field, 0);
        // Tab and arrows both switch fields, wrapping.
        flow.on_key(key(KeyCode::Tab));
        assert_eq!(flow.field, 1);
        flow.on_key(key(KeyCode::Up));
        assert_eq!(flow.field, 0);
    }

    #[test]
    fn device_auth_swallows_keys_until_it_finishes() {
        let mut flow = new_flow();
        flow.enter_device_auth("claude-subscription".into(), "Claude subscription".into());

        assert_eq!(flow.on_key(key(KeyCode::Enter)), ConnectOutcome::Handled);
        assert_eq!(
            flow.on_key(key(KeyCode::Char('x'))),
            ConnectOutcome::Handled
        );

        flow.device_set_code("ABCD-1234".into(), "https://example.test/device".into());
        assert_eq!(flow.on_key(key(KeyCode::Enter)), ConnectOutcome::Handled);

        flow.device_set_success("token-1".into());
        assert_eq!(
            flow.on_key(key(KeyCode::Char(' '))),
            ConnectOutcome::Submit(ConnectSubmit::DeviceAuthToken {
                provider_id: "claude-subscription".into(),
                provider_name: "Claude subscription".into(),
                token: "token-1".into(),
            })
        );
    }

    #[test]
    fn a_device_auth_error_is_dismissed_by_any_key() {
        let mut flow = new_flow();
        flow.enter_device_auth("claude-subscription".into(), "Claude subscription".into());
        flow.device_set_error("timed out".into());

        assert_eq!(flow.on_key(key(KeyCode::Enter)), ConnectOutcome::Closed);
    }

    #[test]
    fn paste_lands_in_the_active_field_only() {
        let mut flow = new_flow();
        // The provider step has no field, so the paste is refused.
        assert!(!flow.insert_paste("sk-leak"));

        flow.enter_api_key("anthropic".into(), "Anthropic".into());
        assert!(flow.insert_paste("sk-pasted"));
        assert_eq!(flow.input, "sk-pasted");

        let mut flow = new_flow();
        flow.enter_custom_provider("custom".into(), "Custom endpoint".into(), None);
        flow.on_key(key(KeyCode::Tab));
        assert!(flow.insert_paste("second"));
        assert_eq!(flow.input, "");
        assert_eq!(flow.input2, "second");
    }

    #[test]
    fn pasted_control_characters_are_stripped() {
        let mut flow = new_flow();
        flow.enter_api_key("anthropic".into(), "Anthropic".into());
        assert!(flow.insert_paste("sk-a\nb\tc"));
        assert_eq!(flow.input, "sk-abc");
    }

    #[test]
    fn clicking_an_option_row_confirms_it() {
        let mut flow = new_flow();
        rendered(&mut flow, 60, 20);

        let row = flow
            .item_rows
            .iter()
            .find(|(_, index)| *index == 2)
            .map(|(row, _)| *row)
            .expect("row for the third provider");

        assert_eq!(
            flow.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4, row)),
            ConnectOutcome::ProviderPicked {
                id: "openai".into(),
                name: "OpenAI".into(),
            }
        );
    }

    #[test]
    fn scrolling_the_wheel_moves_the_highlight() {
        let mut flow = new_flow();
        rendered(&mut flow, 60, 20);

        flow.on_mouse(mouse(MouseEventKind::ScrollDown, 4, 3));
        assert_eq!(flow.selected, 1);
        flow.on_mouse(mouse(MouseEventKind::ScrollUp, 4, 3));
        assert_eq!(flow.selected, 0);

        // Events outside the flow are ignored rather than moving the highlight.
        let outside = mouse(MouseEventKind::ScrollDown, 4, 23);
        assert_eq!(flow.on_mouse(outside), ConnectOutcome::Handled);
        assert_eq!(flow.selected, 0);
    }

    #[test]
    fn the_model_step_picks_a_model() {
        let mut flow = new_flow();
        flow.enter_models(vec![
            SelectItem::new("m-one", "m-one").description("first"),
            SelectItem::new("m-two", "m-two").description("second"),
        ]);

        flow.on_key(key(KeyCode::Down));
        assert_eq!(
            flow.on_key(key(KeyCode::Enter)),
            ConnectOutcome::ModelPicked {
                model_id: "m-two".into()
            }
        );
        assert_eq!(flow.on_key(key(KeyCode::Esc)), ConnectOutcome::Closed);
    }

    #[test]
    fn desired_height_grows_with_the_list_and_is_at_least_min_height() {
        let mut flow = new_flow();
        let height = flow.desired_height(60);
        assert!(
            (12..=20).contains(&height),
            "provider step wanted {height} rows"
        );

        let mut small = new_flow();
        small.enter_api_key("anthropic".into(), "Anthropic".into());
        assert!(
            small.desired_height(60) >= small.min_height(),
            "a text step must not shrink below the minimum"
        );
    }

    #[test]
    fn the_provider_step_renders_the_claude_style_block() {
        let mut flow = new_flow();
        let text = rendered(&mut flow, 70, 18);
        let rows: Vec<&str> = text.lines().collect();

        assert_eq!(rows[0], "Connect");
        assert_eq!(rows[1], "");
        assert!(rows[2].starts_with("solaris can be used with a provider subscription"));
        assert!(text.contains("Select a provider:"));
        assert!(text.contains("❯ 1. Anthropic · Claude models — API key"));
        assert!(text.contains("  2. Claude subscription · Sign in with a Pro or Max plan"));
        assert!(
            text.contains("FREE"),
            "the Groq badge should render:\n{text}"
        );
        assert!(
            text.contains("LOCAL"),
            "the local badge should render:\n{text}"
        );
        // The hint row is pinned to the bottom.
        assert!(
            text.lines().last().unwrap().contains("esc cancel"),
            "{text}"
        );
        assert!(text.contains("1/8"));
    }

    #[test]
    fn a_long_list_scrolls_so_the_highlighted_row_stays_visible() {
        let mut flow = new_flow();
        let mut providers: Vec<SelectItem> = (0..20)
            .map(|i| SelectItem::new(format!("p{i}"), format!("provider {i}")))
            .collect();
        providers.truncate(20);
        flow.providers = providers;

        // Move past the window and check the list followed along.
        for _ in 0..12 {
            flow.on_key(key(KeyCode::Down));
        }
        let text = rendered(&mut flow, 60, 12);
        assert!(text.contains("❯ 13. provider 12"), "{text}");
        assert!(!text.contains("1. provider 0"), "{text}");
        assert!(flow.scroll > 0);
    }

    #[test]
    fn the_api_key_step_masks_all_but_the_last_four_characters() {
        let mut flow = new_flow();
        flow.enter_api_key("anthropic".into(), "Anthropic".into());
        for ch in "sk-1234567890".chars() {
            flow.on_key(key(KeyCode::Char(ch)));
        }

        let text = rendered(&mut flow, 60, 10);
        assert!(text.contains("Connect Anthropic"), "{text}");
        assert!(text.contains("Paste your API key:"), "{text}");
        assert!(
            text.contains(&format!("{}7890_", "\u{2022}".repeat(9))),
            "the key should be masked with a cursor:\n{text}"
        );
        assert!(!text.contains("sk-1234567890"), "the key leaked:\n{text}");
    }

    #[test]
    fn the_empty_api_key_field_shows_a_placeholder() {
        let mut flow = new_flow();
        flow.enter_api_key("anthropic".into(), "Anthropic".into());
        let text = rendered(&mut flow, 60, 10);
        assert!(text.contains("paste your API key here…_"), "{text}");
    }

    #[test]
    fn the_device_auth_step_shows_the_user_code() {
        let mut flow = new_flow();
        flow.enter_device_auth("claude-subscription".into(), "Claude subscription".into());

        let waiting = rendered(&mut flow, 60, 10);
        assert!(waiting.contains("Requesting device code…"), "{waiting}");

        flow.device_set_code("SO-4F7Q-9X2M".into(), "https://example.test/device".into());
        let showing = rendered(&mut flow, 60, 12);
        assert!(showing.contains("Waiting for authorization…"), "{showing}");
        assert!(showing.contains("SO-4F7Q-9X2M"), "{showing}");
        assert!(showing.contains("https://example.test/device"), "{showing}");
        assert!(showing.contains("esc cancel"), "{showing}");

        flow.device_set_success("token".into());
        let done = rendered(&mut flow, 60, 10);
        assert!(
            done.contains("Authorized — press any key to continue"),
            "{done}"
        );
        // Once authorized the hint no longer offers a cancel.
        assert!(!done.contains("esc cancel"), "{done}");
    }

    #[test]
    fn the_custom_endpoint_step_renders_both_fields() {
        let mut flow = new_flow();
        flow.enter_custom_provider("custom".into(), "Custom endpoint".into(), None);
        let text = rendered(&mut flow, 70, 12);

        assert!(text.contains("Connect Custom endpoint"), "{text}");
        assert!(text.contains("Endpoint URL:"), "{text}");
        assert!(
            text.contains("https://your-openai-compatible-endpoint/v1_"),
            "{text}"
        );
        assert!(text.contains("API key (optional):"), "{text}");
        assert!(text.contains("tab switch field"), "{text}");
    }

    #[test]
    fn a_narrow_area_is_not_painted_out_of_bounds() {
        let mut flow = new_flow();
        // Tiny and degenerate sizes must simply draw less.
        for (width, height) in [(1, 1), (8, 2), (12, 3), (30, 4)] {
            let _ = rendered(&mut flow, width, height);
        }
    }

    #[test]
    fn the_title_names_the_provider_being_connected() {
        let mut flow = new_flow();
        flow.enter_api_key("openai".into(), "OpenAI".into());
        assert!(rendered(&mut flow, 60, 8).contains("Connect OpenAI"));
    }
}
