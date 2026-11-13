//! Per-request accounting, independent from context projection and turn completion.
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use solaris_backend::{AgentBackend, AgentEvent, BackendError, Usage};

use crate::{SessionRecord, conversation::Conversation};

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub usage: Usage,
    pub cost_usd: f64,
}
impl UsageTotals {
    pub(crate) fn add(&mut self, usage: Usage, cost_usd: f64) {
        self.usage = self.usage.merge(usage);
        self.cost_usd += cost_usd;
    }

    pub(crate) fn restore(records: &[SessionRecord]) -> Self {
        let mut totals = Self::default();
        let mut request_records = false;
        for record in records {
            match record {
                SessionRecord::Prompt { .. } => request_records = false,
                SessionRecord::Usage {
                    usage,
                    cost_usd,
                    kind,
                } => {
                    totals.add(*usage, *cost_usd);
                    request_records |= *kind != UsageKind::Manual;
                }
                SessionRecord::Event {
                    event: AgentEvent::TurnComplete { usage, cost_usd },
                } if !request_records => {
                    // Legacy turns only have their cumulative completion event.
                    totals.add(*usage, *cost_usd);
                }
                SessionRecord::Context { messages } if messages.is_empty() => {
                    totals = Self::default();
                    request_records = false;
                }
                _ => {}
            }
        }
        totals
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageKind {
    Model,
    Child,
    #[default]
    Manual,
}

pub(crate) struct AccountingBackend {
    pub inner: Arc<dyn AgentBackend>,
    pub conversation: Arc<Conversation>,
    pub kind: UsageKind,
}
#[async_trait]
impl AgentBackend for AccountingBackend {
    async fn model_client(self: Arc<Self>) -> Result<solaris_backend::ModelClient, BackendError> {
        let client = Arc::clone(&self.inner).model_client().await?;
        let conversation = Arc::clone(&self.conversation);
        let kind = self.kind;
        Ok(client.with_usage_observer(move |usage, cost| {
            if conversation.record_billing(usage, cost.unwrap_or(0.0), kind) {
                Ok(())
            } else {
                Err(BackendError::new(
                    "cannot persist request usage; session stopped",
                ))
            }
        }))
    }

    fn label(&self) -> &str {
        self.inner.label()
    }
    fn capabilities(&self) -> solaris_backend::ModelCapabilities {
        self.inner.capabilities()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_turns_and_new_request_records_are_not_double_counted() {
        let records = vec![
            SessionRecord::Prompt { text: "old".into() },
            SessionRecord::Event {
                event: AgentEvent::TurnComplete {
                    usage: Usage::new(100, 10),
                    cost_usd: 0.01,
                },
            },
            SessionRecord::Prompt { text: "new".into() },
            SessionRecord::Usage {
                usage: Usage::new(200, 20),
                cost_usd: 0.02,
                kind: UsageKind::Model,
            },
            SessionRecord::Usage {
                usage: Usage::new(50, 5),
                cost_usd: 0.005,
                kind: UsageKind::Manual,
            },
            SessionRecord::Event {
                event: AgentEvent::TurnComplete {
                    usage: Usage::new(200, 20),
                    cost_usd: 0.02,
                },
            },
        ];
        let total = UsageTotals::restore(&records);
        assert_eq!(total.usage.total(), 385);
        assert!((total.cost_usd - 0.035).abs() < 1e-9);
    }
}
