//! Shared platform rules for terminating the process tree owned by a command.

#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(unix)]
fn kill_group(pid: u32) {
    // SAFETY: callers provide the leader of a process group they created.
    let _ = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
}

#[cfg(windows)]
fn kill_command(pid: u32) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let root = std::env::var_os("SystemRoot").map_or_else(
        || std::path::PathBuf::from(r"C:\Windows"),
        std::path::PathBuf::from,
    );
    let mut command = std::process::Command::new(root.join("System32").join("taskkill.exe"));
    command
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}

pub(crate) async fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    #[cfg(unix)]
    kill_group(pid);
    #[cfg(windows)]
    let _ = tokio::process::Command::from(kill_command(pid))
        .status()
        .await;
}
