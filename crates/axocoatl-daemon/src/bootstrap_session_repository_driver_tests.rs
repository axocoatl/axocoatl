use super::*;
use crate::session_dispatch::{AutonomousActivationFactory, AutonomousNodeInput};

struct Factory {
    config: AgentConfig,
    profile: ExecutionProfile,
    provider: Arc<Provider>,
}
#[async_trait::async_trait]
impl AutonomousActivationFactory for Factory {
    async fn resources(
        &self,
        _: &ActivationInputManifest,
    ) -> std::result::Result<AutonomousActivationResources, String> {
        Ok(AutonomousActivationResources {
            config: self.config.clone(),
            profile: self.profile.clone(),
            provider: self.provider.clone(),
            counter: Arc::new(Counter),
            tools: Arc::new(axocoatl_tools::ToolExecutor::new()),
        })
    }
}
fn seed(r: &Run) -> AutonomousNodeInput {
    let snapshot = r.controller.snapshot().unwrap();
    let input = &snapshot.contract().activations()[0].input;
    AutonomousNodeInput {
        node_id: input.activation.node_id.clone(),
        guidance: input.guidance.clone(),
        attachments: input.attachments.clone(),
        repository: input.repository.clone(),
        budget: input.budget.clone(),
        grant: input.grant.clone(),
    }
}

#[tokio::test]
async fn ordinary_driver_routes_recorded_repository_to_its_owned_port_and_keeps_plaintext_path() {
    for recorded in [false, true] {
        let mut f = fixture().await;
        let r = run(&mut f, &[], recorded);
        let provider = Provider::new(vec![]);
        let factory = Arc::new(Factory {
            config: r.config.clone(),
            profile: r.profile.clone(),
            provider: provider.clone(),
        });
        let driver = r
            .controller
            .autonomous_turn_driver(vec![seed(&r)], factory)
            .unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(5), driver.run())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            outcome.snapshot.contract().state(),
            Some(LogicalTurnState::Completed)
        );
        assert_eq!(outcome.finalized.unwrap().promotion().selected.len(), 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        let requests = provider.requests.lock().unwrap();
        let actual_context = requests[0]
            .iter()
            .filter_map(ChatMessage::text_content)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            actual_context.contains(r.resource.reference().as_str()),
            recorded
        );
        assert!(f.owner.execution_is_idle().unwrap());
        assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn driver_never_falls_back_to_plaintext_when_recorded_repository_owner_is_stale() {
    let mut f = fixture().await;
    let r = run(&mut f, &[], true);
    let provider = Provider::new(vec![]);
    let factory = Arc::new(Factory {
        config: r.config.clone(),
        profile: r.profile.clone(),
        provider: provider.clone(),
    });
    let driver = r
        .controller
        .autonomous_turn_driver(vec![seed(&r)], factory)
        .unwrap();
    f.owner.inner.sandboxes.lock().await.clear();
    let outcome = tokio::time::timeout(Duration::from_secs(5), driver.run())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert!(outcome.finalized.is_none());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(outcome
        .snapshot
        .contract()
        .current_accepted_activations()
        .is_empty());
    assert!(f.operation.try_lock().is_err());
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn native_driver_rejects_ready_e2b_owner_before_provider_or_repository_execution() {
    let mut f = fixture().await;
    // Model an otherwise valid Ready E2B owner, including matching retained,
    // live and durable runtime identities. A mismatched identity would already
    // fail without checking native process-supervision support.
    let mut sandbox = ControlledSandbox::new(f.owner.root(), "remote-incarnation");
    sandbox.remote_id = Some("remote-incarnation".into());
    let sandbox = Arc::new(sandbox);
    let registered: Arc<dyn Sandbox> = sandbox.clone();
    {
        let inner = Arc::get_mut(&mut f.owner.inner).unwrap();
        inner.runtime.backend = "e2b".into();
        inner.runtime.id = "remote-incarnation".into();
        inner.runtime.remote_root = Some(inner.metadata.runtime_root.to_string_lossy().into());
        inner.runtime.control_plane = Some("https://api.e2b.test".into());
        inner.runtime.data_plane_domain = Some("e2b.test".into());
        inner.metadata.backend = inner.runtime.backend.clone();
        inner.metadata.runtime_id = inner.runtime.id.clone();
        inner.metadata.execution_identity = sandbox.incarnation.clone();
        inner.sandbox = registered.clone();
        inner
            .sandboxes
            .lock()
            .await
            .insert(inner.metadata.session_id.clone(), registered);
        let session = inner
            .sessions
            .lock()
            .await
            .set_environment(
                &inner.metadata.session_id,
                SessionEnvironmentState::Ready,
                Some("e2b:base".into()),
                Some(inner.runtime.clone()),
                vec![],
                None,
            )
            .unwrap();
        inner.metadata.environment_generation = session.environment.generation;
        // Ready remains usable by compatibility paths. Only native ownership
        // must refuse this backend, without changing or deleting its runtime.
        require_session_environment_ready(&session).unwrap();
        assert!(validate_session_owner(&session, &inner.identity)
            .unwrap_err()
            .to_string()
            .contains("requires local Podman process supervision"));
    }
    f.sandbox = sandbox;
    let r = run(&mut f, &["write_file"], true);
    let provider = Provider::new(vec![(
        "write_file",
        serde_json::json!({"path":"unsupported-effect", "content":"must not be written"}),
    )]);
    let factory = Arc::new(Factory {
        config: r.config.clone(),
        profile: r.profile.clone(),
        provider: provider.clone(),
    });
    let driver = r
        .controller
        .autonomous_turn_driver(vec![seed(&r)], factory)
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), driver.run())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert!(outcome.finalized.is_none());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(!f._workspace.path().join("unsupported-effect").exists());
    assert!(f.owner.execution_is_idle().unwrap());
    assert_eq!(f.sandbox.stop_calls.load(Ordering::SeqCst), 0);
    let session = f
        .owner
        .inner
        .sessions
        .lock()
        .await
        .get(&f.owner.metadata().session_id)
        .unwrap();
    assert_eq!(session.environment.state, SessionEnvironmentState::Ready);
    assert_eq!(session.environment.runtime.as_ref().unwrap().backend, "e2b");
}

