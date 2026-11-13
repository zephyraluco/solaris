use crate::UsageTotals;
use crate::storage::memory::Conversation;
use std::sync::Arc;

/// Session services are runtime state, never user configuration.
#[derive(Clone, Default)]
pub(crate) struct SessionServices {
    pub budget: Option<Arc<crate::ExecutionBudget>>,
    pub child_backend: Option<crate::ChildBackendFactory>,
    pub tasks: crate::TaskControls,
    pub parent_ledger: Option<Arc<Conversation>>,
    pub child_requests: Option<Arc<std::sync::atomic::AtomicUsize>>,
    pub report: Option<ChildReport>,
}
#[derive(Clone)]
pub(crate) struct ChildReport {
    pub totals: Arc<std::sync::Mutex<UsageTotals>>,
    pub tx: Option<tokio::sync::mpsc::UnboundedSender<crate::AgentEvent>>,
}
impl SessionServices {
    pub fn for_turn(&self, budget: Arc<crate::ExecutionBudget>) -> Self {
        Self {
            budget: Some(budget),
            ..self.clone()
        }
    }
}
