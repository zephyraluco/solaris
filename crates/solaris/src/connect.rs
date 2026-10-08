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
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use solaris_provider::PROVIDERS;
use solaris_provider::mask_secret;
use solaris_tui::components::inline_select::{InlineSelect, InlineSelectOutcome, InlineStyles};
use solaris_tui::components::select_list::SelectItem;
use solaris_tui::theme::Theme;

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
/// Absolute or block-relative rows to `(row, entry index)` pairs, used for
/// mouse hit-testing.
type RowMap = Vec<(usize, usize)>;

/// The `/connect` wizard.
pub struct ConnectFlow {
    step: ConnectStep,
    styles: InlineStyles,

    // the two inline pickers, provider and model
    providers: InlineSelect,
    models: InlineSelect,

    // the provider being set up
    provider_id: String,
    provider_name: String,

    // text entry: `input` is the API key or the endpoint URL, `input2` is the
    // custom endpoint's key, `field` selects between multi-field steps.
    input: String,
    input2: String,
    field: usize,
    /// Per field: whether it still holds exactly what `/connect` opened it
    /// with. A paste replaces such a field whole instead of being appended to
    /// a secret the user never touched; any edit clears the flag.
    seeded: [bool; 2],

    // device auth
    device_status: DeviceAuthStatus,
    user_code: String,
    verification_uri: String,

    // render state
    last_area: Rect,
    field_rows: Vec<(u16, usize)>,
}

impl ConnectFlow {
    /// Step 1 — the provider picker, built from the catalogue.
    /// Step 1 — the provider picker, built from the catalogue.
    pub fn new(theme: &Theme) -> Self {
        let styles = InlineStyles::from_theme(theme);
        let items = PROVIDERS
            .iter()
            .map(|spec| {
                let item = SelectItem::new(spec.id, spec.name).description(spec.description);
                match spec.badge {
                    Some(badge) => item.badge(badge),
                    None => item,
                }
            })
            .collect();

        let providers = InlineSelect::new(styles, "Connect", "Select a provider:", items).with_note(
            "solaris can be used with a provider subscription or billed based on API usage through \
             an API key.",
        );

        Self {
            step: ConnectStep::Provider,
            styles,
            providers,
            models: InlineSelect::new(styles, "Connect", "Select a model:", Vec::new()),
            provider_id: String::new(),
            provider_name: String::new(),
            input: String::new(),
            input2: String::new(),
            field: 0,
            seeded: [false, false],
            device_status: DeviceAuthStatus::Idle,
            user_code: String::new(),
            verification_uri: String::new(),
            last_area: Rect::default(),
            field_rows: Vec::new(),
        }
    }

