//! Bounded raw output capture; large results spill to a private artifact.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::AsyncWriteExt;

use crate::DEFAULT_MAX_BYTES;

const MEMORY_LIMIT: usize = DEFAULT_MAX_BYTES * 2;
/// Per-command disk budget. Exceeding it stops the command rather than filling the disk.
pub(crate) const DISK_LIMIT: u64 = 100 * 1024 * 1024;
static NEXT_OUTPUT: AtomicU64 = AtomicU64::new(0);

pub(crate) struct OutputAccumulator {
    tail: VecDeque<u8>,
    total: u64,
    artifact: Option<(PathBuf, tokio::fs::File)>,
    directory: PathBuf,
}

impl OutputAccumulator {
    pub(crate) fn new(directory: &Path) -> Self {
        Self {
            tail: VecDeque::new(),
            total: 0,
            artifact: None,
            directory: directory.into(),
        }
    }

    pub(crate) async fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.total.saturating_add(bytes.len() as u64) > DISK_LIMIT {
            return Err(io::Error::other(
                "command output exceeded the 100 MiB disk budget",
            ));
        }
        if self.artifact.is_none() && self.total + bytes.len() as u64 > MEMORY_LIMIT as u64 {
            tokio::fs::create_dir_all(&self.directory).await?;
            let (path, mut file) = loop {
                let sequence = NEXT_OUTPUT.fetch_add(1, Ordering::Relaxed);
                let path = self.directory.join(format!(
                    "solaris-output-{}-{sequence}.log",
                    std::process::id()
                ));
                let mut options = tokio::fs::OpenOptions::new();
                options.create_new(true).write(true);
                #[cfg(unix)]
                options.mode(0o600);
                match options.open(&path).await {
                    Ok(file) => break (path, file),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error),
                }
            };
            file.write_all(self.tail.make_contiguous()).await?;
            self.artifact = Some((path, file));
        }
        if let Some((_, file)) = self.artifact.as_mut() {
            file.write_all(bytes).await?;
        }
        self.total += bytes.len() as u64;
        for byte in bytes {
            self.tail.push_back(*byte);
        }
        while self.tail.len() > MEMORY_LIMIT {
            self.tail.pop_front();
        }
        Ok(())
    }

    pub(crate) async fn finish(mut self) -> io::Result<(String, Option<PathBuf>)> {
        let path = if let Some((path, mut file)) = self.artifact {
            file.flush().await?;
            Some(path)
        } else {
            None
        };
        // Removing a prefix can split the first UTF-8 scalar. Drop continuation
        // bytes at that boundary; decoding the complete tail preserves chunk splits.
        if path.is_some() {
            while self.tail.front().is_some_and(|byte| byte & 0xc0 == 0x80) {
                self.tail.pop_front();
            }
        }
        Ok((
            String::from_utf8_lossy(self.tail.make_contiguous()).into_owned(),
            path,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn large_output_has_a_bounded_tail_and_complete_artifact() {
        let directory = std::env::temp_dir().join(format!(
            "solaris-capture-{}",
            NEXT_OUTPUT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut capture = OutputAccumulator::new(&directory);
        let block = "中文".repeat(4096);
        for _ in 0..64 {
            capture.append(block.as_bytes()).await.expect("append");
        }
        assert!(capture.tail.len() <= MEMORY_LIMIT);
        let (tail, path) = capture.finish().await.expect("finish");
        assert!(!tail.contains('\u{fffd}'));
        let path = path.expect("artifact");
        assert_eq!(
            tokio::fs::metadata(path).await.expect("stat").len(),
            (block.len() * 64) as u64
        );
        tokio::fs::remove_dir_all(directory).await.expect("cleanup");
    }

    #[tokio::test]
    async fn split_utf8_is_preserved() {
        let mut capture = OutputAccumulator::new(&std::env::temp_dir());
        for byte in "中文".as_bytes() {
            capture.append(&[*byte]).await.expect("append");
        }
        assert_eq!(capture.finish().await.expect("finish").0, "中文");
    }
}
