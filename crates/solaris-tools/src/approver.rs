//! Whether a tool call may run.
//!
//! The hook exists so a UI can ask before something irreversible happens. It is
//! asynchronous because the answer may have to come from somewhere else: the
//! loop runs in a background task, so a prompt has to travel to whatever draws
//! the screen and the answer has to come back. [`ChannelApprover`] is that
//! shape; a policy that decides on the spot stays synchronous in spirit and just
//! answers immediately.

use async_trait::async_trait;
use solaris_core::ToolCall;
use tokio::sync::{mpsc::UnboundedSender, oneshot};

/// The answer to "may this call run?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// Run it.
    Approved,
    /// Do not run it. The model is told the call was refused.
    Denied,
}

/// Decides whether a tool call may run.
#[async_trait]
pub trait Approver: Send + Sync {
    /// Whether `call` may run.
    ///
    /// Answering may take as long as it takes: a UI that draws a prompt answers
    /// when the user does, and the loop waits for it.
    async fn approve(&self, call: &ToolCall) -> Approval;
}

/// Approves everything.
///
/// The default: a terminal agent is already acting on the user's behalf, and a
/// prompt for every read would be noise. It is the wrong default for a session
/// nobody is watching, which is why the hook exists.
#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysApprove;

#[async_trait]
impl Approver for AlwaysApprove {
    async fn approve(&self, _call: &ToolCall) -> Approval {
        Approval::Approved
    }
}

/// Approves nothing, for a session that must not touch anything.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyAll;

#[async_trait]
impl Approver for DenyAll {
    async fn approve(&self, _call: &ToolCall) -> Approval {
        Approval::Denied
    }
}

/// An approver built from a closure, for callers with their own policy.
pub struct FnApprover<F>(F);

impl<F> FnApprover<F> {
    /// Wrap `decide`.
    pub fn new(decide: F) -> Self {
        Self(decide)
    }
}

#[async_trait]
impl<F: Fn(&ToolCall) -> Approval + Send + Sync> Approver for FnApprover<F> {
    async fn approve(&self, call: &ToolCall) -> Approval {
        (self.0)(call)
    }
}

/// A decision the loop is waiting for.
#[derive(Debug)]
pub struct ApprovalRequest {
    /// The call awaiting an answer.
    pub call: ToolCall,
    /// Where the answer goes.
    pub reply: oneshot::Sender<Approval>,
}

/// Asks a UI over a channel, and waits for its answer.
///
/// This is the shape a terminal needs: the loop runs in a background task, so a
/// prompt has to reach whatever draws the screen, and the answer has to come
/// back the same way. A UI that has gone away denies the call, which is the safe
/// direction to fail in.
pub struct ChannelApprover {
    requests: UnboundedSender<ApprovalRequest>,
}

impl ChannelApprover {
    /// An approver that sends each call to whoever holds the other end.
    pub fn new(requests: UnboundedSender<ApprovalRequest>) -> Self {
        Self { requests }
    }
}

#[async_trait]
impl Approver for ChannelApprover {
    async fn approve(&self, call: &ToolCall) -> Approval {
        let (reply, answer) = oneshot::channel();
        let request = ApprovalRequest {
            call: call.clone(),
            reply,
        };
        if self.requests.send(request).is_err() {
            return Approval::Denied;
        }
        // A dropped sender means the UI went away without answering, which must
        // not be read as permission.
        answer.await.unwrap_or(Approval::Denied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str) -> ToolCall {
        ToolCall::new("call-1", name, json!({}))
    }

    #[tokio::test]
    async fn the_default_approves_the_other_denies() {
        assert_eq!(
            AlwaysApprove.approve(&call("bash")).await,
            Approval::Approved
        );
        assert_eq!(DenyAll.approve(&call("bash")).await, Approval::Denied);
    }

    #[tokio::test]
    async fn a_closure_becomes_an_approver() {
        let read_only = FnApprover::new(|call: &ToolCall| {
            if call.name == "read" {
                Approval::Approved
            } else {
                Approval::Denied
            }
        });

        assert_eq!(read_only.approve(&call("read")).await, Approval::Approved);
        assert_eq!(read_only.approve(&call("bash")).await, Approval::Denied);
    }

    #[tokio::test]
    async fn a_channel_approver_carries_the_call_and_returns_the_answer() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let approver = ChannelApprover::new(tx);

        let asked = tokio::spawn(async move { approver.approve(&call("write")).await });

        let request = rx.recv().await.expect("a request");
        assert_eq!(request.call.name, "write");
        request.reply.send(Approval::Denied).expect("answered");

        assert_eq!(asked.await.expect("joined"), Approval::Denied);
    }

    #[tokio::test]
    async fn a_channel_approver_denies_when_the_ui_is_gone() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let approver = ChannelApprover::new(tx);

        assert_eq!(approver.approve(&call("write")).await, Approval::Denied);
    }
}
