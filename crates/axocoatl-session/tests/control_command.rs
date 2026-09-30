use axocoatl_session::control_authority::*;
use axocoatl_session::control_command::*;
use axocoatl_session::turn_contract::*;
use serde::Deserialize;
use tempfile::TempDir;

#[derive(Deserialize)]
struct Scenario {
    request: ControlCommandRequest,
    updates: Vec<ControlReceiptUpdate>,
}

fn scenario() -> Scenario {
    serde_json::from_str(include_str!("fixtures/control-command-stop.v1.json")).unwrap()
}

fn evidence(id: &str) -> EvidenceRef {
    EvidenceRef::new(id).unwrap()
}

fn owner() -> ControlCommandOwner {
    ControlCommandOwner {
        workspace_id: "workspace-client-a".into(),
        session_id: SessionId::new("session-qa").unwrap(),
        turn_id: LogicalTurnId::new("turn-build-184").unwrap(),
    }
}

fn human() -> TrustedCommandSource {
    TrustedCommandSource::human(
        owner().session_id,
        owner().turn_id,
        evidence("human-stop-action"),
    )
}

fn store(root: &TempDir) -> ControlCommandStore {
    ControlCommandStore::open(root.path(), owner()).unwrap()
}

fn activation() -> ActivationRef {
    let ControlParameters::StopActivation { activation } = scenario().request.parameters else {
        unreachable!()
    };
    activation
}

fn manifest(node: &str, generation: u32, epoch: &str) -> ActivationInputManifest {
    let mut target = activation();
    target.node_id = TurnNodeId::new(node).unwrap();
    target.generation = generation;
    target.execution_epoch_id = ExecutionEpochId::new(epoch).unwrap();
    target.activation_id = ActivationId::new(format!("{node}-{generation}-{epoch}")).unwrap();
    ActivationInputManifest {
        manifest_id: InputManifestId::new(format!("input-{node}-{generation}-{epoch}")).unwrap(),
        activation: target,
        definition: DefinitionSnapshotRef {
            definition_id: AgentDefinitionId::new("qa-runner").unwrap(),
            snapshot: evidence("definition-v1"),
        },
        conversation_id: NodeConversationId::new(format!("conversation-{node}")).unwrap(),
        starting_savepoint: ConversationSavepoint::Empty,
        parents: vec![],
        guidance: vec![],
        attachments: vec![],
        repository: RepositoryInput::Recorded {
            snapshot: evidence("repository-184"),
        },
        budget: evidence("budget-184"),
        grant: None,
        revision_context: None,
    }
}

fn failure() -> CommandFailure {
    CommandFailure {
        code: "effect_unknown".into(),
        message: "The invocation still requires authoritative reconciliation.".into(),
        evidence: Some(evidence("invocation-audit-unknown")),
        blocker: Some(evidence("blocker-invocation-1")),
    }
}

#[test]
fn serialized_lifecycle_reopens_at_every_boundary_without_inventing_settlement() {
    let fixture = scenario();
    let states = [
        ControlCommandState::Requested,
        ControlCommandState::Accepted,
        ControlCommandState::Applied,
        ControlCommandState::Settled,
    ];
    assert_eq!(states.len(), fixture.updates.len() + 1);
    for (cut, expected_state) in states.iter().enumerate() {
        let root = TempDir::new().unwrap();
        let mut journal = store(&root);
        let first = journal
            .record_requested(fixture.request.clone(), human())
            .unwrap();
        let journal_id = first.journal_id().to_owned();
        for update in &fixture.updates[..cut] {
            journal.advance(update.clone()).unwrap();
        }
        let before = journal
            .receipt(&fixture.request.command_id)
            .unwrap()
            .unwrap();
        drop(journal);
        let mut journal = store(&root);
        let restored = journal.lookup_request(&fixture.request).unwrap().unwrap();
        assert_eq!(restored.journal_id(), journal_id);
        assert_eq!(restored.view(), before.view());
        assert_eq!(&restored.view().state, expected_state);
        assert_eq!(restored.view().revision, cut as u64 + 1);
        let replay = journal
            .record_requested(fixture.request.clone(), human())
            .unwrap();
        assert_eq!(replay.view(), restored.view());
        for update in &fixture.updates[..cut] {
            assert_eq!(
                journal.advance(update.clone()).unwrap().view(),
                restored.view()
            );
        }
        assert_eq!(journal.records().unwrap().len(), cut + 1);
        for update in &fixture.updates[cut..] {
            journal.advance(update.clone()).unwrap();
        }
        assert_eq!(journal.records().unwrap().len(), 4);
        assert_eq!(
            journal
                .receipt(&fixture.request.command_id)
                .unwrap()
                .unwrap()
                .view()
                .state,
            ControlCommandState::Settled
        );
    }
}