/// The continuation that reruns one required check of a paused turn.
fn rerun_check(
    controller: &SessionDispatchController,
    check: &str,
) -> crate::session_dispatch::HumanControlActionRequest {
    let snapshot = controller.snapshot().unwrap();
    let contract = snapshot.contract();
    crate::session_dispatch::HumanControlActionRequest {
        schema_version: 1,
        command_id: CommandId::new(format!("rerun-{}", check.replace(':', "-"))).unwrap(),
        session_id: snapshot.owner().session_id.clone(),
        turn_id: snapshot.turn_id().clone(),
        execution_epoch_id: contract.epochs().last().unwrap().id.clone(),
        expected_turn_revision: contract.revision(),
        expected_graph_revision: contract.graph().unwrap().revision,
        activation: None,
        action: crate::session_dispatch::HumanControlAction::Continue,
        instruction: None,
        include_previous_output: false,
        context: None,
        continuation: Some(crate::session_dispatch::HumanContinuationSelection {
            restart: vec![],
            checks: vec![ConditionId::new(check).unwrap()],
        }),
        blocker_id: None,
        human_response: None,
        partial_finish: None,
    }
}

/// A required check the host runs after the Agent: a failing command leaves
/// the turn needing attention with its output, and rerunning it after the
/// person fixes the tree completes the turn. Every command runs in the actual
/// supervised sandbox between two captures of the same candidate.
#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_failing_required_check_needs_attention_and_passing_check_completes() {
    let mut f = fixture().await;
    let sandbox = actual_sandbox(&mut f).await;
    git_init(f._workspace.path());
    std::fs::write(f._workspace.path().join("notes.txt"), "draft\n").unwrap();
    let checks = vec![vec![
        "sh".into(),
        "-c".into(),
        "echo checking approval; test -f approved.txt || { echo missing approval >&2; exit 3; }"
            .into(),
    ]];
    let r = run_checked(&mut f, &["bash"], &checks);
    let authorized = r
        .controller
        .authorize_required_checks(r.resource.reference());
    let provider = Provider::new(vec![]);
    let factory = Arc::new(Factory {
        config: r.config.clone(),
        profile: r.profile.clone(),
        provider: provider.clone(),
    });
    let first = tokio::time::timeout(Duration::from_secs(180), async {
        r.controller
            .autonomous_turn_driver(vec![seed(&r)], factory.clone())?
            .run()
            .await
    })
    .await;
    let failed = r.controller.control_plane();
    // The person makes the change the check asks for and reruns only it.
    std::fs::write(f._workspace.path().join("approved.txt"), "yes\n").unwrap();
    let receipt = r.controller.submit_human_action(
        rerun_check(&r.controller, "required-check:1"),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
    );
    let second = tokio::time::timeout(Duration::from_secs(180), async {
        r.controller
            .autonomous_turn_driver(vec![seed(&r)], factory.clone())?
            .run()
            .await
    })
    .await;
    let passed = r.controller.control_plane();
    let idle = f.owner.execution_is_idle();
    sandbox.stop_checked().await.unwrap();

    authorized.unwrap();
    let first = first.unwrap().unwrap();
    assert_eq!(
        first.snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert!(first.finalized.is_none());
    let contract = first.snapshot.contract();
    let outcome = |id: &str| {
        contract
            .current_condition(&ConditionId::new(id).unwrap())
            .map(|observation| observation.outcome)
    };
    assert_eq!(outcome("required-check:0"), Some(ConditionOutcome::Passed));
    assert_eq!(outcome("required-check:1"), Some(ConditionOutcome::Failed));
    assert_eq!(outcome("required-check:2"), Some(ConditionOutcome::Passed));
    assert_eq!(
        outcome("required-check:ready"),
        Some(ConditionOutcome::Failed)
    );
    let failed = failed.unwrap();
    assert_eq!(failed.required_checks.len(), 1);
    let check = &failed.required_checks[0];
    assert_eq!(check.argv, checks[0]);
    assert_eq!(check.state, "failed");
    assert_eq!(check.exit_code, Some(3));
    assert_eq!(check.stdout, "checking approval\n");
    assert_eq!(check.stderr, "missing approval\n");
    assert!(failed
        .turn_controls
        .unwrap()
        .check_choices
        .iter()
        .any(|choice| choice.condition_id.as_str() == "required-check:1"
            && choice.capability.enabled));

    let receipt = receipt.unwrap();
    assert_eq!(
        receipt.state,
        axocoatl_session::control_command::ControlCommandState::Settled,
        "{receipt:?}"
    );
    let second = second.unwrap().unwrap();
    let contract = second.snapshot.contract();
    assert_eq!(contract.state(), Some(LogicalTurnState::Completed));
    assert!(second.finalized.is_some());
    assert!(contract
        .current_condition(&ConditionId::new("required-check:ready").unwrap())
        .is_some_and(|observation| observation.outcome == ConditionOutcome::Passed));
    // The rerun repeated both captures and the selected check, nothing else.
    assert_eq!(contract.condition_runs().len(), 6);
    let passed = passed.unwrap();
    let check = &passed.required_checks[0];
    assert_eq!(check.state, "passed");
    assert_eq!(check.exit_code, Some(0));
    assert_eq!(check.stdout, "checking approval\n");
    // The Agent answered once; the host ran every check.
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(idle.unwrap());
}
