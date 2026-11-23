//! `bash`, `powershell`, and `cmd` — run a command and report what it printed.
//!
//! One implementation, platform-configured shells:
//! - Unix runs `bash -c` by default.
//! - Windows runs PowerShell (`pwsh` → `powershell`, with `-NoLogo -NoProfile
//!   -NonInteractive -Command` and UTF-8 output encoding, falling back to
//!   `cmd /C` if neither PowerShell binary is usable), and also supports `cmd`
//!   or `bash` (auto-detecting Git Bash or an explicit `shell_path`).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::runner::describe_spawn_failure;
use crate::tool::{ExecutionMode, Tool, ToolContext, ToolOutput, optional_count_arg, string_arg};
use crate::truncate::{Limits, format_size, tail};

/// Most seconds a `timeout` argument may ask for.
///
/// The runner arms a millisecond timer with it, so this is the largest value
/// that reaches the timer without wrapping — a longer wait would end early
/// rather than late.
const MAX_TIMEOUT_SECONDS: u64 = 2_147_483;

/// PowerShell flags used for both `pwsh` and `powershell`.
const POWERSHELL_FLAGS: &[&str] = &["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"];

/// UTF-8 prelude prepended to PowerShell commands so Windows PowerShell 5.1 and
/// PowerShell 7 both emit UTF-8 on redirected pipes.
const POWERSHELL_UTF8_PRELUDE: &str = "\
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)\n\
$OutputEncoding = [Console]::OutputEncoding\n";

/// User-supplied shell overrides, independent of the built-in platform defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellOptions {
    /// Explicit shell executable path (e.g. Git Bash or a custom `pwsh.exe`).
    pub shell_path: Option<PathBuf>,
    /// Optional setup snippet prepended to every shell command before execution.
    pub command_prefix: Option<String>,
}

/// Which shell a tool runs commands with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellConfig {
    /// Name the model calls the tool by.
    pub tool_name: &'static str,
    /// One-line description handed to the model.
    pub description: &'static str,
    /// Candidate `(program, args)` pairs, best first. The first usable
    /// executable wins, so Windows uses `pwsh`, then `powershell`, and finally
    /// falls back to `cmd /C`.
    pub programs: &'static [(&'static str, &'static [&'static str])],
}

impl ShellConfig {
    /// `bash`, for Unix — and optionally on Windows via Git Bash or `shell_path`.
    pub const BASH: ShellConfig = ShellConfig {
        tool_name: "bash",
        description: "Run a shell command in the session directory with bash and return everything \
                      it printed, stdout and stderr together. The output is truncated to the last \
                      2000 lines or 50KB; when that happens the full output is written to a file \
                      whose path is reported. Commands that never finish need a `timeout`.",
        programs: &[("bash", &["-c"])],
    };

    /// PowerShell, for Windows, with `cmd /C` fallback if PowerShell is absent.
    pub const POWERSHELL: ShellConfig = ShellConfig {
        tool_name: "powershell",
        description: "Run a command in the session directory with PowerShell (falling back to \
                      cmd.exe if PowerShell is unavailable) and return everything it printed, \
                      stdout and stderr together. The output is truncated to the last 2000 lines \
                      or 50KB; when that happens the full output is written to a file whose path \
                      is reported. Commands that never finish need a `timeout`.",
        programs: &[
            ("pwsh", POWERSHELL_FLAGS),
            ("powershell", POWERSHELL_FLAGS),
            ("cmd", &["/C"]),
        ],
    };

    /// `cmd.exe`, for Windows callers that explicitly want Command Prompt.
    pub const CMD: ShellConfig = ShellConfig {
        tool_name: "cmd",
        description: "Run a command in the session directory with cmd.exe and return everything \
                      it printed, stdout and stderr together. The output is truncated to the last \
                      2000 lines or 50KB; when that happens the full output is written to a file \
                      whose path is reported. Commands that never finish need a `timeout`.",
        programs: &[("cmd", &["/C"])],
    };
}

