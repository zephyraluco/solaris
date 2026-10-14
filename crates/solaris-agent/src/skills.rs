//! Skills: instructions a turn is told about, and reads only when it needs them.
//!
//! A skill is a directory holding a `SKILL.md` — frontmatter saying what the
//! skill is and when it applies, then the instructions themselves — often with
//! scripts, references and assets beside it. Reading every one of them into the
//! system prompt would spend context on work the session may never do, so this
//! module does what [pi](https://github.com/earendil-works/pi) does and splits
//! the two halves:
//!
//! - [`Skills::prompt_section`] lists each skill's name, description and file,
//!   which is what lets a model decide a skill applies at all;
//! - the model then reads that file — with the `read` tool, or whatever plays
//!   its part — and gets the instructions, and
//!   [`Skill::invocation_prompt`] is the same thing asked for by name.
//!
//! The format is the [Agent Skills](https://agentskills.io/specification)
//! specification, so a skill written for another harness works here. Two things
//! this module deliberately is not: it does not fail a session over a bad skill,
//! and it does not fail a skill over a bad name. Everything it cannot use is
//! reported as a [`SkillWarning`] and left behind.
//!
//! Frontmatter uses a YAML parser so portable skill files retain their meaning.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The file that makes a directory a skill.
const SKILL_FILE: &str = "SKILL.md";

/// Longest name the specification allows.
const MAX_NAME_LENGTH: usize = 64;

/// Longest description the specification allows.
const MAX_DESCRIPTION_LENGTH: usize = 1024;

/// Which of the configured places a skill came from.
///
/// Only reported: precedence is decided by the order the roots are given in, not
/// by this, because the caller is the one that knows which of its directories
/// matters most.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    /// A directory belonging to the user, shared by every session.
    User,
    /// A directory belonging to the project being worked in.
    Project,
    /// A path the caller named explicitly.
    Path,
}

impl SkillSource {
    /// How the source is named to a reader.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Path => "path",
        }
    }

    /// The one-letter badge a one-line listing shows it by.
    ///
    /// One letter because the badge sits in a list of invocations, where the
    /// name is what the eye is looking for; `u`ser and `p`roject are the two the
    /// specification knows, and a skill named on the command line is `e`xplicit.
    pub fn badge(self) -> char {
        match self {
            Self::User => 'u',
            Self::Project => 'p',
            Self::Path => 'e',
        }
    }
}

/// A directory to scan for skills, and where its skills count as coming from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRoot {
    /// The directory. A missing one is not an error, it simply has no skills.
    pub dir: PathBuf,
    /// What skills found under it report as their source.
    pub source: SkillSource,
}

impl SkillRoot {
    /// A root at `dir`, whose skills come from `source`.
    pub fn new(dir: impl Into<PathBuf>, source: SkillSource) -> Self {
        Self {
            dir: dir.into(),
            source,
        }
    }
}

/// A problem found while looking for skills.
///
/// A warning is never fatal: the skill it belongs to is either loaded anyway or
/// left out, and either way the session goes on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillWarning {
    /// The file the problem is about.
    pub path: PathBuf,
    /// What is wrong, phrased for whoever is reading the terminal.
    pub message: String,
}

/// One skill, as discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// The name it is called by.
    name: String,
    /// What it does and when it applies, which is what routes the model to it.
    description: String,
    /// The `SKILL.md` it was read from.
    path: PathBuf,
    /// The directory holding it, which its relative paths resolve against.
    base_dir: PathBuf,
    /// Where it was found.
    source: SkillSource,
    /// Whether it is kept out of the model's view of what exists.
    disable_model_invocation: bool,
    /// The instructions, with the frontmatter taken off.
    instructions: String,
}

impl Skill {
    /// The name it is called by.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What it does and when it applies.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The `SKILL.md` it was read from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory holding it.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Where it was found.
    pub fn source(&self) -> SkillSource {
        self.source
    }

    /// Whether the model is kept from finding it on its own.
    ///
    /// A skill that declares `disable-model-invocation: true` is not listed in
    /// the system prompt, so nothing but `/skill:<name>` reaches it.
    pub fn disable_model_invocation(&self) -> bool {
        self.disable_model_invocation
    }

