//! Discovery and environment for the bundled Windows Bash runtime.

use std::path::{Path, PathBuf};

const RUNTIME: &str = "plugins/windows/cygwin";

fn validate(root: PathBuf) -> Result<PathBuf, String> {
    for relative in ["bin/bash.exe", "bin/cygwin1.dll", "etc/profile"] {
        if !root.join(relative).is_file() {
            return Err(format!(
                "Cygwin runtime is incomplete: missing {}. Install the complete Windows distribution.",
                root.join(relative).display()
            ));
        }
    }
    std::path::absolute(root).map_err(|error| format!("cannot resolve Cygwin runtime: {error}"))
}

fn find_root(executable: &Path) -> Result<PathBuf, String> {
    let directory = executable
        .parent()
        .ok_or("executable has no parent directory")?;
    let bundled = directory.join(RUNTIME);
    if bundled.exists() {
        return validate(bundled);
    }
    // Cargo places development and test binaries below target/. Only consider
    // ancestors that identify this source tree; never search the session cwd.
    for ancestor in directory.ancestors() {
        if ancestor.join("Cargo.toml").is_file()
            && ancestor.join("crates/solaris/src/main.rs").is_file()
        {
            return validate(ancestor.join(RUNTIME));
        }
    }
    Err(format!(
        "Cygwin runtime not found at {}. Install the complete Windows distribution.",
        bundled.display()
    ))
}

/// Windows environment shared by startup and Bash subprocesses.
#[cfg(windows)]
pub struct Environment {
    /// Absolute path to the bundled Bash executable.
    pub bash: PathBuf,
    /// Native Windows PATH with the Cygwin binaries first.
    pub path: std::ffi::OsString,
}

/// Resolve the runtime once without changing the process environment.
#[cfg(windows)]
pub fn environment() -> Result<&'static Environment, String> {
    static ENVIRONMENT: std::sync::OnceLock<Result<Environment, String>> =
        std::sync::OnceLock::new();
    ENVIRONMENT
        .get_or_init(|| {
            let executable = std::env::current_exe().map_err(|error| error.to_string())?;
            let root = find_root(&executable)?;
            // Git and ZIP archives do not preserve empty directories. The
            // login profile and Bash here-documents expect /tmp to exist.
            std::fs::create_dir_all(root.join("tmp"))
                .map_err(|error| format!("cannot create Cygwin temporary directory: {error}"))?;
            let mut paths = vec![root.join("bin")];
            paths.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            let path = std::env::join_paths(paths).map_err(|error| error.to_string())?;
            Ok(Environment {
                bash: root.join("bin/bash.exe"),
                path,
            })
        })
        .as_ref()
        .map_err(Clone::clone)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(root: &Path) {
        for relative in ["bin/bash.exe", "bin/cygwin1.dll", "etc/profile"] {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, []).unwrap();
        }
    }

    #[test]
    fn discovers_packaged_runtime_in_paths_with_spaces_and_unicode() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("Solaris 中文 app");
        runtime(&install.join(RUNTIME));
        assert_eq!(
            find_root(&install.join("solaris.exe")).unwrap(),
            install.join(RUNTIME)
        );
    }

    #[test]
    fn discovers_source_runtime_for_cargo_binaries() {
        let dir = tempfile::tempdir().unwrap();
        runtime(&dir.path().join(RUNTIME));
        std::fs::create_dir_all(dir.path().join("crates/solaris/src")).unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), []).unwrap();
        std::fs::write(dir.path().join("crates/solaris/src/main.rs"), []).unwrap();
        let executable = dir.path().join("target/debug/deps/test.exe");
        assert_eq!(find_root(&executable).unwrap(), dir.path().join(RUNTIME));
    }

    #[test]
    fn missing_runtime_reports_expected_location() {
        let dir = tempfile::tempdir().unwrap();
        let error = find_root(&dir.path().join("solaris.exe")).unwrap_err();
        assert!(error.contains(&dir.path().join(RUNTIME).display().to_string()));
    }

    #[test]
    fn incomplete_install_does_not_fall_back_to_development_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(RUNTIME);
        runtime(&root);
        std::fs::remove_file(root.join("bin/cygwin1.dll")).unwrap();
        assert!(
            find_root(&dir.path().join("solaris.exe"))
                .unwrap_err()
                .contains("cygwin1.dll")
        );
    }
}
