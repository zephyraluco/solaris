use crate::UsageKind;
use crate::identity::new_id;
use crate::runtime::services::SessionServices;
use crate::storage::memory::Conversation;
use async_trait::async_trait;
use solaris_backend::{BackendError, ModelConnector};
use std::sync::Arc;

/// One explicit settlement pipeline: persist, account, enforce, then publish.
pub(crate) struct ExecutionConnector {
    pub inner: Arc<dyn ModelConnector>,
    pub conversation: Arc<Conversation>,
    pub services: SessionServices,
    pub kind: UsageKind,
}
#[async_trait]
impl ModelConnector for ExecutionConnector {
    async fn model_client(self: Arc<Self>) -> Result<solaris_backend::ModelClient, BackendError> {
        let budget = self.services.budget.clone().expect("execution budget");
        budget.request()?;
        if let Some(requests) = &self.services.child_requests
            && requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 20
        {
            return Err(BackendError::new(
                "subagent exceeded its 20 model-request budget",
            ));
        }
        let client = Arc::clone(&self.inner).model_client().await?;
        budget.validate_pricing(client.capabilities().price_known)?;
        let request_id = new_id();
        Ok(client.with_settlement(move |usage, cost| {
            // Parent ledger is authoritative even when a child later fails.
            if let Some(parent) = &self.services.parent_ledger {
                parent.record_billing(&request_id, usage, cost, UsageKind::Child)?;
            }
            self.conversation
                .record_billing(&request_id, usage, cost, self.kind)?;
            // Record known consumption before denying further requests.
            if let Some(report) = &self.services.report {
                report
                    .totals
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .add(usage, cost);
            }
            let decision = budget.settle(cost);
            if let Some(report) = &self.services.report
                && let Some(tx) = &report.tx
            {
                let _ = tx.send(crate::AgentEvent::SubagentUsage {
                    usage,
                    cost_usd: cost.map_or(0.0, |value| value.total),
                });
            }
            decision
        }))
    }
    fn label(&self) -> &str {
        self.inner.label()
    }
    fn capabilities(&self) -> solaris_backend::ModelCapabilities {
        self.inner.capabilities()
    }
}
