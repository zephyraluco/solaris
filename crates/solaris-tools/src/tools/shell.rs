//! `bash` and `powershell` — run a command and report what it printed.
//!
//! One implementation, two configurations: the tools differ only in which
//! program runs the command and what the model is told about it. Which one
//! exists is a property of the platform, so the registry registers exactly one.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::runner::describe_spawn_failure;
use crate::tool::{Tool, ToolContext, ToolOutput, optional_count_arg, string_arg};
use crate::truncate::{Limits, format_size, tail};

/// Which shell a tool runs commands with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellConfig {
    /// Name the model calls the tool by.
    pub tool_name: &'static str,
    /// One-line description handed to the model.
    pub description: &'static str,
    /// Programs that can run it, best first. The first one present on `PATH`
    /// wins, so a machine with PowerShell 7 uses `pwsh` and one without still
    /// has `powershell`.
    pub programs: &'static [&'static str],
    /// Argument that makes the program run the string it is given.
    pub command_flag: &'static str,
}

impl ShellConfig {
    /// `bash`, for Unix.
    pub const BASH: ShellConfig = ShellConfig {
        tool_name: "bash",
        description: "Run a shell command in the session directory with bash and return everything \
                      it printed, stdout and stderr together. The output is truncated to the last \
                      2000 lines or 50KB; when that happens the full output is written to a file \
                      whose path is reported. Commands that never finish need a `timeout`.",
        programs: &["bash"],
        command_flag: "-c",
    };

    /// PowerShell, for Windows.
    pub const POWERSHELL: ShellConfig = ShellConfig {
        tool_name: "powershell",
        description: "Run a command in the session directory with PowerShell and return everything \
                      it printed, stdout and stderr together. The output is truncated to the last \
                      2000 lines or 50KB; when that happens the full output is written to a file \
                      whose path is reported. Commands that never finish need a `timeout`.",
        programs: &["pwsh", "powershell"],
        command_flag: "-Command",
    };
}

/// What one shell command produced.
#[derive(Debug, Clone, Default)]
pub struct ShellOutcome {
    /// Everything the command printed, stdout and stderr together.
    pub output: String,
    /// Its exit code, or `None` when a signal killed it.
    pub code: Option<i32>,
    /// Whether the timeout ran out and the command was killed.
    pub timed_out: bool,
    /// Whether the caller asked to stop and the command was killed.
    pub cancelled: bool,
}

/// Runs one shell command.
///
/// The seam a test stands in: the suite drives a canned runner instead of
/// spawning a shell, so it asserts on what the tool does with an outcome rather
/// than on how a platform's shell behaves.
#[async_trait]
pub trait ShellRunner: Send + Sync {
    /// Run `command` with the working directory and cancellation handle `ctx`
    /// carries, killing it if it outlives `timeout`.
    async fn run(
        &self,
        config: ShellConfig,
        command: &str,
        ctx: &ToolContext,
        timeout: Option<Duration>,
    ) -> Result<ShellOutcome, String>;
}

/// The real runner: a shell process whose output is piped back.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalShell;

