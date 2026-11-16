//! Ordered blocking work outside the terminal's input and drawing thread.
use solaris_provider::AuthStore;
use std::sync::mpsc;

pub(crate) enum Completion {
    Saved {
        label: &'static str,
        result: Result<(), String>,
    },
    Auth {
        submitted: AuthStore,
        result: Result<AuthStore, String>,
    },
    Copied {
        success: String,
        failure: String,
        ok: bool,
    },
    Pasted {
        generation: u64,
        text: Option<String>,
    },
}
type Task = Box<dyn FnOnce() -> Completion + Send>;
#[derive(Default)]
pub(crate) struct BackgroundIo {
    worker: Option<(mpsc::Sender<Task>, std::thread::JoinHandle<()>)>,
    results: Option<mpsc::Receiver<Completion>>,
    pending: usize,
}
impl BackgroundIo {
    pub fn submit(&mut self, task: impl FnOnce() -> Completion + Send + 'static) {
        if self.worker.is_none() {
            let (tx, rx) = mpsc::channel::<Task>();
            let (done, results) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                for task in rx {
                    let _ = done.send(task());
                }
            });
            self.worker = Some((tx, handle));
            self.results = Some(results);
        }
        if self.worker.as_ref().unwrap().0.send(Box::new(task)).is_ok() {
            self.pending += 1;
        }
    }
    pub fn drain(&mut self, wait: bool) -> Vec<Completion> {
        let mut events = Vec::new();
        if let Some(rx) = &self.results {
            while self.pending > 0 {
                let event = if wait {
                    rx.recv().ok()
                } else {
                    rx.try_recv().ok()
                };
                let Some(event) = event else { break };
                self.pending -= 1;
                events.push(event);
            }
        }
        events
    }
}
impl Drop for BackgroundIo {
    fn drop(&mut self) {
        if let Some((sender, worker)) = self.worker.take() {
            drop(sender);
            let _ = worker.join();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn blocked_storage_does_not_block_submission_and_writes_keep_order() {
        let mut io = BackgroundIo::default();
        let (release, wait) = mpsc::channel();
        io.submit(move || {
            wait.recv().unwrap();
            Completion::Saved {
                label: "first",
                result: Ok(()),
            }
        });
        io.submit(|| Completion::Saved {
            label: "second",
            result: Ok(()),
        });
        assert!(io.drain(false).is_empty());
        release.send(()).unwrap();
        let events = io.drain(true);
        assert!(matches!(
            &events[..],
            [
                Completion::Saved { label: "first", .. },
                Completion::Saved {
                    label: "second",
                    ..
                }
            ]
        ));
    }
}
