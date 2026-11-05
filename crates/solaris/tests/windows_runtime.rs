//! Startup failures must identify an incomplete Windows distribution.
#![cfg(windows)]

#[test]
fn standalone_executable_requires_its_bundled_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("solaris.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_solaris"), &executable).unwrap();
    let output = std::process::Command::new(executable)
        .arg("--print-config")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("Cygwin runtime not found"), "{error}");
    assert!(
        error.contains("plugins/windows/cygwin") || error.contains(r"plugins\windows\cygwin"),
        "{error}"
    );
}
