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
use std::sync::{Arc, Weak};

use tokio::sync::Mutex;

/// One lock per path.
///
/// The table holds the locks weakly, so a path's lock lives exactly as long as
/// somebody is using it. While a caller holds one — running or still waiting
/// for its turn — every later caller for that path upgrades to the same lock;
/// once the last of them finishes, the entry is swept away. Nothing else bounds
/// the table, because a session may touch any number of paths.
#[derive(Debug, Default)]
pub struct FileMutationQueue {
    locks: Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>,
}

impl FileMutationQueue {
    /// A queue with nothing held.
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `f` with `path`'s lock held, waiting for whoever holds it first.
    ///
    /// `f` is called once the lock is held, so nothing it captures runs before
    /// its turn.
    pub async fn with_lock<T, F, Fut>(&self, path: &Path, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let lock = self.lock_for(path).await;
        let _guard = lock.lock().await;
        f().await
    }

    /// `path`'s lock, making one when none is in use.
    ///
    /// A failed upgrade is what makes handing out a fresh lock safe: it means no
    /// caller holds a lock for this path, running or waiting, so the new one
    /// cannot overlap anybody.
    async fn lock_for(&self, path: &Path) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
            return lock;
        }

        // The entry is missing or dead. Sweeping while the table is locked costs
        // nothing on the hot path, and a miss is the only moment it is needed.
        locks.retain(|_, lock| lock.strong_count() > 0);

        let lock = Arc::new(Mutex::new(()));
        locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
        lock
    }

    /// How many paths the table still tracks.
    ///
    /// Test-only, but the count is the point of the weak table: nothing else
    /// would notice it growing with every path a session ever touched.
    #[cfg(test)]
    async fn tracked_paths(&self) -> usize {
        self.locks.lock().await.len()
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
                    .with_lock(Path::new("same.txt"), || async {
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
        let value = queue.with_lock(Path::new("a.txt"), || async { 7 }).await;
        assert_eq!(value, 7);
    }

    #[tokio::test]
    async fn the_table_does_not_grow_with_the_paths_touched() {
        let queue = FileMutationQueue::new();
        for path in ["a.txt", "b.txt", "c.txt", "d.txt"] {
            queue.with_lock(Path::new(path), || async {}).await;
        }

        // Every call sweeps the paths nobody holds any more, so what is left is
        // what is in use rather than everything the session ever touched.
        assert_eq!(queue.tracked_paths().await, 1, "the dead entries were kept");
    }

    #[tokio::test]
    async fn a_caller_waiting_for_a_path_holds_its_lock_open() {
        let queue = Arc::new(FileMutationQueue::new());
        let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let path = Path::new("shared.txt");

        // The first call starts a second one from inside its own critical
        // section, so the second has certainly upgraded the same lock — and is
        // waiting on it — by the time the first leaves.
        let first = tokio::spawn({
            let queue = Arc::clone(&queue);
            let log = Arc::clone(&log);
            async move {
                queue
                    .with_lock(path, || async {
                        log.lock().await.push("first in");
                        tokio::spawn({
                            let queue = Arc::clone(&queue);
                            let log = Arc::clone(&log);
                            async move {
                                queue
                                    .with_lock(path, || async {
                                        log.lock().await.push("second in");
                                        tokio::time::sleep(Duration::from_millis(5)).await;
                                        log.lock().await.push("second out");
                                    })
                                    .await;
                            }
                        });
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        log.lock().await.push("first out");
                    })
                    .await;
            }
        });

        first.await.expect("the first call finished");

        // A third caller arriving now has to queue behind the second rather
        // than take a lock of its own: the second is the one holding it.
        queue
            .with_lock(path, || async {
                log.lock().await.push("third in");
                log.lock().await.push("third out");
            })
            .await;

        let log = log.lock().await.clone();
        assert_eq!(
            log,
            vec![
                "first in",
                "first out",
                "second in",
                "second out",
                "third in",
                "third out",
            ],
            "{log:?}"
        );
    }
}
