//! Compatibility probes for replacing the production driver with the SDK runner.
//! These use the complete executor, not a separately driven AgentRun.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use rig_agent::agent::{
    CompletionCallAction, CompletionCallEvent, DispatchAction, DispatchEvent, ModelTurnAction,
    ModelTurnFinished, RequestPatch,
};
use rig_agent::{AgentBuilder, AgentHook, HookContext};
use rig_core::completion::FinishReason;
use rig_core::message::{Message, ToolName};
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_core::tool::{DynamicTool, ToolOutput};
use serde_json::json;

fn tool(runs: Arc<AtomicUsize>) -> DynamicTool {
    DynamicTool::new_with_context(
        ToolName::new("read").unwrap(),
        "Read",
        json!({"type":"object"}),
        move |_, _| {
            let runs = Arc::clone(&runs);
            Box::pin(async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(ToolOutput::text("contents"))
            })
        },
    )
}

#[tokio::test]
async fn complete_executor_drives_tools_without_a_host_model_loop() {
    let runs = Arc::new(AtomicUsize::new(0));
    let model = MockCompletionModel::from_turns([
        MockTurn::tool_call("c1", "read", json!({})),
        MockTurn::text("done"),
    ]);
    let response = AgentBuilder::new(model.clone())
        .dynamic_tool(tool(Arc::clone(&runs)))
        .build()
        .prompt("read")
        .max_turns(3)
        .run()
        .await
        .unwrap();
    assert_eq!(response.output(), "done");
    assert_eq!(model.request_count(), 2);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

struct Deny;
impl AgentHook for Deny {
    async fn on_dispatch(&self, _: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        if event.tool_name().is_some() {
            DispatchAction::skip("declined")
        } else {
            DispatchAction::proceed()
        }
    }
}

#[tokio::test]
async fn dispatch_denial_returns_feedback_without_running_the_tool() {
    let runs = Arc::new(AtomicUsize::new(0));
    let model = MockCompletionModel::from_turns([
        MockTurn::tool_call("c1", "read", json!({})),
        MockTurn::text("understood"),
    ]);
    AgentBuilder::new(model.clone())
        .dynamic_tool(tool(Arc::clone(&runs)))
        .add_hook(Deny)
        .build()
        .prompt("read")
        .max_turns(3)
        .run()
        .await
        .unwrap();
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert!(
        serde_json::to_string(&model.requests()[1])
            .unwrap()
            .contains("declined")
    );
}

struct ProjectOnce;
impl AgentHook for ProjectOnce {
    async fn on_completion_call(
        &self,
        _: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        if event.turn == 1 {
            CompletionCallAction::patch(RequestPatch::new().history([Message::user("summary")]))
        } else {
            CompletionCallAction::continue_run()
        }
    }
}

#[tokio::test]
async fn history_patch_is_request_only_and_original_history_returns_on_the_next_call() {
    let model = MockCompletionModel::from_turns([
        MockTurn::tool_call("c1", "read", json!({})),
        MockTurn::text("done"),
    ]);
    AgentBuilder::new(model.clone())
        .dynamic_tool(tool(Arc::new(AtomicUsize::new(0))))
        .add_hook(ProjectOnce)
        .build()
        .prompt("read")
        .history([Message::user("large original history")])
        .max_turns(3)
        .run()
        .await
        .unwrap();
    let requests = model.requests();
    let first = serde_json::to_string(&requests[0]).unwrap();
    let second = serde_json::to_string(&requests[1]).unwrap();
    assert!(first.contains("summary"));
    assert!(!first.contains("large original history"));
    assert!(second.contains("large original history"));
    assert!(!second.contains("summary"));
}

struct Steer;
impl AgentHook for Steer {
    async fn on_completion_call(
        &self,
        _: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        let mut history = event.history.to_vec();
        history.push(Arc::new(Message::user("new steering")));
        CompletionCallAction::patch(RequestPatch::new().history(history))
    }
}

#[tokio::test]
async fn history_patch_inserts_steering_before_the_current_prompt_and_does_not_commit_it() {
    let model = MockCompletionModel::text("done");
    let response = AgentBuilder::new(model.clone())
        .add_hook(Steer)
        .build()
        .prompt("original prompt")
        .run()
        .await
        .unwrap();
    let request = serde_json::to_string(&model.requests()[0]).unwrap();
    assert!(request.find("new steering").unwrap() < request.find("original prompt").unwrap());
    assert!(
        !serde_json::to_string(&response.messages)
            .unwrap()
            .contains("new steering")
    );
}

struct RetryTruncated;
impl AgentHook for RetryTruncated {
    async fn on_model_turn_finished(
        &self,
        _: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        if matches!(event.finish_reason, Some(FinishReason::Length)) {
            ModelTurnAction::retry_with_feedback("ask again with complete arguments")
        } else {
            ModelTurnAction::continue_run()
        }
    }
}

#[tokio::test]
async fn tool_bearing_truncation_cannot_use_the_model_retry_hook() {
    let runs = Arc::new(AtomicUsize::new(0));
    let model = MockCompletionModel::from_turns([
        MockTurn::tool_call("c1", "read", json!({})).with_finish_reason(FinishReason::Length),
        MockTurn::text("recovered"),
    ]);
    let error = AgentBuilder::new(model.clone())
        .dynamic_tool(tool(Arc::clone(&runs)))
        .add_hook(RetryTruncated)
        .build()
        .prompt("read")
        .max_turns(3)
        .run()
        .await
        .unwrap_err();
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(model.request_count(), 1);
    assert!(error.to_string().contains("tool-bearing"), "{error}");
}

struct ApprovalGate {
    entered: Arc<tokio::sync::Notify>,
    allowed: Arc<tokio::sync::Notify>,
}
impl AgentHook for ApprovalGate {
    async fn on_dispatch(&self, _: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        if event.tool_name().is_some() {
            self.entered.notify_one();
            self.allowed.notified().await;
        }
        DispatchAction::proceed()
    }
}

#[tokio::test]
async fn asynchronous_approval_blocks_side_effects_until_the_host_answers() {
    let runs = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let allowed = Arc::new(tokio::sync::Notify::new());
    let agent = AgentBuilder::new(MockCompletionModel::from_turns([
        MockTurn::tool_call("c1", "read", json!({})),
        MockTurn::text("done"),
    ]))
    .dynamic_tool(tool(Arc::clone(&runs)))
    .add_hook(ApprovalGate {
        entered: Arc::clone(&entered),
        allowed: Arc::clone(&allowed),
    })
    .build();
    let task = tokio::spawn(agent.prompt("read").max_turns(3).run());
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    allowed.notify_one();
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(response.output(), "done");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}
