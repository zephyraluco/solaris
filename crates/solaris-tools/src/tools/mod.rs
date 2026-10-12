//! The built-in tools, one module each.

pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod shell;
pub mod write;

/// A fresh directory under the system temp dir for one test.
///
/// Each call gets its own name, so tests never share a file and can run in
/// parallel.
#[cfg(test)]
pub(crate) fn test_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "solaris-tools-{}-{tag}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}
