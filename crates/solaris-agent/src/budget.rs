//! A shared per-turn request, time and observed-cost budget for parent and children.
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use solaris_backend::{BackendError, ModelConnector};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Limits apply to all model requests, including compaction and child requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetLimits {
    pub max_requests: usize,
    pub max_seconds: u64,
    pub max_cost_usd: Option<f64>,
}
impl Default for BudgetLimits {
    fn default() -> Self {
        Self {
            max_requests: 100,
            max_seconds: 1800,
            max_cost_usd: None,
        }
    }
}

/// One turn's accounting, shared by every child without resetting its deadline.
pub struct ExecutionBudget {
    limits: BudgetLimits,
    deadline: tokio::time::Instant,
    state: Mutex<(usize, f64)>,
}
impl ExecutionBudget {
    pub fn new(limits: BudgetLimits) -> Arc<Self> {
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(limits.max_seconds.min(7 * 86400));
        Arc::new(Self {
            limits,
            deadline,
            state: Mutex::new((0, 0.0)),
        })
    }
    pub fn check(&self) -> Result<(), String> {
        let state = self.state.lock().map_err(|_| "budget lock poisoned")?;
        if tokio::time::Instant::now() >= self.deadline {
            return Err("execution deadline exceeded".into());
        }
        if self
            .limits
            .max_cost_usd
            .is_some_and(|limit| state.1 >= limit)
        {
            return Err("observed model cost budget exhausted".into());
        }
        Ok(())
    }
    fn request(&self) -> Result<(), BackendError> {
        self.check().map_err(BackendError::new)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| BackendError::new("budget lock poisoned"))?;
        if state.0 >= self.limits.max_requests {
            return Err(BackendError::new("shared model request budget exhausted"));
        }
        state.0 += 1;
        Ok(())
    }
    fn charge(&self, cost: f64) {
        if let Ok(mut state) = self.state.lock()
            && cost.is_finite()
            && cost > 0.0
        {
            state.1 += cost;
        }
    }
    pub(crate) fn deadline(&self) -> tokio::time::Instant {
        self.deadline
    }
}

pub(crate) struct BudgetedBackend {
    pub inner: Arc<dyn ModelConnector>,
    pub budget: Arc<ExecutionBudget>,
}
#[async_trait]
impl ModelConnector for BudgetedBackend {
    async fn model_client(self: Arc<Self>) -> Result<solaris_backend::ModelClient, BackendError> {
        self.budget.check().map_err(BackendError::new)?;
        let client = Arc::clone(&self.inner).model_client().await?;
        if self.budget.limits.max_cost_usd.is_some()
            && client.capabilities().price_known == Some(false)
        {
            return Err(BackendError::new(
                "cost budget requires known model pricing; configure models.json or omit --max-cost",
            ));
        }
        self.budget.request()?;
        let budget = Arc::clone(&self.budget);
        Ok(client.with_usage_observer(move |_, cost| {
            if let Some(cost) = cost {
                budget.charge(cost.total);
            }
            if budget.limits.max_cost_usd.is_some() && !cost.is_some_and(|cost| cost.is_complete()) {
                return Err(BackendError::new(
                    "Rig did not report complete request cost; execution stopped because a cost budget is enabled",
                ));
            }
            Ok(())
        }))
    }

    fn capabilities(&self) -> solaris_backend::ModelCapabilities {
        self.inner.capabilities()
    }

    fn label(&self) -> &str {
        self.inner.label()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cost_budget_stops_when_rig_does_not_report_a_charge() {
        use solaris_backend::{Backend, Endpoint, TurnPlan, Wire};
        let mut spec = crate::testing::parameters("test", 100)
            .facts()
            .spec()
            .unwrap()
            .clone();
        spec.pricing = Some(rig_core::catalog::Pricing::new(1.0, 1.0));
        let inner = Backend::new(TurnPlan {
            endpoint: Endpoint {
                wire: Wire::OpenAiChat,
                session_header: None,
                base_url: "http://127.0.0.1:1/v1".into(),
                secret: None,
                name: "test".into(),
                label: "test".into(),
                env_keys: &[],
            },
            model: solaris_backend::ModelParameters::new(spec, Some(100), true),
            prompt_cache_key: None,
        })
        .unwrap();
        let source = Arc::new(BudgetedBackend {
            inner: Arc::new(inner),
            budget: ExecutionBudget::new(BudgetLimits {
                max_cost_usd: Some(1.0),
                ..Default::default()
            }),
        });
        let partial: solaris_backend::Cost =
            serde_json::from_value(serde_json::json!({"input":0.1,"total":0.1,"incomplete":true}))
                .unwrap();
        for cost in [None, Some(partial)] {
            let client = source.clone().model_client().await.unwrap();
            let error = client
                .settle(solaris_backend::Usage::new(100, 1), cost)
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Rig did not report complete request cost")
            );
        }
    }

    #[tokio::test]
    async fn children_share_request_counts_and_observed_cost() {
        let budget = ExecutionBudget::new(BudgetLimits {
            max_requests: 2,
            max_seconds: 30,
            max_cost_usd: Some(1.0),
        });
        let child = Arc::clone(&budget);
        budget.request().expect("parent");
        child.request().expect("child");
        assert!(budget.request().is_err());
        child.charge(1.0);
        assert!(budget.check().expect_err("cost").contains("cost"));
    }
    #[tokio::test(start_paused = true)]
    async fn deadline_does_not_reset_for_a_child() {
        let budget = ExecutionBudget::new(BudgetLimits {
            max_seconds: 1,
            ..Default::default()
        });
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(
            Arc::clone(&budget)
                .check()
                .expect_err("deadline")
                .contains("deadline")
        );
    }
}