#[test]
fn exact_replay_precedes_stale_revision_but_changed_payloads_never_alias() {
    let fixture = scenario();
    let root = TempDir::new().unwrap();
    let mut journal = store(&root);
    journal
        .record_requested(fixture.request.clone(), human())
        .unwrap();
    journal.advance(fixture.updates[0].clone()).unwrap();
    journal.advance(fixture.updates[1].clone()).unwrap();
    let bytes = std::fs::read(journal.path()).unwrap();
    assert_eq!(
        journal
            .advance(fixture.updates[0].clone())
            .unwrap()
            .view()
            .state,
        ControlCommandState::Applied
    );
    let mut stale = fixture.updates[0].clone();
    stale.update_id = CommandId::new("different-acceptance").unwrap();
    assert!(matches!(
        journal.advance(stale),
        Err(ControlCommandError::StaleRevision { .. })
    ));
    let mut changed = fixture.request.clone();
    changed.expected_turn_revision += 1;
    assert!(matches!(
        journal.lookup_request(&changed),
        Err(ControlCommandError::CommandConflict)
    ));
    assert!(matches!(
        journal.record_requested(changed, human()),
        Err(ControlCommandError::CommandConflict)
    ));
    let mut conflict = fixture.updates[0].clone();
    conflict.transition = ControlTransition::Rejected { failure: failure() };
    assert!(matches!(
        journal.advance(conflict),
        Err(ControlCommandError::CommandConflict)
    ));
    let mut collision = fixture.request.clone();
    collision.command_id = fixture.updates[0].update_id.clone();
    assert!(matches!(
        journal.lookup_request(&collision),
        Err(ControlCommandError::CommandConflict)
    ));
    assert_eq!(std::fs::read(journal.path()).unwrap(), bytes);
}

#[test]
fn terminal_results_are_immutable_and_every_legal_failure_path_is_retained() {
    let fixture = scenario();
    for accepted_updates in 0..=2 {
        let root = TempDir::new().unwrap();
        let mut journal = store(&root);
        journal
            .record_requested(fixture.request.clone(), human())
            .unwrap();
        // Direct settlement is illegal even if the caller claims success.
        let mut jump = fixture.updates[2].clone();
        jump.expected_receipt_revision = 1;
        assert!(matches!(
            journal.advance(jump),
            Err(ControlCommandError::InvalidTransition)
        ));
        for update in &fixture.updates[..accepted_updates] {
            journal.advance(update.clone()).unwrap();
        }
        let mut terminal = fixture.updates[2].clone();
        terminal.expected_receipt_revision = accepted_updates as u64 + 1;
        terminal.transition = if accepted_updates == 0 {
            ControlTransition::Rejected { failure: failure() }
        } else {
            ControlTransition::Failed { failure: failure() }
        };
        let result = journal.advance(terminal.clone()).unwrap();
        assert!(result.view().state.is_terminal());
        let bytes = std::fs::read(journal.path()).unwrap();
        assert_eq!(
            journal.advance(terminal.clone()).unwrap().view(),
            result.view()
        );
        terminal.update_id = CommandId::new("alternate-terminal").unwrap();
        terminal.expected_receipt_revision += 1;
        terminal.transition = ControlTransition::Settled {
            result: evidence("invented-success"),
        };
        assert!(matches!(
            journal.advance(terminal),
            Err(ControlCommandError::InvalidTransition)
        ));
        assert_eq!(std::fs::read(journal.path()).unwrap(), bytes);
        drop(journal);
        assert_eq!(
            store(&root)
                .receipt(&fixture.request.command_id)
                .unwrap()
                .unwrap()
                .view(),
            result.view()
        );
    }
}

