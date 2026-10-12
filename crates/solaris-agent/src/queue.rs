//! Messages waiting to reach the model, and the queue both kinds share.
//!
//! A turn can take a long time — that is what a tool loop is — so the user has
//! two different things to say while one runs, and they are not the same thing:
//!
//! - **Steering** corrects the turn that is running. "No, the other file" has to
//!   reach the model before it finishes, or the whole turn was wasted.
//! - **A follow-up** is for after it. "Then run the tests" wants its own turn, so
//!   the transcript and the cost stay attributable.
//!
//! Both are the same [`Queue`], drained by the agent at different moments: the
//! steering queue before each round and again when the model looks finished; the
//! follow-up queue when the turn really would end. Keeping one type for both is
//! deliberate — what distinguishes them is *when* the agent reads them, and a
//! second type would only duplicate the four methods below.
//!
//! A queue rather than a channel: the handle outlives any one turn (the
//! application holds it for the session), while the loop that drains it lives for
//! exactly one turn and has to be able to look without blocking.

use std::sync::{Arc, Mutex, MutexGuard};

/// Messages waiting for the agent to read them.
///
/// Cloning shares the queue: the application keeps one clone and every turn gets
/// another. Blank messages are dropped rather than sent, because a prompt of only
/// whitespace would reach the model as an empty user turn.
#[derive(Debug, Clone, Default)]
pub struct Queue {
    pending: Arc<Mutex<Vec<String>>>,
}

impl Queue {
    /// An empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `message` to the queue.
    ///
    /// Returns whether it was queued: a blank message is not, and a caller may
    /// want to say so instead of reporting a message that will never arrive.
    pub fn push(&self, message: impl Into<String>) -> bool {
        let message = message.into();
        if message.trim().is_empty() {
            return false;
        }
        self.lock().push(message);
        true
    }

    /// Take everything waiting, oldest first.
    ///
    /// Draining rather than reading is deliberate: a message is put into the
    /// conversation exactly once, and returning it again would ask the model
    /// about the same thing twice.
    pub fn drain(&self) -> Vec<String> {
        std::mem::take(&mut *self.lock())
    }

    /// Whether anything is waiting, without taking it.
    pub fn has_pending(&self) -> bool {
        !self.lock().is_empty()
    }

    /// How many messages are waiting.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing is waiting.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// The queue, recovering from a panic in another holder rather than panicking
    /// here: a poisoned queue must not take down the turn that is already running.
    fn lock(&self) -> MutexGuard<'_, Vec<String>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_one_clone_pushes_another_drains() {
        let handle = Queue::new();
        let loop_side = handle.clone();

        assert!(handle.is_empty());
        assert!(handle.push("use the other file"));

        assert!(loop_side.has_pending());
        assert_eq!(loop_side.drain(), vec!["use the other file"]);
        assert!(handle.is_empty(), "the message is taken once");
    }

    #[test]
    fn messages_come_back_in_the_order_they_arrived() {
        let queue = Queue::new();
        queue.push("first");
        queue.push("second");
        queue.push("third");

        assert_eq!(queue.len(), 3);
        assert_eq!(queue.drain(), ["first", "second", "third"]);
    }

    #[test]
    fn a_blank_message_is_not_queued() {
        let queue = Queue::new();
        assert!(!queue.push("   \n "));
        assert!(!queue.push(""));
        assert!(queue.is_empty());

        assert!(queue.push("  keep me  "));
        assert_eq!(queue.drain(), ["  keep me  "], "kept as typed");
    }

    #[test]
    fn draining_an_empty_queue_yields_nothing() {
        let queue = Queue::new();
        assert!(queue.drain().is_empty());
        assert!(queue.drain().is_empty());
    }

    #[test]
    fn two_queues_do_not_share_what_they_hold() {
        // The whole point of having both: what is queued to correct a turn must
        // not also be read as the next one.
        let steering = Queue::new();
        let follow_up = Queue::new();
        steering.push("no, the other file");
        follow_up.push("then run the tests");

        assert_eq!(steering.drain(), ["no, the other file"]);
        assert_eq!(follow_up.drain(), ["then run the tests"]);
        assert!(steering.is_empty() && follow_up.is_empty());
    }
}
