//! Running the external programs the search tools lean on.
//!
//! `grep` and `find` are thin wrappers over ripgrep and fd rather than
//! reimplementations: those programs already know how to respect `.gitignore`,
//! skip binaries and walk a tree quickly, and a second implementation would
//! drift from them. What this module adds is the seam a test can stand in — the
//! suite drives a canned runner, so it needs neither program installed nor a
//! real directory tree.

use std::path::Path;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// What a finished program left behind.
#[derive(Debug, Clone, Default)]
pub struct CommandOutput {
    /// Everything the program wrote to stdout.
    pub stdout: String,
    /// Everything it wrote to stderr.
    pub stderr: String,
    /// Its exit code, or `None` when a signal killed it.
    pub code: Option<i32>,
}

impl CommandOutput {
    /// Whether the program reported success.
    pub fn succeeded(&self) -> bool {
        self.code == Some(0)
    }

    /// The best explanation available for a non-zero exit.
    pub fn failure_message(&self) -> String {
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return stderr.to_string();
        }
        match self.code {
            Some(code) => format!("the command exited with code {code}"),
            None => "the command was killed".to_string(),
        }
    }
}

/// Runs an external program and collects what it printed.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run `program` with `args` in `cwd`.
    async fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<CommandOutput, String>;
}

/// The real runner: a child process with piped output.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalRunner;

#[async_trait]
impl CommandRunner for LocalRunner {
    async fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<CommandOutput, String> {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Without this a program started from a windowed parent gets a console
        // of its own on Windows, which flashes on screen for every search.
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command
            .spawn()
            .map_err(|error| describe_spawn_failure(program, &error))?;

        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stdout.take() {
            let _ = pipe.read_to_string(&mut stdout).await;
        }
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr).await;
        }

        let status = child
            .wait()
            .await
            .map_err(|error| format!("could not wait for {program}: {error}"))?;

        Ok(CommandOutput {
            stdout,
            stderr,
            code: status.code(),
        })
    }
}

/// Why a program could not be started, in words the user can act on.
///
/// A missing binary is the common case and the message has to say what to
/// install: "No such file or directory" leaves a person guessing.
pub fn describe_spawn_failure(program: &str, error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        format!("`{program}` was not found on PATH — install it and try again")
    } else {
        format!("could not run `{program}`: {error}")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A runner that returns what it was told to, whatever it is asked.
    pub(crate) struct StubRunner(pub CommandOutput);

    #[async_trait]
    impl CommandRunner for StubRunner {
        async fn run(
            &self,
            _program: &str,
            _args: &[String],
            _cwd: &Path,
        ) -> Result<CommandOutput, String> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn a_missing_program_explains_itself() {
        let error = LocalRunner
            .run("solaris-a-program-that-does-not-exist", &[], Path::new("."))
            .await
            .expect_err("nothing is installed under that name");
        assert!(error.contains("PATH"), "{error}");
    }

    #[test]
    fn a_failure_prefers_the_programs_own_wording() {
        let output = CommandOutput {
            stdout: String::new(),
            stderr: "  rg: regex parse error  \n".to_string(),
            code: Some(2),
        };
        assert_eq!(output.failure_message(), "rg: regex parse error");
        assert!(!output.succeeded());

        let silent = CommandOutput {
            code: Some(1),
            ..Default::default()
        };
        assert_eq!(silent.failure_message(), "the command exited with code 1");
    }
}
