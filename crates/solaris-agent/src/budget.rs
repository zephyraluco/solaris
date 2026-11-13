//! A shared per-turn request, time and observed-cost budget for parent and children.
use serde::{Deserialize, Serialize};
use solaris_backend::BackendError;
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
    pub(crate) fn request(&self) -> Result<(), BackendError> {
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

impl ExecutionBudget {
    pub(crate) fn validate_pricing(&self, known: Option<bool>) -> Result<(), BackendError> {
        if self.limits.max_cost_usd.is_some() && known == Some(false) {
            return Err(BackendError::new(
                "cost budget requires known model pricing; configure models.json or omit --max-cost",
            ));
        }
        Ok(())
    }
    pub(crate) fn settle(&self, cost: Option<solaris_backend::Cost>) -> Result<(), BackendError> {
        if let Some(cost) = cost {
            self.charge(cost.total);
        }
        if self.limits.max_cost_usd.is_some() && !cost.is_some_and(|value| value.is_complete()) {
            return Err(BackendError::new(
                "Rig did not report complete request cost; execution stopped because a cost budget is enabled",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cost_budget_stops_when_rig_does_not_report_a_charge() {
        let budget = ExecutionBudget::new(BudgetLimits {
            max_cost_usd: Some(1.0),
            ..Default::default()
        });
        let partial: solaris_backend::Cost =
            serde_json::from_value(serde_json::json!({"input":0.1,"total":0.1,"incomplete":true}))
                .unwrap();
        for cost in [None, Some(partial)] {
            let error = budget.settle(cost).unwrap_err();
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
