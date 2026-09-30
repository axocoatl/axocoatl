// Included inside default_behavior::tests so the agent-owned tool branch can
// be exercised with the same controlled provider as the shared executor branch.
use crate::execution_boundary::{
    AdmittedToolInvocation, InvocationAdmission, ToolExecutionBoundary, ToolInvocationOutcome,
    ToolInvocationRequest,
};
use std::sync::atomic::{AtomicUsize, Ordering as BoundaryOrdering};
use std::sync::Mutex as BoundaryMutex;

struct BoundaryProvider {
    calls: AtomicUsize,
    tools: Vec<ToolCall>,
    requests: BoundaryMutex<Vec<ChatRequest>>,
}

#[async_trait::async_trait]
impl LlmProvider for BoundaryProvider {
    fn provider_id(&self) -> &str {
        "boundary-probe"
    }
    fn model_id(&self) -> &str {
        "boundary-probe"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            ..Default::default()
        }
    }
    async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
        unreachable!("the real actor uses streaming")
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        self.requests.lock().unwrap().push(request);
        let first = self.calls.fetch_add(1, BoundaryOrdering::SeqCst) == 0;
        let sends_tools = first && !self.tools.is_empty();
        let mut events = Vec::new();
        if sends_tools {
            for (index, call) in self.tools.iter().enumerate() {
                events.push(Ok(StreamEvent::ToolCallDelta {
                    index: Some(index),
                    id: call.id.clone(),
                    name: Some(call.name.clone()),
                    args_delta: call.arguments.to_string(),
                }));
            }
        } else {
            events.push(Ok(StreamEvent::TextDelta {
                delta: "done".into(),
            }));
        }
        events.push(Ok(StreamEvent::Usage(TokenUsageStats::new(10, 2))));
        events.push(Ok(StreamEvent::Done {
            finish_reason: if sends_tools {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
        }));
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

#[derive(Default)]
struct BoundaryProbe {
    requests: BoundaryMutex<Vec<ToolInvocationRequest>>,
    outcomes: Arc<BoundaryMutex<Vec<(usize, ToolInvocationOutcome)>>>,
    deny_index: Option<usize>,
    fail_outcome: bool,
    wait_before_denial: Option<Arc<tokio::sync::Notify>>,
    refuse_before_admission: Option<String>,
    /// Declines once this many calls of one response passed the early check.
    allow_per_group: Option<u32>,
    /// Declines this call index under the admission lock.
    decline_index: Option<usize>,
}

struct BoundaryAdmission {
    index: usize,
    outcomes: Arc<BoundaryMutex<Vec<(usize, ToolInvocationOutcome)>>>,
    fail: bool,
}

#[async_trait::async_trait]
impl AdmittedToolInvocation for BoundaryAdmission {
    async fn record_outcome(
        self: Box<Self>,
        outcome: &ToolInvocationOutcome,
    ) -> Result<(), String> {
        self.outcomes
            .lock()
            .unwrap()
            .push((self.index, outcome.clone()));
        if self.fail {
            Err("injected outcome acknowledgement failure".into())
        } else {
            Ok(())
        }
    }
}

#[async_trait::async_trait]
impl ToolExecutionBoundary for BoundaryProbe {
    fn preadmission_refusal(&self, _request: &ToolInvocationRequest, earlier: u32) -> Option<String> {
        if self.allow_per_group.is_some_and(|allowed| earlier >= allowed) {
            return Some("group allowance held for the host".into());
        }
        self.refuse_before_admission.clone()
    }

    async fn admit_or_decline(
        &self,
        request: &ToolInvocationRequest,
    ) -> Result<InvocationAdmission, String> {
        if self.decline_index == Some(request.provider_call_index) {
            return Ok(InvocationAdmission::Declined("declined under the admission lock".into()));
        }
        self.admit(request).await.map(InvocationAdmission::Admitted)
    }

    async fn admit(
        &self,
        request: &ToolInvocationRequest,
    ) -> Result<Box<dyn AdmittedToolInvocation>, String> {
        self.requests.lock().unwrap().push(request.clone());
        if self.deny_index == Some(request.provider_call_index) {
            if let Some(wait) = &self.wait_before_denial {
                wait.notified().await;
            }
            return Err("injected intent acknowledgement failure".into());
        }
        Ok(Box::new(BoundaryAdmission {
            index: request.provider_call_index,
            outcomes: self.outcomes.clone(),
            fail: self.fail_outcome,
        }))
    }
}

struct BoundaryCountingTool {
    executions: AtomicUsize,
    received: BoundaryMutex<Vec<serde_json::Value>>,
    policy: axocoatl_llm::ConcurrencyPolicy,
    started: Arc<tokio::sync::Notify>,
    release: Option<Arc<tokio::sync::Notify>>,
    panic_after_effect: bool,
}

impl BoundaryCountingTool {
    fn new(policy: axocoatl_llm::ConcurrencyPolicy) -> Self {
        Self {
            executions: AtomicUsize::new(0),
            received: BoundaryMutex::new(Vec::new()),
            policy,
            started: Arc::new(tokio::sync::Notify::new()),
            release: None,
            panic_after_effect: false,
        }
    }
}

#[async_trait::async_trait]
impl axocoatl_tools::BuiltinTool for BoundaryCountingTool {
    fn description(&self) -> &str {
        "Counts actual backend entry"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn concurrency_policy(&self) -> axocoatl_llm::ConcurrencyPolicy {
        self.policy
    }
    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, axocoatl_tools::ToolError> {
        self.executions.fetch_add(1, BoundaryOrdering::SeqCst);
        self.received.lock().unwrap().push(arguments.clone());
        self.started.notify_one();
        if let Some(release) = &self.release {
            release.notified().await;
        }
        assert!(!self.panic_after_effect, "injected panic after effect");
        Ok(serde_json::json!({"raw":arguments}))
    }
}

fn boundary_provider(count: usize) -> Arc<BoundaryProvider> {
    Arc::new(BoundaryProvider {
        calls: AtomicUsize::new(0),
        requests: BoundaryMutex::new(Vec::new()),
        tools: (0..count)
            .map(|index| ToolCall {
                // Deliberately id-less, same-name calls must retain original indices.
                id: String::new(),
                name: "effect".into(),
                arguments: serde_json::json!({"index":index}),
                provider_metadata: Default::default(),
            })
            .collect(),
    })
}

async fn boundary_behavior(
    provider: Arc<BoundaryProvider>,
    tool: Arc<BoundaryCountingTool>,
    behavior_owned: bool,
) -> DefaultAgentBehavior {
    let mut executor = ToolExecutor::new();
    if !behavior_owned {
        executor.register_builtin("effect", tool.clone());
    }
    let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
        .with_tool_executor(Arc::new(executor));
    behavior.on_start(&AgentConfig::default()).await.unwrap();
    if behavior_owned {
        behavior.core_memory_tools.push(("effect".into(), tool));
    }
    behavior
}

fn boundary_control(boundary: Arc<BoundaryProbe>) -> AgentRunControl {
    AgentRunControl::new(crate::AgentRunId::new("exact-activation-test"))
        .with_execution_boundary(boundary)
}

#[tokio::test]
async fn acknowledged_admission_failure_prevents_executor_and_behavior_owned_effects() {
    for behavior_owned in [false, true] {
        let provider = boundary_provider(1);
        let tool = Arc::new(BoundaryCountingTool::new(
            axocoatl_llm::ConcurrencyPolicy::Exclusive,
        ));
        let mut behavior = boundary_behavior(provider.clone(), tool.clone(), behavior_owned).await;
        let boundary = Arc::new(BoundaryProbe {
            deny_index: Some(0),
            ..Default::default()
        });
        let error = behavior
            .execute_controlled(
                AgentInput::text("do work"),
                boundary_control(boundary.clone()),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("admission failed"));
        assert_eq!(tool.executions.load(BoundaryOrdering::SeqCst), 0);
        assert_eq!(provider.calls.load(BoundaryOrdering::SeqCst), 1);
        assert!(boundary.outcomes.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_host_refusal_before_admission_reaches_the_model_as_a_tool_error() {
    let provider = boundary_provider(1);
    let tool = Arc::new(BoundaryCountingTool::new(
        axocoatl_llm::ConcurrencyPolicy::Exclusive,
    ));
    let mut behavior = boundary_behavior(provider.clone(), tool.clone(), false).await;
    let boundary = Arc::new(BoundaryProbe {
        refuse_before_admission: Some("allowance held for the host".into()),
        ..Default::default()
    });
    let output = behavior
        .execute_controlled(
            AgentInput::text("do work"),
            boundary_control(boundary.clone()),
        )
        .await
        .expect("the model finishes after the refusal");
    assert!(boundary.requests.lock().unwrap().is_empty(), "nothing is admitted");
    assert!(boundary.outcomes.lock().unwrap().is_empty());
    assert_eq!(tool.executions.load(BoundaryOrdering::SeqCst), 0);
    assert_eq!(provider.calls.load(BoundaryOrdering::SeqCst), 2);
    let followup = provider.requests.lock().unwrap()[1].clone();
    assert!(followup
        .messages
        .iter()
        .any(|message| serde_json::to_string(&message.content).unwrap().contains("allowance held for the host")));
    assert_eq!(output.output().content, "done");
}

#[tokio::test]
async fn a_group_counts_earlier_calls_and_a_locked_decline_is_not_fatal() {
    let provider = boundary_provider(3);
    let tool = Arc::new(BoundaryCountingTool::new(
        axocoatl_llm::ConcurrencyPolicy::Exclusive,
    ));
    let mut behavior = boundary_behavior(provider.clone(), tool.clone(), false).await;
    let boundary = Arc::new(BoundaryProbe {
        allow_per_group: Some(2),
        decline_index: Some(1),
        ..Default::default()
    });
    let output = behavior
        .execute_controlled(
            AgentInput::text("do work"),
            boundary_control(boundary.clone()),
        )
        .await
        .expect("declines reach the model and it finishes");
    // Call 0 is admitted, call 1 is declined under the lock, and call 2 is
    // refused early because two calls of its response were already let through.
    let admitted: Vec<usize> = boundary
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request.provider_call_index)
        .collect();
    assert_eq!(admitted, vec![0]);
    assert_eq!(tool.executions.load(BoundaryOrdering::SeqCst), 1);
    let followup = serde_json::to_string(&provider.requests.lock().unwrap()[1].messages).unwrap();
    assert!(followup.contains("declined under the admission lock"));
    assert!(followup.contains("group allowance held for the host"));
    assert_eq!(output.output().content, "done");
}

struct BoundaryRewriteHook;
#[async_trait::async_trait]
impl axocoatl_tools::ToolHook for BoundaryRewriteHook {
    fn name(&self) -> &str {
        "rewrite-evidence-probe"
    }
    fn phases(&self) -> Vec<axocoatl_tools::HookPhase> {
        vec![
            axocoatl_tools::HookPhase::Pre,
            axocoatl_tools::HookPhase::Post,
        ]
    }
    async fn execute(&self, ctx: &axocoatl_tools::HookContext) -> axocoatl_tools::HookAction {
        axocoatl_tools::HookAction::Transform {
            value: match ctx.phase {
                axocoatl_tools::HookPhase::Pre => serde_json::json!({"rewritten":true}),
                axocoatl_tools::HookPhase::Post => serde_json::json!({"display":"post-hook"}),
            },
        }
    }
}

#[tokio::test]
async fn acknowledged_arguments_are_post_hook_and_outcomes_are_raw() {
    let provider = boundary_provider(1);
    let tool = Arc::new(BoundaryCountingTool::new(
        axocoatl_llm::ConcurrencyPolicy::Exclusive,
    ));
    let mut behavior = boundary_behavior(provider.clone(), tool.clone(), false).await;
    let mut hooks = HookRegistry::new();
    hooks.register_global(Arc::new(BoundaryRewriteHook));
    behavior = behavior.with_hook_registry(Arc::new(hooks));
    let boundary = Arc::new(BoundaryProbe::default());
    let output = behavior
        .execute_controlled(
            AgentInput::text("do work"),
            boundary_control(boundary.clone()),
        )
        .await
        .unwrap();
    let actual = serde_json::json!({"rewritten":true});
    assert_eq!(
        boundary.requests.lock().unwrap()[0].tool_call.arguments,
        actual
    );
    assert_eq!(
        tool.received.lock().unwrap().as_slice(),
        std::slice::from_ref(&actual)
    );
    assert_eq!(
        boundary.outcomes.lock().unwrap()[0].1,
        ToolInvocationOutcome::Returned(Ok(serde_json::json!({"raw":actual})))
    );
    assert_eq!(
        output.output().tool_calls[0].result,
        Some(serde_json::json!({"display":"post-hook"}))
    );
    assert_eq!(provider.calls.load(BoundaryOrdering::SeqCst), 2);
}

#[tokio::test]
async fn acknowledged_parallel_idless_calls_keep_exact_provider_indices() {
    let provider = boundary_provider(3);
    let tool = Arc::new(BoundaryCountingTool::new(
        axocoatl_llm::ConcurrencyPolicy::Safe,
    ));
    let mut behavior = boundary_behavior(provider.clone(), tool.clone(), false).await;
    let boundary = Arc::new(BoundaryProbe::default());
    let output = behavior
        .execute_controlled(
            AgentInput::text("do work"),
            boundary_control(boundary.clone()),
        )
        .await
        .unwrap();
    assert_eq!(tool.executions.load(BoundaryOrdering::SeqCst), 3);
    let mut requests = boundary.requests.lock().unwrap().clone();
    requests.sort_by_key(|request| request.provider_call_index);
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(request.provider_call_index, index);
        assert_eq!(request.provider_call_count, 3);
        assert_eq!(
            request.provider_response_group,
            requests[0].provider_response_group
        );
        assert_eq!(
            request.tool_call.arguments,
            serde_json::json!({"index":index})
        );
        assert_eq!(
            output.output().tool_calls[index].arguments,
            request.tool_call.arguments
        );
    }
    let mut outcomes = boundary.outcomes.lock().unwrap().clone();
    outcomes.sort_by_key(|(index, _)| *index);
    assert_eq!(outcomes.len(), 3);
    for (index, (_, outcome)) in outcomes.iter().enumerate() {
        assert_eq!(
            *outcome,
            ToolInvocationOutcome::Returned(Ok(serde_json::json!({"raw":{"index":index}})))
        );
    }
}

#[tokio::test]
async fn acknowledged_outcome_failure_prevents_model_retry() {
    let provider = boundary_provider(1);
    let tool = Arc::new(BoundaryCountingTool::new(
        axocoatl_llm::ConcurrencyPolicy::Exclusive,
    ));
    let mut behavior = boundary_behavior(provider.clone(), tool.clone(), false).await;
    let boundary = Arc::new(BoundaryProbe {
        fail_outcome: true,
        ..Default::default()
    });
    let error = behavior
        .execute_controlled(AgentInput::text("do work"), boundary_control(boundary))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("outcome persistence failed"));
    assert_eq!(tool.executions.load(BoundaryOrdering::SeqCst), 1);
    assert_eq!(provider.calls.load(BoundaryOrdering::SeqCst), 1);
}

#[tokio::test]
async fn acknowledged_backend_panic_stays_unknown_and_prevents_model_retry() {
    let provider = boundary_provider(1);
    let tool = Arc::new(BoundaryCountingTool {
        panic_after_effect: true,
        ..BoundaryCountingTool::new(axocoatl_llm::ConcurrencyPolicy::Exclusive)
    });
    let mut behavior = boundary_behavior(provider.clone(), tool.clone(), false).await;
    let boundary = Arc::new(BoundaryProbe::default());
    let error = behavior
        .execute_controlled(
            AgentInput::text("do work"),
            boundary_control(boundary.clone()),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unknown"));
    assert_eq!(tool.executions.load(BoundaryOrdering::SeqCst), 1);
    assert_eq!(provider.calls.load(BoundaryOrdering::SeqCst), 1);
    assert!(matches!(
        boundary.outcomes.lock().unwrap()[0].1,
        ToolInvocationOutcome::Unknown { .. }
    ));
}

#[tokio::test]
async fn acknowledged_stop_waits_for_claimed_effect_outcome() {
    let provider = boundary_provider(1);
    let release = Arc::new(tokio::sync::Notify::new());
    let tool = Arc::new(BoundaryCountingTool {
        release: Some(release.clone()),
        ..BoundaryCountingTool::new(axocoatl_llm::ConcurrencyPolicy::Exclusive)
    });
    let mut behavior = boundary_behavior(provider.clone(), tool.clone(), false).await;
    let boundary = Arc::new(BoundaryProbe::default());
    let control = boundary_control(boundary.clone());
    let runner_control = control.clone();
    let run = tokio::spawn(async move {
        behavior
            .execute_controlled(AgentInput::text("do work"), runner_control)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), tool.started.notified())
        .await
        .unwrap();
    control.cancel();
    assert!(!run.is_finished());
    assert!(boundary.outcomes.lock().unwrap().is_empty());
    release.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(outcome.is_cancelled());
    assert_eq!(boundary.outcomes.lock().unwrap().len(), 1);
    assert_eq!(provider.calls.load(BoundaryOrdering::SeqCst), 1);
}

#[tokio::test]
async fn acknowledged_parallel_failure_joins_already_claimed_sibling() {
    let provider = boundary_provider(2);
    let release = Arc::new(tokio::sync::Notify::new());
    let tool = Arc::new(BoundaryCountingTool {
        release: Some(release.clone()),
        ..BoundaryCountingTool::new(axocoatl_llm::ConcurrencyPolicy::Safe)
    });
    let mut behavior = boundary_behavior(provider.clone(), tool.clone(), false).await;
    let boundary = Arc::new(BoundaryProbe {
        deny_index: Some(1),
        wait_before_denial: Some(tool.started.clone()),
        ..Default::default()
    });
    let control = boundary_control(boundary.clone());
    let runner_control = control.clone();
    let run = tokio::spawn(async move {
        behavior
            .execute_controlled(AgentInput::text("do work"), runner_control)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), control.cancelled())
        .await
        .unwrap();
    assert_eq!(tool.executions.load(BoundaryOrdering::SeqCst), 1);
    assert!(
        !run.is_finished(),
        "the claimed sibling still owes a durable outcome"
    );
    release.notify_one();
    let error = tokio::time::timeout(std::time::Duration::from_secs(2), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("admission failed"));
    assert_eq!(boundary.outcomes.lock().unwrap().len(), 1);
    assert_eq!(provider.calls.load(BoundaryOrdering::SeqCst), 1);
}

#[tokio::test]
async fn acknowledged_stop_before_run_skips_admission_and_backend() {
    let provider = boundary_provider(1);
    let tool = Arc::new(BoundaryCountingTool::new(
        axocoatl_llm::ConcurrencyPolicy::Exclusive,
    ));
    let mut behavior = boundary_behavior(provider.clone(), tool.clone(), false).await;
    let boundary = Arc::new(BoundaryProbe::default());
    let control = boundary_control(boundary.clone());
    control.cancel();
    assert!(behavior
        .execute_controlled(AgentInput::text("do work"), control)
        .await
        .unwrap()
        .is_cancelled());
    assert!(boundary.requests.lock().unwrap().is_empty());
    assert_eq!(tool.executions.load(BoundaryOrdering::SeqCst), 0);
    assert_eq!(provider.calls.load(BoundaryOrdering::SeqCst), 0);
}
