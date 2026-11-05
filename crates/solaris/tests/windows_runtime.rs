//! Startup failures must identify an incomplete Windows distribution.
#![cfg(windows)]

#[test]
fn complete_distribution_does_not_require_writing_to_its_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("solaris.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_solaris"), &executable).unwrap();
    let runtime = directory.path().join("plugins/windows/cygwin");
    for relative in ["bin/bash.exe", "bin/cygwin1.dll", "etc/profile"] {
        let file = runtime.join(relative);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, []).unwrap();
    }
    // A file at this path makes the old create_dir_all(root/tmp) fail even
    // when the test runner has permission to write the installation tree.
    std::fs::write(runtime.join("tmp"), "not a directory").unwrap();
    let output = std::process::Command::new(executable)
        .arg("--print-config")
        .env("SOLARIS_CONFIG_DIR", directory.path().join("config"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(runtime.join("tmp").is_file());
}

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