/// Resolve a `bash` executable path: explicit override first, then Git for
/// Windows standard install locations on Windows, then `PATH`.
pub fn resolve_bash_path(override_path: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = override_path {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        return Err(format!(
            "configured `shell_path` does not exist: {}",
            path.display()
        ));
    }
    #[cfg(windows)]
    {
        for env_var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(root) = std::env::var_os(env_var) {
                let candidate = PathBuf::from(root).join("Git").join("bin").join("bash.exe");
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
        if let Some(found) = find_on_path("bash.exe") {
            return Ok(found);
        }
        Err(
            "no bash shell found — install Git for Windows or set `shell_path` in settings.json"
                .to_string(),
        )
    }
    #[cfg(not(windows))]
    {
        Ok(PathBuf::from("bash"))
    }
}

#[cfg(windows)]
fn find_on_path(executable: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(executable);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn is_powershell_program(program: &Path) -> bool {
    program
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| {
            stem.eq_ignore_ascii_case("pwsh") || stem.eq_ignore_ascii_case("powershell")
        })
}

fn flags_for_program(program: &Path) -> &'static [&'static str] {
    if is_powershell_program(program) {
        POWERSHELL_FLAGS
    } else if program
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case("cmd"))
    {
        &["/C"]
    } else {
        &["-c"]
    }
}

fn prepare_command(program: &Path, command: &str, prefix: Option<&str>) -> String {
    let body = match prefix.map(str::trim).filter(|value| !value.is_empty()) {
        Some(prefix) => format!("{prefix}\n{command}"),
        None => command.to_owned(),
    };
    if cfg!(windows) && is_powershell_program(program) {
        format!("{POWERSHELL_UTF8_PRELUDE}{body}")
    } else {
        body
    }
}

fn should_try_next_candidate(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
    ) {
        return true;
    }
    // Windows Store App Execution Aliases (e.g. WindowsApps\pwsh.exe) can fail
    // with ERROR_CANT_ACCESS_FILE (1920) when inaccessible in restricted hosts.
    #[cfg(windows)]
    if error.raw_os_error() == Some(1920) {
        return true;
    }
    false
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
    /// Private full-output artifact when capture exceeded the memory budget.
    pub artifact: Option<std::path::PathBuf>,
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
#[derive(Debug, Default, Clone)]
pub struct LocalShell {
    options: ShellOptions,
}

impl LocalShell {
    /// A runner with user-configured shell path or command prefix overrides.
    pub fn with_options(options: ShellOptions) -> Self {
        Self { options }
    }

    fn candidates(
        &self,
        config: ShellConfig,
    ) -> Result<Vec<(PathBuf, &'static [&'static str])>, String> {
        if let Some(path) = &self.options.shell_path {
            if !path.is_file() {
                return Err(format!(
                    "configured `shell_path` does not exist: {}",
                    path.display()
                ));
            }
            return Ok(vec![(path.clone(), flags_for_program(path))]);
        }
        if cfg!(windows) && config.tool_name == "bash" {
            let bash = resolve_bash_path(None)?;
            return Ok(vec![(bash, &["-c"])]);
        }
        Ok(config
            .programs
            .iter()
            .map(|(program, flags)| (PathBuf::from(program), *flags))
            .collect())
    }
}

