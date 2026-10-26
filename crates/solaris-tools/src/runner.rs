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

use crate::{Cancel, ToolContext};
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
    /// Run with turn cancellation; custom runners may override this method.
    async fn run_with_context(
        &self,
        program: &str,
        args: &[String],
        ctx: &ToolContext,
    ) -> Result<CommandOutput, String> {
        self.run(program, args, &ctx.cwd).await
    }

    /// Run `program` with `args` in `cwd`.
    async fn run(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
    ) -> Result<CommandOutput, String>;
}

/// The real runner: a child process with piped output.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalRunner;

#[async_trait]
impl CommandRunner for LocalRunner {
    async fn run(
        &self,
        program: &str,
        args: &[String],
        cwd: &Path,
    ) -> Result<CommandOutput, String> {
        run_local(program, args, cwd, &Cancel::new()).await
    }

    async fn run_with_context(
        &self,
        program: &str,
        args: &[String],
        ctx: &ToolContext,
    ) -> Result<CommandOutput, String> {
        run_local(program, args, &ctx.cwd, &ctx.cancel).await
    }
}

async fn run_local(
    program: &str,
    args: &[String],
    cwd: &Path,
    cancel: &Cancel,
) -> Result<CommandOutput, String> {
    const CAP: usize = 2 * 1024 * 1024;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let mut child = command
        .spawn()
        .map_err(|error| describe_spawn_failure(program, &error))?;
    let pid = child.id();
    let mut stdout = child.stdout.take().ok_or("stdout unavailable")?;
    let mut stderr = child.stderr.take().ok_or("stderr unavailable")?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut out_open = true;
    let mut err_open = true;
    let mut out_block = [0; 8192];
    let mut err_block = [0; 8192];
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(50));
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(60));
    tokio::pin!(deadline);
    let mut exceeded = false;
    let mut failure = None;
    while out_open || err_open {
        tokio::select! {
            bytes = stdout.read(&mut out_block), if out_open => match bytes {
                Ok(0) => out_open = false,
                Ok(n) => { let keep = n.min(CAP.saturating_sub(out.len())); out.extend_from_slice(&out_block[..keep]); if keep < n { exceeded = true; break; } },
                Err(error) => { failure = Some(error.to_string()); break; }
            },
            bytes = stderr.read(&mut err_block), if err_open => match bytes {
                Ok(0) => err_open = false,
                Ok(n) => { let keep = n.min(CAP.saturating_sub(err.len())); err.extend_from_slice(&err_block[..keep]); if keep < n { exceeded = true; break; } },
                Err(error) => { failure = Some(error.to_string()); break; }
            },
            _ = ticker.tick() => if cancel.is_cancelled() { failure = Some("search cancelled".into()); break; },
            _ = &mut deadline => { failure = Some("search exceeded 60 seconds".into()); break; }
        }
    }
    if exceeded || failure.is_some() {
        crate::tools::shell::kill_tree(pid).await;
        let _ = child.kill().await;
    }
    let status = tokio::select! {
        status = child.wait() => status.map_err(|error| error.to_string())?,
        _ = &mut deadline => { crate::tools::shell::kill_tree(pid).await; let _ = child.kill().await; let _ = child.wait().await; return Err("search exceeded 60 seconds".into()); }
    };
    if let Some(error) = failure {
        return Err(error);
    }
    let mut stdout = String::from_utf8_lossy(&out).into_owned();
    if exceeded {
        stdout.push_str("\n[search output truncated at the 2 MiB capture limit; narrow the query]");
    }
    Ok(CommandOutput {
        stdout,
        stderr: String::from_utf8_lossy(&err).into_owned(),
        code: if exceeded { Some(0) } else { status.code() },
    })
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

    #[cfg(unix)]
    #[tokio::test]
    async fn both_pipes_are_drained_without_deadlock_and_output_is_bounded() {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            LocalRunner.run(
                "bash",
                &[
                    "-c".into(),
                    "head -c 300000 /dev/zero >&2; head -c 300000 /dev/zero".into(),
                ],
                Path::new("."),
            ),
        )
        .await
        .expect("no pipe deadlock")
        .expect("run");
        assert_eq!(result.stdout.len(), 300000);
        assert_eq!(result.stderr.len(), 300000);
        let result = LocalRunner
            .run("bash", &["-c".into(), "yes data".into()], Path::new("."))
            .await
            .expect("bounded capture");
        assert!(result.stdout.len() < 2 * 1024 * 1024 + 100);
        assert!(result.stdout.contains("truncated"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_running_search_observes_cancellation() {
        let ctx = ToolContext {
            cwd: Path::new(".").into(),
            temp_dir: std::env::temp_dir(),
            cancel: Cancel::new(),
        };
        let cancel = ctx.cancel.clone();
        let work = tokio::spawn(async move {
            LocalRunner
                .run_with_context("bash", &["-c".into(), "sleep 30".into()], &ctx)
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), work)
            .await
            .expect("prompt cancellation")
            .expect("join");
        assert!(result.expect_err("cancelled").contains("cancelled"));
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