    /// Repaint with another theme's palette.
    pub fn set_theme(&mut self, theme: &Theme) {
        self.styles = InlineStyles::from_theme(theme);
        self.providers.set_styles(self.styles);
        self.models.set_styles(self.styles);
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
    ///
    /// A key already stored for the provider pre-fills the field, masked, so
    /// reconnecting shows what is saved instead of a blank one.
    pub fn enter_api_key(
        &mut self,
        provider_id: String,
        provider_name: String,
        current_key: Option<String>,
    ) {
        self.enter_text_step(ConnectStep::ApiKey, provider_id, provider_name);
        self.input = current_key.unwrap_or_default();
        self.seeded[0] = !self.input.is_empty();
    }

    /// Step 2b — collect an endpoint URL and an optional key.
    ///
    /// Both fields pre-fill from what is already stored for the provider.
    pub fn enter_custom_provider(
        &mut self,
        provider_id: String,
        provider_name: String,
        current_url: Option<String>,
        current_key: Option<String>,
    ) {
        self.enter_text_step(ConnectStep::CustomProvider, provider_id, provider_name);
        self.input = current_url.unwrap_or_default();
        self.input2 = current_key.unwrap_or_default();
        self.seeded = [!self.input.is_empty(), !self.input2.is_empty()];
    }

    /// Step 2c — device-code OAuth for a provider.
    pub fn enter_device_auth(&mut self, provider_id: String, provider_name: String) {
        self.enter_text_step(ConnectStep::DeviceAuth, provider_id, provider_name);
        self.device_status = DeviceAuthStatus::WaitingForCode;
    }

    /// Step 3 — pick a model from the provider that just connected.
    ///
    /// The list may still be empty: a gateway or a custom endpoint publishes
    /// its models over the wire rather than in the catalogue, and that answer
    /// arrives from a background task. The step opens anyway and says the list
    /// is on its way; calling this again with the reported list fills it in.
    /// `current` is highlighted when it is on the list, the way `/model` does.
    pub fn enter_models(&mut self, models: Vec<SelectItem>, current: Option<&str>) {
        self.step = ConnectStep::Model;

        let mut picker = InlineSelect::new(
            self.styles,
            format!("Connect {}", self.provider_name),
            "Select a model:",
            models,
        );
        if picker.is_empty() {
            picker = picker.with_note(format!("Asking {} what it offers…", self.provider_name));
        }
        if let Some(current) = current.filter(|name| !name.is_empty()) {
            picker.select_value(current);
        }
        self.models = picker;
    }

    fn enter_text_step(&mut self, step: ConnectStep, provider_id: String, provider_name: String) {
        self.step = step;
        self.provider_id = provider_id;
        self.provider_name = provider_name;
        self.input.clear();
        self.input2.clear();
        self.field = 0;
        self.seeded = [false, false];
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
    /// The model highlighted on the model step.
    pub fn selected_model(&self) -> Option<&str> {
        self.models.selected_value()
    }
    /// drops the paste instead of leaking it into the prompt behind.
    pub fn insert_paste(&mut self, data: &str) -> bool {
        if self.active_input_mut().is_none() {
            return false;
        }

        // A field still holding what it was opened with is replaced whole: a
        // pasted key or URL is a new value, not a suffix for a stored one.
        let untouched = std::mem::take(&mut self.seeded[self.field]);
        let cleaned: String = data.chars().filter(|c| !c.is_control()).collect();
        if let Some(input) = self.active_input_mut() {
            if untouched {
                input.clear();
            }
            input.push_str(&cleaned);
        }
        true
    }

    // -- navigation --------------------------------------------------------

    fn field_count(&self) -> usize {
        match self.step {
            ConnectStep::CustomProvider => 2,
            _ => 0,
        }
    }

    /// Move the highlight, wrapping around both ends like the pickers do.
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
        self.seeded[self.field] = false;
        if let Some(input) = self.active_input_mut() {
            input.pop();
        }
    }

    /// Append a typed character to the active field.
    ///
    /// Typing edits whatever is there — a stored secret included — so the field
    /// stops counting as untouched and a later paste appends to it.
    fn type_char(&mut self, c: char) {
        self.seeded[self.field] = false;
        if let Some(input) = self.active_input_mut() {
            input.push(c);
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

        // Ctrl+U kills the active field, matching the editor's kill-to-start:
        // the way out of a field that opened holding a stored secret.
        if key.code == KeyCode::Char('u') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.seeded[self.field] = false;
            if let Some(input) = self.active_input_mut() {
                input.clear();
            }
            return ConnectOutcome::Handled;
        }

        match self.step {
            ConnectStep::Provider => match self.providers.on_key(key) {
                InlineSelectOutcome::Picked(index) => self.confirm_provider(index),
                InlineSelectOutcome::Handled => ConnectOutcome::Handled,
            },
            ConnectStep::Model => match self.models.on_key(key) {
                InlineSelectOutcome::Picked(_) => self.confirm_model(),
                InlineSelectOutcome::Handled => ConnectOutcome::Handled,
            },
            ConnectStep::ApiKey => self.api_key_key(key),
            ConnectStep::CustomProvider => self.custom_provider_key(key),
            ConnectStep::DeviceAuth => self.device_auth_key(),
        }
    }
    fn confirm_provider(&mut self, index: usize) -> ConnectOutcome {
        let Some(item) = self.providers.items().get(index) else {
            return ConnectOutcome::Handled;
        };
        ConnectOutcome::ProviderPicked {
            id: item.value.clone(),
            name: item.label.clone(),
        }
    }

    fn confirm_model(&mut self) -> ConnectOutcome {
        match self.models.selected_value() {
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
                self.type_char(c);
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
                self.type_char(c);
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
        match self.step {
            ConnectStep::Provider => match self.providers.on_mouse(mouse) {
                InlineSelectOutcome::Picked(index) => self.confirm_provider(index),
                InlineSelectOutcome::Handled => ConnectOutcome::Handled,
            },
            ConnectStep::Model => match self.models.on_mouse(mouse) {
                InlineSelectOutcome::Picked(_) => self.confirm_model(),
                InlineSelectOutcome::Handled => ConnectOutcome::Handled,
            },
            // The field and device steps only take a click on a field.
            _ => {
                if !rect_contains(self.last_area, mouse.column, mouse.row) {
                    return ConnectOutcome::Handled;
                }
                if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                    if let Some(field) = self.field_at_row(mouse.row) {
                        self.field = field;
                    }
                }
                ConnectOutcome::Handled
            }
        }
    }

    fn field_at_row(&self, row: u16) -> Option<usize> {
        self.field_rows
            .iter()
            .find(|(r, _)| *r == row)
            .map(|(_, index)| *index)
    }

    // -- layout ------------------------------------------------------------

    /// Rows the fixed part of the block occupies at `width`.
    /// Rows the field and device steps occupy at `width`.
    fn fixed_rows(&self, width: u16) -> u16 {
        self.field_block(width).0.len() as u16
    }

    /// Total rows (block plus hint) the wizard wants at `width`.
    pub fn desired_height(&self, width: u16) -> u16 {
        match self.step {
            // The list steps are exactly their selector.
            ConnectStep::Provider => self.providers.desired_height(width),
            ConnectStep::Model => self.models.desired_height(width),
            _ => self.fixed_rows(width) + FOOTER_ROWS,
        }
    }
    /// Smallest height the wizard can still be used in.
    pub fn min_height(&self) -> u16 {
        4
    }

    fn title(&self) -> String {
        format!("Connect {}", self.provider_name)
    }

    fn question(&self) -> String {
        match self.step {
            ConnectStep::ApiKey => "Paste your API key:".to_string(),
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
        // The list steps draw their own hint inside the selector.
        if matches!(self.step, ConnectStep::Provider | ConnectStep::Model) {
            return Line::from("");
        }

        let hints: Vec<&str> = match self.step {
            ConnectStep::ApiKey => vec!["enter confirm", "ctrl+u clear"],
            ConnectStep::CustomProvider => {
                vec!["tab switch field", "enter confirm", "ctrl+u clear"]
            }
            ConnectStep::DeviceAuth => match self.device_status {
                DeviceAuthStatus::Success(_) | DeviceAuthStatus::Error(_) => {
                    vec!["any key continue"]
                }
                _ => Vec::new(),
            },
            ConnectStep::Provider | ConnectStep::Model => Vec::new(),
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

        Line::from(spans)
    }
    /// Draw the wizard into `area` (the prompt region of the layout).
    pub fn render(&mut self, buf: &mut Buffer, area: Rect) {
        self.last_area = area;
        self.field_rows.clear();

        if area.width == 0 || area.height < 2 {
            return;
        }

        match self.step {
            ConnectStep::Provider => {
                self.providers.render(buf, area);
                return;
            }
            ConnectStep::Model => {
                self.models.render(buf, area);
                return;
            }
            _ => {}
        }

        let width = area.width;
        // The hint row is pinned to the bottom so a squeezed layout still tells
        // the user how to get out.
        let body_area = Rect {
            height: area.height - FOOTER_ROWS,
            ..area
        };
        let hint_area = Rect {
            y: area.y + area.height - FOOTER_ROWS,
            height: FOOTER_ROWS,
            ..area
        };

        let (lines, field_rows) = self.field_block(width);
        for (row, line) in lines.iter().take(body_area.height as usize).enumerate() {
            buf.set_line(body_area.x, body_area.y + row as u16, line, body_area.width);
        }
        buf.set_line(hint_area.x, hint_area.y, &self.hint_line(), hint_area.width);

        // Hit-testing works on absolute rows, so translate what the block
        // recorded.
        self.field_rows = field_rows
            .into_iter()
            .map(|(row, index)| (body_area.y.saturating_add(row as u16), index))
            .collect();
    }

    /// The block the field and device steps draw, with row indices relative to
    /// its first line. The list steps are drawn by their selector instead.
    fn field_block(&self, width: u16) -> (Vec<Line<'static>>, RowMap) {
        let _ = width;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut field_rows: RowMap = Vec::new();

        lines.push(Line::from(Span::styled(self.title(), self.styles.title)));

        match self.step {
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
            // The list steps never reach here.
            ConnectStep::Provider | ConnectStep::Model => {}
        }

        (lines, field_rows)
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
        assert_eq!(
            flow.on_key(key(KeyCode::Enter)),
            ConnectOutcome::ProviderPicked {
                id: "anthropic".into(),
                name: "Anthropic".into(),
            },
            "down from the last row should wrap to the first"
        );
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
        assert_eq!(
            flow.on_key(key(KeyCode::Enter)),
            ConnectOutcome::ProviderPicked {
                id: "anthropic".into(),
                name: "Anthropic".into(),
            },
            "zero must leave the highlight on the first row"
        );
    }

    #[test]
    fn a_digit_beyond_the_list_is_ignored() {
        let mut flow = new_flow();
        // The catalogue holds eight providers, so nine is out of range.
        assert_eq!(
            flow.on_key(key(KeyCode::Char('9'))),
            ConnectOutcome::Handled
        );
        assert_eq!(
            flow.on_key(key(KeyCode::Enter)),
            ConnectOutcome::ProviderPicked {
                id: "anthropic".into(),
                name: "Anthropic".into(),
            },
            "an out-of-range digit must leave the highlight alone"
        );
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
        flow.enter_api_key("anthropic".into(), "Anthropic".into(), None);
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
        flow.enter_api_key("anthropic".into(), "Anthropic".into(), None);
        assert_eq!(flow.on_key(key(KeyCode::Enter)), ConnectOutcome::Handled);
    }

    #[test]
    fn text_fields_ignore_control_characters() {
        let mut flow = new_flow();
        flow.enter_api_key("anthropic".into(), "Anthropic".into(), None);

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
        flow.enter_custom_provider("custom".into(), "Custom endpoint".into(), None, None);

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
    fn the_custom_provider_prefills_the_stored_url_and_key() {
        let mut flow = new_flow();
        flow.enter_custom_provider(
            "custom".into(),
            "Custom endpoint".into(),
            Some("https://stored.test/v1".into()),
            Some("sk-stored-9876".into()),
        );

        assert_eq!(flow.input, "https://stored.test/v1");
        assert_eq!(flow.input2, "sk-stored-9876");
        assert_eq!(flow.field, 0);
        // Tab and arrows both switch fields, wrapping.
        flow.on_key(key(KeyCode::Tab));
        assert_eq!(flow.field, 1);
        flow.on_key(key(KeyCode::Up));
        assert_eq!(flow.field, 0);

        // The stored key shows masked, and only masked.
        let text = rendered(&mut flow, 70, 12);
        assert!(
            text.contains(&format!("{}9876", "\u{2022}".repeat(10))),
            "{text}"
        );
        assert!(!text.contains("sk-stored-9876"), "the key leaked:\n{text}");
    }

    #[test]
    fn the_api_key_step_prefills_the_stored_key() {
        let mut flow = new_flow();
        flow.enter_api_key(
            "anthropic".into(),
            "Anthropic".into(),
            Some("sk-live-1234".into()),
        );

        let text = rendered(&mut flow, 60, 10);
        assert!(
            text.contains(&format!("{}1234", "\u{2022}".repeat(8))),
            "the stored key should be masked:\n{text}"
        );
        assert!(!text.contains("sk-live-1234"), "the key leaked:\n{text}");

        // Confirming the untouched field resubmits what was already stored,
        // rather than an empty key that would wipe it.
        assert_eq!(
            flow.on_key(key(KeyCode::Enter)),
            ConnectOutcome::Submit(ConnectSubmit::ApiKey {
                provider_id: "anthropic".into(),
                provider_name: "Anthropic".into(),
                key: "sk-live-1234".into(),
            })
        );
    }

    #[test]
    fn a_paste_replaces_an_untouched_prefilled_field() {
        let mut flow = new_flow();
        flow.enter_custom_provider(
            "custom".into(),
            "Custom endpoint".into(),
            Some("https://stored.test/v1".into()),
            Some("sk-stored-9876".into()),
        );

        assert!(flow.insert_paste("https://new.test/v1"));
        assert_eq!(flow.input, "https://new.test/v1");
        // A second paste lands in a field that has been edited, so it appends.
        assert!(flow.insert_paste("/v2"));
        assert_eq!(flow.input, "https://new.test/v1/v2");

        flow.on_key(key(KeyCode::Tab));
        assert!(flow.insert_paste("sk-new"));
        assert_eq!(flow.input2, "sk-new");

        // Typing counts as editing too, so the field is no longer untouched.
        flow.on_key(key(KeyCode::Char('x')));
        assert!(flow.insert_paste("-tail"));
        assert_eq!(flow.input2, "sk-newx-tail");
    }

    #[test]
    fn ctrl_u_clears_the_active_field_only() {
        let mut flow = new_flow();
        flow.enter_custom_provider(
            "custom".into(),
            "Custom endpoint".into(),
            Some("https://stored.test/v1".into()),
            Some("sk-stored-9876".into()),
        );

        flow.on_key(key(KeyCode::Tab));
        flow.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(flow.input, "https://stored.test/v1");
        assert_eq!(flow.input2, "");

        // A new key can be typed straight into the emptied field.
        for ch in "sk-new".chars() {
            flow.on_key(key(KeyCode::Char(ch)));
        }
        assert_eq!(flow.input2, "sk-new");
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

        flow.enter_api_key("anthropic".into(), "Anthropic".into(), None);
        assert!(flow.insert_paste("sk-pasted"));
        assert_eq!(flow.input, "sk-pasted");

        let mut flow = new_flow();
        flow.enter_custom_provider("custom".into(), "Custom endpoint".into(), None, None);
        flow.on_key(key(KeyCode::Tab));
        assert!(flow.insert_paste("second"));
        assert_eq!(flow.input, "");
        assert_eq!(flow.input2, "second");
    }

    #[test]
    fn pasted_control_characters_are_stripped() {
        let mut flow = new_flow();
        flow.enter_api_key("anthropic".into(), "Anthropic".into(), None);
        assert!(flow.insert_paste("sk-a\nb\tc"));
        assert_eq!(flow.input, "sk-abc");
    }

    #[test]
    fn clicking_an_option_row_confirms_it() {
        let mut flow = new_flow();
        let text = rendered(&mut flow, 60, 20);

        let row = text
            .lines()
            .position(|line| line.contains("OpenAI"))
            .expect("the OpenAI row") as u16;

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
        assert_eq!(
            flow.on_key(key(KeyCode::Enter)),
            ConnectOutcome::ProviderPicked {
                id: "claude-subscription".into(),
                name: "Claude subscription".into(),
            },
            "the wheel should move the highlight"
        );

        // Events outside the flow are ignored rather than moving the highlight.
        flow.on_mouse(mouse(MouseEventKind::ScrollUp, 4, 3));
        let outside = mouse(MouseEventKind::ScrollDown, 4, 23);
        assert_eq!(flow.on_mouse(outside), ConnectOutcome::Handled);
        assert_eq!(
            flow.on_key(key(KeyCode::Enter)),
            ConnectOutcome::ProviderPicked {
                id: "anthropic".into(),
                name: "Anthropic".into(),
            },
            "a wheel event outside the flow must not move the highlight"
        );
    }

    #[test]
    fn the_model_step_picks_a_model() {
        let mut flow = new_flow();
        flow.enter_models(
            vec![
                SelectItem::new("m-one", "m-one").description("first"),
                SelectItem::new("m-two", "m-two").description("second"),
            ],
            None,
        );

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
    fn the_model_step_opens_before_the_provider_has_answered() {
        let mut flow = new_flow();
        flow.enter_custom_provider(
            "new-api".into(),
            "New API".into(),
            Some("https://gateway.test/v1".into()),
            None,
        );
        flow.enter_models(Vec::new(), None);

        // The step is up even though there is nothing to pick yet, and it says
        // why — parking the user on a bare question would read as a dead end.
        let text = rendered(&mut flow, 70, 18);
        assert!(text.contains("Select a model:"), "{text}");
        assert!(text.contains("Asking New API"), "{text}");

        // The answer fills the same step in, with the model already in use as
        // the highlighted row so Enter keeps it.
        flow.enter_models(
            vec![
                SelectItem::new("m-one", "m-one"),
                SelectItem::new("m-two", "m-two"),
            ],
            Some("m-two"),
        );

        let text = rendered(&mut flow, 70, 18);
        assert!(!text.contains("Asking New API"), "{text}");
        assert_eq!(
            flow.on_key(key(KeyCode::Enter)),
            ConnectOutcome::ModelPicked {
                model_id: "m-two".into()
            }
        );
    }

    #[test]
    fn desired_height_grows_with_the_list_and_is_at_least_min_height() {
        let flow = new_flow();
        let height = flow.desired_height(60);
        assert!(
            (12..=20).contains(&height),
            "provider step wanted {height} rows"
        );

        let mut small = new_flow();
        small.enter_api_key("anthropic".into(), "Anthropic".into(), None);
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
            text.contains("5. New API · Self-hosted OpenAI-compatible gateway"),
            "the gateway row should render:\n{text}"
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
    fn the_api_key_step_masks_all_but_the_last_four_characters() {
        let mut flow = new_flow();
        flow.enter_api_key("anthropic".into(), "Anthropic".into(), None);
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
        flow.enter_api_key("anthropic".into(), "Anthropic".into(), None);
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
        flow.enter_custom_provider("custom".into(), "Custom endpoint".into(), None, None);
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
        flow.enter_api_key("openai".into(), "OpenAI".into(), None);
        assert!(rendered(&mut flow, 60, 8).contains("Connect OpenAI"));
    }
}
