use serde_json::json;
use solaris_tools::environment::{ExecutionEnvironment, display_path};
use solaris_tools::{SHELL_TOOL, ToolContext, ToolRegistry};

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

#[test]
fn environment_describes_the_execution_contract() {
    let description = ExecutionEnvironment.description();
    if cfg!(windows) {
        assert!(description.contains("PowerShell"));
        assert!(description.contains("cmd.exe"));
        assert_eq!(SHELL_TOOL, "powershell");
    } else {
        assert!(description.contains("System Bash"));
        assert_eq!(SHELL_TOOL, "bash");
    }
}
