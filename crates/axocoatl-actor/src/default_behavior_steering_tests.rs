// Included in the existing native behavior test module.
struct SteeringProbe {
    final_only: bool,
    issued: std::sync::atomic::AtomicBool,
    acknowledged: Arc<AtomicUsize>,
    fail_ack: bool,
}
struct SteeringProbeAck { count: Arc<AtomicUsize>, fail: bool }
impl crate::SteeringAcknowledgement for SteeringProbeAck {
    fn acknowledge(self: Box<Self>) -> Result<(), String> {
        self.count.fetch_add(1, BoundaryOrdering::SeqCst);
        if self.fail { Err("guidance acknowledgement failed".into()) } else { Ok(()) }
    }
}
#[async_trait::async_trait]
impl ToolExecutionBoundary for SteeringProbe {
    async fn admit(&self, _: &ToolInvocationRequest) -> Result<Box<dyn AdmittedToolInvocation>, String> {
        Err("fixture never authorizes tools".into())
    }
    fn take_guidance(&self, final_boundary: bool) -> Result<Option<crate::SteeringDelivery>, String> {
        if (self.final_only && !final_boundary) || self.issued.swap(true, BoundaryOrdering::SeqCst) { return Ok(None); }
        Ok(Some(crate::SteeringDelivery { attachments:Vec::new(), text: "Keep the completed answer and clarify it".into(),
            acknowledgement: Box::new(SteeringProbeAck { count: self.acknowledged.clone(), fail: self.fail_ack }) }))
    }
}

#[tokio::test]
async fn native_final_answer_guidance_preserves_prior_response_and_measured_usage() {
    let provider = boundary_provider(0);
    let tool = Arc::new(BoundaryCountingTool::new(axocoatl_llm::ConcurrencyPolicy::Exclusive));
    let mut behavior = boundary_behavior(provider.clone(), tool, false).await;
    let acknowledgements = Arc::new(AtomicUsize::new(0));
    let control = AgentRunControl::new(crate::AgentRunId::new("same-activation"))
        .with_execution_boundary(Arc::new(SteeringProbe { final_only: true, issued: std::sync::atomic::AtomicBool::new(false),
            acknowledged: acknowledgements.clone(), fail_ack: false }));
    let output = behavior.execute_controlled(AgentInput::text("initial request"), control).await.unwrap();
    assert!(matches!(output, AgentRunOutcome::Completed(_)));
    assert_eq!(output.output().content, "done");
    assert_eq!(output.output().token_usage, TokenUsageStats::new(20, 4));
    assert_eq!(provider.calls.load(BoundaryOrdering::SeqCst), 2);
    assert_eq!(acknowledgements.load(BoundaryOrdering::SeqCst), 1);
    let messages = behavior.session.messages();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].content, "initial request");
    assert_eq!(messages[1].role, MessageRole::Assistant);
    assert_eq!(messages[2].content, "Keep the completed answer and clarify it");
    assert_eq!(messages[3].content, "done");
    let second = serde_json::to_string(&provider.requests.lock().unwrap()[1].messages).unwrap();
    assert!(second.contains("Keep the completed answer and clarify it"));
}

#[tokio::test]
async fn guidance_acknowledgement_failure_preserves_incurred_usage_and_prevents_next_provider() {
    let provider = boundary_provider(0);
    let tool = Arc::new(BoundaryCountingTool::new(axocoatl_llm::ConcurrencyPolicy::Exclusive));
    let mut behavior = boundary_behavior(provider.clone(), tool, false).await;
    let acknowledgements = Arc::new(AtomicUsize::new(0));
    let control = AgentRunControl::new(crate::AgentRunId::new("ack-failure"))
        .with_execution_boundary(Arc::new(SteeringProbe { final_only: true, issued: std::sync::atomic::AtomicBool::new(false),
            acknowledged: acknowledgements.clone(), fail_ack: true }));
    let result = behavior.execute_controlled(AgentInput::text("initial request"), control.clone()).await;
    assert!(result.unwrap_err().to_string().contains("guidance acknowledgement failed"));
    assert_eq!(provider.calls.load(BoundaryOrdering::SeqCst), 1);
    assert_eq!(acknowledgements.load(BoundaryOrdering::SeqCst), 1);
    assert!(control.execution_boundary_failure().is_some());
    assert_eq!(behavior.cumulative_token_usage_measurement().usage, TokenUsageStats::new(10, 2));
}
