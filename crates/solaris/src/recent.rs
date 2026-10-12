//! Recent activity for the welcome box.
//!
//! A tiny record of the prompts a session has started, persisted next to the
//! credentials so the box can show what you were doing last time. Kept pure:
//! reading and writing the file is the binary's job, exactly like the credential
//! store in `solaris-provider`.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How many entries the box shows (and how many are kept on disk).
pub const MAX_ENTRIES: usize = 5;
/// Longest label kept, so one enormous prompt cannot fill the column.
const MAX_LABEL_CHARS: usize = 48;

/// One recorded prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentEntry {
    pub label: String,
    pub at_ms: u64,
}

/// The most recent prompts, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentActivity {
    entries: Vec<RecentEntry>,
}

impl RecentActivity {
    /// An empty history.
    pub fn new() -> Self {
        Self::default()
    }

    /// The recorded entries, newest first.
    pub fn entries(&self) -> &[RecentEntry] {
        &self.entries
    }

    /// Whether anything has been recorded.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many entries are recorded.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Record `label` at `at_ms`.
    ///
    /// Repeating the newest label refreshes its time instead of duplicating it
    /// (pressing enter twice on the same prompt is one activity), and the list
    /// is capped at [`MAX_ENTRIES`].
    pub fn record(&mut self, label: &str, at_ms: u64) {
        let label = sanitize(label);
        if label.is_empty() {
            return;
        }

        match self.entries.first_mut() {
            Some(newest) if newest.label == label => newest.at_ms = at_ms,
            _ => self.entries.insert(0, RecentEntry { label, at_ms }),
        }
        self.entries.truncate(MAX_ENTRIES);
    }

    /// Label and relative time for each entry, newest first.
    pub fn rows(&self, now_ms: u64) -> Vec<(String, String)> {
        self.entries
            .iter()
            .map(|entry| (entry.label.clone(), relative_time(entry.at_ms, now_ms)))
            .collect()
    }

    /// Serialize for `recent.json`.
    pub fn to_json(&self) -> Result<String, RecentError> {
        serde_json::to_string_pretty(self).map_err(|error| RecentError::Encode(error.to_string()))
    }

    /// Parse a previously saved history.
    pub fn from_json(text: &str) -> Result<Self, RecentError> {
        serde_json::from_str(text).map_err(|error| RecentError::Decode(error.to_string()))
    }
}

/// Why a recent-activity file could not be read or written.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RecentError {
    #[error("could not encode recent activity: {0}")]
    Encode(String),
    #[error("could not read recent activity: {0}")]
    Decode(String),
}

/// Wall-clock milliseconds since the Unix epoch (0 if the clock is before it).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Short relative time, in the buckets the welcome box uses.
pub fn relative_time(at_ms: u64, now_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(at_ms) / 1_000;
    match seconds {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// Collapse a prompt into a single trimmed label.
fn sanitize(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let mut label: String = line.chars().take(MAX_LABEL_CHARS).collect();
    if line.chars().count() > MAX_LABEL_CHARS {
        label.push('…');
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_newest_first_and_capped() {
        let mut recent = RecentActivity::new();
        for index in 0..MAX_ENTRIES + 3 {
            recent.record(&format!("prompt {index}"), index as u64);
        }

        assert_eq!(recent.len(), MAX_ENTRIES);
        let labels: Vec<&str> = recent
            .entries()
            .iter()
            .map(|entry| entry.label.as_str())
            .collect();
        assert_eq!(labels[0], "prompt 7");
        assert!(!labels.contains(&"prompt 0"), "{labels:?}");
    }

    #[test]
    fn repeating_the_newest_prompt_refreshes_it() {
        let mut recent = RecentActivity::new();
        recent.record("hello", 10);
        recent.record("hello", 20);

        assert_eq!(recent.len(), 1);
        assert_eq!(recent.entries()[0].at_ms, 20);
    }

    #[test]
    fn labels_are_single_line_and_bounded() {
        let mut recent = RecentActivity::new();
        recent.record("\n\n  fix the parser  \nand the lexer", 1);
        assert_eq!(recent.entries()[0].label, "fix the parser");

        recent.record("   ", 2);
        assert_eq!(recent.len(), 1, "blank prompts record nothing");

        recent.record(&"x".repeat(200), 3);
        let label = &recent.entries()[0].label;
        assert_eq!(label.chars().count(), MAX_LABEL_CHARS + 1, "{label}");
        assert!(label.ends_with('…'));
    }

    #[test]
    fn relative_times_use_the_expected_buckets() {
        let now = 100_000_000_000u64;
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now - 59_000, now), "just now");
        assert_eq!(relative_time(now - 60_000, now), "1m ago");
        assert_eq!(relative_time(now - 3_600_000, now), "1h ago");
        assert_eq!(relative_time(now - 86_400_000, now), "1d ago");
        // A clock that jumped backwards never produces a negative age.
        assert_eq!(relative_time(now + 5_000, now), "just now");
    }

    #[test]
    fn rows_pair_each_label_with_its_age() {
        let now = 10_000_000u64;
        let mut recent = RecentActivity::new();
        recent.record("older", now - 120_000);
        recent.record("newer", now - 10_000);

        assert_eq!(
            recent.rows(now),
            vec![
                ("newer".to_string(), "just now".to_string()),
                ("older".to_string(), "2m ago".to_string()),
            ]
        );
    }

    #[test]
    fn history_round_trips_through_json() {
        let mut recent = RecentActivity::new();
        recent.record("first", 1);
        recent.record("second", 2);

        let json = recent.to_json().expect("encode");
        assert_eq!(RecentActivity::from_json(&json).expect("decode"), recent);

        let error = RecentActivity::from_json("[]").expect_err("not an object");
        assert!(matches!(error, RecentError::Decode(_)), "{error:?}");
    }

    #[test]
    fn the_clock_returns_a_plausible_timestamp() {
        // Later than 2020-01-01 and earlier than 2100.
        let now = now_ms();
        assert!(
            (1_577_836_800_000..4_102_444_800_000).contains(&now),
            "{now}"
        );
    }
}