    /// The instructions, with the frontmatter taken off.
    pub fn instructions(&self) -> &str {
        &self.instructions
    }

    /// The prompt that loads this skill by name, with `args` as its request.
    ///
    /// This is what `/skill:<name> [args]` sends: the whole point of the command
    /// is to reach the instructions without waiting for the model to decide it
    /// wants them.
    pub fn invocation_prompt(&self, args: &str) -> String {
        let mut prompt = format!(
            "Use the `{}` skill.\nSkill file: {}\nResolve relative references against: {}\n\nIts instructions follow:\n\n{}\n",
            self.name,
            self.path.display(),
            self.base_dir.display(),
            self.instructions.trim()
        );
        let args = args.trim();
        if !args.is_empty() {
            prompt.push_str("\nRequest: ");
            prompt.push_str(args);
            prompt.push('\n');
        }
        prompt
    }

    /// Read the current instructions, rather than invoking a stale cached body.
    pub fn read_invocation(&self, args: &str) -> Result<String, crate::InputError> {
        let text =
            std::fs::read_to_string(&self.path).map_err(|source| crate::InputError::Read {
                path: self.path.clone(),
                source,
            })?;
        let (frontmatter, instructions) =
            parse_document(&text).map_err(|message| crate::InputError::Invalid {
                path: self.path.clone(),
                message,
            })?;
        if frontmatter
            .description
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            return Err(crate::InputError::Invalid {
                path: self.path.clone(),
                message: "description is required".to_string(),
            });
        }
        let current = Self {
            instructions,
            ..self.clone()
        };
        Ok(current.invocation_prompt(args))
    }

    /// The description as one line.
    ///
    /// Frontmatter may write a description over several lines, which suits a
    /// bullet in the system prompt no better than a row in a list of commands.
    pub fn description_inline(&self) -> String {
        self.description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Every skill a session found, and everything it could not use.
#[derive(Debug, Clone, Default)]
pub struct Skills {
    skills: Vec<Skill>,
    warnings: Vec<SkillWarning>,
}

impl Skills {
    /// A session with no skills, which is what a caller that found none has.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Discover skills under `roots`, plus anything `extra` names.
    ///
    /// Roots are read in order and the first skill to claim a name keeps it, so
    /// the caller's order is the precedence. An entry in `extra` may be a
    /// directory or a Markdown file; one that is neither, or that does not
    /// exist, is reported rather than ignored.
    pub fn load(roots: &[SkillRoot], extra: &[PathBuf]) -> Self {
        let mut found = Vec::new();
        let mut warnings = Vec::new();

        for root in roots {
            scan(
                &root.dir,
                root.source,
                true,
                &mut BTreeSet::new(),
                &mut found,
                &mut warnings,
            );
        }

        for path in extra {
            if !path.exists() {
                warnings.push(SkillWarning {
                    path: path.clone(),
                    message: "the skill path does not exist".to_string(),
                });
                continue;
            }
            match path.metadata() {
                Ok(metadata) if metadata.is_dir() => {
                    scan(
                        path,
                        SkillSource::Path,
                        true,
                        &mut BTreeSet::new(),
                        &mut found,
                        &mut warnings,
                    );
                }
                Ok(metadata) if metadata.is_file() && is_markdown(path) => {
                    found.push(Found {
                        path: path.clone(),
                        declared: is_skill_file(path),
                        source: SkillSource::Path,
                    });
                }
                Ok(_) => warnings.push(SkillWarning {
                    path: path.clone(),
                    message: "the skill path is neither a directory nor a Markdown file"
                        .to_string(),
                }),
                Err(error) => warnings.push(SkillWarning {
                    path: path.clone(),
                    message: format!("the skill path could not be read: {error}"),
                }),
            }
        }

        Self::collect(found, warnings)
    }

    /// Read every found file, keeping the first skill to claim each name.
    fn collect(found: Vec<Found>, mut warnings: Vec<SkillWarning>) -> Self {
        let mut skills: Vec<Skill> = Vec::new();
        // A skill reachable by two routes, such as a symlinked directory, is one
        // skill: the real path is what says so.
        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();

        for file in found {
            let Ok(real) = file.path.canonicalize() else {
                continue;
            };
            if !seen.insert(real) {
                continue;
            }

            let (skill, mut read) = read_skill(&file.path, file.declared, file.source);
            warnings.append(&mut read);
            let Some(skill) = skill else {
                continue;
            };

            if let Some(existing) = skills.iter().find(|held| held.name == skill.name) {
                warnings.push(SkillWarning {
                    path: skill.path.clone(),
                    message: format!(
                        "the name `{}` is already taken by {} — this one is ignored",
                        skill.name,
                        existing.path.display()
                    ),
                });
                continue;
            }

            skills.push(skill);
        }

        Self { skills, warnings }
    }

    /// Whether no skill was found.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// How many were found, hidden ones included.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Every skill, in discovery order.
    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.iter()
    }

    /// The skill called `name`, for `/skill:<name>`.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    /// Everything that could not be used.
    pub fn warnings(&self) -> &[SkillWarning] {
        &self.warnings
    }

    /// The skills the model is told about.
    pub fn visible(&self) -> impl Iterator<Item = &Skill> {
        self.skills
            .iter()
            .filter(|skill| !skill.disable_model_invocation)
    }

    /// How many skills the model is told about.
    pub fn visible_len(&self) -> usize {
        self.visible().count()
    }

    /// The system prompt's `# Skills` section, or nothing when there is none.
    ///
    /// `reader` names the tool a model should open a skill's file with, when the
    /// turn declares one; `None` leaves the mechanism unsaid, which is all that
    /// can honestly be said when nothing can read a file.
    ///
    /// Only names, descriptions and paths appear here. The instructions
    /// themselves stay in the file until something asks for them, which is the
    /// whole reason a skill is worth having.
    pub fn prompt_section(&self, reader: Option<&str>) -> String {
        let visible: Vec<&Skill> = self.visible().collect();
        if visible.is_empty() {
            return String::new();
        }

        let mut section = String::from("\n# Skills\n\n");
        section.push_str(
            "Specialized instructions for particular kinds of work, listed here but kept out of \
             this prompt until you need them.\n",
        );
        match reader {
            Some(reader) => {
                let _ = writeln!(
                    section,
                    "When a skill's description matches what you are about to do, read its file \
                     with the `{reader}` tool before you start and follow what it says;"
                );
            }
            None => section.push_str(
                "When a skill's description matches what you are about to do, read its file before \
                 you start and follow what it says;",
            ),
        }
        section.push_str(
            " resolve any relative path it mentions against that skill's own directory.\n\n",
        );

        for skill in visible {
            let _ = writeln!(
                section,
                "- `{}`: {} — {}",
                skill.name,
                skill.description_inline(),
                skill.path.display()
            );
        }

        section
    }
}

