//! Actual Podman → owned controller → durable condition → lifecycle proof.
//! This exercises the internal host port; it does not enable live v2 ingress.
#![cfg(unix)]

use super::*;
use crate::bootstrap::session_dispatch::SessionDispatchRegistry;
use crate::session_dispatch::SessionDispatchController;
use axocoatl_core::TokenUsageStats;
use axocoatl_isolation::{SandboxNetwork, SandboxPolicy, SessionSandbox};
use axocoatl_session::control_authority::{AuthorityGrant, ConditionPermission, GrantLimits};
use axocoatl_session::execution_content::{
    ActivationEvidenceContent, ActivationOutputContent, ConditionProcessStatus,
    ExecutionContentStore, ExecutionRequestContent, ExecutionUsage, OutputKind,
    RepositoryCheckDefinition,
};
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::execution_ownership::LegacyFormatOwnership;
use axocoatl_session::execution_store::ExecutionStoreOwner;
use axocoatl_session::turn_contract::{
    CheckpointSource, ConditionEffectResolution, ConditionKind, ConditionOutcome, ConditionRunId,
    ConditionRunRef, EvidenceRef, GrantId, GrantSnapshotRef, SessionId, TurnContractEnvelope,
    TurnContractEvent,
};
use axocoatl_session::{SessionEnvironmentState, SessionMode};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Default)]
struct RuntimeCleanup {
    names: Mutex<Vec<String>>,
    sandboxes: Mutex<Vec<Arc<SessionSandbox>>>,
}

impl RuntimeCleanup {
    async fn remove_owned(&self) -> Vec<String> {
        let mut failures = Vec::new();
        let sandboxes = self.sandboxes.lock().unwrap().clone();
        for sandbox in sandboxes {
            if let Err(error) = sandbox.stop_checked().await {
                failures.push(error.to_string());
            }
        }
        let names = self.names.lock().unwrap().clone();
        for name in names {
            // Every name is a fresh Session created by this test. Never sweep
            // another daemon's, test's, or user's containers.
            if let Err(error) = SessionSandbox::remove_named_with_dependencies(&name).await {
                failures.push(error.to_string());
            }
            match SessionSandbox::named_running(&name).await {
                Ok(false) => (),
                Ok(true) => failures.push(format!("test Session {name} is still running")),
                Err(error) => failures.push(error.to_string()),
            }
        }
        failures
    }
}

async fn observe_file(path: &Path, expected: &[u8]) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if std::fs::read(path).is_ok_and(|bytes| bytes == expected) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing process observation at {}", path.display()));
}

async fn background_is_alive(sandbox: &SessionSandbox, workspace: &Path, task: &str) {
    let path = workspace.join("unrelated-progress");
    let before = std::fs::metadata(&path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > before) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("check cancellation and idle retirement must preserve unrelated processes");
    assert!(sandbox
        .list_tasks()
        .iter()
        .any(|item| item.id == task && item.status == "running"));
}

