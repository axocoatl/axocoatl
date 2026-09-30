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
