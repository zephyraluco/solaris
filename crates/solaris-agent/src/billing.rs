//! Per-request accounting, independent from context projection and turn completion.
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use solaris_backend::{
    AgentBackend, AgentEvent, AgentEventStream, BackendError, TurnRequest, Usage,
};

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
    fn label(&self) -> &str {
        self.inner.label()
    }
    fn capabilities(&self) -> solaris_backend::ModelCapabilities {
        self.inner.capabilities()
    }
    async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
        let stream = self.inner.run_turn(request).await?;
        let conversation = Arc::clone(&self.conversation);
        let kind = self.kind;
        Ok(Box::pin(futures::stream::unfold(
            (stream, conversation, false),
            move |(mut stream, conversation, ended)| async move {
                if ended {
                    return None;
                }
                let mut event = stream.next().await?;
                if let AgentEvent::TurnComplete { usage, cost_usd } = event
                    && !conversation.record_billing(usage, cost_usd, kind)
                {
                    event =
                        AgentEvent::Error("cannot persist request usage; session stopped".into());
                }
                let ended = event.is_terminal();
                Some((event, (stream, conversation, ended)))
            },
        )))
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
