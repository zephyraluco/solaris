//! A single Bash boundary for commands and user-file access.
//!
//! Arguments are never interpolated into scripts. File contents travel on stdin.
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::Cancel;

/// Environment selected for this build. No agent-side host branching is needed.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExecutionEnvironment;

/// Raw process result; binary file contents are never decoded by the runner.
#[derive(Debug)]
pub(crate) struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: Option<i32>,
}

// Only this fixed, backslash-free script crosses the native Windows argv
// boundary. Cygwin startup globbing must never see scripts or user arguments.
#[cfg(any(windows, test))]
const DISPATCHER: &str = r#"
IFS= read -r -d '' __solaris_script || exit 64
IFS= read -r -d '' __solaris_count || exit 64
__solaris_args=()
for ((__solaris_i=0; __solaris_i<__solaris_count; __solaris_i++)); do
    IFS= read -r -d '' __solaris_arg || exit 64
    __solaris_args+=("$__solaris_arg")
done
set -- "${__solaris_args[@]}"
eval "$__solaris_script"
"#;

#[cfg(any(windows, test))]
fn frame(script: &str, args: &[&Path], input: &[u8]) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut field = |value: &str| -> io::Result<()> {
        if value.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "script and arguments cannot contain NUL",
            ));
        }
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(0);
        Ok(())
    };
    field(script)?;
    field(&args.len().to_string())?;
    for arg in args {
        field(arg.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "argument is not valid Unicode")
        })?)?;
    }
    bytes.extend_from_slice(input);
    Ok(bytes)
}

