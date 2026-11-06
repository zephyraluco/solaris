use serde_json::json;
use solaris_tools::environment::{ExecutionEnvironment, display_path};
use solaris_tools::{ToolContext, ToolRegistry};

#[tokio::test]
async fn file_tools_share_the_shell_path_namespace_and_undo() {
    let directory = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(directory.path(), directory.path().join("state"));
    let registry = ToolRegistry::builtin();
    let file = directory.path().join("space 中文 'file");
    let path = display_path(&file);
    let written = registry
        .get("write")
        .unwrap()
        .run(json!({"path":path,"content":"before\r\n"}), &ctx)
        .await;
    assert!(!written.is_error, "{}", written.text);
    let edited = registry
        .get("edit")
        .unwrap()
        .run(
            json!({"path":path,"edits":[{"oldText":"before","newText":"after"}]}),
            &ctx,
        )
        .await;
    assert!(!edited.is_error, "{}", edited.text);
    let read = registry
        .get("read")
        .unwrap()
        .run(json!({"path":path}), &ctx)
        .await;
    assert!(!read.is_error, "{}", read.text);
    assert!(read.text.contains("after"));
    let listed = registry
        .get("ls")
        .unwrap()
        .run(json!({"path":display_path(directory.path())}), &ctx)
        .await;
    assert!(!listed.is_error, "{}", listed.text);
    assert!(listed.text.contains("space 中文 'file"));
    let id = edited.metadata.details["change"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let state = ctx.temp_dir.clone();
    tokio::task::spawn_blocking(move || solaris_tools::changes::undo(&state, &id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read(file).unwrap(), b"before\r\n");
}

#[cfg(windows)]
#[tokio::test]
async fn cygwin_search_tools_work_without_host_path_lookup() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("needle.txt"),
        "unique search marker\n",
    )
    .unwrap();
    let ctx = ToolContext::new(directory.path(), directory.path());
    let registry = ToolRegistry::builtin();
    let path = display_path(directory.path());
    assert!(path.starts_with("/cygdrive/"), "{path}");
    for (tool, input) in [
        (
            "grep",
            json!({"path":path,"pattern":"unique search marker"}),
        ),
        ("find", json!({"path":path,"pattern":"*.txt"})),
    ] {
        let result = registry.get(tool).unwrap().run(input, &ctx).await;
        assert!(!result.is_error, "{}", result.text);
        assert!(result.text.contains("needle.txt"), "{}", result.text);
    }
}

#[test]
fn environment_describes_the_execution_contract() {
    let description = ExecutionEnvironment.description();
    if cfg!(windows) {
        assert!(description.contains("Cygwin"));
        assert!(description.contains("/cygdrive/"));
    } else {
        assert!(description.contains("System Bash"));
    }
}

#[cfg(windows)]
#[test]
fn bundled_profile_discards_an_inherited_host_path() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("rg.exe"),
        "not the bundled executable",
    )
    .unwrap();
    let environment = ExecutionEnvironment;
    let output = environment
        .command()
        .unwrap()
        .env("PATH", directory.path())
        .args([
            "-c",
            &environment.bootstrap("command -v rg; rg --version; fd --version"),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("/usr/local/bin/rg"), "{stdout}");
    assert!(stdout.contains("ripgrep"), "{stdout}");
    assert!(stdout.contains("fd "), "{stdout}");
}
