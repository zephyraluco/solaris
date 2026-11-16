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
pub(crate) struct Worker {
    worker: Option<(mpsc::Sender<Task>, std::thread::JoinHandle<()>)>,
    results: Option<mpsc::Receiver<Completion>>,
    pending: usize,
    stopping: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl Worker {
    pub fn submit(&mut self, task: impl FnOnce() -> Completion + Send + 'static) {
        if self.worker.is_none() {
            let (tx, rx) = mpsc::channel::<Task>();
            let (done, results) = mpsc::channel();
            let stopping = self.stopping.clone();
            let handle = std::thread::spawn(move || {
                for task in rx {
                    if stopping.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
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
impl Worker {
    fn finish(&mut self) -> Result<(), String> {
        if let Some((sender, worker)) = self.worker.take() {
            drop(sender);
            worker
                .join()
                .map_err(|_| "persistence worker stopped unexpectedly".to_string())?;
        }
        Ok(())
    }
    fn close(&mut self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::Release);
        self.worker.take();
    }
}
#[derive(Default)]
pub(crate) struct DesktopIo {
    files: Worker,
    clipboard: Worker,
}
impl DesktopIo {
    pub fn submit(&mut self, task: impl FnOnce() -> Completion + Send + 'static) {
        self.files.submit(task);
    }
    pub fn submit_clipboard(&mut self, task: impl FnOnce() -> Completion + Send + 'static) {
        self.clipboard.submit(task);
    }
    pub fn drain(&mut self, wait: bool) -> Vec<Completion> {
        let mut result = self.files.drain(wait);
        result.extend(self.clipboard.drain(wait));
        result
    }
    /// Credentials/preferences must finish; pending clipboard work is nonessential on exit.
    pub fn shutdown(&mut self) -> Result<(), String> {
        let worker = self.files.finish();
        self.clipboard.close();
        let mut errors = self
            .files
            .drain(false)
            .into_iter()
            .filter_map(|event| match event {
                Completion::Saved {
                    label,
                    result: Err(error),
                } => Some(format!("cannot save {label}: {error}")),
                Completion::Auth {
                    result: Err(error), ..
                } => Some(format!("cannot save credentials: {error}")),
                _ => None,
            })
            .collect::<Vec<_>>();
        if let Err(error) = worker {
            errors.push(error);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}
pub mod clipboard;
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slow_clipboard_does_not_delay_files_or_durable_shutdown() {
        let mut io = DesktopIo::default();
        let (started, wait_started) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        io.submit_clipboard(move || {
            started.send(()).unwrap();
            wait.recv().unwrap();
            Completion::Copied {
                success: "ok".into(),
                failure: "failed".into(),
                ok: true,
            }
        });
        wait_started.recv().unwrap();
        io.submit(|| Completion::Saved {
            label: "preferences",
            result: Ok(()),
        });
        assert!(matches!(
            io.files.drain(true).as_slice(),
            [Completion::Saved {
                label: "preferences",
                ..
            }]
        ));
        io.shutdown().unwrap();
        release.send(()).unwrap();
    }
    #[test]
    fn shutdown_reports_a_pending_save_failure() {
        let mut io = DesktopIo::default();
        io.submit(|| Completion::Saved {
            label: "preferences",
            result: Err("disk full".into()),
        });
        assert!(io.shutdown().unwrap_err().contains("disk full"));
    }

    #[test]
    fn blocked_storage_does_not_block_submission_and_writes_keep_order() {
        let mut io = DesktopIo::default();
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