/// A file worth reading, and whether it announced itself as a skill.
struct Found {
    path: PathBuf,
    /// Whether it was named `SKILL.md`, as opposed to a loose `.md` file.
    declared: bool,
    source: SkillSource,
}

/// Walk `dir`, collecting the skill files under it.
///
/// The shape is the specification's: a directory holding `SKILL.md` is a skill
/// and is not walked into, because everything below it belongs to that skill;
/// `include_root_files` is what lets a loose `.md` file in a configured
/// directory be a skill of its own, and it is only ever true at the top.
fn scan(
    dir: &Path,
    source: SkillSource,
    include_root_files: bool,
    visited: &mut BTreeSet<PathBuf>,
    found: &mut Vec<Found>,
    warnings: &mut Vec<SkillWarning>,
) {
    let Ok(real) = dir.canonicalize() else {
        return;
    };
    if !visited.insert(real) {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            warnings.push(SkillWarning {
                path: dir.to_path_buf(),
                message: format!("could not scan skill directory: {error}"),
            });
            return;
        }
    };
    let mut entries: Vec<std::fs::DirEntry> = entries.filter_map(Result::ok).collect();
    // Read order is whatever the filesystem feels like; sorting is what makes
    // discovery, and so the prompt, the same twice.
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in &entries {
        if entry.file_name().as_os_str() != OsStr::new(SKILL_FILE) {
            continue;
        }
        let path = entry.path();
        // `metadata` rather than `file_type`, so a symlinked SKILL.md counts.
        match path.metadata() {
            Ok(metadata) if metadata.is_file() => found.push(Found {
                path,
                declared: true,
                source,
            }),
            Ok(_) => {}
            Err(error) => warnings.push(SkillWarning {
                path,
                message: format!("the skill file could not be read: {error}"),
            }),
        }
        // Whether or not it was usable, this directory is a skill — or was meant
        // to be — so it is not also a tree of other skills.
        return;
    }

    for entry in &entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }

        let path = entry.path();
        let Ok(metadata) = path.metadata() else {
            // A broken symlink is not a skill and not worth reporting.
            continue;
        };

        if metadata.is_dir() {
            scan(&path, source, false, visited, found, warnings);
        } else if metadata.is_file() && include_root_files && is_markdown(&path) {
            found.push(Found {
                path,
                declared: false,
                source,
            });
        }
    }
}