#[async_trait]
impl ShellRunner for LocalShell {
    async fn run(
        &self,
        config: ShellConfig,
        command: &str,
        ctx: &ToolContext,
        timeout: Option<Duration>,
    ) -> Result<ShellOutcome, String> {
        let mut last_missing = String::new();

        for program in config.programs {
            // `2>&1` inside the command string is what merges the two streams in
            // the order they happened; piping them separately would need a loop
            // that still got the interleaving wrong.
            let mut child = match Command::new(program)
                .arg(config.command_flag)
                .arg(format!("{command} 2>&1"))
                .current_dir(&ctx.cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    last_missing = describe_spawn_failure(program, &error);
                    continue;
                }
                Err(error) => return Err(format!("could not run {program}: {error}")),
            };

            let mut pipe = child
                .stdout
                .take()
                .ok_or_else(|| format!("{program} produced no output pipe"))?;
            let mut buffer = Vec::new();
            let mut timed_out = false;
            let mut cancelled = false;

            let deadline: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
                match timeout {
                    Some(duration) => Box::pin(tokio::time::sleep(duration)),
                    None => Box::pin(futures::future::pending()),
                };
            tokio::pin!(deadline);

            // A cancelled turn has to stop a runaway command, so the flag is
            // polled rather than checked once.
            let mut ticker = tokio::time::interval(Duration::from_millis(100));
            ticker.tick().await;

            loop {
                tokio::select! {
                    read = pipe.read_buf(&mut buffer) => match read {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    },
                    _ = &mut deadline => {
                        timed_out = true;
                        break;
                    }
                    _ = ticker.tick() => {
                        if ctx.is_cancelled() {
                            cancelled = true;
                            break;
                        }
                    }
                }
            }

            let code = if timed_out || cancelled {
                kill(child.id()).await;
                let _ = child.kill().await;
                let _ = child.wait().await;
                None
            } else {
                child
                    .wait()
                    .await
                    .map_err(|error| format!("could not wait for {program}: {error}"))?
                    .code()
            };

            return Ok(ShellOutcome {
                output: String::from_utf8_lossy(&buffer).into_owned(),
                code,
                timed_out,
                cancelled,
            });
        }

        Err(last_missing)
    }
}

/// Kill a whole process tree, which on Windows is the only way to stop the
/// grandchildren a shell started.
///
/// `child.kill()` reaches the shell itself; a command that spawned something
/// would leave it running.
async fn kill(pid: Option<u32>) {
    #[cfg(windows)]
    if let Some(pid) = pid {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    #[cfg(not(windows))]
    let _ = pid;
}

/// Runs commands with a shell.
#[derive(Clone)]
pub struct ShellTool {
    config: ShellConfig,
    runner: Arc<dyn ShellRunner>,
    limits: Limits,
}

impl ShellTool {
    /// A tool that runs commands with `config`'s shell.
    pub fn new(config: ShellConfig, runner: Arc<dyn ShellRunner>) -> Self {
        Self {
            config,
            runner,
            limits: Limits::default(),
        }
    }

    /// The `bash` tool.
    pub fn bash(runner: Arc<dyn ShellRunner>) -> Self {
        Self::new(ShellConfig::BASH, runner)
    }

    /// The `powershell` tool.
    pub fn powershell(runner: Arc<dyn ShellRunner>) -> Self {
        Self::new(ShellConfig::POWERSHELL, runner)
    }

    /// A tool with its own output budget.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        self.config.tool_name
    }

    fn description(&self) -> &str {
        self.config.description
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Command to run",
                },
                "timeout": {
                    "type": "integer",
                    "description": "Seconds to let the command run before killing it; omit for no limit",
                },
            },
            "required": ["command"],
        })
    }

    async fn run(&self, input: Value, ctx: &ToolContext) -> ToolOutput {
        let command = match string_arg(&input, "command") {
            Ok(command) => command,
            Err(error) => return error,
        };
        let timeout = match optional_count_arg(&input, "timeout") {
            Ok(timeout) => timeout.map(|seconds| Duration::from_secs(seconds as u64)),
            Err(error) => return error,
        };

        let outcome = match self.runner.run(self.config, &command, ctx, timeout).await {
            Ok(outcome) => outcome,
            Err(error) => return ToolOutput::error(error),
        };

        let truncation = tail(&outcome.output, self.limits);
        let truncated = truncation.truncated();
        let mut text = if truncation.content.trim().is_empty() {
            "(the command printed nothing)".to_string()
        } else {
            truncation.content
        };

        let mut notices = Vec::new();
        if truncated {
            match write_full_output(&outcome.output, ctx).await {
                Ok(path) => notices.push(format!(
                    "output truncated to the last {} lines / {}; full output at {}",
                    self.limits.max_lines,
                    format_size(self.limits.max_bytes),
                    path.display()
                )),
                Err(error) => notices.push(format!(
                    "output truncated to the last {} lines / {} and could not be saved: {error}",
                    self.limits.max_lines,
                    format_size(self.limits.max_bytes),
                )),
            }
        }
        if outcome.timed_out {
            notices.push(format!(
                "the command was still running after {}s and was killed",
                timeout.map(|duration| duration.as_secs()).unwrap_or_default()
            ));
        }
        if outcome.cancelled {
            notices.push("the command was cancelled and killed".to_string());
        }
        if let Some(code) = outcome.code.filter(|code| *code != 0) {
            notices.push(format!("exit code {code}"));
        }
        if outcome.code.is_none() && !outcome.timed_out && !outcome.cancelled {
            notices.push("the command was killed by a signal".to_string());
        }

        if !notices.is_empty() {
            text.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }
        ToolOutput::ok(text)
    }
}

