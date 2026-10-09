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

/// Keeps a program spawned from a windowed parent from opening a console of its
/// own, which flashes on screen for every command.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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
            let mut builder = Command::new(program);
            builder
                .arg(config.command_flag)
                // The command is handed over untouched: appending to it — a
                // `2>&1`, say — rewrites what the model asked for and breaks
                // anything ending in a comment or a continuation.
                .arg(command)
                .current_dir(&ctx.cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                // Both streams are piped and merged here instead. What the shell
                // itself reports about the command only ever reaches its stderr,
                // so a redirect written into the command cannot carry it.
                .stderr(Stdio::piped());

            #[cfg(windows)]
            builder.creation_flags(CREATE_NO_WINDOW);

            // A process group of its own is what lets a timeout or a cancel
            // reach the grandchildren, not just the shell.
            #[cfg(unix)]
            builder.process_group(0);

            let mut child = match builder.spawn() {
                Ok(child) => child,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    last_missing = describe_spawn_failure(program, &error);
                    continue;
                }
                Err(error) => return Err(format!("could not run {program}: {error}")),
            };

            // One reader per pipe, both feeding one channel, so what is reported
            // keeps the order the bytes arrived in rather than every line of
            // stdout followed by every line of stderr.
            let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel();
            let mut readers = Vec::new();
            if let Some(pipe) = child.stdout.take() {
                readers.push(spawn_reader(pipe, chunk_tx.clone()));
            }
            if let Some(pipe) = child.stderr.take() {
                readers.push(spawn_reader(pipe, chunk_tx.clone()));
            }
            drop(chunk_tx);

            let mut buffer = Vec::new();
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
                        Some(bytes) => buffer.extend_from_slice(&bytes),
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

            if timed_out || cancelled {
                kill_tree(child.id()).await;
                let _ = child.kill().await;
                let _ = child.wait().await;
            }

            // Take what the readers already hold, then let them go: the last
            // words of a command that has ended are worth having, but a pipe an
            // escaped grandchild still holds must not keep the turn running.
            collect_pending(&mut chunk_rx, &mut buffer).await;
            for reader in readers {
                reader.abort();
            }

            if let Some(error) = wait_error {
                // What the process is doing is unknown, so stop it rather than
                // leave it running behind a failed call.
                kill_tree(child.id()).await;
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(format!("could not wait for {program}: {error}"));
            }

            return Ok(ShellOutcome {
                output: sanitize(&String::from_utf8_lossy(&buffer)),
                code: exit_code(status),
                timed_out,
                cancelled,
            });
        }

        Err(last_missing)
    }
}

/// Read one of a command's pipes to its end, forwarding what it yields.
fn spawn_reader(
    mut pipe: impl AsyncRead + Unpin + Send + 'static,
    chunks: mpsc::UnboundedSender<Vec<u8>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut block = [0u8; 8 * 1024];
        loop {
            match pipe.read(&mut block).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if chunks.send(block[..read].to_vec()).is_err() {
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
async fn collect_pending(chunks: &mut mpsc::UnboundedReceiver<Vec<u8>>, buffer: &mut Vec<u8>) {
    let drain = async {
        while let Some(bytes) = chunks.recv().await {
            buffer.extend_from_slice(&bytes);
        }
    };
    let _ = tokio::time::timeout(Duration::from_millis(50), drain).await;
}

/// Kill a command and everything it started.
///
/// `child.kill()` reaches the shell itself; a command that spawned something
/// would leave it running.
async fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };

    #[cfg(windows)]
    {
        // `taskkill.exe` by full path out of System32: stopping a runaway
        // command should not depend on what PATH happens to point at.
        let taskkill = std::env::var_os("SystemRoot")
            .map_or_else(
                || std::path::PathBuf::from(r"C:\Windows"),
                std::path::PathBuf::from,
            )
            .join("System32")
            .join("taskkill.exe");
        let _ = Command::new(taskkill)
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }

    #[cfg(unix)]
    {
        // The command leads its own process group (see the spawn above), so the
        // negated pid reaches every process it started.
        // SAFETY: the pid belongs to a child this process started, and the
        // outcome of the kill is deliberately ignored.
        let _ = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
}

/// The exit code to report, using the shell convention for a signalled process.
///
/// A process killed by a signal has no exit code of its own; `128 + signal` is
/// what a shell reports, and it keeps a kill from reading as a success.
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

    /// A command can change anything, and two of them racing over the same
    /// directory is exactly the trouble this avoids, so it runs alone.
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
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
        ToolOutput::ok(text)
    }
}

/// Save the whole output somewhere the model can read it in pieces.
async fn write_full_output(output: &str, ctx: &ToolContext) -> Result<std::path::PathBuf, String> {
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = ctx.temp_dir.join(format!(
        "solaris-output-{}-{unique}.log",
        std::process::id()
    ));

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
            .run(
                json!({ "command": "sleep 99", "timeout": 2 }),
                &context(&dir),
            )
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
        let command = if cfg!(windows) {
            "Write-Output hello"
        } else {
            "echo hello"
        };

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
        assert!(
            output.text.to_lowercase().contains("hello"),
            "{}",
            output.text
        );
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
        assert_eq!(seen[0].1, Some(Duration::from_secs(MAX_TIMEOUT_SECONDS)));
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
        let (config, command) = if cfg!(windows) {
            (
                ShellConfig::POWERSHELL,
                "Write-Output hello; Write-Error boom # why",
            )
        } else {
            (ShellConfig::BASH, "echo hello; echo boom 1>&2 # why")
        };

        let output = ShellTool::new(config, Arc::new(LocalShell))
            .run(json!({ "command": command }), &context(&dir))
            .await;

        assert!(!output.is_error, "{}", output.text);
        assert!(output.text.contains("hello"), "{}", output.text);
        assert!(output.text.contains("boom"), "{}", output.text);
    }

    /// The command a real-shell test runs when it must outlive its waiting.
    fn a_command_that_never_finishes() -> (ShellConfig, &'static str) {
        if cfg!(windows) {
            (ShellConfig::POWERSHELL, "Start-Sleep -Seconds 30")
        } else {
            (ShellConfig::BASH, "sleep 30")
        }
    }

    #[tokio::test]
    async fn a_real_command_that_outlives_its_timeout_is_killed_and_returns() {
        let dir = test_dir("shell-real-timeout");
        let (config, command) = a_command_that_never_finishes();
        let started = std::time::Instant::now();

        let output = ShellTool::new(config, Arc::new(LocalShell))
            .run(json!({ "command": command, "timeout": 1 }), &context(&dir))
            .await;

        assert!(output.text.contains("killed"), "{}", output.text);
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the command was killed rather than waited out: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_real_command_is_killed_when_the_turn_is_cancelled() {
        let dir = test_dir("shell-real-cancel");
        let (config, command) = a_command_that_never_finishes();
        let ctx = context(&dir);

        let tool = ShellTool::new(config, Arc::new(LocalShell));
        let running = tool.run(json!({ "command": command }), &ctx);
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            cancel.cancel();
        });

        let output = run_with_deadline(running).await;
        assert!(output.text.contains("cancelled"), "{}", output.text);
    }

    /// Poll `work` to its end, giving up on it rather than hanging a test run.
    async fn run_with_deadline(work: impl std::future::Future<Output = ToolOutput>) -> ToolOutput {
        tokio::time::timeout(Duration::from_secs(15), work)
            .await
            .expect("the command was killed rather than waited out")
    }
}
