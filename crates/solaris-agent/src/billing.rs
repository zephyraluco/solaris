//! Per-request accounting, independent from context projection and turn completion.
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use solaris_backend::{AgentBackend, BackendError, Usage};

use crate::{SessionRecord, conversation::Conversation};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub usage: Usage,
    pub cost_usd: f64,
    pub cost_complete: bool,
}
impl Default for UsageTotals {
    fn default() -> Self {
        Self {
            usage: Usage::default(),
            cost_usd: 0.0,
            cost_complete: true,
        }
    }
}
impl UsageTotals {
    pub(crate) fn add(&mut self, usage: Usage, cost: Option<solaris_backend::Cost>) {
        self.usage = self.usage.merge(usage);
        if let Some(cost) = cost {
            self.cost_usd += cost.total;
            self.cost_complete &= cost.is_complete();
        } else {
            self.cost_complete = false;
        }
    }
    pub(crate) fn merge(&mut self, other: Self) {
        self.usage = self.usage.merge(other.usage);
        self.cost_usd += other.cost_usd;
        self.cost_complete &= other.cost_complete;
    }
    pub(crate) fn restore(records: &[SessionRecord]) -> Self {
        let mut totals = Self::default();
        for record in records {
            match record {
                SessionRecord::Usage { usage, cost, .. } => totals.add(*usage, *cost),
                SessionRecord::Context { messages } if messages.is_empty() => {
                    totals = Self::default()
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
            if conversation.record_billing(usage, cost, kind) {
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
    fn charge(value: f64) -> solaris_backend::Cost {
        solaris_backend::Cost::from_parts(value, 0.0, 0.0, 0.0)
    }
    #[test]
    fn only_request_records_are_restored_and_missing_or_partial_cost_stays_incomplete() {
        let partial: solaris_backend::Cost = serde_json::from_value(
            serde_json::json!({"total":0.02,"input":0.02,"incomplete":true}),
        )
        .unwrap();
        let records = [
            SessionRecord::Event {
                event: solaris_backend::AgentEvent::TurnComplete {
                    usage: Usage::new(999, 999),
                    cost_usd: 99.0,
                },
            },
            SessionRecord::Usage {
                usage: Usage::new(100, 10),
                cost: Some(charge(0.01)),
                kind: UsageKind::Model,
            },
            SessionRecord::Usage {
                usage: Usage::new(200, 20),
                cost: Some(partial),
                kind: UsageKind::Model,
            },
            SessionRecord::Usage {
                usage: Usage::new(50, 5),
                cost: None,
                kind: UsageKind::Child,
            },
        ];
        let total = UsageTotals::restore(&records);
        assert_eq!(total.usage.total(), 385);
        assert!((total.cost_usd - 0.03).abs() < 1e-9);
        assert!(!total.cost_complete);
    }
}
