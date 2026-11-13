//! Per-request accounting, independent from context projection and turn completion.

use serde::{Deserialize, Serialize};
use solaris_backend::Usage;

use crate::SessionRecord;

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
        let mut seen = std::collections::HashSet::new();
        for record in records {
            match record {
                SessionRecord::Usage {
                    request_id,
                    usage,
                    cost,
                    ..
                } if seen.insert(request_id) => totals.add(*usage, *cost),
                SessionRecord::Cleared => totals = Self::default(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::connection::ExecutionConnector;
    use crate::runtime::services::SessionServices;
    use crate::storage::memory::Conversation;
    use solaris_backend::ModelConnector;
    use std::sync::Arc;
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
                event: crate::AgentEvent::TurnComplete {
                    usage: Usage::new(999, 999),
                    cost_usd: 99.0,
                },
            },
            SessionRecord::Usage {
                request_id: "fixture-0".into(),
                usage: Usage::new(100, 10),
                cost: Some(charge(0.01)),
                kind: UsageKind::Model,
            },
            SessionRecord::Usage {
                request_id: "fixture-1".into(),
                usage: Usage::new(200, 20),
                cost: Some(partial),
                kind: UsageKind::Model,
            },
            SessionRecord::Usage {
                request_id: "fixture-2".into(),
                usage: Usage::new(50, 5),
                cost: None,
                kind: UsageKind::Child,
            },
        ];
        let mut replayed = records.to_vec();
        replayed.push(records[1].clone());
        let total = UsageTotals::restore(&replayed);
        assert_eq!(total.usage.total(), 385);
        assert!((total.cost_usd - 0.03).abs() < 1e-9);
        assert!(!total.cost_complete);
    }
    #[tokio::test]
    async fn partial_charge_is_persisted_before_budget_refusal_and_never_settled_twice() {
        use solaris_backend::{Backend, Endpoint, TurnPlan, Wire};
        let directory = tempfile::tempdir().unwrap();
        let store = crate::SessionStore::open(&directory.path().join("sessions.sqlite")).unwrap();
        let journal = store.create(directory.path(), "fixture", None).unwrap();
        let conversation = Arc::new(Conversation::new());
        conversation.attach(Arc::clone(&journal)).unwrap();
        let connector = Arc::new(ExecutionConnector {
            inner: Arc::new(
                Backend::new(TurnPlan {
                    endpoint: Endpoint {
                        wire: Wire::OpenAiChat,
                        session_header: None,
                        base_url: "http://127.0.0.1:1/v1".into(),
                        secret: None,
                        name: "fixture".into(),
                        label: "fixture".into(),
                        env_keys: &[],
                    },
                    model: {
                        let mut spec = crate::testing::parameters("fixture", 100)
                            .facts()
                            .spec()
                            .unwrap()
                            .clone();
                        spec.pricing = Some(rig_core::catalog::Pricing::new(1.0, 1.0));
                        solaris_backend::ModelParameters::new(spec, Some(100), true)
                    },
                    prompt_cache_key: None,
                })
                .unwrap(),
            ),
            conversation: Arc::clone(&conversation),
            services: SessionServices::default().for_turn(crate::ExecutionBudget::new(
                crate::BudgetLimits {
                    max_cost_usd: Some(1.0),
                    ..Default::default()
                },
            )),
            kind: UsageKind::Model,
        });
        let client = connector.model_client().await.unwrap();
        let partial =
            serde_json::from_value(serde_json::json!({"total":0.1,"input":0.1,"incomplete":true}))
                .unwrap();
        for _ in 0..2 {
            assert!(client.settle(Usage::new(10, 1), Some(partial)).is_err());
        }
        assert_eq!(
            journal
                .store()
                .records(journal.id())
                .unwrap()
                .iter()
                .filter(|record| matches!(record, SessionRecord::Usage { .. }))
                .count(),
            1
        );
        let totals = conversation.usage_totals().unwrap();
        assert_eq!(totals.usage.total(), 11);
        assert!(!totals.cost_complete);
    }
}
