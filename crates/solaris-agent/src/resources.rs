//! Minimal, reloadable filesystem resources shared by the application and agent.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::{SkillRoot, SkillSource, SkillWarning, Skills};

/// An instruction file with its source retained for the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptFile {
    /// Absolute source path.
    pub path: PathBuf,
    /// Instructions read from the file.
    pub content: String,
}

/// User and project instructions appended to the built-in prompt.
#[derive(Debug, Clone, Default)]
pub struct PromptContext {
    /// Global instructions followed by ancestor and working-directory rules.
    pub project_rules: Vec<PromptFile>,
    /// User APPEND_SYSTEM.md followed by project APPEND_SYSTEM.md.
    pub append_system: Vec<PromptFile>,
}

/// One consistent view of the resources advertised to the model and user.
#[derive(Debug, Clone, Default)]
pub struct ResourceSnapshot {
    /// Discovered skills, including explicitly invoked ones.
    pub skills: Skills,
    /// Instructions used to build the system prompt.
    pub prompt: PromptContext,
    /// Nonfatal instruction-file diagnostics. Skill diagnostics live in skills.
    pub warnings: Vec<SkillWarning>,
}

/// Locations to load at startup and again on /reload.
#[derive(Debug, Clone)]
pub struct ResourcePaths {
    /// Working directory; all relative configured paths resolve here.
    pub cwd: PathBuf,
    /// Solaris user configuration directory.
    pub config_dir: Option<PathBuf>,
    /// Home containing the shared .agents/skills directory.
    pub home_dir: Option<PathBuf>,
    /// Additional skill files or directories, in CLI order.
    pub extra_skills: Vec<PathBuf>,
}

impl ResourcePaths {
    /// Load a new snapshot. A missing working directory is a structural failure;
    /// individual bad resource files instead produce diagnostics.
    pub fn load(&self) -> std::io::Result<ResourceSnapshot> {
        let cwd = std::path::absolute(&self.cwd)?;
        // Do not replace a good snapshot with an empty one after losing access
        // to the directory the session is meant to operate in.
        std::fs::read_dir(&cwd)?;
        let config = self
            .config_dir
            .as_deref()
            .map(|path| absolute_from(&cwd, path));
        let ancestors = project_ancestors(&cwd);
        let mut roots = Vec::new();
        if let Some(dir) = &config {
            roots.push(SkillRoot::new(dir.join("skills"), SkillSource::User));
        }
        if let Some(home) = &self.home_dir {
            roots.push(SkillRoot::new(
                absolute_from(&cwd, home).join(".agents/skills"),
                SkillSource::User,
            ));
        }
        roots.push(SkillRoot::new(
            cwd.join(".solaris/skills"),
            SkillSource::Project,
        ));
        // Nearest project definition wins; global skills keep their precedence.
        roots.extend(
            ancestors
                .iter()
                .map(|dir| SkillRoot::new(dir.join(".agents/skills"), SkillSource::Project)),
        );
        let extra: Vec<_> = self
            .extra_skills
            .iter()
            .map(|path| absolute_from(&cwd, path))
            .collect();
        let mut snapshot = ResourceSnapshot {
            skills: Skills::load(&roots, &extra),
            ..Default::default()
        };

        let mut seen = BTreeSet::new();
        if let Some(dir) = &config {
            load_rule(
                dir,
                &mut snapshot.prompt.project_rules,
                &mut seen,
                &mut snapshot.warnings,
            );
        }
        for dir in ancestors.iter().rev() {
            load_rule(
                dir,
                &mut snapshot.prompt.project_rules,
                &mut seen,
                &mut snapshot.warnings,
            );
        }
        // Addenda are a distinct layer, with independent source deduplication.
        seen.clear();
        if let Some(dir) = &config {
            load_file(
                &dir.join("APPEND_SYSTEM.md"),
                &mut snapshot.prompt.append_system,
                &mut seen,
                &mut snapshot.warnings,
            );
        }
        load_file(
            &cwd.join(".solaris/APPEND_SYSTEM.md"),
            &mut snapshot.prompt.append_system,
            &mut seen,
            &mut snapshot.warnings,
        );
        Ok(snapshot)
    }
}

