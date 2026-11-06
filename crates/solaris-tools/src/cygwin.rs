//! Discovery and environment for the bundled Windows Bash runtime.

use std::path::{Path, PathBuf};

const RUNTIME: &str = "plugins/windows/cygwin";

/// Files required for the complete Bash-backed tool environment.
pub const REQUIRED_FILES: &[&str] = &[
    "bin/bash.exe",
    "bin/cygwin1.dll",
    "etc/profile",
    "bin/cygpath.exe",
    "bin/cat.exe",
    "bin/head.exe",
    "bin/cp.exe",
    "bin/ls.exe",
    "bin/chmod.exe",
    "bin/mkdir.exe",
    "bin/mktemp.exe",
    "bin/mv.exe",
    "bin/rm.exe",
    "bin/readlink.exe",
    "usr/local/bin/rg.exe",
    "usr/local/bin/fd.exe",
];

/// Command search is confined to the bundled POSIX environment.
pub const PATH: &str = "/usr/local/bin:/usr/bin:/bin";

fn validate(root: PathBuf) -> Result<PathBuf, String> {
    for relative in REQUIRED_FILES {
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

/// Bundled Windows Bash runtime, applied only to Bash subprocesses.
#[cfg(windows)]
pub struct Environment {
    /// Absolute path to the bundled Bash executable.
    pub bash: PathBuf,
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
            Ok(Environment {
                bash: root.join("bin/bash.exe"),
            })
        })
        .as_ref()
        .map_err(Clone::clone)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn login_profile_discards_inherited_command_search_paths() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let native_bin = dir.path().join("native-bin");
        std::fs::create_dir(&native_bin).unwrap();
        let native_command = native_bin.join("native-only.exe");
        std::fs::write(&native_command, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&native_command, std::fs::Permissions::from_mode(0o755)).unwrap();
        let profile = dir.path().join("profile");
        std::fs::write(
            &profile,
            include_str!("../../../plugins/windows/cygwin/etc/profile"),
        )
        .unwrap();
        let output = std::process::Command::new("/bin/bash")
            .args([
                "--noprofile",
                "--norc",
                "-c",
                "source \"$1\"; printf '%s\\n' \"$PATH\"; command -v native-only.exe",
                "bash",
            ])
            .arg(profile)
            .env("PATH", native_bin)
            .env("HOME", dir.path())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{PATH}\n")
        );
    }

    fn runtime(root: &Path) {
        for relative in REQUIRED_FILES {
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
