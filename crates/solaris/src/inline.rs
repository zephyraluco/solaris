//! The inline surfaces that own the prompt region.
//!
//! `/connect` and `/model` both take the input line over rather than opening an
//! overlay, but they are different animals — a wizard and a picker — so they
//! live here behind one type and `App` only has to hold an `Option<Inline>`.

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use solaris_tui::components::select_list::SelectItem;
use solaris_tui::theme::Theme;

use crate::connect::{ConnectFlow, ConnectOutcome, ConnectStep};
use crate::model::{ModelOutcome, ModelPicker};

/// Which inline surface owns the prompt region.
///
/// A command that picks or asks takes the input line over — the way claurst's
/// wizard and Claude Code's `/login` screen do — instead of opening an overlay.
/// Both surfaces draw through the framework's `InlineSelect`, so only their
/// policy differs; `App` keeps them behind one type and dispatches here.
// The wizard is a big state machine and the picker is a thin one; boxing the
// former would only add indirection to a value that lives for one screen.
#[expect(clippy::large_enum_variant)]
pub(crate) enum Inline {
    /// The `/connect` wizard.
    Connect(ConnectFlow),
    /// The `/model` picker.
    Model(ModelPicker),
}

/// What an inline surface made of an event.
pub(crate) enum InlineOutcome {
    Connect(ConnectOutcome),
    Model(ModelOutcome),
}

impl Inline {
    pub(crate) fn is_connect(&self) -> bool {
        matches!(self, Self::Connect(_))
    }

    pub(crate) fn step(&self) -> Option<ConnectStep> {
        match self {
            Self::Connect(flow) => Some(flow.step()),
            Self::Model(_) => None,
        }
    }

    pub(crate) fn area(&self) -> Rect {
        match self {
            Self::Connect(flow) => flow.area(),
            Self::Model(picker) => picker.area(),
        }
    }

    pub(crate) fn set_theme(&mut self, theme: &Theme) {
        match self {
            Self::Connect(flow) => flow.set_theme(theme),
            Self::Model(picker) => picker.set_theme(theme),
        }
    }

    pub(crate) fn insert_paste(&mut self, text: &str) -> bool {
        match self {
            Self::Connect(flow) => flow.insert_paste(text),
            Self::Model(picker) => picker.insert_paste(text),
        }
    }

    pub(crate) fn on_key(&mut self, key: KeyEvent) -> InlineOutcome {
        match self {
            Self::Connect(flow) => InlineOutcome::Connect(flow.on_key(key)),
            Self::Model(picker) => InlineOutcome::Model(picker.on_key(key)),
        }
    }

    pub(crate) fn on_mouse(&mut self, mouse: MouseEvent) -> InlineOutcome {
        match self {
            Self::Connect(flow) => InlineOutcome::Connect(flow.on_mouse(mouse)),
            Self::Model(picker) => InlineOutcome::Model(picker.on_mouse(mouse)),
        }
    }

    pub(crate) fn desired_height(&mut self, width: u16) -> u16 {
        match self {
            Self::Connect(flow) => flow.desired_height(width),
            Self::Model(picker) => picker.desired_height(width),
        }
    }

    pub(crate) fn render(&mut self, buf: &mut Buffer, area: Rect) {
        match self {
            Self::Connect(flow) => flow.render(buf, area),
            Self::Model(picker) => picker.render(buf, area),
        }
    }

    // Wizard-only steps: no-ops unless the connect flow is the one on screen.
    pub(crate) fn enter_api_key(
        &mut self,
        provider_id: impl Into<String>,
        provider_name: impl Into<String>,
        current_key: Option<String>,
    ) {
        if let Self::Connect(flow) = self {
            flow.enter_api_key(provider_id.into(), provider_name.into(), current_key);
        }
    }

    pub(crate) fn enter_custom_provider(
        &mut self,
        provider_id: impl Into<String>,
        provider_name: impl Into<String>,
        current_url: Option<String>,
        current_key: Option<String>,
    ) {
        if let Self::Connect(flow) = self {
            flow.enter_custom_provider(
                provider_id.into(),
                provider_name.into(),
                current_url,
                current_key,
            );
        }
    }

    pub(crate) fn enter_models(&mut self, models: Vec<SelectItem>, current: Option<&str>) {
        if let Self::Connect(flow) = self {
            flow.enter_models(models, current);
        }
    }
}