impl ExecutionEnvironment {
    /// User-facing execution contract, independent of the host's native shell.
    pub fn description(self) -> &'static str {
        if cfg!(windows) {
            "Cygwin with Bash. Use POSIX paths; system drives are /cygdrive/<letter>/ (for example /cygdrive/c/Users). Programs come from the bundled Cygwin PATH."
        } else {
            "System Bash and system tools. Use native POSIX paths and the session working directory."
        }
    }

    /// Prepare Bash with the same profile and environment for every tool.
    pub fn command(self) -> io::Result<Command> {
        #[cfg(not(windows))]
        let command = Command::new("bash");
        #[cfg(windows)]
        let command = {
            use std::os::windows::process::CommandExt;
            let runtime = crate::cygwin::environment().map_err(io::Error::other)?;
            let mut command = Command::new(&runtime.bash);
            command
                .args(["--noprofile", "--norc"])
                .env("PATH", crate::cygwin::PATH)
                .env("SHELL", "/usr/bin/bash")
                .env("CHERE_INVOKING", "1")
                .env("TMPDIR", std::env::temp_dir())
                .env_remove("BASH_ENV")
                .env_remove("ENV")
                .env_remove("SHELLOPTS")
                .env_remove("BASHOPTS")
                .env_remove("CDPATH");
            if let Some(home) = std::env::var_os("USERPROFILE") {
                command.env("HOME", home);
            }
            command.creation_flags(0x0800_0000);
            command
        };
        Ok(command)
    }

    /// Load only the bundled profile; personal login files cannot reintroduce
    /// the host PATH. Script callers supply code; structured callers use fixed code.
    pub fn bootstrap(self, script: &str) -> String {
        if cfg!(windows) {
            format!("source /etc/profile || exit;\n{script}")
        } else {
            script.to_owned()
        }
    }

    /// Configure the process and its stdin together. On Windows payloads use
    /// NUL-delimited stdin framing, bypassing Cygwin's native argv globbing.
    pub(crate) fn invocation(
        self,
        script: &str,
        args: &[&Path],
        input: &[u8],
    ) -> io::Result<(Command, Vec<u8>)> {
        let mut command = self.command()?;
        let script = self.bootstrap(script);
        #[cfg(windows)]
        {
            command.args(["-c", DISPATCHER, "solaris"]);
            Ok((command, frame(&script, args, input)?))
        }
        #[cfg(not(windows))]
        {
            command.args(["-c", &script, "solaris"]).args(args);
            Ok((command, input.to_vec()))
        }
    }

    /// Model-visible path. Native Windows paths enter only at app boundaries.
    pub fn display_path(self, path: &Path) -> io::Result<String> {
        #[cfg(windows)]
        {
            let output = self.checked("cygpath -au -- \"$1\"", &[path], &[], None)?;
            one_path(output)
        }
        #[cfg(not(windows))]
        Ok(path.to_string_lossy().into_owned())
    }

    /// Normalize path spelling for resource rules and lock identities. Globs
    /// remain literal patterns; Cygwin's case-insensitive drive convention is
    /// applied conservatively, while native POSIX paths remain case-sensitive.
    pub fn comparison_path(self, resource: &str) -> String {
        if cfg!(windows) {
            let path = if Path::new(resource).is_absolute() {
                display_path(Path::new(resource))
            } else {
                resource.to_owned()
            };
            path.to_lowercase()
        } else {
            resource.to_owned()
        }
    }

    /// Resolve existing symlink ancestors, including a not-yet-created suffix.
    pub fn canonical_path(self, path: &Path) -> io::Result<PathBuf> {
        let kind = if cfg!(windows) { "cygwin" } else { "native" };
        let output = self.checked(
            include_str!("scripts/canonical.sh"),
            &[path, Path::new(kind)],
            &[],
            None,
        )?;
        one_path(output).map(PathBuf::from)
    }

    pub(crate) fn checked(
        self,
        script: &str,
        args: &[&Path],
        input: &[u8],
        cancel: Option<&Cancel>,
    ) -> io::Result<Vec<u8>> {
        #[cfg(windows)]
        let normalized: Vec<PathBuf> = args
            .iter()
            .map(|path| {
                let value = path.to_string_lossy();
                PathBuf::from(
                    if value.starts_with('/') || value.starts_with("\\cygdrive\\") {
                        value.replace('\\', "/")
                    } else {
                        value.into_owned()
                    },
                )
            })
            .collect();
        #[cfg(windows)]
        let paths: Vec<&Path> = normalized.iter().map(PathBuf::as_path).collect();
        #[cfg(windows)]
        let args = paths.as_slice();
        let output = self.run(script, args, input, None, cancel, 32 * 1024 * 1024)?;
        if output.code != Some(0) {
            let kind = match output.code {
                Some(44) => io::ErrorKind::NotFound,
                Some(43) => io::ErrorKind::PermissionDenied,
                _ => io::ErrorKind::Other,
            };
            return Err(io::Error::new(
                kind,
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        Ok(output.stdout)
    }

    /// Execute a fixed script with positional parameters and bounded binary I/O.
    pub(crate) fn run(
        self,
        script: &str,
        args: &[&Path],
        input: &[u8],
        cwd: Option<&Path>,
        cancel: Option<&Cancel>,
        cap: usize,
    ) -> io::Result<Output> {
        if cancel.is_some_and(Cancel::is_cancelled) {
            return Err(io::Error::other("tool cancelled"));
        }
        let (mut command, data) = self.invocation(script, args, input)?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn()?;
        let pid = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("stdin unavailable"))?;
        let writer = std::thread::spawn(move || {
            let mut stdin = stdin;
            stdin.write_all(&data)
        });
        let exceeded = Arc::new(AtomicBool::new(false));
        fn reader(
            stream: impl Read + Send + 'static,
            cap: usize,
            exceeded: Arc<AtomicBool>,
        ) -> std::thread::JoinHandle<io::Result<Vec<u8>>> {
            std::thread::spawn(move || {
                let mut data = Vec::new();
                stream.take((cap + 1) as u64).read_to_end(&mut data)?;
                if data.len() > cap {
                    exceeded.store(true, Ordering::Release);
                }
                Ok(data)
            })
        }
        let stdout = reader(
            child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("stdout unavailable"))?,
            cap,
            Arc::clone(&exceeded),
        );
        let stderr = reader(
            child
                .stderr
                .take()
                .ok_or_else(|| io::Error::other("stderr unavailable"))?,
            256 * 1024,
            Arc::clone(&exceeded),
        );
        let started = Instant::now();
        let mut status = None;
        let mut failure = None;
        loop {
            if status.is_none() {
                status = child.try_wait()?;
            }
            if status.is_some() && stdout.is_finished() && stderr.is_finished() {
                break;
            }
            if cancel.is_some_and(Cancel::is_cancelled) {
                failure = Some("tool cancelled");
            }
            if exceeded.load(Ordering::Acquire) {
                failure = Some("tool output exceeds capture limit");
            }
            if started.elapsed() > Duration::from_secs(60) {
                failure = Some("tool exceeded 60 seconds");
            }
            if failure.is_some() {
                kill_tree(pid);
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = writer.join();
        let out = stdout
            .join()
            .map_err(|_| io::Error::other("stdout reader failed"))??;
        let err = stderr
            .join()
            .map_err(|_| io::Error::other("stderr reader failed"))??;
        if exceeded.load(Ordering::Acquire) {
            return Err(io::Error::other("tool output exceeds capture limit"));
        }
        if let Some(reason) = failure {
            return Err(io::Error::other(reason));
        }
        Ok(Output {
            stdout: out,
            stderr: err,
            code: status.and_then(|status| status.code()),
        })
    }
}

fn one_path(mut bytes: Vec<u8>) -> io::Result<String> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    String::from_utf8(bytes).map_err(io::Error::other)
}

fn kill_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        use std::os::windows::process::CommandExt;
        let _ = Command::new(PathBuf::from(root).join("System32/taskkill.exe"))
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(0x0800_0000)
            .output();
    }
}

