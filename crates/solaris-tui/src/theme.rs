//! Colour palette used by solaris-tui components and the application.

use ratatui::style::Color;

/// A named colour palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    /// Window background.
    pub bg: Color,
    /// Raised panel/dialog surface, used by the flat dialog chrome.
    pub surface: Color,
    /// Default text.
    pub fg: Color,
    /// Secondary text.
    pub muted: Color,
    /// Disabled / hint text.
    pub dim: Color,
    /// Default accent.
    pub accent: Color,
    /// Accent while in build mode.
    pub accent_build: Color,
    /// Accent while in plan mode.
    pub accent_plan: Color,
    /// Panel and dialog borders.
    pub border: Color,
    /// User message text.
    pub user: Color,
    /// Assistant message text.
    pub assistant: Color,
    /// System annotation text.
    pub system: Color,
    pub error: Color,
    pub warning: Color,
    pub success: Color,
    pub selection_bg: Color,
    pub selection_fg: Color,
    pub code_bg: Color,
    pub heading: Color,
}

impl Theme {
    /// Names accepted by [`Theme::by_name`].
    pub const NAMES: &'static [&'static str] = &["dark", "light"];

    /// Dark palette (the default).
    pub fn dark() -> Self {
        Self {
            name: "dark".to_string(),
            bg: Color::Rgb(24, 24, 30),
            surface: Color::Rgb(33, 34, 45),
            fg: Color::Rgb(220, 220, 228),
            muted: Color::Rgb(150, 152, 165),
            dim: Color::Rgb(108, 110, 122),
            accent: Color::Rgb(120, 170, 255),
            accent_build: Color::Rgb(233, 30, 99),
            accent_plan: Color::Rgb(66, 135, 245),
            border: Color::Rgb(78, 80, 98),
            user: Color::Rgb(150, 220, 180),
            assistant: Color::Rgb(230, 230, 238),
            system: Color::Rgb(140, 142, 155),
            error: Color::Rgb(255, 105, 105),
            warning: Color::Rgb(240, 190, 90),
            success: Color::Rgb(120, 210, 140),
            selection_bg: Color::Rgb(58, 62, 84),
            selection_fg: Color::Rgb(240, 240, 250),
            code_bg: Color::Rgb(36, 38, 52),
            heading: Color::Rgb(170, 200, 255),
        }
    }

    /// Light palette.
    pub fn light() -> Self {
        Self {
            name: "light".to_string(),
            bg: Color::Rgb(248, 248, 250),
            surface: Color::Rgb(255, 255, 255),
            fg: Color::Rgb(32, 34, 42),
            muted: Color::Rgb(96, 100, 112),
            dim: Color::Rgb(140, 144, 156),
            accent: Color::Rgb(40, 100, 220),
            accent_build: Color::Rgb(200, 30, 90),
            accent_plan: Color::Rgb(40, 100, 220),
            border: Color::Rgb(180, 184, 196),
            user: Color::Rgb(20, 120, 80),
            assistant: Color::Rgb(40, 42, 52),
            system: Color::Rgb(110, 114, 126),
            error: Color::Rgb(200, 40, 40),
            warning: Color::Rgb(170, 120, 20),
            success: Color::Rgb(30, 140, 70),
            selection_bg: Color::Rgb(210, 222, 245),
            selection_fg: Color::Rgb(24, 26, 34),
            code_bg: Color::Rgb(232, 234, 240),
            heading: Color::Rgb(30, 80, 190),
        }
    }

    /// Look up a palette by name, falling back to dark.
    pub fn by_name(name: &str) -> Self {
        match name {
            "light" => Self::light(),
            _ => Self::dark(),
        }
    }

    /// Cycle to the next palette name.
    pub fn cycle_name(name: &str) -> &'static str {
        let idx = Self::NAMES.iter().position(|n| *n == name).unwrap_or(0);
        Self::NAMES[(idx + 1) % Self::NAMES.len()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_theme_resolves_and_is_self_consistent() {
        for name in Theme::NAMES {
            let theme = Theme::by_name(name);
            assert_eq!(&theme.name, name);
        }
    }

    #[test]
    fn unknown_theme_falls_back_to_dark() {
        assert_eq!(Theme::by_name("nope").name, "dark");
    }

    #[test]
    fn surface_is_distinct_from_the_window_background() {
        for name in Theme::NAMES {
            let theme = Theme::by_name(name);
            assert_ne!(
                theme.surface, theme.bg,
                "{name}: a borderless panel would be invisible"
            );
        }
    }

    #[test]
    fn theme_names_cycle() {
        assert_eq!(Theme::cycle_name("dark"), "light");
        assert_eq!(Theme::cycle_name("light"), "dark");
    }
}
