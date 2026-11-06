//! Resolving the paths a model hands a tool.

use std::path::{Path, PathBuf};

/// `path` as an absolute path, resolved against `cwd` when it is relative.
///
/// A model works in the session's directory, so a relative path means relative
/// to that and nothing else — never to wherever the process happens to have been
/// started.
pub fn resolve(cwd: &Path, path: &str) -> PathBuf {
    #[cfg(windows)]
    {
        let base = crate::environment::display_path(cwd);
        if path.starts_with('/') {
            PathBuf::from(path)
        } else if Path::new(path).is_absolute() {
            // Native paths from app-owned attachments remain supported at this boundary.
            PathBuf::from(crate::environment::display_path(Path::new(path)))
        } else {
            PathBuf::from(format!("{}/{path}", base.trim_end_matches('/')))
        }
    }
    #[cfg(not(windows))]
    {
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        }
    }
}

/// Whether something exists at `path`.
pub async fn exists(path: &Path) -> bool {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || crate::filesystem::exists(&path))
        .await
        .unwrap_or(false)
}

/// Whether `path` is a directory.
pub async fn is_dir(path: &Path) -> bool {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || crate::filesystem::is_dir(&path))
        .await
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_resolves_against_the_session_directory() {
        let cwd = Path::new("/work");
        assert_eq!(
            resolve(cwd, "src/main.rs"),
            PathBuf::from("/work/src/main.rs")
        );
        assert_eq!(resolve(cwd, "."), PathBuf::from("/work"));
    }

    #[test]
    fn an_absolute_path_is_left_alone() {
        let cwd = Path::new("/work");
        let absolute = if cfg!(windows) {
            r"C:\tmp\a.txt"
        } else {
            "/tmp/a.txt"
        };
        assert_eq!(
            resolve(cwd, absolute),
            PathBuf::from(crate::environment::display_path(Path::new(absolute)))
        );
    }

    #[tokio::test]
    async fn existence_checks_do_not_panic_on_missing_paths() {
        let missing = Path::new("a-file-that-is-not-there-9f3a.txt");
        assert!(!exists(missing).await);
        assert!(!is_dir(missing).await);
        assert!(is_dir(Path::new(".")).await);
    }
}