async fn prove_owned_check(
    data_path: PathBuf,
    workspace_path: PathBuf,
    cleanup: Arc<RuntimeCleanup>,
) {
    let image = std::env::var("AXO_SUPERVISOR_TEST_IMAGE")
        .expect("set AXO_SUPERVISOR_TEST_IMAGE to the explicitly prepared root tools image");
    let data_root = SecureDir::open(&data_path).unwrap();
    let mut workspaces = WorkspaceStore::new_in_secure(&data_root, "workspaces").unwrap();
    let workspace = workspaces
        .register(&workspace_path, Some("Actual controller check"))
        .unwrap();
    let mut sessions = SessionStore::new_in_secure(&data_root, "sessions").unwrap();
    let session = sessions
        .create_with_environment(
            "Actual controller check",
            &workspace.id,
            &workspace.canonical_path,
            SessionMode::SingleAgent {
                agent_id: "agent".into(),
            },
            vec![],
            vec![],
            Some(image.clone()),
            None,
            false,
            true,
        )
        .unwrap();
    cleanup.names.lock().unwrap().push(session.id.clone());
    let supervisor_root = data_root.child("execution-supervisors").unwrap();
    let policy = SandboxPolicy {
        allow_untrusted_image: true,
        network: SandboxNetwork::None,
        runtime_authority: Some(format!("{:x}", Sha256::digest(session.id.as_bytes()))),
        supervisor_installation: Some(supervisor_root.clone()),
        ..SandboxPolicy::default()
    };
    let sandbox = Arc::new(
        SessionSandbox::start(
            &session.id,
            &workspace.canonical_path,
            Some(&image),
            &[],
            &[],
            &policy,
        )
        .await
        .expect("actual Ready sandbox with automatic embedded supervision"),
    );
    cleanup.sandboxes.lock().unwrap().push(sandbox.clone());
    let execution_identity = sandbox.execution_identity().unwrap().to_owned();
    let runtime = SessionRuntimeIdentity {
        backend: "podman".into(),
        id: session.id.clone(),
        remote_root: None,
        control_plane: None,
        data_plane_domain: None,
        authority_fingerprint: None,
        ownership_token: None,
        cleanup_confirmed: false,
    };
    let session = sessions
        .set_environment(
            &session.id,
            SessionEnvironmentState::Ready,
            None,
            Some(runtime.clone()),
            vec![],
            None,
        )
        .unwrap();
    let format = Arc::new(
        LegacyFormatOwnership::acquire(&data_path)
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    let store_owner = ExecutionStoreOwner {
        workspace_id: workspace.id.clone(),
        session_id: SessionId::new(session.id.clone()).unwrap(),
    };
    let mut canonical = SessionExecutionStore::open(format.clone(), store_owner.clone()).unwrap();
    canonical.verify_data_root(&data_root).unwrap();
    let mut content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let command = "trap '' TERM; printf x >> check-executions; printf '\\000\\377QA'; printf 'diagnostic\\n' >&2; printf 'started\\n' > check-started; while :; do sleep 1; done";
    let check_definition = RepositoryCheckDefinition {
        argv: vec!["sh".into(), "-c".into(), command.into()],
        timeout_ms: 30_000,
        stdout_bytes: 64,
        stderr_bytes: 64,
    };
    let definition = content
        .retain_repository_check_definition(check_definition.clone())
        .unwrap();
    let source: serde_json::Value = serde_json::from_str(include_str!(
        "../../axocoatl-session/tests/fixtures/turn_contract/check_only_recovery_preserves_accepted_generations.json"
    )).unwrap();
    let mut events: Vec<TurnContractEnvelope> = source["steps"].as_array().unwrap()[..3]
        .iter()
        .map(|step| serde_json::from_value(step["envelope"].clone()).unwrap())
        .collect();
    for event in &mut events {
        event.session_id = store_owner.session_id.clone();
        match &mut event.event {
            TurnContractEvent::StartActivation { input } => {
                input.activation.session_id = store_owner.session_id.clone()
            }
            TurnContractEvent::AcceptActivation { activation, .. } => {
                activation.session_id = store_owner.session_id.clone()
            }
            TurnContractEvent::Begin { .. } => (),
            _ => panic!("unexpected setup fixture event"),
        }
    }
    let kind = ConditionKind::RepositoryCheck {
        definition: definition.reference().clone(),
    };
    let TurnContractEvent::Begin { graph, .. } = &mut events[0].event else {
        panic!("Begin fixture");
    };
    graph.conditions[0].kind = kind.clone();
    let condition_id = graph.conditions[0].condition_id.clone();
    let request = content
        .retain_request(ExecutionRequestContent {
            turn_id: events[0].turn_id.clone(),
            recorded_at_unix_ms: 1,
            display_input: "Run the QA check".into(),
            effective_input: "Run the QA check".into(),
            context: vec![],
            target_definition: None,
            model: None,
        })
        .unwrap();
    canonical
        .begin_with_request(events[0].clone(), &request)
        .unwrap();
    canonical.append(events[1].clone()).unwrap();
    let TurnContractEvent::StartActivation { input } = &events[1].event else {
        panic!("started activation fixture");
    };
    let accepted_input = input.as_ref().clone();
    let TurnContractEvent::AcceptActivation {
        activation,
        checkpoint,
        output,
    } = &mut events[2].event
    else {
        panic!("accepted activation fixture");
    };
    // The fixture's original Session is replaced above. Rebind the complete
    // accepted checkpoint to the activation and conversation actually started.
    *activation = accepted_input.activation.clone();
    checkpoint.session_id = activation.session_id.clone();
    checkpoint.conversation_id = accepted_input.conversation_id.clone();
    checkpoint.source = CheckpointSource::Accepted {
        activation: activation.clone(),
    };
    let accepted_checkpoint = checkpoint.as_ref().clone();
    let run = ConditionRunRef {
        session_id: activation.session_id.clone(),
        turn_id: activation.turn_id.clone(),
        epoch_id: activation.execution_epoch_id.clone(),
        condition_id,
        run_id: ConditionRunId::new("actual-owned-repository-check").unwrap(),
        activations: vec![activation.clone()],
    };
    *output = content
        .retain_output(
            &canonical.snapshot(&run.turn_id).unwrap(),
            ActivationOutputContent {
                activation: activation.clone(),
                recorded_at_unix_ms: 2,
                text: "accepted implementation".into(),
                usage: ExecutionUsage::Measured {
                    usage: TokenUsageStats::default(),
                },
                kind: OutputKind::Final,
            },
        )
        .unwrap()
        .reference()
        .clone();
    let accepted_output = output.clone();
    canonical.append(events[2].clone()).unwrap();
    drop(content);

    let identity = canonical.identity().unwrap();
    validate_session_owner(&session, &identity).unwrap();
    let workspace_root = SecureDir::open(&workspace.canonical_path).unwrap();
    let registered: Arc<dyn Sandbox> = sandbox.clone();
    let operation = Arc::new(AsyncMutex::new(()));
    let operation_guard = operation.clone().lock_owned().await;
    let (_shutdown, receiver) = tokio::sync::watch::channel(false);
    let session_records = Arc::new(AsyncMutex::new(sessions));
    let owner = SessionRepositoryOwner {
        inner: Arc::new(RepositoryOwnerInner {
            attempt: None,
            identity,
            metadata: SessionRepositoryMetadata {
                workspace_id: workspace.id,
                session_id: session.id.clone(),
                environment_generation: session.environment.generation,
                backend: runtime.backend.clone(),
                runtime_id: runtime.id.clone(),
                execution_identity: execution_identity.clone(),
                runtime_root: workspace.canonical_path.clone(),
                host_workspace_inode: workspace_root.inode_identity().unwrap(),
            },
            runtime,
            sandbox: registered.clone(),
            data_root,
            workspace_root,
            sessions: session_records.clone(),
            workspaces: Arc::new(AsyncMutex::new(workspaces)),
            sandboxes: Arc::new(AsyncMutex::new(HashMap::from([(
                session.id.clone(),
                registered,
            )]))),
            start: Arc::new(AsyncMutex::new(())),
            shutdown: receiver,
            workspace_operation: Mutex::new(Some(operation_guard)),
            workspace_gate: operation.clone(),
            execution: Arc::new(AsyncMutex::new(())),
            state: Mutex::new(ExecutionState::default()),
            changed: tokio::sync::Notify::new(),
        }),
    };
    owner.validate_current().await.unwrap();
    let controller = SessionDispatchController::open(canonical, run.turn_id.clone()).unwrap();
    let registry = SessionDispatchRegistry::default();
    let repository = registry
        .register(controller.clone(), owner.clone())
        .unwrap();
    let grant_policy = AuthorityGrant {
        id: "actual-check-grant".into(),
        revision: 1,
        issuer_evidence: EvidenceRef::new("explicit-test-check-authorization").unwrap(),
        holder: run.activations[0].node_id.clone(),
        descendants: vec![],
        allow_stop_descendants: false,
        delegation: None,
        profiles: vec![],
        conditions: vec![ConditionPermission {
            kind,
            nodes: vec![run.activations[0].node_id.clone()],
            repository: repository.clone(),
            isolation: "podman".into(),
            max_timeout_ms: 30_000,
            max_stdout_bytes: 64,
            max_stderr_bytes: 64,
        }],
        limits: GrantLimits {
            activations: 0,
            invocations: 2,
            tokens: 0,
            cost_microunits: 0,
        },
        expires_at_ms: u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
            + 3_600_000,
    };
    let grant = GrantSnapshotRef {
        grant_id: GrantId::new(grant_policy.id.clone()).unwrap(),
        revision: 1,
        evidence: controller
            .retain_activation_evidence(ActivationEvidenceContent::Grant {
                policy: grant_policy.clone(),
            })
            .unwrap(),
    };
    controller.install_grant(grant_policy).unwrap();
    let unrelated = sandbox.spawn_background(
        "printf 'ready\\n' > unrelated-ready; while :; do printf x >> unrelated-progress; sleep 0.1; done",
    );
    observe_file(
        &workspace.canonical_path.join("unrelated-ready"),
        b"ready\n",
    )
    .await;
    let waiter = controller
        .start_repository_check(owner.clone(), run.clone(), repository.clone(), grant)
        .await
        .expect("actual granted controller repository port");
    observe_file(
        &workspace.canonical_path.join("check-started"),
        b"started\n",
    )
    .await;
    assert!(!owner.execution_is_idle().unwrap());
    assert!(operation.try_lock().is_err());
    let probe = Arc::downgrade(&owner.inner);
    // An idle observer must remain usable without counting as an active
    // repository execution ticket or preventing lifecycle cleanup.
    let observer = controller.clone();
    drop(owner);
    drop(controller);
    drop(waiter);
    assert!(
        operation.try_lock().is_err(),
        "dropping the caller cannot release Workspace ownership"
    );
    assert!(
        SessionExecutionStore::open(format.clone(), store_owner.clone()).is_err(),
        "the retained controller still owns the canonical Session writer"
    );

    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let retained = SessionRepositoryOwner {
                inner: probe.upgrade().expect("registered owner must remain alive"),
            };
            let idle = retained.execution_is_idle().unwrap();
            let observed = observer.snapshot().unwrap();
            let resolved = observed
                .contract()
                .condition_run(&run.run_id)
                .is_some_and(|condition| condition.resolution.is_some());
            if idle && resolved {
                break;
            }
            drop(retained);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the separately owned task must persist and settle after waiter Drop");
    {
        let retained = SessionRepositoryOwner {
            inner: probe.upgrade().unwrap(),
        };
        // Only the registry retains a strong registration gate. The observer
        // has a Weak gate, so this cannot succeed on observer ownership alone.
        assert_eq!(
            observer.retain_repository_resource(retained).unwrap(),
            repository
        );
    }
    assert!(
        operation.try_lock().is_err(),
        "process settlement alone must not retire its Workspace owner"
    );
    background_is_alive(&sandbox, &workspace.canonical_path, &unrelated).await;
    let durable_before_cleanup = observer.snapshot().unwrap();
    let mut ticket = registry
        .prepare_session_cleanup(&session.id, Duration::from_secs(5))
        .await
        .expect("idle observers must not block registry cleanup");
    let transferred = ticket
        .take_operation()
        .expect("the actual Workspace guard must transfer to lifecycle");
    assert!(operation.try_lock().is_err());
    assert_eq!(
        observer.snapshot().unwrap().journal_id(),
        durable_before_cleanup.journal_id()
    );
    background_is_alive(&sandbox, &workspace.canonical_path, &unrelated).await;
    // Retirement preserved the shared runtime. The explicit local Close now
    // stops it and persists Closed while still holding the transferred guard.
    sandbox.stop_checked().await.unwrap();
    session_records.lock().await.close(&session.id).unwrap();
    registry.complete_session_cleanup(&ticket).unwrap();
    drop(ticket);
    assert!(operation.try_lock().is_err());
    drop(transferred);
    assert!(operation.try_lock().is_ok());
    drop(observer);
    drop(durable_before_cleanup);

    let canonical = SessionExecutionStore::open(format, store_owner).unwrap();
    let snapshot = canonical.snapshot(&run.turn_id).unwrap();
    let content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let arguments = content
        .condition_arguments(&snapshot, &run.run_id)
        .unwrap()
        .unwrap();
    let result = content.condition_result(&arguments).unwrap().unwrap();
    assert_eq!(arguments.run(), &run);
    assert_eq!(arguments.repository_ref(), &repository);
    assert_eq!(arguments.inputs().len(), 1);
    assert_eq!(arguments.inputs()[0].input, accepted_input);
    assert_eq!(arguments.inputs()[0].checkpoint, accepted_checkpoint);
    assert_eq!(arguments.inputs()[0].output, accepted_output);
    assert_eq!(result.status(), &ConditionProcessStatus::Interrupted);
    assert_eq!(result.stdout().retained_bytes().unwrap(), b"\0\xffQA");
    assert_eq!(result.stderr().retained_bytes().unwrap(), b"diagnostic\n");
    assert!(result.stdout().complete() && result.stderr().complete());
    let supervision = result
        .supervision()
        .expect("actual supervisor identity must be retained");
    assert!(supervision.quiescent && supervision.launched);
    assert!(supervision.primary_exit.is_some());
    assert_eq!(supervision.runtime_identity, execution_identity);
    assert_eq!(supervision.invocation_id, run.run_id.as_str());
    assert_eq!(
        supervision.request_sha256,
        axocoatl_exec::protocol::ExecRequest {
            protocol: axocoatl_exec::protocol::PROTOCOL_VERSION,
            stdin: None,
            invocation_id: run.run_id.as_str().into(),
            argv: check_definition.argv,
            timeout_ms: check_definition.timeout_ms,
            stdout_bytes: check_definition.stdout_bytes,
            stderr_bytes: check_definition.stderr_bytes,
            write_restriction: None,
        }
        .digest()
        .unwrap()
    );
    assert!(uuid::Uuid::parse_str(&supervision.transport_identity).is_ok());
    let installed = supervisor_root
        .path()
        .join(format!("supervisor-sha256-{}", supervision.program_sha256));
    assert_eq!(
        format!("{:x}", Sha256::digest(std::fs::read(installed).unwrap())),
        supervision.program_sha256
    );
    let condition = snapshot.contract().condition_run(&run.run_id).unwrap();
    assert_eq!(
        condition.resolution,
        Some(ConditionEffectResolution::OutcomeRecorded {
            evidence: result.reference().clone()
        })
    );
    assert!(snapshot
        .contract()
        .conditions()
        .iter()
        .any(|condition| condition.condition_id == run.condition_id
            && condition.evidence == *result.reference()
            && condition.outcome == ConditionOutcome::Failed));
    assert_eq!(
        std::fs::read(workspace.canonical_path.join("check-executions")).unwrap(),
        b"x",
        "dropping and reopening observers must not replay the command"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and a configured running Podman connection"]
async fn actual_controller_check_survives_waiter_drop_and_releases_only_through_owned_lifecycle() {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let data_path = data.path().canonicalize().unwrap();
    let workspace_path = workspace.path().canonicalize().unwrap();
    let cleanup = Arc::new(RuntimeCleanup::default());
    let task_cleanup = cleanup.clone();
    let result =
        tokio::spawn(
            async move { prove_owned_check(data_path, workspace_path, task_cleanup).await },
        )
        .await;
    // Keep temporary roots alive until real runtime cleanup, including after
    // assertion panics, so no live container outlives its repository fixture.
    let failures = cleanup.remove_owned().await;
    assert!(
        failures.is_empty(),
        "owned runtime cleanup failed: {failures:?}; test: {result:?}"
    );
    if let Err(error) = result {
        if error.is_panic() {
            std::panic::resume_unwind(error.into_panic());
        }
        panic!("owned controller test was cancelled: {error}");
    }
}
