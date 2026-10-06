//! Application configuration.

/// Agent operating mode; drives the accent colour and backend behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Build,
    Plan,
}

impl Mode {
    /// Uppercase badge label shown in the status bar.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Build => "BUILD",
            Mode::Plan => "PLAN",
        }
    }

    /// Cycle to the other mode (Tab).
    pub fn next(self) -> Self {
        match self {
            Mode::Build => Mode::Plan,
            Mode::Plan => Mode::Build,
        }
    }
}

/// User-visible configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Model identifier; the mock backend echoes it back.
    pub model: String,
    /// Theme name (`dark` or `light`).
    pub theme: String,
    /// Active agent mode.
    pub mode: Mode,
    /// Context window size in tokens, used by the footer gauge.
    pub context_window: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "solaris-mock-1".to_string(),
            theme: "dark".to_string(),
            mode: Mode::default(),
            context_window: 128_000,
        }
    }
}

impl Config {
    /// Theme names the theme dialog offers.
    /// Models the model dialog offers: name plus the one-line description shown
    /// after the ` · ` separator in the picker.
    pub const MODEL_OPTIONS: &'static [(&'static str, &'static str)] = &[
        ("solaris-mock-1", "balanced mock replies"),
        ("solaris-mock-1-mini", "shorter, faster replies"),
        ("solaris-mock-reason", "longer thinking trace"),
    ];
}