/// Save the whole output somewhere the model can read it in pieces.
async fn write_full_output(output: &str, ctx: &ToolContext) -> Result<std::path::PathBuf, String> {
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = ctx
        .temp_dir
        .join(format!("solaris-output-{}-{unique}.log", std::process::id()));

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| error.to_string())?;
    }
    tokio::fs::write(&path, output.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_dir;
    use std::path::Path;

    /// A runner that returns one canned outcome and records the call.
    struct StubShell {
        outcome: ShellOutcome,
        seen: std::sync::Mutex<Vec<(String, Option<Duration>)>>,
    }

    impl StubShell {
        fn new(outcome: ShellOutcome) -> Arc<Self> {
            Arc::new(Self {
                outcome,
                seen: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl ShellRunner for StubShell {
        async fn run(
            &self,
            _config: ShellConfig,
            command: &str,
            _ctx: &ToolContext,
            timeout: Option<Duration>,
        ) -> Result<ShellOutcome, String> {
            self.seen
                .lock()
                .expect("lock")
                .push((command.to_string(), timeout));
            Ok(self.outcome.clone())
        }
    }

    fn context(dir: &Path) -> ToolContext {
        ToolContext::new(dir, dir)
    }

    #[tokio::test]
    async fn a_successful_command_returns_its_output() {
        let dir = test_dir("shell-ok");
        let runner = StubShell::new(ShellOutcome {
            output: "hello\n".to_string(),
            code: Some(0),
            ..Default::default()
        });

        let output = ShellTool::bash(runner.clone())
            .run(json!({ "command": "echo hello" }), &context(&dir))
            .await;

        assert!(!output.is_error, "{}", output.text);
        // Output is line-based, so a trailing newline is not preserved.
        assert_eq!(output.text, "hello");
        let seen = runner.seen.lock().expect("lock");
        assert_eq!(seen[0].0, "echo hello");
        assert_eq!(seen[0].1, None, "no timeout was asked for");
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_reported_without_failing_the_tool() {
        let dir = test_dir("shell-exit");
        let runner = StubShell::new(ShellOutcome {
            output: "boom\n".to_string(),
            code: Some(3),
            ..Default::default()
        });

        let output = ShellTool::bash(runner)
            .run(json!({ "command": "false" }), &context(&dir))
            .await;

        assert!(!output.is_error, "the model can react to a failed command");
        assert!(output.text.contains("exit code 3"), "{}", output.text);
    }

    #[tokio::test]
    async fn a_timeout_is_passed_through_and_reported() {
        let dir = test_dir("shell-timeout");
        let runner = StubShell::new(ShellOutcome {
            output: "partial".to_string(),
            code: None,
            timed_out: true,
            cancelled: false,
        });

        let output = ShellTool::bash(runner.clone())
            .run(json!({ "command": "sleep 99", "timeout": 2 }), &context(&dir))
            .await;

        assert!(output.text.contains("killed"), "{}", output.text);
        let seen = runner.seen.lock().expect("lock");
        assert_eq!(seen[0].1, Some(Duration::from_secs(2)));
    }

    #[tokio::test]
    async fn a_cancelled_command_says_so() {
        let dir = test_dir("shell-cancel");
        let runner = StubShell::new(ShellOutcome {
            output: String::new(),
            code: None,
            timed_out: false,
            cancelled: true,
        });

        let output = ShellTool::bash(runner)
            .run(json!({ "command": "long" }), &context(&dir))
            .await;

        assert!(output.text.contains("cancelled"), "{}", output.text);
    }

    #[tokio::test]
    async fn a_command_that_printed_nothing_says_so() {
        let dir = test_dir("shell-silent");
        let runner = StubShell::new(ShellOutcome {
            output: String::new(),
            code: Some(0),
            ..Default::default()
        });

        let output = ShellTool::bash(runner)
            .run(json!({ "command": "true" }), &context(&dir))
            .await;

        assert!(output.text.contains("printed nothing"), "{}", output.text);
    }

    #[tokio::test]
    async fn the_tail_survives_and_the_whole_output_is_saved() {
        let dir = test_dir("shell-truncate");
        let long: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        let runner = StubShell::new(ShellOutcome {
            output: long,
            code: Some(0),
            ..Default::default()
        });

        let tool = ShellTool::bash(runner).with_limits(Limits {
            max_lines: 3,
            max_bytes: 64 * 1024,
        });
        let output = tool
            .run(json!({ "command": "noisy" }), &context(&dir))
            .await;

        assert!(output.text.starts_with("line 98\nline 99\nline 100"), "{}", output.text);
        let notice = output.text.split('[').nth(1).expect("a notice");
        assert!(notice.contains("full output at"), "{notice}");

        let saved = notice
            .split("full output at ")
            .nth(1)
            .expect("a path")
            .trim_end_matches(']');
        let saved = std::fs::read_to_string(saved).expect("saved output");
        assert!(saved.contains("line 1\n"), "the whole output is there");
    }

    #[tokio::test]
    async fn a_shell_that_cannot_start_is_an_error_result() {
        struct Missing;
        #[async_trait]
        impl ShellRunner for Missing {
            async fn run(
                &self,
                _config: ShellConfig,
                _command: &str,
                _ctx: &ToolContext,
                _timeout: Option<Duration>,
            ) -> Result<ShellOutcome, String> {
                Err("`bash` was not found on PATH — install it and try again".to_string())
            }
        }

        let dir = test_dir("shell-missing");
        let output = ShellTool::bash(Arc::new(Missing))
            .run(json!({ "command": "echo hi" }), &context(&dir))
            .await;

        assert!(output.is_error);
        assert!(output.text.contains("PATH"), "{}", output.text);
    }

    #[test]
    fn each_shell_declares_its_own_name() {
        let bash = ShellTool::bash(StubShell::new(ShellOutcome::default()));
        assert_eq!(bash.name(), "bash");

        let powershell = ShellTool::powershell(StubShell::new(ShellOutcome::default()));
        assert_eq!(powershell.name(), "powershell");
        assert!(powershell.description().contains("PowerShell"));
    }

    #[tokio::test]
    async fn a_real_command_runs_and_reports_its_output() {
        // The only test that spawns a shell: it proves the local runner wires a
        // process up correctly, which a stub cannot.
        let dir = test_dir("shell-real");
        let command = if cfg!(windows) { "Write-Output hello" } else { "echo hello" };

        let output = ShellTool::new(
            if cfg!(windows) {
                ShellConfig::POWERSHELL
            } else {
                ShellConfig::BASH
            },
            Arc::new(LocalShell),
        )
        .run(json!({ "command": command }), &context(&dir))
        .await;

        assert!(!output.is_error, "{}", output.text);
        assert!(output.text.to_lowercase().contains("hello"), "{}", output.text);
    }
}
