// Included inside coordinator::tests to exercise the actual worker path.
#[derive(Default)]
struct CoordinatorBoundaryProbe {
    children: std::sync::Mutex<Vec<(crate::ChildExecutionRequest, AgentRunControl)>>,
    refuse: bool,
}

#[async_trait]
impl crate::ToolExecutionBoundary for CoordinatorBoundaryProbe {
    async fn admit(
        &self,
        _: &crate::ToolInvocationRequest,
    ) -> Result<Box<dyn crate::AdmittedToolInvocation>, String> {
        Err("the text-only child must not dispatch tools".into())
    }

    async fn provision_child(
        &self,
        request: &crate::ChildExecutionRequest,
        control: AgentRunControl,
    ) -> Result<Arc<dyn crate::ToolExecutionBoundary>, String> {
        if self.refuse {
            return Err("host refused proposed child".into());
        }
        self.children
            .lock()
            .unwrap()
            .push((request.clone(), control));
        // Each child's host boundary is a distinct object. It must not receive
        // the parent boundary's implicit authority or cancellation handle.
        Ok(Arc::new(CoordinatorBoundaryProbe {
            refuse: true,
            ..Default::default()
        }))
    }
}

#[tokio::test]
async fn acknowledged_coordinator_provisions_distinct_children_before_execution() {
    let mut coordinator = CoordinatorBehavior::new(Arc::new(MockLlm), Arc::new(SimpleCounter));
    coordinator
        .on_start(&AgentConfig {
            id: AgentId::new("boundary-distinct-children"),
            ..coord_config()
        })
        .await
        .unwrap();
    let boundary = Arc::new(CoordinatorBoundaryProbe::default());
    let parent = AgentRunControl::new(crate::AgentRunId::new("parent-activation"))
        .with_execution_boundary(boundary.clone());
    coordinator
        .execute_controlled(AgentInput::text("do work"), parent.clone())
        .await
        .unwrap();
    {
        let children = boundary.children.lock().unwrap();
        assert_eq!(children.len(), 2);
        assert_ne!(children[0].0.actor_id, children[1].0.actor_id);
        assert_ne!(children[0].1.id(), children[1].1.id());
        assert_eq!(coordinator.worker_results.len(), 2);
        children[0].1.cancel();
        assert!(!children[1].1.is_cancelled());
        assert!(!parent.is_cancelled());
        parent.cancel();
        assert!(children[1].1.is_cancelled());
    }
    coordinator.on_stop().await.unwrap();
    assert!(coordinator.active_workers.is_empty());
    assert!(coordinator.worker_handles.is_empty());
}

#[tokio::test]
async fn acknowledged_coordinator_child_refusal_cannot_fall_back_to_legacy() {
    let mut coordinator = CoordinatorBehavior::new(Arc::new(MockLlm), Arc::new(SimpleCounter));
    coordinator
        .on_start(&AgentConfig {
            id: AgentId::new("boundary-child-refusal"),
            ..coord_config()
        })
        .await
        .unwrap();
    let boundary = Arc::new(CoordinatorBoundaryProbe {
        refuse: true,
        ..Default::default()
    });
    let control = AgentRunControl::new(crate::AgentRunId::new("parent-activation"))
        .with_execution_boundary(boundary);
    let error = coordinator
        .execute_controlled(AgentInput::text("do work"), control)
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("child activation admission failed"));
    assert!(coordinator.worker_results.is_empty());
    assert!(coordinator.active_workers.is_empty());
    coordinator.on_stop().await.unwrap();
    assert!(coordinator.worker_handles.is_empty());
}