/// Convert native app paths for model-facing diagnostics and instructions.
pub fn display_path(path: &Path) -> String {
    ExecutionEnvironment
        .display_path(path)
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

/// The same path resolver is used before approval and before file operations.
pub fn resolve_path(cwd: &Path, input: &str) -> PathBuf {
    crate::path_utils::resolve(cwd, input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framed_arguments_preserve_regex_quotes_empty_values_and_binary_stdin() {
        let args = [
            r"^\[|^name|^\[dependencies\]",
            r"\\regex",
            "",
            "a 'quote' \"double\" * ? [abc]",
            "line\nend",
        ];
        let paths: Vec<&Path> = args.iter().map(Path::new).collect();
        let input = b"\0\xffbinary\n";
        let script = r#"printf '%s\0' "$@"; cat"#;
        let framed = frame(script, &paths, input).unwrap();
        let mut command = ExecutionEnvironment.command().unwrap();
        command
            .args(["-c", DISPATCHER, "solaris"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(&framed).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut expected = Vec::new();
        for arg in args {
            expected.extend_from_slice(arg.as_bytes());
            expected.push(0);
        }
        expected.extend_from_slice(input);
        assert_eq!(output.stdout, expected);
        assert!(frame("script", &[Path::new("bad\0arg")], &[]).is_err());
    }

    #[test]
    fn positional_arguments_are_data_and_stdin_is_binary() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("must-not-exist");
        let payload = format!("space 中文 ; $(touch {})\n'quoted'", marker.display());
        let out = ExecutionEnvironment
            .run(
                "printf '%s' \"$1\"; cat",
                &[Path::new(&payload)],
                b"\0\xff\n",
                Some(dir.path()),
                None,
                4096,
            )
            .unwrap();
        assert_eq!(out.code, Some(0));
        let mut expected = payload.into_bytes();
        expected.extend_from_slice(b"\0\xff\n");
        assert_eq!(out.stdout, expected);
        assert!(!marker.exists());
    }

    #[test]
    fn binary_capture_limits_fail_instead_of_returning_partial_data() {
        let result = ExecutionEnvironment.run("printf 123456789", &[], &[], None, None, 4);
        assert!(result.unwrap_err().to_string().contains("capture limit"));
    }

    #[test]
    fn cancelled_workers_stop_the_process_group() {
        let cancel = Cancel::new();
        let token = cancel.clone();
        let work = std::thread::spawn(move || {
            ExecutionEnvironment.run("while :; do :; done", &[], &[], None, Some(&token), 4096)
        });
        std::thread::sleep(Duration::from_millis(100));
        cancel.cancel();
        assert!(
            work.join()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
    }
}