/// Whether `path` is a Markdown file, by the name a skill file is given.
fn is_markdown(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == OsStr::new("md"))
}

/// Whether `path` is named `SKILL.md`.
fn is_skill_file(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name == OsStr::new(SKILL_FILE))
}

/// Read one candidate file into a skill, or into the reasons it is not one.
fn read_skill(
    path: &Path,
    declared: bool,
    source: SkillSource,
) -> (Option<Skill>, Vec<SkillWarning>) {
    let absolute = match std::path::absolute(path) {
        Ok(path) => path,
        Err(error) => {
            return (
                None,
                vec![SkillWarning {
                    path: path.to_path_buf(),
                    message: format!("could not resolve skill path: {error}"),
                }],
            );
        }
    };
    let path = absolute.as_path();
    let mut warnings = Vec::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        warnings.push(SkillWarning {
            path: path.to_path_buf(),
            message: "the skill file could not be read as text".to_string(),
        });
        return (None, warnings);
    };

    let (frontmatter, instructions) = match parse_document(&text) {
        Ok(document) => document,
        Err(message) => {
            warnings.push(SkillWarning {
                path: path.to_path_buf(),
                message,
            });
            return (None, warnings);
        }
    };

    // A loose Markdown file is only a skill if it says what it does; without a
    // description there is nothing to route a model by, and nothing to gain by
    // reporting a file the user never meant as a skill.
    let description = frontmatter.description.unwrap_or_default();
    if description.is_empty() {
        if declared {
            warnings.push(SkillWarning {
                path: path.to_path_buf(),
                message: "the skill has no description, so nothing can match it".to_string(),
            });
        }
        return (None, warnings);
    }
    if description.chars().count() > MAX_DESCRIPTION_LENGTH {
        warnings.push(SkillWarning {
            path: path.to_path_buf(),
            message: format!("the description is longer than {MAX_DESCRIPTION_LENGTH} characters"),
        });
    }

    // The directory's name is the fallback, which is what lets the common case
    // — a skill in `pdf-tools/` — declare nothing but its description.
    let base_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let name = frontmatter.name.unwrap_or_else(|| {
        // A `SKILL.md` is named for the directory written around it; a loose
        // Markdown file is named for itself, because the directory it sits in is
        // only the one it happened to be found in.
        let fallback = if declared {
            base_dir.file_name()
        } else {
            path.file_stem()
        };
        fallback
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    for problem in validate_name(&name) {
        warnings.push(SkillWarning {
            path: path.to_path_buf(),
            message: problem,
        });
    }

    let skill = Skill {
        name,
        description,
        path: path.to_path_buf(),
        base_dir,
        source,
        disable_model_invocation: frontmatter.disable_model_invocation,
        instructions,
    };
    (Some(skill), warnings)
}

/// The fields of a `SKILL.md` frontmatter block that this crate reads.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(rename = "disable-model-invocation")]
    disable_model_invocation: bool,
}

