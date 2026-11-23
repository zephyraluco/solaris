//! Native execution environment contract and path normalization.
use std::io;
use std::path::{Component, Path, PathBuf};

/// Environment selected for this build.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExecutionEnvironment;

impl ExecutionEnvironment {
    /// User-facing execution contract for the host shell and paths.
    pub fn description(self) -> &'static str {
        if cfg!(windows) {
            "Windows with PowerShell (falling back to cmd.exe if PowerShell is unavailable). Use native Windows paths and the session working directory. Programs come from the system PATH."
        } else {
            "System Bash and system tools. Use native POSIX paths and the session working directory."
        }
    }

    /// Model-visible path representation.
    pub fn display_path(self, path: &Path) -> io::Result<String> {
        Ok(display_path(path))
    }

    /// Normalize path spelling for resource rules and lock identities.
    /// Windows paths are compared case-insensitively with canonical separators
    /// and without verbatim prefixes; POSIX paths remain case-sensitive.
    pub fn comparison_path(self, resource: &str) -> String {
        if cfg!(windows) {
            strip_verbatim(resource).replace('/', "\\").to_lowercase()
        } else {
            resource.to_owned()
        }
    }

    /// Resolve existing symlink ancestors, including a not-yet-created suffix.
    pub fn canonical_path(self, path: &Path) -> io::Result<PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };

        let mut current = PathBuf::new();
        for component in absolute.components() {
            match component {
                Component::Prefix(prefix) => current.push(prefix.as_os_str()),
                Component::RootDir => current.push(component.as_os_str()),
                Component::CurDir => {}
                Component::ParentDir => {
                    if current.exists()
                        && let Ok(real) = std::fs::canonicalize(&current)
                    {
                        current = real;
                    }
                    current.pop();
                    if current.exists()
                        && let Ok(real) = std::fs::canonicalize(&current)
                    {
                        current = real;
                    }
                }
                Component::Normal(part) => {
                    current.push(part);
                    if std::fs::symlink_metadata(&current).is_ok()
                        && let Ok(real) = std::fs::canonicalize(&current)
                    {
                        current = real;
                    }
                }
            }
        }
        Ok(current)
    }
}

fn strip_verbatim(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        path.to_owned()
    }
}

/// Convert native app paths for model-facing diagnostics and instructions.
pub fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        strip_verbatim(&text)
    } else {
        text.into_owned()
    }
}

/// The same path resolver is used before approval and before file operations.
pub fn resolve_path(cwd: &Path, input: &str) -> PathBuf {
    crate::path_utils::resolve(cwd, input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_path_resolves_existing_ancestors_and_missing_suffixes() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("missing").join("..").join("new.txt");
        let resolved = ExecutionEnvironment.canonical_path(&nested).unwrap();
        assert_eq!(resolved, dir.path().canonicalize().unwrap().join("new.txt"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_comparison_path_strips_verbatim_prefix_and_normalizes_separators() {
        assert_eq!(
            ExecutionEnvironment.comparison_path(r"\\?\C:\Users\Alice/Project"),
            r"c:\users\alice\project"
        );
        assert_eq!(
            ExecutionEnvironment.comparison_path(r"C:/Users/Alice\Project"),
            r"c:\users\alice\project"
        );
    }
}