#[test]
fn public_json_cannot_claim_human_origin_or_add_ambiguous_parameter_fields() {
    let fixture = scenario();
    let bytes = serde_json::to_vec(&fixture.request).unwrap();
    assert_eq!(
        ControlCommandRequest::decode(&bytes).unwrap(),
        fixture.request
    );
    let mut json = serde_json::to_value(&fixture.request).unwrap();
    json["source"] = serde_json::json!({"kind":"human"});
    assert!(ControlCommandRequest::decode(&serde_json::to_vec(&json).unwrap()).is_err());
    json.as_object_mut().unwrap().remove("source");
    json["parameters"]["include_unrelated_work"] = true.into();
    assert!(ControlCommandRequest::decode(&serde_json::to_vec(&json).unwrap()).is_err());
    assert!(matches!(
        ControlCommandRequest::decode(br#"{"schema_version":99,"future_payload":{}}"#),
        Err(ControlCommandError::UnsupportedVersion(99))
    ));
    let mut too_large = bytes;
    too_large.extend(std::iter::repeat_n(b' ', MAX_CONTROL_REQUEST_BYTES));
    assert!(matches!(
        ControlCommandRequest::decode(&too_large),
        Err(ControlCommandError::Capacity)
    ));
}

#[test]
fn every_parameter_variant_roundtrips_with_exact_targets_and_rejects_cross_epoch_inputs() {
    let variants = vec![
        ControlParameters::StopActivation {
            activation: activation(),
        },
        ControlParameters::RetryActivation {
            activation: activation(),
            input: Box::new(manifest("tester", 2, "epoch-1")),
            replay_decisions: vec![],
        },
        ControlParameters::SteerActivation {
            activation: activation(),
            instruction: evidence("steer-next"),
            mode: SteerMode::NextSafeBoundary,
        },
        ControlParameters::ReviseActivation {
            activation: activation(),
            input: Box::new(manifest("tester", 2, "epoch-1")),
            instruction: evidence("revise-output"),
            invalidate: vec![],
        },
        ControlParameters::ResumeBlocked {
            activation: activation(),
            blocker_id: evidence("blocker-1"),
            response: BlockerResponse::Evidence {
                evidence: evidence("check-results"),
            },
        },
        ControlParameters::ContinueTurn {
            plan: ContinuationPlan {
                condition_runs: vec![],
                source_epoch_id: ExecutionEpochId::new("epoch-1").unwrap(),
                epoch_id: ExecutionEpochId::new("epoch-2").unwrap(),
                selections: vec![ContinuationSelection::Retry {
                    previous: activation(),
                    input: Box::new(manifest("tester", 2, "epoch-2")),
                }],
            },
            replay_decisions: vec![],
        },
        ControlParameters::AddAgent {
            input: Box::new(manifest("reviewer", 1, "epoch-1")),
            dependencies: vec![TurnNodeId::new("tester").unwrap()],
        },
        ControlParameters::ReplaceFutureAgent {
            target: TurnNodeId::new("unstarted-reviewer").unwrap(),
            input: Box::new(manifest("replacement-reviewer", 1, "epoch-1")),
            rewire_dependents: vec![TurnNodeId::new("summary").unwrap()],
        },
        ControlParameters::FinishTurn {
            mode: FinishMode::Normal,
        },
        ControlParameters::FinishTurn {
            mode: FinishMode::ForcePartial {
                approval: evidence("exact-human-partial-finish"),
                missing_conditions: vec![evidence("missing-browser-check")],
                stop_activations: vec![activation()],
                selected_activations: vec![],
                missing_condition_ids: vec![],
            },
        },
    ];
    for parameters in variants {
        let mut request = scenario().request;
        request.parameters = parameters;
        let bytes = serde_json::to_vec(&request).unwrap();
        assert_eq!(ControlCommandRequest::decode(&bytes).unwrap(), request);
    }
    let mut request = scenario().request;
    request.parameters = ControlParameters::RetryActivation {
        activation: activation(),
        input: Box::new(manifest("tester", 2, "foreign-epoch")),
        replay_decisions: vec![],
    };
    assert!(ControlCommandRequest::decode(&serde_json::to_vec(&request).unwrap()).is_err());
    request.parameters = ControlParameters::RetryActivation {
        activation: activation(),
        input: Box::new(manifest("tester", 4, "epoch-1")),
        replay_decisions: vec![],
    };
    assert!(ControlCommandRequest::decode(&serde_json::to_vec(&request).unwrap()).is_err());
}

#[test]
fn source_target_and_transition_owners_must_match_without_mutation() {
    let root = TempDir::new().unwrap();
    let mut journal = store(&root);
    let fixture = scenario();
    let wrong_human = TrustedCommandSource::human(
        SessionId::new("another-session").unwrap(),
        owner().turn_id,
        evidence("human"),
    );
    assert!(matches!(
        journal.record_requested(fixture.request.clone(), wrong_human),
        Err(ControlCommandError::OwnerConflict)
    ));
    let mut wrong_target = fixture.request.clone();
    let ControlParameters::StopActivation { activation } = &mut wrong_target.parameters else {
        unreachable!()
    };
    activation.execution_epoch_id = ExecutionEpochId::new("another-epoch").unwrap();
    assert!(matches!(
        journal.record_requested(wrong_target, human()),
        Err(ControlCommandError::OwnerConflict)
    ));
    journal
        .record_requested(fixture.request.clone(), human())
        .unwrap();
    let bytes = std::fs::read(journal.path()).unwrap();
    for field in ["session", "turn", "epoch"] {
        let mut wrong = fixture.updates[0].clone();
        match field {
            "session" => wrong.session_id = SessionId::new("another-session").unwrap(),
            "turn" => wrong.turn_id = LogicalTurnId::new("another-turn").unwrap(),
            _ => wrong.execution_epoch_id = ExecutionEpochId::new("another-epoch").unwrap(),
        }
        assert!(matches!(
            journal.advance(wrong),
            Err(ControlCommandError::OwnerConflict)
        ));
    }
    assert_eq!(std::fs::read(journal.path()).unwrap(), bytes);
}

#[test]
fn revision_and_continuation_can_name_retained_older_generations_without_retargeting_new_input() {
    let mut request = scenario().request;
    request.execution_epoch_id = ExecutionEpochId::new("epoch-2").unwrap();
    request.parameters = ControlParameters::ReviseActivation {
        activation: activation(),
        input: Box::new(manifest("tester", 2, "epoch-2")),
        instruction: evidence("revise-retained-output"),
        invalidate: vec![],
    };
    ControlCommandRequest::decode(&serde_json::to_vec(&request).unwrap()).unwrap();
    request.parameters = ControlParameters::ContinueTurn {
        plan: ContinuationPlan {
            condition_runs: vec![],
            source_epoch_id: ExecutionEpochId::new("epoch-2").unwrap(),
            epoch_id: ExecutionEpochId::new("epoch-3").unwrap(),
            selections: vec![ContinuationSelection::Retry {
                previous: activation(),
                input: Box::new(manifest("tester", 2, "epoch-3")),
            }],
        },
        replay_decisions: vec![InvocationDecisionRef {
            invocation_id: InvocationId::new("old-invocation").unwrap(),
            activation: activation(),
            decision: evidence("exact-reconciliation-decision"),
        }],
    };
    ControlCommandRequest::decode(&serde_json::to_vec(&request).unwrap()).unwrap();
    let ControlParameters::ContinueTurn { plan, .. } = &mut request.parameters else {
        unreachable!()
    };
    let ContinuationSelection::Retry { input, .. } = &mut plan.selections[0] else {
        unreachable!()
    };
    input.activation.execution_epoch_id = ExecutionEpochId::new("epoch-2").unwrap();
    assert!(ControlCommandRequest::decode(&serde_json::to_vec(&request).unwrap()).is_err());
}

fn profile() -> ExecutionProfile {
    ExecutionProfile {
        definition: "qa-runner".into(),
        provider: "test".into(),
        model: "test".into(),
        isolation: "local-test".into(),
        tools: vec![],
        write_scope: None,
    }
}

fn grant() -> AuthorityGrant {
    AuthorityGrant {
        id: "grant-qa".into(),
        revision: 1,
        issuer_evidence: evidence("human-grant"),
        holder: TurnNodeId::new("tester").unwrap(),
        descendants: vec![],
        allow_stop_descendants: false,
        delegation: None,
        conditions: vec![],
        profiles: vec![profile()],
        limits: GrantLimits {
            activations: 5,
            invocations: 5,
            tokens: 100,
            cost_microunits: 100,
        },
        expires_at_ms: 1000,
    }
}

#[test]
fn agent_attribution_requires_a_live_lease_and_cannot_change_recorded_origin() {
    let authority_root = TempDir::new().unwrap();
    let gate =
        ControlAuthority::open(authority_root.path(), owner().session_id, owner().turn_id).unwrap();
    gate.install_grant(grant(), gate.revision().unwrap())
        .unwrap();
    let lease = gate
        .register_activation(
            activation(),
            "grant-qa",
            profile(),
            gate.revision().unwrap(),
            100,
        )
        .unwrap();
    let attestation = gate.attest_control_source(&lease, 100).unwrap();
    let root = TempDir::new().unwrap();
    let mut journal = store(&root);
    let request = scenario().request;
    let receipt = journal
        .record_requested(request.clone(), attestation)
        .unwrap();
    assert!(
        matches!(&receipt.view().source, CommandSourceRecord::Agent { activation: source, grant_id, grant_revision: 1, .. } if source == &activation() && grant_id == "grant-qa")
    );
    assert!(matches!(
        journal.record_requested(request.clone(), human()),
        Err(ControlCommandError::CommandConflict)
    ));
    assert!(gate.attest_control_source(&lease, 1000).is_err());
    let foreign_root = TempDir::new().unwrap();
    let foreign_gate =
        ControlAuthority::open(foreign_root.path(), owner().session_id, owner().turn_id).unwrap();
    assert!(foreign_gate.attest_control_source(&lease, 100).is_err());
    let mut foreign_request = request.clone();
    foreign_request.command_id = CommandId::new("different-epoch-command").unwrap();
    foreign_request.execution_epoch_id = ExecutionEpochId::new("epoch-2").unwrap();
    foreign_request.parameters = ControlParameters::FinishTurn {
        mode: FinishMode::Normal,
    };
    assert!(matches!(
        journal.record_requested(
            foreign_request,
            gate.attest_control_source(&lease, 100).unwrap()
        ),
        Err(ControlCommandError::OwnerConflict)
    ));
    gate.stop_activation(&activation(), gate.revision().unwrap())
        .unwrap();
    assert!(gate.attest_control_source(&lease, 100).is_err());
    let next_activation = manifest("tester", 2, "epoch-1").activation;
    let next_lease = gate
        .register_activation(
            next_activation,
            "grant-qa",
            profile(),
            gate.revision().unwrap(),
            100,
        )
        .unwrap();
    assert!(gate.attest_control_source(&next_lease, 100).is_ok());
    gate.revoke_grant("grant-qa", gate.revision().unwrap())
        .unwrap();
    assert!(gate.attest_control_source(&next_lease, 100).is_err());
    // Authenticated owner lookup can return historical evidence after the live
    // lease closes. That receipt is neither a fresh grant nor new acceptance.
    assert_eq!(
        journal
            .lookup_request(&request)
            .unwrap()
            .unwrap()
            .view()
            .state,
        ControlCommandState::Requested
    );
    drop(gate);
    let reopened =
        ControlAuthority::open(authority_root.path(), owner().session_id, owner().turn_id).unwrap();
    assert!(reopened.attest_control_source(&lease, 100).is_err());
}

#[test]
fn malformed_future_or_cross_owner_stores_are_not_rewritten() {
    let fixture = scenario();
    for mutation in ["version", "sequence", "revision", "duplicate", "owner"] {
        let root = TempDir::new().unwrap();
        let mut journal = store(&root);
        journal
            .record_requested(fixture.request.clone(), human())
            .unwrap();
        let path = journal.path();
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        drop(journal);
        match mutation {
            "version" => json["schema_version"] = 55.into(),
            "sequence" => json["records"][0]["sequence"] = 2.into(),
            "revision" => json["records"][0]["receipt_revision"] = 2.into(),
            "owner" => json["owner"]["workspace_id"] = "different-workspace".into(),
            _ => {
                let duplicate = json["records"][0].clone();
                json["records"].as_array_mut().unwrap().push(duplicate);
            }
        }
        let bytes = serde_json::to_vec(&json).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(ControlCommandStore::open(root.path(), owner()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn failed_storage_requires_reopen_and_missing_roots_are_never_created() {
    let root = TempDir::new().unwrap();
    let missing = root.path().join("missing");
    assert!(ControlCommandStore::open(&missing, owner()).is_err());
    assert!(!missing.exists());
    let mut journal = store(&root);
    assert!(ControlCommandStore::open(root.path(), owner()).is_err());
    let bytes = std::fs::read(journal.path()).unwrap();
    std::fs::remove_file(journal.path()).unwrap();
    std::fs::create_dir(journal.path()).unwrap();
    let request = scenario().request;
    assert!(journal.record_requested(request.clone(), human()).is_err());
    assert!(matches!(
        journal.lookup_request(&request),
        Err(ControlCommandError::RecoveryRequired)
    ));
    assert!(matches!(
        journal.records(),
        Err(ControlCommandError::RecoveryRequired)
    ));
    std::fs::remove_dir(journal.path()).unwrap();
    std::fs::write(journal.path(), bytes).unwrap();
    drop(journal);
    let mut journal = store(&root);
    assert!(journal.lookup_request(&request).unwrap().is_none());
    assert_eq!(
        journal
            .record_requested(request, human())
            .unwrap()
            .view()
            .state,
        ControlCommandState::Requested
    );
}