/// Split a `SKILL.md` into its frontmatter and its instructions.
///
/// A block only counts when the file opens with `---` and a later line is
/// exactly `---`. An opening fence without a closing fence is an error.
fn parse_document(text: &str) -> Result<(Frontmatter, String), String> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let normalized = normalized.strip_prefix('\u{feff}').unwrap_or(&normalized);

    let body = normalized.trim();
    let Some(rest) = normalized.strip_prefix("---\n") else {
        return Ok((Frontmatter::default(), body.to_string()));
    };
    let Some((yaml_end, body_start)) = find_closing_fence(rest) else {
        return Err("frontmatter has no closing --- fence".to_string());
    };

    let yaml = &rest[..yaml_end];
    let mut frontmatter: Frontmatter = if yaml.trim().is_empty() {
        Frontmatter::default()
    } else {
        serde_yaml_ng::from_str(yaml)
            .map_err(|error| format!("invalid YAML frontmatter: {error}"))?
    };
    frontmatter.name = frontmatter.name.and_then(|value| non_empty(&value));
    frontmatter.description = frontmatter.description.and_then(|value| non_empty(&value));
    Ok((frontmatter, rest[body_start..].trim().to_string()))
}

/// Where `rest` stops being frontmatter and starts being instructions.
///
/// Returns the end of the YAML block and the start of the body, which are
/// different offsets because the closing `---` line sits between them.
fn find_closing_fence(rest: &str) -> Option<(usize, usize)> {
    let mut start = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r']).trim() == "---" {
            return Some((start, start + line.len()));
        }
        start += line.len();
    }
    None
}

/// `value` when it has anything in it, so an empty one reads as absent.
fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Why `name` does not fit the specification.
///
/// These are warnings rather than refusals: a name is how a skill is invoked,
/// not what it does, and a skill whose name is merely unconventional still
/// works.
fn validate_name(name: &str) -> Vec<String> {
    let mut problems = Vec::new();

    if name.is_empty() {
        problems.push("the skill has no name".to_string());
    }
    if name.chars().count() > MAX_NAME_LENGTH {
        problems.push(format!(
            "the name is longer than {MAX_NAME_LENGTH} characters"
        ));
    }
    if !name.chars().all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
    }) {
        problems.push("the name should use lowercase letters, digits and hyphens only".to_string());
    }
    if name.starts_with('-') || name.ends_with('-') {
        problems.push("the name should not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        problems.push("the name should not repeat a hyphen".to_string());
    }

    problems
}

/// A skill built by hand, for the tests of the prompt that lists them.
#[cfg(test)]
impl Skill {
    /// A skill with the fields a `# Skills` section shows, and nothing else.
    pub(crate) fn sample(name: &str, description: &str, path: &str) -> Self {
        let path = PathBuf::from(path);
        Self {
            name: name.to_string(),
            description: description.to_string(),
            base_dir: path.parent().map(Path::to_path_buf).unwrap_or_default(),
            path,
            source: SkillSource::Project,
            disable_model_invocation: false,
            instructions: String::new(),
        }
    }
}