#[async_trait]
impl ShellRunner for LocalShell {
    async fn run(
        &self,
        config: ShellConfig,
        command: &str,
        ctx: &ToolContext,
        timeout: Option<Duration>,
    ) -> Result<ShellOutcome, String> {
        let candidates = self.candidates(config)?;
        let mut last_missing = String::new();

        for (program, flags) in candidates {
            let prepared =
                prepare_command(&program, command, self.options.command_prefix.as_deref());
            let mut builder = Command::new(&program);
            builder
                .args(flags)
                // The command is handed over untouched: appending to it — a
                // `2>&1`, say — rewrites what the model asked for and breaks
                // anything ending in a comment or a continuation.
                .arg(&prepared)
                .current_dir(&ctx.cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                // Both streams are piped and merged here instead. What the shell
                // itself reports about the command only ever reaches its stderr,
                // so a redirect written into the command cannot carry it.
                .stderr(Stdio::piped())
                .kill_on_drop(true);

            #[cfg(windows)]
            builder.creation_flags(crate::process::CREATE_NO_WINDOW);

            // A process group of its own is what lets a timeout or a cancel
            // reach the grandchildren, not just the shell.
            #[cfg(unix)]
            builder.process_group(0);

            let program_label = program.to_string_lossy();
            let mut child = match builder.spawn() {
                Ok(child) => child,
                Err(error) if should_try_next_candidate(&error) => {
                    last_missing = describe_spawn_failure(&program_label, &error);
                    continue;
                }
                Err(error) => return Err(format!("could not run `{program_label}`: {error}")),
            };

            // One reader per pipe, both feeding one channel, so what is reported
            // keeps the order the bytes arrived in rather than every line of
            // stdout followed by every line of stderr.
            let (chunk_tx, mut chunk_rx) = mpsc::channel(16);
            let mut readers = Vec::new();
            if let Some(pipe) = child.stdout.take() {
                readers.push(spawn_reader(pipe, chunk_tx.clone()));
            }
            if let Some(pipe) = child.stderr.take() {
                readers.push(spawn_reader(pipe, chunk_tx.clone()));
            }
            drop(chunk_tx);

            let mut buffer = crate::output::OutputAccumulator::new(&ctx.temp_dir);
            let mut capture_error = None;
            let pid = child.id();
            let mut timed_out = false;
            let mut cancelled = false;
            let mut status = None;
            let mut wait_error = None;
            let mut pipes_open = true;

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
                    chunk = chunk_rx.recv(), if pipes_open => match chunk {
                        // Both pipes are closed, so no more output can arrive.
                        None => pipes_open = false,
                        Some(bytes) => if let Err(error) = buffer.append(&bytes).await {
                            capture_error = Some(error);
                            break;
                        },
                    },
                    // Waiting on the process rather than on its pipes is what
                    // keeps a descendant that inherited them from holding the
                    // tool open: the command is over either way.
                    waited = child.wait() => {
                        match waited {
                            Ok(exit) => status = Some(exit),
                            Err(error) => wait_error = Some(error),
                        }
                        break;
                    }
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

            if timed_out || cancelled || capture_error.is_some() {
                crate::process::kill_tree(pid).await;
                let _ = child.kill().await;
                let _ = child.wait().await;
            }

            // Take what the readers already hold, then let them go: the last
            // words of a command that has ended are worth having, but a pipe an
            // escaped grandchild still holds must not keep the turn running.
            if let Err(error) = collect_pending(&mut chunk_rx, &mut buffer).await {
                capture_error = Some(error);
            }
            for reader in readers {
                reader.abort();
                let _ = reader.await;
            }

            if let Some(error) = wait_error {
                // What the process is doing is unknown, so stop it rather than
                // leave it running behind a failed call.
                crate::process::kill_tree(child.id()).await;
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(format!("could not wait for `{program_label}`: {error}"));
            }

            if let Some(error) = capture_error {
                return Err(error.to_string());
            }
            let (output, artifact) = buffer.finish().await.map_err(|error| error.to_string())?;
            return Ok(ShellOutcome {
                output: sanitize(&output),
                artifact,
                code: exit_code(status),
                timed_out,
                cancelled,
            });
        }

        Err(if last_missing.is_empty() {
            format!("no shell configured for `{}`", config.tool_name)
        } else {
            last_missing
        })
    }
}

/// Read one of a command's pipes to its end, forwarding what it yields.
fn spawn_reader(
    mut pipe: impl AsyncRead + Unpin + Send + 'static,
    chunks: mpsc::Sender<Vec<u8>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut block = [0u8; 8 * 1024];
        loop {
            match pipe.read(&mut block).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if chunks.send(block[..read].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    })
}

/// Take whatever the readers have already handed over, briefly.
///
/// Called once the command has ended. Both pipes close with it, so this usually
/// returns straight away; the wait is capped so that a descendant which
/// inherited them cannot hold the turn open.
async fn collect_pending(
    chunks: &mut mpsc::Receiver<Vec<u8>>,
    buffer: &mut crate::output::OutputAccumulator,
) -> std::io::Result<()> {
    let drain = async {
        while let Some(bytes) = chunks.recv().await {
            buffer.append(&bytes).await?;
        }
        Ok(())
    };
    tokio::time::timeout(Duration::from_millis(250), drain)
        .await
        .unwrap_or(Ok(()))
}

fn exit_code(status: Option<std::process::ExitStatus>) -> Option<i32> {
    let status = status?;
    if let Some(code) = status.code() {
        return Some(code);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Some(128 + signal);
        }
    }

