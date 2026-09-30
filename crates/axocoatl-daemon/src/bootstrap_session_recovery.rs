//! Restart recovery from canonical schema-2 stores. No provider, generation,
//! repository owner, or executor is recreated by reading durable history.
use super::*;
use crate::session_dispatch::{RetainedSessionStores, SessionDispatchController};
use axocoatl_memory::activation_state::ActivationStateStore;
use axocoatl_session::execution_content::ExecutionContentStore;
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::execution_ownership::UpgradedFormatOwnership;
use axocoatl_session::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
use axocoatl_session::turn_contract::{SessionId, TurnContractEvent};

/// Minted only by an exclusive mkdir of the final data-root component in this
/// process. Existing empty directories do not acquire first-install authority.
pub(super) struct CreatedDataRoot {
    directory: SecureDir,
}

pub(super) fn open_data_root(
    path: &Path,
) -> Result<(SecureDir, Option<CreatedDataRoot>), DaemonError> {
    let Some(name) = path.file_name() else {
        return SecureDir::open_existing_all(path)
            .map(|directory| (directory, None))
            .map_err(recovery_error);
    };
    let parent = SecureDir::open_or_create_all(
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )
    .map_err(recovery_error)?;
    match parent.create_child(name) {
        Ok(directory) => {
            directory.sync_all().map_err(recovery_error)?;
            parent.sync_all().map_err(recovery_error)?;
            Ok((directory.clone(), Some(CreatedDataRoot { directory })))
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => parent
            .existing_child(Path::new(name))
            .map(|directory| (directory, None))
            .map_err(recovery_error),
        Err(error) => Err(recovery_error(error)),
    }
}

impl CreatedDataRoot {
    pub(super) fn upgrade(self, lease: DataDirLease) -> Result<DataDirLease, DaemonError> {
        self.directory
            .verify_ambient_identity()
            .map_err(recovery_error)?;
        lease
            .ownership
            .verify_root(&self.directory)
            .map_err(recovery_error)?;
        let entries = self.directory.entries_limited(2).map_err(recovery_error)?;
        if entries.len() != 1
            || entries[0].name != axocoatl_session::execution_ownership::LEGACY_LOCK_NAME
            || entries[0].file_type != SecureEntryType::File
        {
            return Err(DaemonError::Session(
                "new data root changed before its format boundary was installed".into(),
            ));
        }
        Ok(DataDirLease {
            ownership: axocoatl_session::execution_ownership::DataRootFormatOwnership::Upgraded(
                lease.ownership.into_upgraded().map_err(recovery_error)?,
            ),
        })
    }
}

pub(super) fn recover_sessions(
    ownership: Arc<UpgradedFormatOwnership>,
    sessions: &[Session],
    registry: &session_dispatch::SessionDispatchRegistry,
) -> Result<usize, DaemonError> {
    let mut recovered = 0;
    for session in sessions {
        let result = (|| {
            let stores = recover_session_stores(ownership.clone(), session)?;
            registry.retain_existing_session(&mut Some(stores))?;
            if session.status == axocoatl_session::SessionStatus::Closed {
                registry.fence_closed_history(&session.id)?;
            }
            Ok::<_, DaemonError>(())
        })();
        result.map_err(|error| DaemonError::Session(format!(
            "could not recover canonical history for Session '{}': {error}; existing data was not replaced and no work was replayed", session.id
        )))?;
        recovered += 1;
    }
    Ok(recovered)
}

pub(super) fn recover_session_stores(
    ownership: Arc<UpgradedFormatOwnership>,
    session: &Session,
) -> Result<RetainedSessionStores, DaemonError> {
    let canonical = SessionExecutionStore::open_existing(
        ownership.clone(),
        ExecutionStoreOwner {
            workspace_id: session.workspace_id.clone(),
            session_id: SessionId::new(&session.id).map_err(recovery_error)?,
        },
    )
    .map_err(recovery_error)?;
    let content = ExecutionContentStore::open_owned(
        canonical
            .existing_component_namespace(
                ExecutionComponent::ExecutionContent,
                Path::new("execution-content.v1.json"),
            )
            .map_err(recovery_error)?,
    )
    .map_err(recovery_error)?;
    let memory = ActivationStateStore::open_owned(
        canonical
            .existing_component_namespace(
                ExecutionComponent::ActivationState,
                Path::new("activation-state.json"),
            )
            .map_err(recovery_error)?,
    )
    .map_err(recovery_error)?;
    let latest = canonical
        .records()
        .map_err(recovery_error)?
        .iter()
        .rev()
        .find(|record| matches!(record.event, TurnContractEvent::Begin { .. }))
        .map(|record| record.turn_id.clone());
    let stores = RetainedSessionStores {
        canonical,
        content,
        memory,
    };
    let stores = match latest {
        Some(turn) => {
            SessionDispatchController::recover_retained(stores, turn).map_err(recovery_error)?
        }
        None => stores,
    };
    Ok(stores)
}

fn recovery_error(error: impl std::fmt::Display) -> DaemonError {
    DaemonError::Session(error.to_string())
}

impl AxocoatlDaemon {
    /// Provision the current user's data root. Only an exclusive first creation
    /// installs the native execution format; an existing root is never inferred
    /// empty or converted merely because its legacy files are absent.
    pub fn initialize_data_root(path: &Path) -> Result<SecureDir, DaemonError> {
        let (directory, created) = open_data_root(path)?;
        admit_and_restrict_data_root(&directory)?;
        if let Some(created) = created {
            let lease = DataDirLease::acquire(&directory)?;
            drop(created.upgrade(lease)?);
        }
        Ok(directory)
    }

    /// An explicit Send/control may attach the latest recovered turn only after
    /// the real Ready Session/Workspace/runtime owner is reacquired. Zero-turn
    /// pending entries stay pending for their normal first Begin path.
    pub(crate) async fn ensure_registered_native_session(
        &self,
        session_id: &str,
    ) -> Result<bool, DaemonError> {
        self.require_runtime_admission()?;
        let Some((token, turn)) = self
            .session_dispatch_lifecycles
            .pending_existing_turn(session_id)?
        else {
            return Ok(false);
        };
        let owner = self.pending_session_repository_owner(&token).await?;
        self.require_runtime_admission()?;
        self.validate_repository_daemon_binding(&owner)?;
        let (controller, _) = self
            .session_dispatch_lifecycles
            .attach_existing_turn(&token, turn, owner)?;
        controller
            .attach_stream_bus(self.stream_bus.clone())
            .map_err(recovery_error)?;
        Ok(true)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use axocoatl_session::execution_content::{ActivationEvidenceContent, ExecutionRequestContent};
    use axocoatl_session::execution_ownership::LegacyFormatOwnership;
    use axocoatl_session::turn_contract::*;

    fn native_session(
        root: &tempfile::TempDir,
        workspace: &tempfile::TempDir,
    ) -> (Arc<UpgradedFormatOwnership>, Session, RetainedSessionStores) {
        let data = SecureDir::open(root.path()).unwrap();
        let mut sessions = SessionStore::new_in_secure(&data, "sessions").unwrap();
        let ownership = Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        );
        let (session, receipt) = sessions
            .create_native_with_environment(
                &ownership,
                "Recovery",
                "workspace",
                workspace.path(),
                SessionMode::SingleAgent {
                    agent_id: "agent".into(),
                },
                vec![],
                vec![],
                None,
                None,
                false,
                true,
            )
            .unwrap();
        let mut canonical =
            SessionExecutionStore::open(ownership.clone(), receipt.owner().clone()).unwrap();
        canonical.record_native_origin(&receipt).unwrap();
        let content = ExecutionContentStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap();
        let memory = ActivationStateStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ActivationState)
                .unwrap(),
        )
        .unwrap();
        (
            ownership,
            session,
            RetainedSessionStores {
                canonical,
                content,
                memory,
            },
        )
    }

    #[test]
    fn only_exclusive_data_root_creation_can_install_empty_native_format() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("created");
        let (directory, receipt) = open_data_root(&path).unwrap();
        let lease = receipt
            .unwrap()
            .upgrade(DataDirLease::acquire(&directory).unwrap())
            .unwrap();
        assert!(matches!(
            lease.ownership,
            axocoatl_session::execution_ownership::DataRootFormatOwnership::Upgraded(_)
        ));
        assert!(directory
            .existing_child(axocoatl_session::execution_ownership::LEGACY_LOCK_NAME)
            .is_ok());
        drop(lease);
        let (_, repeat) = open_data_root(&path).unwrap();
        assert!(repeat.is_none());
        let existing = parent.path().join("existing-empty");
        std::fs::create_dir(&existing).unwrap();
        assert!(open_data_root(&existing).unwrap().1.is_none());
    }

    #[test]
    fn new_root_content_before_lease_prevents_empty_root_conversion() {
        let parent = tempfile::tempdir().unwrap();
        let (directory, receipt) = open_data_root(&parent.path().join("created")).unwrap();
        directory.atomic_write("prior-state", b"preserve").unwrap();
        assert!(receipt
            .unwrap()
            .upgrade(DataDirLease::acquire(&directory).unwrap())
            .is_err());
        assert_eq!(directory.read("prior-state").unwrap(), b"preserve");
    }

    #[test]
    fn startup_retains_empty_native_history_without_creating_legacy_history_or_owner() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (ownership, session, stores) = native_session(&root, &workspace);
        let identity = stores.canonical.identity().unwrap();
        drop(stores);
        let registry = session_dispatch::SessionDispatchRegistry::default();
        assert_eq!(
            recover_sessions(ownership, std::slice::from_ref(&session), &registry).unwrap(),
            1
        );
        assert!(registry
            .pending_existing_turn(&session.id)
            .unwrap()
            .is_none());
        let token = registry.prepare_first_turn(&session.id).unwrap();
        assert_eq!(
            registry
                .pending_identity(&token, &SecureDir::open(root.path()).unwrap())
                .unwrap(),
            identity
        );
        assert!(!root.path().join("session-history").exists());
        registry.close_all_admission().unwrap();
    }

    #[test]
    fn startup_interrupts_the_prior_epoch_and_preserves_the_exact_request_without_dispatch() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (ownership, session, mut stores) = native_session(&root, &workspace);
        let turn_id = LogicalTurnId::new("recovered-turn").unwrap();
        let definition_id = AgentDefinitionId::new("definition").unwrap();
        let definition = stores
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Definition {
                definition_id: definition_id.clone(),
                revision: 1,
                profile: axocoatl_session::control_authority::ExecutionProfile {
                    definition: "definition".into(),
                    provider: "local".into(),
                    model: "model".into(),
                    isolation: "in-process".into(),
                    tools: vec![],
                    write_scope: None,
                },
                configuration: "{}".into(),
            })
            .unwrap();
        let request = stores
            .content
            .retain_request(ExecutionRequestContent {
                turn_id: turn_id.clone(),
                recorded_at_unix_ms: 1,
                display_input: "Keep this exact request".into(),
                effective_input: "Keep this exact request".into(),
                context: vec![],
                target_definition: None,
                model: None,
            })
            .unwrap();
        stores
            .canonical
            .begin_with_request(
                TurnContractEnvelope {
                    schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                    command_id: CommandId::new("begin").unwrap(),
                    expected_revision: 0,
                    session_id: SessionId::new(&session.id).unwrap(),
                    turn_id: turn_id.clone(),
                    event: TurnContractEvent::Begin {
                        epoch_id: ExecutionEpochId::new("previous-process").unwrap(),
                        predecessor: None,
                        graph: TurnGraphSnapshot {
                            snapshot_id: GraphSnapshotId::new("graph").unwrap(),
                            revision: 1,
                            nodes: vec![GraphNode {
                                node_id: TurnNodeId::new("node").unwrap(),
                                slot_id: SessionTeamSlotId::new("slot").unwrap(),
                                definition: DefinitionSnapshotRef {
                                    definition_id,
                                    snapshot: definition.reference().clone(),
                                },
                                conversation_id: NodeConversationId::new("conversation").unwrap(),
                                starting_savepoint: ConversationSavepoint::Empty,
                                required: true,
                            }],
                            dependencies: vec![],
                            conditions: vec![],
                        },
                    },
                },
                &request,
            )
            .unwrap();
        let controller = SessionDispatchController::open_retained(stores, turn_id.clone())
            .map_err(|failed| failed.error)
            .unwrap();
        drop(controller);
        let registry = session_dispatch::SessionDispatchRegistry::default();
        recover_sessions(ownership, std::slice::from_ref(&session), &registry).unwrap();
        let (token, latest) = registry
            .pending_existing_turn(&session.id)
            .unwrap()
            .unwrap();
        assert_eq!(latest, turn_id);
        registry
            .prepare_first_turn_content(&token, |canonical, content, _, _| {
                let snapshot = canonical.snapshot(&turn_id).unwrap();
                assert_eq!(
                    snapshot.contract().state(),
                    Some(LogicalTurnState::NeedsAttention)
                );
                assert_eq!(
                    snapshot.contract().epochs()[0].state,
                    EpochState::Interrupted
                );
                assert!(snapshot.contract().activations().is_empty());
                let projection = content.project(&snapshot).unwrap();
                assert!(matches!(
                    projection.request,
                    axocoatl_session::execution_content::ContentResolution::Available { .. }
                ));
                assert_eq!(canonical.records().unwrap().len(), 2);
                Ok(())
            })
            .unwrap();
        registry.close_all_admission().unwrap();
    }

    #[test]
    fn recovered_pre_invocation_failure_can_finish_without_ready_runtime_and_unblock_rebuild() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (ownership, mut session, mut stores) = native_session(&root, &workspace);
        let turn_id = LogicalTurnId::new("recovered-turn").unwrap();
        let definition_id = AgentDefinitionId::new("definition").unwrap();
        let definition = stores
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Definition {
                definition_id: definition_id.clone(),
                revision: 1,
                profile: axocoatl_session::control_authority::ExecutionProfile {
                    definition: "definition".into(),
                    provider: "local".into(),
                    model: "model".into(),
                    isolation: "in-process".into(),
                    tools: vec![],
                    write_scope: None,
                },
                configuration: "{}".into(),
            })
            .unwrap();
        let request = stores
            .content
            .retain_request(ExecutionRequestContent {
                turn_id: turn_id.clone(),
                recorded_at_unix_ms: 1,
                display_input: "Keep this exact request".into(),
                effective_input: "Keep this exact request".into(),
                context: vec![],
                target_definition: None,
                model: None,
            })
            .unwrap();
        stores
            .canonical
            .begin_with_request(
                TurnContractEnvelope {
                    schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                    command_id: CommandId::new("begin").unwrap(),
                    expected_revision: 0,
                    session_id: SessionId::new(&session.id).unwrap(),
                    turn_id: turn_id.clone(),
                    event: TurnContractEvent::Begin {
                        epoch_id: ExecutionEpochId::new("previous-process").unwrap(),
                        predecessor: None,
                        graph: TurnGraphSnapshot {
                            snapshot_id: GraphSnapshotId::new("graph").unwrap(),
                            revision: 1,
                            nodes: vec![GraphNode {
                                node_id: TurnNodeId::new("node").unwrap(),
                                slot_id: SessionTeamSlotId::new("slot").unwrap(),
                                definition: DefinitionSnapshotRef {
                                    definition_id,
                                    snapshot: definition.reference().clone(),
                                },
                                conversation_id: NodeConversationId::new("conversation").unwrap(),
                                starting_savepoint: ConversationSavepoint::Empty,
                                required: true,
                            }],
                            dependencies: vec![],
                            conditions: vec![],
                        },
                    },
                },
                &request,
            )
            .unwrap();
        let epoch = ExecutionEpochId::new("previous-process").unwrap();
        let activation = ActivationRef {
            session_id: SessionId::new(&session.id).unwrap(),
            turn_id: turn_id.clone(),
            execution_epoch_id: epoch.clone(),
            node_id: TurnNodeId::new("node").unwrap(),
            generation: 1,
            activation_id: ActivationId::new("failed-before-provider").unwrap(),
        };
        let budget = stores
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Budget {
                limits: axocoatl_session::control_authority::GrantLimits {
                    activations: 1,
                    invocations: 2,
                    tokens: 8192,
                    cost_microunits: 0,
                },
            })
            .unwrap()
            .reference()
            .clone();
        let input = ActivationInputManifest {
            manifest_id: InputManifestId::new("pre-invocation-input").unwrap(),
            activation: activation.clone(),
            definition: DefinitionSnapshotRef {
                definition_id: AgentDefinitionId::new("definition").unwrap(),
                snapshot: definition.reference().clone(),
            },
            conversation_id: NodeConversationId::new("conversation").unwrap(),
            starting_savepoint: ConversationSavepoint::Empty,
            parents: vec![],
            guidance: vec![],
            attachments: vec![],
            repository: RepositoryInput::Unavailable,
            grant: None,
            budget,
            revision_context: None,
        };
        stores
            .canonical
            .append(TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new("start").unwrap(),
                expected_revision: 1,
                session_id: activation.session_id.clone(),
                turn_id: turn_id.clone(),
                event: TurnContractEvent::StartActivation {
                    input: Box::new(input),
                },
            })
            .unwrap();
        let reason = stores
            .content
            .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                text: "Resource preparation failed before any provider invocation".into(),
            })
            .unwrap()
            .reference()
            .clone();
        stores
            .canonical
            .append(TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new("failed").unwrap(),
                expected_revision: 2,
                session_id: activation.session_id.clone(),
                turn_id: turn_id.clone(),
                event: TurnContractEvent::FailActivation {
                    activation: activation.clone(),
                    evidence: reason,
                },
            })
            .unwrap();
        stores
            .canonical
            .append(TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new("paused").unwrap(),
                expected_revision: 3,
                session_id: activation.session_id.clone(),
                turn_id: turn_id.clone(),
                event: TurnContractEvent::PauseEpoch {
                    epoch_id: epoch.clone(),
                },
            })
            .unwrap();
        let controller = SessionDispatchController::open_retained(stores, turn_id.clone())
            .map_err(|failed| failed.error)
            .unwrap();
        drop(controller);
        // A restart can leave no usable repository owner. No sandbox/provider
        // fixture is installed: successful closure must be metadata-only.
        session.environment.state = SessionEnvironmentState::Failed;
        session.environment.error =
            Some("environment preparation was cancelled; rebuild the environment".into());
        let registry = session_dispatch::SessionDispatchRegistry::default();
        recover_sessions(ownership.clone(), std::slice::from_ref(&session), &registry).unwrap();
        assert!(registry
            .require_native_environment_change_ready(&session.id)
            .is_err());
        // A paused turn with nothing running, no stop and no unknown effect,
        // and no runtime owner, may have its unchanged plan prepared again at
        // the same generation; any other runtime change still requires closure.
        registry
            .require_native_environment_retry_ready(&session.id)
            .unwrap();
        assert!(registry
            .require_native_environment_retry_ready("ses-not-retained")
            .is_err());
        let request = crate::session_dispatch::HumanControlActionRequest {
            schema_version: 1,
            command_id: CommandId::new("explicit-empty-finish").unwrap(),
            session_id: activation.session_id.clone(),
            turn_id: turn_id.clone(),
            execution_epoch_id: epoch,
            expected_turn_revision: 4,
            expected_graph_revision: 1,
            activation: None,
            action: crate::session_dispatch::HumanControlAction::Finish,
            instruction: None,
            include_previous_output: false,
            context: None,
            continuation: None,
            blocker_id: None,
            human_response: None,
            partial_finish: Some(crate::session_dispatch::HumanPartialFinishSelection {
                selected_activations: vec![],
                stop_activations: vec![],
                missing_conditions: vec![],
                unrun_nodes: vec![],
                confirmed: true,
            }),
        };
        let mut stale = request.clone();
        stale.command_id = CommandId::new("stale-empty-finish").unwrap();
        stale.expected_turn_revision = 3;
        assert_eq!(
            registry
                .finish_pending_without_result(&stale, 9)
                .unwrap()
                .unwrap()
                .state,
            axocoatl_session::control_command::ControlCommandState::Rejected
        );
        // Refused commands restore the same retained stores and cannot silently
        // select a failed result or waive the explicit partial-finish review.
        let mut selected = request.clone();
        selected
            .partial_finish
            .as_mut()
            .unwrap()
            .selected_activations
            .push(activation.clone());
        assert!(registry
            .finish_pending_without_result(&selected, 9)
            .unwrap()
            .is_none());
        let mut unconfirmed = request.clone();
        unconfirmed.partial_finish.as_mut().unwrap().confirmed = false;
        assert!(registry
            .finish_pending_without_result(&unconfirmed, 9)
            .unwrap()
            .is_none());
        assert!(registry
            .require_native_environment_change_ready(&session.id)
            .is_err());
        let receipt = registry
            .finish_pending_without_result(&request, 10)
            .unwrap()
            .unwrap();
        assert_eq!(
            receipt.state,
            axocoatl_session::control_command::ControlCommandState::Settled
        );
        assert!(registry
            .require_native_environment_change_ready(&session.id)
            .is_ok());
        registry
            .require_native_environment_retry_ready(&session.id)
            .unwrap();
        let token = registry.session_team_token(&session.id).unwrap();
        assert_eq!(
            registry
                .repeated_session_human_action(&token, &request)
                .unwrap()
                .unwrap(),
            receipt
        );
        registry
            .with_session_team_stores(&token, |canonical, _, memory| {
                let snapshot = canonical.snapshot(&turn_id).unwrap();
                assert_eq!(
                    snapshot.contract().state(),
                    Some(LogicalTurnState::Finished)
                );
                assert!(snapshot.contract().invocations().is_empty());
                assert!(snapshot.contract().condition_runs().is_empty());
                assert_eq!(snapshot.contract().activations().len(), 1);
                assert_eq!(snapshot.contract().activations()[0].activation, activation);
                assert!(memory.promotion(&snapshot).unwrap().is_some());
                assert!(canonical.unfinished_turn().unwrap().is_none());
                Ok(())
            })
            .unwrap();
        drop(token);
        registry.close_all_admission().unwrap();
        drop(registry);
        let reopened = recover_session_stores(ownership, &session).unwrap();
        assert!(reopened.canonical.unfinished_turn().unwrap().is_none());
        assert_eq!(
            reopened
                .canonical
                .snapshot(&turn_id)
                .unwrap()
                .contract()
                .state(),
            Some(LogicalTurnState::Finished)
        );
    }

    #[test]
    fn startup_refuses_missing_previously_initialized_content_without_replacement() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (ownership, session, stores) = native_session(&root, &workspace);
        let content_path = stores
            .canonical
            .path()
            .parent()
            .unwrap()
            .join("execution-content/execution-content.v1.json");
        drop(stores);
        std::fs::remove_file(&content_path).unwrap();
        let registry = session_dispatch::SessionDispatchRegistry::default();
        assert!(recover_sessions(ownership, &[session], &registry).is_err());
        assert!(!content_path.exists());
    }
    #[tokio::test]
    async fn successful_close_retains_owned_history_and_reopen_does_not_create_execution() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (ownership, session, stores) = native_session(&root, &workspace);
        let identity = stores.canonical.identity().unwrap();
        let registry = session_dispatch::SessionDispatchRegistry::default();
        let token = registry.retain_existing_session(&mut Some(stores)).unwrap();
        registry
            .prepare_first_turn_content(&token, |_, content, _, _| {
                content
                    .retain_activation_evidence(ActivationEvidenceContent::Guidance {
                        text: "Retained before Close".into(),
                    })
                    .unwrap();
                Ok(())
            })
            .unwrap();
        let cleanup = registry
            .prepare_session_cleanup(&session.id, Duration::from_secs(1))
            .await
            .unwrap();
        registry.complete_session_cleanup(&cleanup).unwrap();
        drop(cleanup);
        let reopened = recover_session_stores(ownership, &session).unwrap();
        assert_eq!(reopened.canonical.identity().unwrap(), identity);
        registry
            .retain_existing_lifecycle_session(&mut Some(reopened), true)
            .unwrap();
        assert!(registry
            .history_snapshot(&session.id)
            .unwrap()
            .unwrap()
            .entries(HistoryVisibility::IncludingSuperseded)
            .is_empty());
        assert!(registry.session_team_token(&session.id).is_err());
        assert!(registry.prepare_first_turn(&session.id).is_err());
        assert!(registry.live_native_turns().unwrap().is_empty());
        registry.require_session_reopenable(&session.id).unwrap();
        registry.reopen_session(&session.id).unwrap();
        let current = registry.prepare_first_turn(&session.id).unwrap();
        assert_eq!(
            registry
                .pending_identity(&current, &SecureDir::open(root.path()).unwrap())
                .unwrap(),
            identity
        );
        assert!(registry
            .pending_identity(&token, &SecureDir::open(root.path()).unwrap())
            .is_err());
        assert!(registry.live_native_turns().unwrap().is_empty());
    }

    #[test]
    fn startup_closed_session_history_stays_fenced_until_explicit_reopen() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (ownership, mut session, stores) = native_session(&root, &workspace);
        drop(stores);
        let data = SecureDir::open(root.path()).unwrap();
        let mut sessions = SessionStore::new_in_secure(&data, "sessions").unwrap();
        sessions.load_all().unwrap();
        sessions.close(&session.id).unwrap();
        session = sessions.get(&session.id).unwrap();
        let registry = session_dispatch::SessionDispatchRegistry::default();
        recover_sessions(ownership, std::slice::from_ref(&session), &registry).unwrap();
        assert!(registry.history_snapshot(&session.id).unwrap().is_some());
        assert!(registry.pending_existing_turn(&session.id).is_err());
        assert!(registry.session_team_token(&session.id).is_err());
        registry.require_session_reopenable(&session.id).unwrap();
        registry.reopen_session(&session.id).unwrap();
        assert!(registry
            .pending_existing_turn(&session.id)
            .unwrap()
            .is_none());
        assert!(registry.live_native_turns().unwrap().is_empty());
    }
}