/// A set holding exactly `skills`, for a caller that did not have to find them.
#[cfg(test)]
impl Skills {
    /// A set built from `skills`, without touching the filesystem.
    pub(crate) fn from_skills(skills: Vec<Skill>) -> Self {
        Self {
            skills,
            warnings: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh directory under the system temp dir for one test.
    fn test_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "solaris-agent-skills-{}-{tag}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Write `contents` to `dir/relative`, making the directories on the way.
    ///
    /// The relative path is split so the result is spelled the way the scan will
    /// spell it, which is what lets a test compare the two.
    fn write(dir: &Path, relative: &str, contents: &str) -> PathBuf {
        let path = relative
            .split('/')
            .fold(dir.to_path_buf(), |path, part| path.join(part));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent dir");
        }
        std::fs::write(&path, contents).expect("write");
        path
    }

    fn load(dir: &Path) -> Skills {
        Skills::load(&[SkillRoot::new(dir, SkillSource::Project)], &[])
    }

    #[test]
    fn yaml_escapes_are_decoded_and_malformed_documents_are_diagnosed() {
        let dir = test_dir("yaml");
        write(
            &dir,
            "escaped/SKILL.md",
            "---\ndescription: \"Use \\u4E2D\\u6587\"\n---\nBody",
        );
        write(
            &dir,
            "broken/SKILL.md",
            "---\ndescription: [broken\n---\nBody",
        );
        let skills = load(&dir);
        assert_eq!(skills.len(), 1);
        assert_eq!(
            skills.get("escaped").expect("skill").description(),
            "Use 中文"
        );
        assert!(
            skills
                .warnings()
                .iter()
                .any(|warning| warning.message.contains("invalid YAML"))
        );
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn deleted_or_malformed_skill_cannot_invoke_cached_instructions() {
        let dir = test_dir("fresh-body");
        let path = write(&dir, "skill/SKILL.md", "---\ndescription: Work\n---\nBody");
        let skills = load(&dir);
        std::fs::write(&path, "---\ndescription: [broken\n---\nBody").expect("update");
        assert!(matches!(
            crate::expand_input("/skill:skill", &skills),
            Err(crate::InputError::Invalid { .. })
        ));
        std::fs::remove_file(path).expect("remove");
        assert!(matches!(
            crate::expand_input("/skill:skill", &skills),
            Err(crate::InputError::Read { .. })
        ));
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn a_skill_directory_is_read_from_its_frontmatter() {
        let dir = test_dir("basic");
        write(
            &dir,
            "pdf-tools/SKILL.md",
            "---\nname: pdf-tools\ndescription: Extract text from PDFs.\n---\n\n# PDF tools\n\nRead the reference first.\n",
        );

        let skills = load(&dir);
        assert!(skills.warnings().is_empty(), "{:?}", skills.warnings());
        assert_eq!(skills.len(), 1);

        let skill = skills.get("pdf-tools").expect("found");
        assert_eq!(skill.description(), "Extract text from PDFs.");
        assert_eq!(
            skill.instructions(),
            "# PDF tools\n\nRead the reference first."
        );
        assert_eq!(skill.base_dir(), dir.join("pdf-tools"));
        assert_eq!(skill.source(), SkillSource::Project);
        assert!(!skill.disable_model_invocation());
    }

    #[test]
    fn the_directory_name_stands_in_for_a_missing_name() {
        let dir = test_dir("unnamed");
        write(
            &dir,
            "inner/outer/SKILL.md",
            "---\ndescription: Does something useful.\n---\nBody.\n",
        );

        let skills = load(&dir);
        assert!(
            skills.get("outer").is_some(),
            "{:?}",
            skills.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_skill_without_a_description_is_left_out_with_a_warning() {
        let dir = test_dir("nodesc");
        let path = write(&dir, "vague/SKILL.md", "---\nname: vague\n---\nBody.\n");

        let skills = load(&dir);
        assert!(skills.is_empty());
        assert_eq!(skills.warnings().len(), 1);
        assert_eq!(skills.warnings()[0].path, path);
        assert!(skills.warnings()[0].message.contains("no description"));
    }

    #[test]
    fn a_loose_markdown_file_without_a_description_is_skipped_silently() {
        let dir = test_dir("loose");
        write(&dir, "notes.md", "Just some notes.\n");

        let skills = load(&dir);
        assert!(skills.is_empty());
        assert!(skills.warnings().is_empty(), "{:?}", skills.warnings());
    }

    #[test]
    fn a_loose_markdown_file_with_a_description_is_a_skill() {
        let dir = test_dir("looseok");
        write(
            &dir,
            "release.md",
            "---\ndescription: Cut a release.\n---\nSteps.\n",
        );

        let skills = load(&dir);
        assert!(
            skills.get("release").is_some(),
            "{:?}",
            skills.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_directory_holding_a_skill_is_not_walked_into() {
        let dir = test_dir("nested");
        write(
            &dir,
            "tools/SKILL.md",
            "---\ndescription: The toolbelt.\n---\n",
        );
        write(
            &dir,
            "tools/inner/SKILL.md",
            "---\ndescription: Belongs to the toolbelt.\n---\n",
        );

        let skills = load(&dir);
        assert_eq!(skills.len(), 1);
        assert!(skills.get("tools").is_some());
    }

    #[test]
    fn nested_directories_are_walked_into() {
        let dir = test_dir("deep");
        write(
            &dir,
            "a/b/c/SKILL.md",
            "---\ndescription: Deeply nested.\n---\n",
        );

        let skills = load(&dir);
        assert!(
            skills.get("c").is_some(),
            "{:?}",
            skills.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn hidden_and_dependency_directories_are_skipped() {
        let dir = test_dir("hidden");
        write(&dir, ".hidden/SKILL.md", "---\ndescription: Hidden.\n---\n");
        write(
            &dir,
            "node_modules/pkg/SKILL.md",
            "---\ndescription: Vendored.\n---\n",
        );
        write(&dir, "real/SKILL.md", "---\ndescription: Real.\n---\n");

        let skills = load(&dir);
        assert_eq!(skills.len(), 1);
        assert!(skills.get("real").is_some());
    }

    #[test]
    fn the_first_root_to_claim_a_name_keeps_it() {
        let user = test_dir("collide-user");
        let project = test_dir("collide-project");
        let user_path = write(
            &user,
            "same/SKILL.md",
            "---\ndescription: From the user.\n---\n",
        );
        let project_path = write(
            &project,
            "same/SKILL.md",
            "---\ndescription: From the project.\n---\n",
        );

        let skills = Skills::load(
            &[
                SkillRoot::new(&user, SkillSource::User),
                SkillRoot::new(&project, SkillSource::Project),
            ],
            &[],
        );

        assert_eq!(skills.len(), 1);
        let skill = skills.get("same").expect("found");
        assert_eq!(skill.description(), "From the user.");
        assert_eq!(skill.path(), user_path);
        assert_eq!(skills.warnings().len(), 1);
        assert_eq!(skills.warnings()[0].path, project_path);
        assert!(skills.warnings()[0].message.contains("already taken"));
    }

    #[test]
    fn a_key_this_crate_does_not_read_is_passed_over_whole() {
        let dir = test_dir("metadata");
        write(
            &dir,
            "m/SKILL.md",
            "---\nmetadata:\n  owner: platform\n  tags:\n    - one\n    - two\ndescription: Still read.\n---\n",
        );

        let skills = load(&dir);
        assert_eq!(skills.get("m").expect("found").description(), "Still read.");
    }

    #[test]
    fn a_plain_scalar_continues_on_the_lines_below_it() {
        let dir = test_dir("continue");
        write(
            &dir,
            "c/SKILL.md",
            "---\ndescription: Extract text\n  from PDFs.\nname: c\n---\n",
        );

        let skills = load(&dir);
        assert_eq!(
            skills.get("c").expect("found").description(),
            "Extract text from PDFs."
        );
    }

    #[test]
    fn a_block_scalar_description_is_read() {
        let dir = test_dir("block");
        write(
            &dir,
            "pdf/SKILL.md",
            "---\nname: pdf\ndescription: >\n  Extract text and tables from PDF files.\n  Use when reading a PDF.\n---\n",
        );

        let skills = load(&dir);
        assert_eq!(
            skills.get("pdf").expect("found").description(),
            "Extract text and tables from PDF files. Use when reading a PDF."
        );
    }

    #[test]
    fn quotes_and_comments_come_off_a_scalar() {
        let dir = test_dir("quotes");
        write(
            &dir,
            "q/SKILL.md",
            "---\nname: \"quoted\" # a comment\ndescription: 'It''s quoted.'\n---\n",
        );

        let skills = load(&dir);
        let skill = skills.get("quoted").expect("found");
        assert_eq!(skill.description(), "It's quoted.");
    }

    #[test]
    fn a_file_without_frontmatter_is_instructions_only() {
        let dir = test_dir("plain");
        let path = write(&dir, "plain.md", "# Nothing declared\n");
        // No description, so a loose file is not a skill — but reading it must
        // not treat the body as YAML either.
        assert!(load(&dir).is_empty());

        let (frontmatter, body) =
            parse_document(&std::fs::read_to_string(&path).expect("read")).expect("document");
        assert!(frontmatter.name.is_none());
        assert_eq!(body, "# Nothing declared");
    }

    #[test]
    fn a_body_may_contain_a_fence_that_is_not_the_end() {
        let (frontmatter, body) =
            parse_document("---\ndescription: Has a code fence.\n---\n```\n---\n```\nEnd.\n")
                .expect("document");
        assert_eq!(
            frontmatter.description.as_deref(),
            Some("Has a code fence.")
        );
        assert_eq!(body, "```\n---\n```\nEnd.");
    }

    #[test]
    fn an_unconventional_name_warns_but_still_loads() {
        let dir = test_dir("badname");
        write(
            &dir,
            "x/SKILL.md",
            "---\nname: Not_A_Name\ndescription: Odd name.\n---\n",
        );

        let skills = load(&dir);
        assert_eq!(skills.len(), 1);
        let messages: Vec<&str> = skills
            .warnings()
            .iter()
            .map(|warning| warning.message.as_str())
            .collect();
        assert!(
            messages.iter().any(|message| message.contains("lowercase")),
            "{messages:?}"
        );
    }

    #[test]
    fn an_extra_path_that_is_not_a_skill_is_reported() {
        let dir = test_dir("extra");
        let missing = dir.join("nowhere");
        let text = write(&dir, "notes.txt", "not markdown\n");

        let skills = Skills::load(&[], &[missing, text]);
        assert!(skills.is_empty());
        let messages: Vec<&str> = skills
            .warnings()
            .iter()
            .map(|warning| warning.message.as_str())
            .collect();
        assert!(
            messages.iter().any(|m| m.contains("does not exist")),
            "{messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.contains("neither")),
            "{messages:?}"
        );
    }

    #[test]
    fn an_extra_directory_joins_the_ones_that_are_scanned() {
        let dir = test_dir("extra-dir");
        write(
            &dir,
            "extra/SKILL.md",
            "---\ndescription: Named on the command line.\n---\n",
        );

        let skills = Skills::load(&[], &[dir.join("extra")]);
        let skill = skills.get("extra").expect("found");
        assert_eq!(skill.source(), SkillSource::Path);
    }

    #[test]
    fn the_prompt_section_lists_names_descriptions_and_files() {
        let dir = test_dir("prompt");
        let path = write(
            &dir,
            "pdf-tools/SKILL.md",
            "---\ndescription: Extract text\n  from PDFs.\n---\n",
        );

        let section = load(&dir).prompt_section(Some("read"));
        assert!(section.contains("# Skills"));
        assert!(section.contains("`read` tool"));
        assert!(section.contains("- `pdf-tools`: Extract text from PDFs."));
        assert!(section.contains(&path.display().to_string()));
    }

    #[test]
    fn the_prompt_section_names_no_tool_when_none_can_read_a_file() {
        let dir = test_dir("prompt-noread");
        write(&dir, "a/SKILL.md", "---\ndescription: Something.\n---\n");

        let section = load(&dir).prompt_section(None);
        assert!(section.contains("read its file"));
        assert!(!section.contains("tool"));
    }

    #[test]
    fn the_prompt_section_is_empty_without_skills() {
        assert_eq!(Skills::empty().prompt_section(Some("read")), "");
    }

    #[test]
    fn a_hidden_skill_is_kept_out_of_the_prompt_but_still_invokable() {
        let dir = test_dir("hidden-skill");
        write(
            &dir,
            "secret/SKILL.md",
            "---\ndescription: Only on request.\ndisable-model-invocation: true\n---\nDo it.\n",
        );

        let skills = load(&dir);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills.visible_len(), 0);
        assert_eq!(skills.prompt_section(Some("read")), "");
        assert!(skills.get("secret").is_some());
    }

    #[test]
    fn an_invocation_carries_the_instructions_and_the_request() {
        let dir = test_dir("invoke");
        write(
            &dir,
            "pdf/SKILL.md",
            "---\ndescription: PDFs.\n---\nRead the reference first.\n",
        );

        let skill = load(&dir).get("pdf").cloned().expect("found");
        let prompt = skill.invocation_prompt("extract report.pdf");
        assert!(prompt.contains("Use the `pdf` skill."));
        assert!(prompt.contains("Read the reference first."));
        assert!(prompt.ends_with("Request: extract report.pdf\n"));
    }

    #[test]
    fn an_invocation_without_arguments_stops_after_the_instructions() {
        let dir = test_dir("invoke-bare");
        write(
            &dir,
            "pdf/SKILL.md",
            "---\ndescription: PDFs.\n---\nBody.\n",
        );

        let skill = load(&dir).get("pdf").cloned().expect("found");
        let prompt = skill.invocation_prompt("   ");
        assert!(!prompt.contains("Request:"));
        assert!(prompt.ends_with("Body.\n"));
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        let skills = Skills::load(
            &[SkillRoot::new(
                test_dir("missing").join("nope"),
                SkillSource::User,
            )],
            &[],
        );
        assert!(skills.is_empty());
        assert!(skills.warnings().is_empty());
    }
}
