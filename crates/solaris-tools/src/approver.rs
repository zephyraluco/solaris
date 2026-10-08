//! Whether a tool call may run.
//!
//! The hook exists so a UI can ask before something irreversible happens. It is
//! deliberately synchronous and tiny: the interactive confirmation is not built
//! yet, and a channel-based approver can be dropped in later without the loop
//! or the tools changing.

use solaris_core::ToolCall;

/// The answer to "may this call run?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// Run it.
    Approved,
    /// Do not run it. The model is told the call was refused.
    Denied,
}

/// Decides whether a tool call may run.
pub trait Approver: Send + Sync {
    /// Whether `call` may run.
    fn approve(&self, call: &ToolCall) -> Approval;
}

/// Approves everything.
///
/// The default: a terminal agent is already acting on the user's behalf, and a
/// prompt for every read would be noise. It is the wrong default for a session
/// nobody is watching, which is why the hook exists.
#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysApprove;

impl Approver for AlwaysApprove {
    fn approve(&self, _call: &ToolCall) -> Approval {
        Approval::Approved
    }
}

/// Approves nothing, for a session that must not touch anything.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyAll;

impl Approver for DenyAll {
    fn approve(&self, _call: &ToolCall) -> Approval {
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

impl<F: Fn(&ToolCall) -> Approval + Send + Sync> Approver for FnApprover<F> {
    fn approve(&self, call: &ToolCall) -> Approval {
        (self.0)(call)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str) -> ToolCall {
        ToolCall::new("call-1", name, json!({}))
    }

    #[test]
    fn the_default_approves_the_other_denies() {
        assert_eq!(AlwaysApprove.approve(&call("bash")), Approval::Approved);
        assert_eq!(DenyAll.approve(&call("bash")), Approval::Denied);
    }

    #[test]
    fn a_closure_becomes_an_approver() {
        let read_only = FnApprover::new(|call: &ToolCall| {
            if call.name == "read" {
                Approval::Approved
            } else {
                Approval::Denied
            }
        });

        assert_eq!(read_only.approve(&call("read")), Approval::Approved);
        assert_eq!(read_only.approve(&call("bash")), Approval::Denied);
    }
}
