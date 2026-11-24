//! Shared expansion of user input before it enters any conversation queue.

use std::path::PathBuf;

use crate::Skills;

/// Why a skill command could not be expanded.
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    /// The requested name is absent from the current resource snapshot.
    #[error("no skill named `{0}` — /skills lists what there is")]
    UnknownSkill(String),
    /// The skill was removed or cannot be read.
    #[error("could not read skill {}: {source}", path.display())]
    Read {
        /// File being read.
        path: PathBuf,
        /// Filesystem error.
        source: std::io::Error,
    },
    /// The file is no longer a valid skill document.
    #[error("invalid skill {}: {message}", path.display())]
    Invalid {
        /// File being parsed.
        path: PathBuf,
        /// Parser or validation diagnostic.
        message: String,
    },
}

/// Expand one skill invocation (`@<name> [args]` or legacy `/skill:<name> [args]`),
/// leaving ordinary text untouched.
///
/// Expansion is deliberately non-recursive: instructions are content, not input
/// to the command dispatcher. Hidden skills remain explicitly invokable.
pub fn expand_input(text: &str, skills: &Skills) -> Result<String, InputError> {
    let trimmed = text.trim();
    if let Some(mention) = trimmed.strip_prefix('@') {
        let (name, args) = mention
            .split_once(char::is_whitespace)
            .unwrap_or((mention, ""));
        if !name.is_empty()
            && let Some(skill) = skills.get(name)
        {
            return skill.read_invocation(args);
        }
        return Ok(text.to_string());
    }
    let Some(command) = trimmed.strip_prefix("/skill:") else {
        return Ok(text.to_string());
    };
    let (name, args) = command
        .split_once(char::is_whitespace)
        .unwrap_or((command, ""));
    let skill = skills
        .get(name)
        .ok_or_else(|| InputError::UnknownSkill(name.to_string()))?;
    skill.read_invocation(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SkillRoot, SkillSource};

    #[test]
    fn expands_at_skill_mention_and_leaves_ordinary_mentions_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pdf-tools").join("SKILL.md");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &path,
            "---\ndescription: Extract PDFs.\n---\nRead the PDF.\n",
        )
        .expect("write");
        let skills = Skills::load(&[SkillRoot::new(dir.path(), SkillSource::Project)], &[]);

        let expanded = expand_input("@pdf-tools extract report.pdf", &skills).expect("expand");
        assert!(expanded.contains("Read the PDF."), "{expanded}");
        assert!(expanded.contains("extract report.pdf"), "{expanded}");

        let untouched = expand_input("@someone hello", &skills).expect("passthrough");
        assert_eq!(untouched, "@someone hello");
    }
}
