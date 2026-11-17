use crate::UsageKind;
use crate::identity::new_id;
use crate::runtime::services::SessionServices;
use crate::storage::memory::Conversation;
use async_trait::async_trait;
use solaris_backend::{BackendError, ModelConnector};
use std::sync::Arc;

/// One explicit settlement pipeline: persist, account, then publish.
pub(crate) struct ExecutionConnector {
    pub inner: Arc<dyn ModelConnector>,
    pub conversation: Arc<Conversation>,
    pub services: SessionServices,
    pub kind: UsageKind,
}
#[async_trait]
impl ModelConnector for ExecutionConnector {
    async fn model_client(self: Arc<Self>) -> Result<solaris_backend::ModelClient, BackendError> {
        let client = Arc::clone(&self.inner).model_client().await?;
        let request_id = new_id();
        Ok(client.with_settlement(move |usage, cost| {
            self.conversation
                .record_billing(&request_id, usage, cost, self.kind)?;
            // Preserve known consumption even when the run later fails.
            if let Some(report) = &self.services.report {
                report
                    .totals
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .add(usage, cost);
            }
            if let Some(report) = &self.services.report
                && let Some(tx) = &report.tx
            {
                let _ = tx.send(crate::AgentEvent::SubagentUsage {
                    task_id: report.task_id.clone(),
                    usage,
                    cost_usd: cost.map_or(0.0, |value| value.total),
                });
            }
            Ok(())
        }))
    }
    fn label(&self) -> &str {
        self.inner.label()
    }
    fn capabilities(&self) -> solaris_backend::ModelCapabilities {
        self.inner.capabilities()
    }
}