    None
}

/// Terminal output, with the parts that are not text taken out.
///
/// A command that colours its output, redraws a progress bar or dumps binary
/// emits escape sequences and control characters. They mean something to a
/// terminal and nothing to the model: left in, they misalign the transcript and
/// spend the output budget on noise.
fn sanitize(output: &str) -> String {
    let mut clean = String::with_capacity(output.len());
    let mut line_start = 0;
    let mut returned = false;
    let mut chars = output.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            // An escape sequence, in the shapes that turn up in command output.
            '\u{1b}' => match chars.next() {
                // A control sequence: `ESC [ … ` up to a byte in `@`..`~`.
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                // An operating system command: `ESC ] … ` to BEL or `ESC \`.
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' {
                            chars.next();
                            break;
                        }
                    }
                }
                // Any other escape is two characters long.
                _ => {}
            },
            '\r' => {
                // A carriage return sends the cursor back to the start of the
                // line, so whatever is drawn next replaces what is there: a
                // progress bar ends up saying what it last said instead of
                // every frame of it. What follows decides whether anything is
                // replaced at all, so the replacement waits for it — text a
                // trailing return leaves on screen is still text.
                if chars.peek() != Some(&'\n') {
                    returned = true;
                }
            }
            ch if is_control(ch) => {}
            ch => {
                if returned {
                    clean.truncate(line_start);
                    returned = false;
                }
                clean.push(ch);
                if ch == '\n' {
                    line_start = clean.len();
                }
            }
        }
    }

    clean
}

/// Whether `ch` is a control character that carries nothing to read.
///
/// Tab and newline are not: they are how text is laid out. U+FFF9..FFFB go too,
/// because they break the width measurement the display uses.
fn is_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{0}'..='\u{8}' | '\u{b}'..='\u{c}' | '\u{e}'..='\u{1f}' | '\u{fff9}'..='\u{fffb}'
    )
}

/// Runs commands with a shell.
#[derive(Clone)]
pub struct ShellTool {
    config: ShellConfig,
    runner: Arc<dyn ShellRunner>,
    limits: Limits,
}

impl ShellTool {
    /// A shell tool backed by `config` and `runner`.
    pub fn new(config: ShellConfig, runner: Arc<dyn ShellRunner>) -> Self {
        Self {
            config,
            runner,
            limits: Limits::default(),
        }
    }

    /// The `bash` tool, for Unix — or Windows with Git Bash / `shell_path`.
    pub fn bash(runner: Arc<dyn ShellRunner>) -> Self {
        Self::new(ShellConfig::BASH, runner)
    }

    /// The `powershell` tool, for Windows (with `cmd /C` fallback).
    pub fn powershell(runner: Arc<dyn ShellRunner>) -> Self {
        Self::new(ShellConfig::POWERSHELL, runner)
    }

    /// The `cmd` tool, for explicit Command Prompt execution on Windows.
    pub fn cmd(runner: Arc<dyn ShellRunner>) -> Self {
        Self::new(ShellConfig::CMD, runner)
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

    /// A command can change anything, and two of them racing over the same
    /// directory is exactly the trouble this avoids, so it runs alone.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    fn description(&self) -> &str {
        self.config.description
    }

