//! Application configuration.

use serde::{Deserialize, Serialize};

/// Agent operating mode; drives the accent colour and backend behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// Model identifier.
    ///
    /// Empty means no model has been named yet, so the connected provider is
    /// asked for the first model it offers. Anything set here — by `--model`,
    /// `/model` or the `/connect` wizard — is sent exactly as written.
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
            model: String::new(),
            theme: "dark".to_string(),
            mode: Mode::default(),
            context_window: 128_000,
        }
    }
}

/// What a session starts from: the theme and the mode the last one left.
///
/// Both fields are optional, so one nobody has set — or one an older build never
/// wrote — simply means "no preference" rather than making the file unreadable.
/// The model in use is deliberately absent: it belongs to a provider, and is
/// remembered in `auth.json` beside the provider it was chosen for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preferences {
    /// Theme name, as `/theme` set it.
    #[serde(default)]
    pub theme: Option<String>,
    /// Agent mode, as Tab left it.
    #[serde(default)]
    pub mode: Option<Mode>,
}

impl Preferences {
    /// Encode for `settings.json`.
    pub fn to_json(&self) -> Result<String, ConfigError> {
        serde_json::to_string_pretty(self).map_err(|error| ConfigError::Encode(error.to_string()))
    }

    /// Parse previously saved preferences.
    pub fn from_json(text: &str) -> Result<Self, ConfigError> {
        serde_json::from_str(text).map_err(|error| ConfigError::Decode(error.to_string()))
    }
}

/// Why remembered preferences could not be read or written.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("could not encode the preferences: {0}")]
    Encode(String),
    #[error("could not read the preferences: {0}")]
    Decode(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_survive_a_round_trip() {
        let stored = Preferences {
            theme: Some("light".to_string()),
            mode: Some(Mode::Plan),
        };

        let json = stored.to_json().expect("encode");
        assert_eq!(Preferences::from_json(&json).expect("decode"), stored);
    }

    #[test]
    fn an_empty_file_means_no_preference_rather_than_an_error() {
        let stored = Preferences::from_json("{}").expect("decode");
        assert_eq!(stored, Preferences::default());
        assert_eq!(stored.theme, None);
        assert_eq!(stored.mode, None);
    }
}
