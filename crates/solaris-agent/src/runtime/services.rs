use crate::UsageTotals;
use std::sync::Arc;

/// Session services are runtime state, never user configuration.
#[derive(Clone, Default)]
pub(crate) struct SessionServices {
    pub jobs: crate::Jobs,
    pub depth: u32,
    pub child_backend: Option<crate::ChildBackendFactory>,
    pub report: Option<ChildReport>,
}
#[derive(Clone)]
pub(crate) struct ChildReport {
    pub totals: Arc<std::sync::Mutex<UsageTotals>>,
    pub tx: Option<tokio::sync::mpsc::UnboundedSender<crate::AgentEvent>>,
}