    fn guideline(&self) -> Option<&'static str> {
        Some(
            "Commands run in the session's working directory and their output is returned to \
             you. Prefer one command that prints everything you need over several small ones, \
             and set `timeout` for anything that might hang.",
        )
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
                    "description": format!(
                        "Seconds to let the command run before killing it; omit for no limit, at \
                         most {MAX_TIMEOUT_SECONDS}"
                    ),
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
            Ok(Some(seconds)) if seconds as u64 > MAX_TIMEOUT_SECONDS => {
                return ToolOutput::error(format!(
                    "`timeout` must be at most {MAX_TIMEOUT_SECONDS} seconds"
                ));
            }
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
        let mut artifact_path = None;
        if truncated || outcome.artifact.is_some() {
            let saved = match outcome.artifact {
                Some(path) => Ok(path),
                None => crate::output::save_output(&ctx.temp_dir, outcome.output.as_bytes())
                    .await
                    .map_err(|error| error.to_string()),
            };
            match saved {
                Ok(path) => {
                    notices.push(format!(
                        "output truncated to the last {} lines / {}; full output at {}",
                        self.limits.max_lines,
                        format_size(self.limits.max_bytes),
                        crate::environment::display_path(&path)
                    ));
                    artifact_path = Some(path);
                }
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
                timeout
                    .map(|duration| duration.as_secs())
                    .unwrap_or_default()
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
        let mut output = ToolOutput::ok(text);
        output.metadata.status = if outcome.cancelled {
            crate::ToolStatus::Cancelled
        } else if outcome.timed_out {
            crate::ToolStatus::TimedOut
        } else if outcome.code != Some(0) {
            crate::ToolStatus::Failed
        } else {
            crate::ToolStatus::Success
        };
        output.is_error = output.metadata.status != crate::ToolStatus::Success;
        output.metadata.details = json!({"exit_code": outcome.code});
        if let Some(path) = artifact_path {
            output.metadata.artifacts.push(crate::Artifact {
                path,
                media_type: "text/plain".into(),
            });
            output.metadata.truncation = Some(crate::TruncationInfo {
                max_bytes: self.limits.max_bytes,
                max_lines: self.limits.max_lines,
                kept: "tail".into(),
            });
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_dir;
    use std::path::Path;

    /// A runner that returns one canned outcome and records the call.
    struct StubShell {
        outcome: ShellOutcome,
        seen: std::sync::Mutex<Vec<(ShellConfig, String, Option<Duration>)>>,
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
            config: ShellConfig,
            command: &str,
            _ctx: &ToolContext,
            timeout: Option<Duration>,
        ) -> Result<ShellOutcome, String> {
            self.seen
                .lock()
                .expect("lock")
                .push((config, command.to_string(), timeout));
            Ok(self.outcome.clone())
        }
    }

    fn context(dir: &Path) -> ToolContext {
        ToolContext::new(dir, dir)
    }

    fn platform_shell(runner: Arc<dyn ShellRunner>) -> ShellTool {
        if cfg!(windows) {
            ShellTool::powershell(runner)
        } else {
            ShellTool::bash(runner)
        }
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
        assert_eq!(seen[0].0, ShellConfig::BASH);
        assert_eq!(seen[0].1, "echo hello");
        assert_eq!(seen[0].2, None, "no timeout was asked for");
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_reported_as_a_tool_failure() {
        let dir = test_dir("shell-exit");
        let runner = StubShell::new(ShellOutcome {
            output: "boom\n".to_string(),
            code: Some(3),
            ..Default::default()
        });

        let output = ShellTool::bash(runner)
            .run(json!({ "command": "false" }), &context(&dir))
            .await;

        assert!(
            output.is_error,
            "a nonzero exit remains a tool error, not a failed turn"
        );
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
            artifact: None,
        });

        let output = ShellTool::bash(runner.clone())
            .run(
                json!({ "command": "sleep 99", "timeout": 2 }),
                &context(&dir),
            )
            .await;

        assert!(output.text.contains("killed"), "{}", output.text);
        let seen = runner.seen.lock().expect("lock");
        assert_eq!(seen[0].2, Some(Duration::from_secs(2)));
    }

    #[tokio::test]
    async fn a_cancelled_command_says_so() {
        let dir = test_dir("shell-cancel");
        let runner = StubShell::new(ShellOutcome {
            output: String::new(),
            code: None,
            timed_out: false,
            cancelled: true,
            artifact: None,
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

        assert!(
            output.text.starts_with("line 98\nline 99\nline 100"),
            "{}",
            output.text
        );
        let notice = output.text.split('[').nth(1).expect("a notice");
        assert!(notice.contains("full output at"), "{notice}");

        let saved = notice
            .split("full output at ")
            .nth(1)
            .expect("a path")
            .trim_end_matches(']');
        let saved = crate::tools::read::ReadTool::new()
            .run(json!({"path":saved}), &context(&dir))
            .await;
        assert!(!saved.is_error, "{}", saved.text);
        assert!(saved.text.contains("line 1\n"), "the whole output is there");
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
    fn shell_configs_declare_their_names() {
        let bash = ShellTool::bash(StubShell::new(ShellOutcome::default()));
        let powershell = ShellTool::powershell(StubShell::new(ShellOutcome::default()));
        let cmd = ShellTool::cmd(StubShell::new(ShellOutcome::default()));
        assert_eq!(bash.name(), "bash");
        assert_eq!(powershell.name(), "powershell");
        assert_eq!(cmd.name(), "cmd");
    }

    #[test]
    fn explicit_missing_shell_path_is_reported() {
        let runner = LocalShell::with_options(ShellOptions {
            shell_path: Some(PathBuf::from("/path/that/does/not/exist/bash")),
            command_prefix: None,
        });
        let error = runner
            .candidates(ShellConfig::BASH)
            .expect_err("missing explicit shell_path must fail");
        assert!(error.contains("shell_path"), "{error}");
    }

    #[test]
    fn command_prefix_is_prepended_before_the_command() {
        let prepared = prepare_command(
            Path::new("bash"),
            "echo hi",
            Some("shopt -s expand_aliases"),
        );
        assert_eq!(prepared, "shopt -s expand_aliases\necho hi");
    }

    #[tokio::test]
    async fn a_real_command_runs_and_reports_its_output() {
        // Exercise actual process execution rather than a canned runner.
        let dir = test_dir("shell-real");
        let output = platform_shell(Arc::new(LocalShell::default()))
            .run(json!({ "command": "echo hello" }), &context(&dir))
            .await;

        assert!(!output.is_error, "{}", output.text);
        assert!(
            output.text.to_lowercase().contains("hello"),
            "{}",
            output.text
        );
    }

    #[tokio::test]
    async fn shell_preserves_unicode_working_directory_and_utf8_output() {
        let dir = test_dir("shell workspace 中文");
        std::fs::write(dir.join("probe 中文.txt"), "中文 marker").unwrap();
        let command = if cfg!(windows) {
            "Get-Content -Encoding UTF8 -Raw 'probe 中文.txt'"
        } else {
            "cat 'probe 中文.txt'; cat <<'EOF'\nhere document marker\nEOF"
        };
        let output = platform_shell(Arc::new(LocalShell::default()))
            .run(json!({ "command": command }), &context(&dir))
            .await;
        assert!(!output.is_error, "{}", output.text);
        assert!(output.text.contains("中文 marker"), "{}", output.text);
    }

    #[tokio::test]
    async fn command_prefix_runs_before_user_commands() {
        let dir = test_dir("shell-prefix");
        let (prefix, command) = if cfg!(windows) {
            (
                "$env:SOLARIS_PREFIX_TEST = 'prefix-ok'",
                "Write-Output $env:SOLARIS_PREFIX_TEST",
            )
        } else {
            (
                "SOLARIS_PREFIX_TEST=prefix-ok",
                "printf '%s' \"$SOLARIS_PREFIX_TEST\"",
            )
        };
        let runner = Arc::new(LocalShell::with_options(ShellOptions {
            shell_path: None,
            command_prefix: Some(prefix.into()),
        }));
        let output = platform_shell(runner)
            .run(json!({ "command": command }), &context(&dir))
            .await;
        assert!(!output.is_error, "{}", output.text);
        assert!(output.text.contains("prefix-ok"), "{}", output.text);
    }

    #[tokio::test]
    async fn shell_reports_nonzero_exit_codes() {
        let dir = test_dir("shell-nonzero");
        let config = if cfg!(windows) {
            ShellConfig::POWERSHELL
        } else {
            ShellConfig::BASH
        };
        let outcome = LocalShell::default()
            .run(
                config,
                "exit 7",
                &context(&dir),
                Some(Duration::from_secs(10)),
            )
            .await
            .unwrap();
        assert_eq!(outcome.code, Some(7));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn cmd_shell_runs_commands_and_reports_exit_codes() {
        let dir = test_dir("shell-cmd");
        let output = ShellTool::cmd(Arc::new(LocalShell::default()))
            .run(json!({ "command": "echo cmd-ok" }), &context(&dir))
            .await;
        assert!(!output.is_error, "{}", output.text);
        assert!(output.text.contains("cmd-ok"), "{}", output.text);

        let outcome = LocalShell::default()
            .run(
                ShellConfig::CMD,
                "exit /b 9",
                &context(&dir),
                Some(Duration::from_secs(10)),
            )
            .await
            .unwrap();
        assert_eq!(outcome.code, Some(9));
    }

    #[test]
    fn sanitize_keeps_the_text_and_drops_the_terminal_commands() {
        // Colour, then an erase-line, both as control sequences.
        assert_eq!(sanitize("\u{1b}[31mred\u{1b}[0m\u{1b}[K"), "red");
        // A bar that redraws itself says what it last said.
        assert_eq!(sanitize("50%\r100%\n"), "100%\n");
        assert_eq!(sanitize("one\rtwo\rthree"), "three");
        // But a return with nothing drawn over it is not an erasure.
        assert_eq!(sanitize("progress\r"), "progress");
        // `\r\n` is a line ending, not a cursor return.
        assert_eq!(sanitize("a\r\nb\r\n"), "a\nb\n");
        // Bell and NUL carry nothing; tab and newline are how text is laid out.
        assert_eq!(sanitize("a\u{7}b\u{0}c\td\ne"), "abc\td\ne");
        // A window title goes with its terminator.
        assert_eq!(sanitize("\u{1b}]0;title\u{7}after"), "after");
        // U+FFF9..FFFB break the width measurement the display uses.
        assert_eq!(sanitize("a\u{fff9}b"), "ab");
        // A lone escape at the end is dropped rather than read past.
        assert_eq!(sanitize("x\u{1b}"), "x");
    }

    #[tokio::test]
    async fn a_timeout_past_the_ceiling_is_refused() {
        let dir = test_dir("shell-timeout-ceiling");
        let runner = StubShell::new(ShellOutcome::default());

        let output = ShellTool::bash(runner.clone())
            .run(
                json!({ "command": "sleep 1", "timeout": MAX_TIMEOUT_SECONDS + 1 }),
                &context(&dir),
            )
            .await;

        assert!(output.is_error);
        assert!(output.text.contains("at most"), "{}", output.text);
        assert!(
            runner.seen.lock().expect("lock").is_empty(),
            "a refused timeout must not run the command"
        );
    }

    #[tokio::test]
    async fn a_timeout_at_the_ceiling_is_accepted() {
        let dir = test_dir("shell-timeout-max");
        let runner = StubShell::new(ShellOutcome::default());

        ShellTool::bash(runner.clone())
            .run(
                json!({ "command": "sleep 1", "timeout": MAX_TIMEOUT_SECONDS }),
                &context(&dir),
            )
            .await;

        let seen = runner.seen.lock().expect("lock");
        assert_eq!(seen[0].2, Some(Duration::from_secs(MAX_TIMEOUT_SECONDS)));
    }

    #[test]
    fn a_process_that_never_ran_has_no_exit_code() {
        assert_eq!(exit_code(None), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_process_killed_by_a_signal_reports_the_shell_convention() {
        use std::os::unix::process::ExitStatusExt;

        // Raw status 9 is "terminated by SIGKILL", which a shell calls 137.
        let status = std::process::ExitStatus::from_raw(9);
        assert_eq!(exit_code(Some(status)), Some(137));
    }

    #[tokio::test]
    async fn stderr_is_reported_even_when_the_command_ends_with_a_comment() {
        // Merging the streams by appending `2>&1` to the command would put the
        // redirect inside the comment, and what the shell itself said about the
        // command would be lost with it.
        let dir = test_dir("shell-stderr");
        let command = if cfg!(windows) {
            "Write-Output hello; [Console]::Error.WriteLine('boom') # why"
        } else {
            "echo hello; echo boom 1>&2 # why"
        };
        let output = platform_shell(Arc::new(LocalShell::default()))
            .run(json!({ "command": command }), &context(&dir))
            .await;

        assert!(!output.is_error, "{}", output.text);
        assert!(output.text.contains("hello"), "{}", output.text);
        assert!(output.text.contains("boom"), "{}", output.text);
    }
}