#[tokio::test]
async fn acknowledged_child_persistence_failure_cannot_be_synthesized_as_success() {
    let provider = Arc::new(GatedCoordinatorToolLlm {
        plan: r#"[{"name":"change-one","description":"change-one","tools":["side_effect"]},{"name":"change-two","description":"change-two","tools":["side_effect"]}]"#,
        direct_calls: std::sync::atomic::AtomicUsize::new(0),
        stream_calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release = Arc::new(tokio::sync::Notify::new());
    release.notify_one();
    let mut executor = ToolExecutor::new();
    executor.register_builtin(
        "side_effect",
        Arc::new(GatedCoordinatorTool {
            started: Arc::new(tokio::sync::Notify::new()),
            release,
            finished: finished.clone(),
        }),
    );
    let mut coordinator = CoordinatorBehavior::new(provider.clone(), Arc::new(SimpleCounter))
        .with_tool_executor(Arc::new(executor));
    coordinator
        .on_start(&AgentConfig {
            id: AgentId::new("boundary-persistence-failure"),
            ..coord_config()
        })
        .await
        .unwrap();
    let control = AgentRunControl::new(crate::AgentRunId::new("parent-activation"))
        .with_execution_boundary(Arc::new(CoordinatorBoundaryProbe::default()));
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        coordinator.execute_controlled(AgentInput::text("route"), control.clone()),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("tool invocation admission failed"));
    assert!(control.execution_boundary_failure().is_some());
    assert!(!finished.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(
        provider
            .direct_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "only decomposition ran; synthesis must not turn an evidence persistence failure \
         into success"
    );
    coordinator.on_stop().await.unwrap();
    assert!(coordinator.active_workers.is_empty());
    assert!(coordinator.worker_handles.is_empty());
}

#[derive(Default)]
struct OwnedCoordinatorChildProbe {
    scheduled: std::sync::Mutex<Vec<crate::ChildExecutionRequest>>,
    runs: Arc<std::sync::atomic::AtomicUsize>,
    stages: std::sync::Mutex<Vec<axocoatl_memory::AgentCheckpoint>>,
}
struct OwnedChildProbe {
    runs: Arc<std::sync::atomic::AtomicUsize>,
}
#[async_trait]
impl crate::AdmittedChildExecution for OwnedChildProbe {
    async fn run(
        self: Box<Self>,
    ) -> Result<crate::MeasuredAgentRunOutcome, crate::AgentExecutionFailure> {
        self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(crate::MeasuredAgentRunOutcome {
            outcome: AgentRunOutcome::Completed(AgentOutput::text(
                "accepted controller child output",
            )),
            token_usage: axocoatl_core::MeasuredTokenUsage::known(TokenUsageStats::new(3, 4)),
        })
    }
}
#[async_trait]
impl crate::ToolExecutionBoundary for OwnedCoordinatorChildProbe {
    async fn admit(
        &self,
        _: &crate::ToolInvocationRequest,
    ) -> Result<Box<dyn crate::AdmittedToolInvocation>, String> {
        Err("Coordinator has no direct tool grant".into())
    }
    async fn schedule_child(
        &self,
        request: &crate::ChildExecutionRequest,
        _: AgentRunControl,
    ) -> Result<Option<Box<dyn crate::AdmittedChildExecution>>, String> {
        self.scheduled.lock().unwrap().push(request.clone());
        Ok(Some(Box::new(OwnedChildProbe {
            runs: self.runs.clone(),
        })))
    }
    async fn provision_child(
        &self,
        _: &crate::ChildExecutionRequest,
        _: AgentRunControl,
    ) -> Result<Arc<dyn crate::ToolExecutionBoundary>, String> {
        panic!("a controller child must never fall back to the private Worker actor path")
    }
}
#[async_trait]
impl crate::ActivationCheckpointPort for OwnedCoordinatorChildProbe {
    async fn restore(&self) -> Result<Option<axocoatl_memory::AgentCheckpoint>, String> {
        Ok(None)
    }
    async fn stage(&self, checkpoint: &axocoatl_memory::AgentCheckpoint) -> Result<(), String> {
        self.stages
            .lock()
            .unwrap()
            .push(serde_json::from_value(serde_json::to_value(checkpoint).unwrap()).unwrap());
        Ok(())
    }
}
#[tokio::test]
async fn coordinator_owned_children_use_controller_handoffs_and_one_final_candidate() {
    let host = Arc::new(OwnedCoordinatorChildProbe::default());
    let mut coordinator = CoordinatorBehavior::new(Arc::new(MockLlm), Arc::new(SimpleCounter))
        .with_activation_checkpoint_port(host.clone());
    coordinator
        .on_start(&AgentConfig {
            id: AgentId::new("boundary-owned-children"),
            ..coord_config()
        })
        .await
        .unwrap();
    let control = AgentRunControl::new(crate::AgentRunId::new("owned-parent"))
        .with_execution_boundary(host.clone());
    coordinator
        .execute_controlled(AgentInput::text("do work"), control.clone())
        .await
        .unwrap();
    assert_eq!(host.scheduled.lock().unwrap().len(), 2);
    assert_eq!(host.runs.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(coordinator.active_workers.is_empty());
    assert!(coordinator.worker_handles.is_empty());
    {
        let stages = host.stages.lock().unwrap();
        assert_eq!(
            stages.len(),
            1,
            "only final actual native state uses the immutable candidate slot"
        );
        let state: OrchestrationState =
            serde_json::from_str(stages[0].behavior_state.as_deref().unwrap()).unwrap();
        assert!(state.completed);
    }
    assert!(coordinator
        .execute_controlled(AgentInput::text("again"), control)
        .await
        .is_err());
    assert_eq!(host.runs.load(std::sync::atomic::Ordering::SeqCst), 2);
    coordinator.on_stop().await.unwrap();
}
