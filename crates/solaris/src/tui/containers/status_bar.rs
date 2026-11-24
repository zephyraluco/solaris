//! Status bar rendered at the bottom of [`PromptContainer`](super::PromptContainer).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Gauge, Widget};
use solaris_agent::{ContextUsage, Mode};
use solaris_tui::component::Component;
use solaris_tui::theme::Theme;
use solaris_tui::util::{display_width, truncate_to_width};

use crate::tui::keymap;
use crate::tui::state::NoticeKind;

const CONTEXT_GAUGE_WIDTH: u16 = 10;

/// Read-only view model synced into [`StatusBar`] before each frame.
#[derive(Debug, Clone, Default)]
pub(crate) struct StatusBarState {
    pub(crate) label: String,
    pub(crate) model: String,
    pub(crate) mode: Mode,
    pub(crate) context: ContextUsage,
    pub(crate) notice: Option<(NoticeKind, String)>,
}

/// Single-row footer displaying backend/model/mode, context gauge, and notices.
pub(crate) struct StatusBar {
    state: StatusBarState,
    theme: Theme,
}

impl StatusBar {
    pub(crate) fn new(theme: &Theme) -> Self {
        Self {
            state: StatusBarState::default(),
            theme: theme.clone(),
        }
    }

    pub(crate) fn sync(&mut self, state: StatusBarState) {
        self.state = state;
    }

    pub(crate) fn set_theme(&mut self, theme: &Theme) {
        self.theme = theme.clone();
    }
}

impl Component for StatusBar {
    fn desired_height(&mut self, _width: u16) -> Option<u16> {
        Some(1)
    }

    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let visible_notice = self
            .state
            .notice
            .as_ref()
            .filter(|(kind, _)| *kind != NoticeKind::Error);
        let (right, style) = if let Some((kind, text)) = visible_notice {
            let style = match kind {
                NoticeKind::Warning => Style::default().fg(self.theme.warning),
                NoticeKind::Info | NoticeKind::Error => Style::default().fg(self.theme.muted),
            };
            (format!("{} {} ", kind.marker(), text), style)
        } else {
            (
                format!("{} ", keymap::FOOTER_HINT),
                Style::default().fg(self.theme.dim),
            )
        };

        // The right slot wins the space it needs; the left one is truncated to
        // whatever remains so the two never overlap.
        let right_width = display_width(&right) as u16;
        let show_right = visible_notice.is_some() || right_width + 4 < area.width;
        let left_budget = if show_right {
            area.width.saturating_sub(right_width.min(area.width))
        } else {
            area.width
        };

        // The model slot disappears while no model is named, so the footer
        // never shows an empty gap between two separators.
        let muted = Style::default().fg(self.theme.muted);
        let (ratio, percent, bar_color) = context_metrics(self.state.context, &self.theme);
        let mut prefix_facts = vec![self.state.label.clone()];
        if !self.state.model.is_empty() {
            prefix_facts.push(self.state.model.clone());
        }
        prefix_facts.push(self.state.mode.label().to_string());
        let prefix = format!(" {} · 上下文 ", prefix_facts.join(" · "));
        let suffix = format!(" {percent}% ");
        let prefix_width = display_width(&prefix) as u16;
        let suffix_width = display_width(&suffix) as u16;

        if prefix_width
            .saturating_add(CONTEXT_GAUGE_WIDTH)
            .saturating_add(suffix_width)
            <= left_budget
        {
            buf.set_string(area.x, area.y, &prefix, muted);
            let gauge_x = area.x + prefix_width;
            let gauge_area = Rect::new(gauge_x, area.y, CONTEXT_GAUGE_WIDTH, 1);
            Gauge::default()
                .ratio(ratio)
                .label("")
                .style(Style::default().bg(self.theme.surface))
                .gauge_style(Style::default().fg(bar_color).bg(self.theme.surface))
                .use_unicode(true)
                .render(gauge_area, buf);
            buf.set_string(
                gauge_x + CONTEXT_GAUGE_WIDTH,
                area.y,
                &suffix,
                Style::default().fg(bar_color),
            );
        } else {
            let fallback = format!("{prefix}{percent}% ");
            buf.set_string(
                area.x,
                area.y,
                truncate_to_width(&fallback, left_budget as usize, "…"),
                muted,
            );
        }

        if show_right {
            let width = right_width.min(area.width);
            buf.set_string(
                area.x + area.width - width,
                area.y,
                truncate_to_width(&right, width as usize, "…"),
                style,
            );
        }
    }
}

fn context_metrics(context: ContextUsage, theme: &Theme) -> (f64, u32, ratatui::style::Color) {
    let raw_ratio = if context.window == 0 {
        0.0
    } else {
        f64::from(context.tokens.unwrap_or(0)) / f64::from(context.window)
    };
    let percent = (raw_ratio * 100.0).round().max(0.0) as u32;
    let color = if percent >= 90 {
        theme.error
    } else if percent >= 70 {
        theme.warning
    } else {
        theme.success
    };
    (raw_ratio.clamp(0.0, 1.0), percent, color)
}
