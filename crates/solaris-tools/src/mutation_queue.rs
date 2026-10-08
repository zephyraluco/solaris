//! Serialising writes to the same file.
//!
//! One turn can carry several calls at once — the model may well ask for two
//! edits to one file in a single message — and each of them reads the file,
//! changes it, and writes it back. Whichever write lands second would win, and
//! the other change would vanish. Holding a lock per path keeps those two in
//! order without making unrelated files wait for each other.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

/// One lock per path.
#[derive(Debug, Default)]
pub struct FileMutationQueue {
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl FileMutationQueue {
    /// A queue with nothing held.
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `f` with `path`'s lock held, waiting for whoever holds it first.
    pub async fn with<T>(&self, path: &Path, f: impl Future<Output = T>) -> T {
        let lock = {
            let mut locks = self.locks.lock().await;
            Arc::clone(locks.entry(path.to_path_buf()).or_default())
        };
        let _guard = lock.lock().await;
        f.await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn two_writes_to_one_path_do_not_interleave() {
        let queue = Arc::new(FileMutationQueue::new());
        let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));

        let mut tasks = Vec::new();
        for step in ["first", "second"] {
            let queue = Arc::clone(&queue);
            let log = Arc::clone(&log);
            tasks.push(tokio::spawn(async move {
                queue
                    .with(Path::new("same.txt"), async {
                        log.lock().await.push(step);
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        log.lock().await.push("done");
                    })
                    .await;
            }));
        }
        for task in tasks {
            task.await.expect("task joined");
        }

        // Either order is fine; what must never happen is one closure running
        // inside the other's critical section.
        let log = log.lock().await.clone();
        assert!(
            log == vec!["first", "done", "second", "done"]
                || log == vec!["second", "done", "first", "done"],
            "{log:?}"
        );
    }

    #[tokio::test]
    async fn the_closure_result_comes_back() {
        let queue = FileMutationQueue::new();
        let value = queue.with(Path::new("a.txt"), async { 7 }).await;
        assert_eq!(value, 7);
    }
}