fn absolute_from(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Nearest directory first, stopping at the first repository marker. A .git
/// file counts too, for linked worktrees and submodules.
fn project_ancestors(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for dir in cwd.ancestors() {
        dirs.push(dir.to_path_buf());
        if dir.join(".git").exists() {
            break;
        }
    }
    dirs
}

fn load_rule(
    dir: &Path,
    files: &mut Vec<PromptFile>,
    seen: &mut BTreeSet<PathBuf>,
    warnings: &mut Vec<SkillWarning>,
) {
    for name in ["AGENTS.override.md", "AGENTS.md", "CLAUDE.md"] {
        let path = dir.join(name);
        if path.exists() {
            load_file(&path, files, seen, warnings);
            break;
        }
    }
}

fn load_file(
    path: &Path,
    files: &mut Vec<PromptFile>,
    seen: &mut BTreeSet<PathBuf>,
    warnings: &mut Vec<SkillWarning>,
) {
    if !path.exists() {
        return;
    }
    match std::fs::canonicalize(path).and_then(|real| {
        if seen.contains(&real) {
            return Ok(None);
        }
        let content = std::fs::read_to_string(path)?;
        seen.insert(real);
        Ok(Some(content))
    }) {
        Ok(Some(content)) => files.push(PromptFile {
            path: path.to_path_buf(),
            content: content
                .strip_prefix('\u{feff}')
                .unwrap_or(&content)
                .to_string(),
        }),
        Ok(None) => {}
        Err(error) => warnings.push(SkillWarning {
            path: path.to_path_buf(),
            message: format!("could not read instructions: {error}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subdirectory_inherits_rules_and_nearest_skills_without_crossing_repo_root() {
        let root = std::env::temp_dir().join(format!("solaris-resources-{}", std::process::id()));
        let repo = root.join("repo");
        let cwd = repo.join("src");
        let user = root.join("user");
        for dir in [
            &cwd,
            &user,
            &repo.join(".git"),
            &repo.join(".agents/skills/test"),
            &cwd.join(".agents/skills/test"),
            &cwd.join(".solaris"),
        ] {
            std::fs::create_dir_all(dir).expect("directory");
        }
        for (path, text) in [
            (root.join("AGENTS.md"), "outside"),
            (user.join("AGENTS.md"), "global"),
            (repo.join("AGENTS.md"), "repo"),
            (cwd.join("AGENTS.override.md"), "override"),
            (cwd.join("AGENTS.md"), "shadowed"),
            (user.join("APPEND_SYSTEM.md"), "user addendum"),
            (cwd.join(".solaris/APPEND_SYSTEM.md"), "project addendum"),
            (
                repo.join(".agents/skills/test/SKILL.md"),
                "---\ndescription: ancestor\n---\nOld",
            ),
            (
                cwd.join(".agents/skills/test/SKILL.md"),
                "---\ndescription: nearest\n---\nNew",
            ),
        ] {
            std::fs::write(path, text).expect("file");
        }
        let paths = ResourcePaths {
            cwd,
            config_dir: Some(user),
            home_dir: None,
            extra_skills: Vec::new(),
        };
        let snapshot = paths.load().expect("resources");
        assert_eq!(
            snapshot
                .prompt
                .project_rules
                .iter()
                .map(|file| file.content.as_str())
                .collect::<Vec<_>>(),
            ["global", "repo", "override"]
        );
        assert_eq!(
            snapshot
                .prompt
                .append_system
                .iter()
                .map(|file| file.content.as_str())
                .collect::<Vec<_>>(),
            ["user addendum", "project addendum"]
        );
        assert_eq!(
            snapshot.skills.get("test").expect("skill").description(),
            "nearest"
        );
        std::fs::write(paths.cwd.join("AGENTS.override.md"), "updated").expect("update");
        assert_eq!(
            paths
                .load()
                .expect("reload")
                .prompt
                .project_rules
                .last()
                .expect("rule")
                .content,
            "updated"
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
