//! Platform paths and persisted startup settings.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use solaris_provider::AuthStore;

use crate::{Preferences, RecentActivity, Soul};

/// Where solaris keeps its state: `$SOLARIS_CONFIG_DIR` when set, otherwise the
/// platform's per-user configuration directory.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SOLARIS_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }

    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);

    #[cfg(not(target_os = "windows"))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));

    base.map(|dir| dir.join("solaris"))
}

/// The user's home directory, which is where the shared skills location is.
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let home = std::env::var_os("USERPROFILE");

    #[cfg(not(target_os = "windows"))]
    let home = std::env::var_os("HOME");

    home.map(PathBuf::from)
}

/// Missing credentials start an unconnected session; existing files must be readable and valid.
pub fn load_auth(path: Option<&Path>) -> Result<AuthStore> {
    let Some(path) = path else {
        return Ok(AuthStore::new());
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(AuthStore::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot read credential store {}", path.display()));
        }
    };
    AuthStore::from_json(&text)
        .with_context(|| format!("invalid credential store {}", path.display()))
}

/// The companion's saved name, if it has one.
pub fn load_soul(path: Option<&Path>) -> Option<Soul> {
    let path = path?;
    let text = std::fs::read_to_string(path).ok()?;
    Soul::from_json(&text).ok()
}

/// Read the recorded prompts, treating a missing or corrupt file as empty.
pub fn load_recent(path: Option<&Path>) -> RecentActivity {
    let Some(path) = path else {
        return RecentActivity::new();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| RecentActivity::from_json(&text).ok())
        .unwrap_or_default()
}

/// The preferences saved last time, or none when there is no usable record.
pub fn load_settings(path: Option<&Path>) -> Preferences {
    let Some(path) = path else {
        return Preferences::default();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| Preferences::from_json(&text).ok())
        .unwrap_or_default()
}

/// The current user: names the companion's roll and the welcome greeting.
pub fn user_id() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "solaris".to_string())
}

/// The greeting line for the welcome box.
pub fn greeting(user: &str) -> String {
    if user == "solaris" {
        "Welcome back!".to_string()
    } else {
        format!("Welcome back {user}!")
    }
}

#[cfg(test)]
mod credential_loading_tests {
    use super::*;
    #[test]
    fn existing_invalid_credentials_are_not_silently_discarded() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        let original =
            r#"{"entries":{"anthropic":{"kind":"api_key","key":"test-key"}},"active":"anthropic"}"#;
        std::fs::write(&path, original).unwrap();
        assert!(load_auth(Some(&path)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(
            load_auth(Some(directory.path())).is_err(),
            "unreadable files must not become empty stores"
        );
        assert_eq!(
            load_auth(Some(&directory.path().join("missing.json")))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(load_auth(None).unwrap().len(), 0);
    }
}
