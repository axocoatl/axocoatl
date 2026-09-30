use super::*;
use axocoatl_core::TokenUsageStats;
use axocoatl_session::control_authority::{ConditionPermission, GrantLimits};
use axocoatl_session::execution_content::{
    ActivationOutputContent, ConditionSupervisionEvidence, ExecutionRequestContent, ExecutionUsage,
    OutputKind, RepositoryCheckDefinition,
};
use axocoatl_session::execution_ownership::{LegacyFormatOwnership, UpgradedFormatOwnership};
use axocoatl_session::execution_store::ExecutionStoreOwner;

struct Fixture {
    root: tempfile::TempDir,
    owner: ExecutionStoreOwner,
    controller: SessionDispatchController,
    run: ConditionRunRef,
    repository: EvidenceRef,
    grant: GrantSnapshotRef,
}

fn fixture() -> Fixture {
    let source: serde_json::Value = serde_json::from_str(include_str!(
        "../../axocoatl-session/tests/fixtures/turn_contract/check_only_recovery_preserves_accepted_generations.json"
    )).unwrap();
    let mut events: Vec<TurnContractEnvelope> = source["steps"].as_array().unwrap()[..3]
        .iter()
        .map(|step| serde_json::from_value(step["envelope"].clone()).unwrap())
        .collect();
    let root = tempfile::tempdir().unwrap();
    let owner = ExecutionStoreOwner {
        workspace_id: "check-workspace".into(),
        session_id: events[0].session_id.clone(),
    };
    let mut canonical = SessionExecutionStore::open(
        Arc::new(
            LegacyFormatOwnership::acquire(root.path())
                .unwrap()
                .upgrade()
                .unwrap(),
        ),
        owner.clone(),
    )
    .unwrap();
    let mut content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let definition = content
        .retain_repository_check_definition(RepositoryCheckDefinition {
            argv: vec!["test-only-command".into(), "exact argument".into()],
            timeout_ms: 10_000,
            stdout_bytes: 16,
            stderr_bytes: 8,
        })
        .unwrap();
    let repository = content
        .retain_activation_evidence(ActivationEvidenceContent::Repository {
            description: "owned fixture repository".into(),
            revision: Some("revision".into()),
        })
        .unwrap()
        .reference()
        .clone();
    let TurnContractEvent::Begin { graph, .. } = &mut events[0].event else {
        panic!("begin fixture");
    };
    let kind = ConditionKind::RepositoryCheck {
        definition: definition.reference().clone(),
    };
    graph.conditions[0].kind = kind.clone();
    let condition_id = graph.conditions[0].condition_id.clone();
    let request = content
        .retain_request(ExecutionRequestContent {
            turn_id: events[0].turn_id.clone(),
            recorded_at_unix_ms: 1,
            display_input: "check the answer".into(),
            effective_input: "check the answer".into(),
            context: vec![],
            target_definition: None,
            model: None,
        })
        .unwrap();
    canonical
        .begin_with_request(events[0].clone(), &request)
        .unwrap();
    canonical.append(events[1].clone()).unwrap();
    let TurnContractEvent::AcceptActivation {
        activation, output, ..
    } = &mut events[2].event
    else {
        panic!("accept fixture");
    };
    let run = ConditionRunRef {
        session_id: activation.session_id.clone(),
        turn_id: activation.turn_id.clone(),
        epoch_id: activation.execution_epoch_id.clone(),
        condition_id,
        run_id: ConditionRunId::new("repository-check-one").unwrap(),
        activations: vec![activation.clone()],
    };
    *output = content
        .retain_output(
            &canonical.snapshot(&run.turn_id).unwrap(),
            ActivationOutputContent {
                activation: activation.clone(),
                recorded_at_unix_ms: 2,
                text: "accepted answer".into(),
                usage: ExecutionUsage::Measured {
                    usage: TokenUsageStats::default(),
                },
                kind: OutputKind::Final,
            },
        )
        .unwrap()
        .reference()
        .clone();
    canonical.append(events[2].clone()).unwrap();
    let policy = AuthorityGrant {
        id: "check-grant".into(),
        revision: 1,
        issuer_evidence: EvidenceRef::new("human-approved-check").unwrap(),
        holder: run.activations[0].node_id.clone(),
        descendants: vec![],
        allow_stop_descendants: false,
        delegation: None,
        profiles: vec![],
        conditions: vec![ConditionPermission {
            kind,
            nodes: vec![run.activations[0].node_id.clone()],
            repository: repository.clone(),
            isolation: "owned-check-fixture".into(),
            max_timeout_ms: 10_000,
            max_stdout_bytes: 16,
            max_stderr_bytes: 8,
        }],
        limits: GrantLimits {
            activations: 0,
            invocations: 4,
            tokens: 0,
            cost_microunits: 0,
        },
        expires_at_ms: now_ms().unwrap() + 3_600_000,
    };
    let grant = GrantSnapshotRef {
        grant_id: GrantId::new(policy.id.clone()).unwrap(),
        revision: 1,
        evidence: content
            .retain_activation_evidence(ActivationEvidenceContent::Grant {
                policy: policy.clone(),
            })
            .unwrap()
            .reference()
            .clone(),
    };
    drop(content);
    let controller = SessionDispatchController::open(canonical, run.turn_id.clone()).unwrap();
    controller.install_grant(policy).unwrap();
    Fixture {
        root,
        owner,
        controller,
        run,
        repository,
        grant,
    }
}

fn prepare(fixture: &Fixture) -> Result<PreparedRepositoryCheck> {
    fixture.controller.prepare_repository_check(
        fixture.run.clone(),
        fixture.repository.clone(),
        fixture.grant.clone(),
        "owned-check-fixture",
    )
}

fn capture(bytes: &[u8], capacity: usize, complete: bool) -> ConditionOutputEvidence {
    let mut output = ConditionOutputCapture::new(capacity).unwrap();
    output.observe(bytes).unwrap();
    output.finish(complete)
}

// These are retained observation fixtures for the controller/content seam, not
// runtime receipts. None authorizes releasing a repository execution lease.
fn supervision(fixture: &Fixture) -> ConditionSupervisionEvidence {
    ConditionSupervisionEvidence {
        invocation_id: fixture.run.run_id.as_str().into(),
        request_sha256: "a".repeat(64),
        runtime_identity: "controller-storage-fixture-runtime".into(),
        program_sha256: "b".repeat(64),
        transport_identity: "controller-storage-fixture-transport".into(),
        launched: true,
        quiescent: true,
        primary_exit: Some(ConditionProcessStatus::Exited { code: 0 }),
    }
}

fn reopen(fixture: Fixture) -> Fixture {
    let Fixture {
        root,
        owner,
        controller,
        run,
        repository,
        grant,
    } = fixture;
    drop(controller);
    let canonical = SessionExecutionStore::open(
        Arc::new(UpgradedFormatOwnership::open(root.path()).unwrap()),
        owner.clone(),
    )
    .unwrap();
    let controller = SessionDispatchController::open(canonical, run.turn_id.clone()).unwrap();
    controller.reconcile_repository_checks().unwrap();
    Fixture {
        root,
        owner,
        controller,
        run,
        repository,
        grant,
    }
}

#[test]
fn check_dispatch_requires_durable_intent_claim_and_derives_verdict_from_exit() {
    for (code, expected) in [(0, ConditionOutcome::Passed), (7, ConditionOutcome::Failed)] {
        let fixture = fixture();
        let prepared = prepare(&fixture).unwrap();
        {
            let state = fixture.controller.lock().unwrap();
            let snapshot = state.canonical.snapshot(&fixture.run.turn_id).unwrap();
            assert_eq!(
                snapshot
                    .contract()
                    .condition_run(&fixture.run.run_id)
                    .unwrap()
                    .intent,
                *prepared.arguments().reference()
            );
            assert!(state
                .content
                .condition_result(prepared.arguments())
                .unwrap()
                .is_none());
            assert_eq!(
                state
                    .authority
                    .usage(fixture.grant.grant_id.as_str())
                    .unwrap()
                    .invocations,
                1
            );
            assert!(state
                .authority
                .condition_call(&fixture.run.run_id)
                .unwrap()
                .is_some());
        }
        let started = prepared.begin_dispatch().unwrap();
        assert_eq!(
            started.arguments().definition().argv,
            ["test-only-command", "exact argument"]
        );
        let settled = started
            .settle(
                ConditionProcessStatus::Exited { code },
                capture(b"actual stdout with long suffix", 16, true),
                capture(b"stderr", 8, true),
                100,
            )
            .unwrap();
        assert!(settled.outcome_known);
        assert!(settled.canonical_effect_resolved);
        assert_eq!(settled.verdict, Some(expected));
        assert!(settled.result.stdout().is_truncated());
        let fixture = reopen(fixture);
        let snapshot = fixture.controller.snapshot().unwrap();
        assert_eq!(
            snapshot
                .contract()
                .current_condition(&fixture.run.condition_id)
                .unwrap()
                .outcome,
            expected
        );
        assert!(!snapshot.contract().has_unknown_effects());
        assert!(prepare(&fixture).is_err());
        assert_eq!(
            fixture
                .controller
                .lock()
                .unwrap()
                .authority
                .usage(fixture.grant.grant_id.as_str())
                .unwrap()
                .invocations,
            1
        );
    }
}

#[test]
fn revoked_before_claim_records_positive_no_dispatch_without_charging_or_poisoning() {
    let fixture = fixture();
    {
        let state = fixture.controller.lock().unwrap();
        state
            .authority
            .revoke_grant(
                fixture.grant.grant_id.as_str(),
                state.authority.revision().unwrap(),
            )
            .unwrap();
    }
    assert!(prepare(&fixture).is_err());
    let snapshot = fixture.controller.snapshot().unwrap();
    let run = snapshot
        .contract()
        .condition_run(&fixture.run.run_id)
        .unwrap();
    assert!(matches!(
        run.resolution,
        Some(ConditionEffectResolution::NotDispatched { .. })
    ));
    assert!(snapshot
        .contract()
        .current_condition(&fixture.run.condition_id)
        .is_none());
    {
        let state = fixture.controller.lock().unwrap();
        state.ready().unwrap();
        assert_eq!(
            state
                .authority
                .usage(fixture.grant.grant_id.as_str())
                .unwrap()
                .invocations,
            0
        );
        assert!(state
            .authority
            .condition_call(&fixture.run.run_id)
            .unwrap()
            .is_none());
    }
    let fixture = reopen(fixture);
    assert!(!fixture
        .controller
        .snapshot()
        .unwrap()
        .contract()
        .has_unknown_effects());
}

#[test]
fn stop_between_preparation_and_dispatch_preserves_charge_and_proves_no_dispatch() {
    let fixture = fixture();
    let prepared = prepare(&fixture).unwrap();
    {
        let state = fixture.controller.lock().unwrap();
        state
            .authority
            .close_dispatch(state.authority.revision().unwrap())
            .unwrap();
    }
    assert!(prepared.begin_dispatch().is_err());
    let snapshot = fixture.controller.snapshot().unwrap();
    assert!(matches!(
        snapshot
            .contract()
            .condition_run(&fixture.run.run_id)
            .unwrap()
            .resolution,
        Some(ConditionEffectResolution::NotDispatched { .. })
    ));
    assert!(snapshot.contract().conditions().is_empty());
    assert_eq!(
        fixture
            .controller
            .lock()
            .unwrap()
            .authority
            .usage(fixture.grant.grant_id.as_str())
            .unwrap()
            .invocations,
        1
    );
}

#[test]
fn dropping_prepared_and_dispatched_permits_have_different_truthful_dispositions() {
    let fixture = fixture();
    drop(prepare(&fixture).unwrap());
    assert!(!fixture
        .controller
        .snapshot()
        .unwrap()
        .contract()
        .has_unknown_effects());
    let other = self::fixture();
    drop(prepare(&other).unwrap().begin_dispatch().unwrap());
    let snapshot = other.controller.snapshot().unwrap();
    assert_eq!(
        snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert!(snapshot.contract().has_unknown_effects());
    assert!(snapshot.contract().conditions().is_empty());
    let state = other.controller.lock().unwrap();
    let arguments = state
        .content
        .condition_arguments(&snapshot, &other.run.run_id)
        .unwrap()
        .unwrap();
    assert!(state
        .content
        .condition_result(&arguments)
        .unwrap()
        .is_none());
}

#[test]
fn dropped_check_late_reconciliation_allows_continue_and_new_authorized_check() {
    let fixture = fixture();
    let in_flight = prepare(&fixture).unwrap().begin_dispatch().unwrap();
    let arguments = in_flight.arguments().clone();
    drop(in_flight);
    let fixture = reopen(fixture);
    let snapshot = fixture.controller.snapshot().unwrap();
    assert_eq!(
        snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert!(snapshot.contract().has_unknown_effects());
    {
        let mut state = fixture.controller.lock().unwrap();
        // The supervisor's independently retained completion arrives after the
        // owner was dropped. Recovery is evidence-only; it starts no process.
        state
            .content
            .record_condition_result(
                &arguments,
                ConditionProcessStatus::Exited { code: 1 },
                capture(b"late failing check", 16, true),
                capture(b"", 8, true),
                100,
            )
            .unwrap();
    }
    fixture.controller.reconcile_repository_checks().unwrap();
    let snapshot = fixture.controller.snapshot().unwrap();
    assert!(!snapshot.contract().has_unknown_effects());
    assert_eq!(
        snapshot
            .contract()
            .current_condition(&fixture.run.condition_id)
            .unwrap()
            .outcome,
        ConditionOutcome::Failed
    );
    let next_epoch = ExecutionEpochId::new("reconciled-check-epoch").unwrap();
    fixture
        .controller
        .append_host_event(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("continue-after-late-check").unwrap(),
            expected_revision: snapshot.contract().revision(),
            session_id: fixture.run.session_id.clone(),
            turn_id: fixture.run.turn_id.clone(),
            event: TurnContractEvent::Continue {
                plan: ContinuationPlan {
                    source_epoch_id: fixture.run.epoch_id.clone(),
                    epoch_id: next_epoch.clone(),
                    selections: vec![ContinuationSelection::RetainAccepted {
                        activation: fixture.run.activations[0].clone(),
                    }],
                    condition_runs: vec![fixture.run.condition_id.clone()],
                },
            },
        })
        .unwrap();
    let mut next_run = fixture.run.clone();
    next_run.epoch_id = next_epoch;
    next_run.run_id = ConditionRunId::new("new-check-after-reconciliation").unwrap();
    let new_check = fixture
        .controller
        .prepare_repository_check(
            next_run.clone(),
            fixture.repository.clone(),
            fixture.grant.clone(),
            "owned-check-fixture",
        )
        .unwrap();
    {
        let state = fixture.controller.lock().unwrap();
        let snapshot = state.canonical.snapshot(&fixture.run.turn_id).unwrap();
        let check = axocoatl_session::turn_checks::project_check(
            &snapshot,
            &state.content,
            &next_run.condition_id,
            new_check.arguments().definition(),
        )
        .unwrap();
        assert_eq!(check.run_id.as_ref(), Some(&next_run.run_id));
        assert_eq!(check.state, "outcome_unknown");
        assert!(
            check.process_status.is_none(),
            "old failed process is not the new run"
        );
    }
    let settled = new_check
        .begin_dispatch()
        .unwrap()
        .settle(
            ConditionProcessStatus::Exited { code: 0 },
            capture(b"new check passes", 16, true),
            capture(b"", 8, true),
            101,
        )
        .unwrap();
    assert!(settled.outcome_known);
    assert!(settled.canonical_effect_resolved);
    assert_eq!(settled.verdict, Some(ConditionOutcome::Passed));
    let snapshot = fixture.controller.snapshot().unwrap();
    assert_eq!(snapshot.contract().state(), Some(LogicalTurnState::Running));
    assert_eq!(
        snapshot
            .contract()
            .current_condition(&fixture.run.condition_id)
            .unwrap()
            .epoch_id,
        next_run.epoch_id
    );
    assert_eq!(
        fixture
            .controller
            .lock()
            .unwrap()
            .authority
            .usage(fixture.grant.grant_id.as_str())
            .unwrap()
            .invocations,
        2
    );
}

#[test]
fn closed_late_result_is_known_without_rewriting_canonical_effect_or_verdict() {
    for closure in [TurnClosure::Finished, TurnClosure::Cancelled] {
        let fixture = fixture();
        let in_flight = prepare(&fixture).unwrap().begin_dispatch().unwrap();
        let (canonical_path, closed_bytes, closed_revision) = {
            let mut state = fixture.controller.lock().unwrap();
            state
                .authority
                .close_dispatch(state.authority.revision().unwrap())
                .unwrap();
            // This fixture exercises only the canonical/content boundary, with
            // no real Memory candidate to promote. Closure remains immutable.
            state
                .append(
                    "closed-before-check-result",
                    TurnContractEvent::Close { closure },
                )
                .unwrap();
            let path = state.canonical.path();
            let bytes = std::fs::read(&path).unwrap();
            let revision = state
                .canonical
                .snapshot(&fixture.run.turn_id)
                .unwrap()
                .contract()
                .revision();
            (path, bytes, revision)
        };
        let settled = in_flight
            .settle(
                ConditionProcessStatus::Exited { code: 0 },
                capture(b"late observed pass", 16, true),
                capture(b"", 8, true),
                100,
            )
            .unwrap();
        assert!(settled.outcome_known);
        assert!(!settled.canonical_effect_resolved);
        assert_eq!(settled.verdict, None);
        assert_eq!(std::fs::read(&canonical_path).unwrap(), closed_bytes);
        let snapshot = fixture.controller.snapshot().unwrap();
        assert_eq!(snapshot.contract().revision(), closed_revision);
        assert!(snapshot
            .contract()
            .condition_run(&fixture.run.run_id)
            .unwrap()
            .resolution
            .is_none());
        assert!(snapshot.contract().has_unknown_effects());
        assert!(snapshot.contract().conditions().is_empty());
        fixture.controller.reconcile_repository_checks().unwrap();
        assert_eq!(std::fs::read(canonical_path).unwrap(), closed_bytes);
        let state = fixture.controller.lock().unwrap();
        assert_eq!(
            state
                .content
                .condition_result(settled.result.arguments())
                .unwrap()
                .unwrap(),
            settled.result
        );
        assert!(state
            .authority
            .condition_call(&fixture.run.run_id)
            .unwrap()
            .unwrap()
            .result
            .is_some());
    }
}

#[test]
fn unknown_transport_and_incomplete_exit_retain_output_without_resolving_or_passing() {
    for (status, complete) in [
        (
            ConditionProcessStatus::Uncertain {
                message: "transport failed after observed output".into(),
            },
            true,
        ),
        (ConditionProcessStatus::TimedOut, false),
        (ConditionProcessStatus::Exited { code: 0 }, false),
    ] {
        let fixture = fixture();
        let settled = prepare(&fixture)
            .unwrap()
            .begin_dispatch()
            .unwrap()
            .settle(
                status.clone(),
                capture(b"observed prefix and suffix", 16, complete),
                capture(b"", 8, complete),
                100,
            )
            .unwrap();
        assert!(!settled.outcome_known);
        assert!(!settled.canonical_effect_resolved);
        assert_eq!(settled.verdict, None);
        assert_eq!(settled.result.status(), &status);
        let fixture = reopen(fixture);
        let snapshot = fixture.controller.snapshot().unwrap();
        assert!(snapshot.contract().has_unknown_effects());
        assert!(snapshot.contract().conditions().is_empty());
        assert!(prepare(&fixture).is_err());
    }
}

#[test]
fn supervised_timeout_cancel_and_launch_failure_retain_failures_and_independent_exit() {
    for (status, launched, primary_exit) in [
        (
            ConditionProcessStatus::TimedOut,
            true,
            Some(ConditionProcessStatus::Exited { code: 0 }),
        ),
        (
            ConditionProcessStatus::Interrupted,
            true,
            Some(ConditionProcessStatus::Signalled { signal: 15 }),
        ),
        (
            ConditionProcessStatus::LaunchFailed {
                message: "executable is missing".into(),
            },
            false,
            None,
        ),
    ] {
        let fixture = fixture();
        let mut evidence = supervision(&fixture);
        evidence.launched = launched;
        evidence.primary_exit = primary_exit;
        let settled = prepare(&fixture)
            .unwrap()
            .begin_dispatch()
            .unwrap()
            .settle_observation(
                status.clone(),
                capture(b"", 16, true),
                capture(b"", 8, true),
                100,
                Some(evidence.clone()),
            )
            .unwrap();
        assert!(settled.outcome_known);
        assert!(settled.canonical_effect_resolved);
        assert_eq!(settled.verdict, Some(ConditionOutcome::Failed));
        assert_eq!(settled.result.status(), &status);
        assert_eq!(settled.result.supervision(), Some(&evidence));
        let fixture = reopen(fixture);
        let snapshot = fixture.controller.snapshot().unwrap();
        assert!(!snapshot.contract().has_unknown_effects());
        let observation = snapshot
            .contract()
            .current_condition(&fixture.run.condition_id)
            .unwrap();
        assert_eq!(observation.outcome, ConditionOutcome::Failed);
        assert_eq!(&observation.evidence, settled.result.reference());
        let state = fixture.controller.lock().unwrap();
        assert_eq!(
            state
                .content
                .condition_result(settled.result.arguments())
                .unwrap(),
            Some(settled.result)
        );
    }
}

#[test]
fn supervised_quiescence_false_never_resolves_or_records_a_verdict_after_reload() {
    for (status, launched, primary_exit) in [
        (
            ConditionProcessStatus::Exited { code: 0 },
            true,
            Some(ConditionProcessStatus::Exited { code: 0 }),
        ),
        (
            ConditionProcessStatus::TimedOut,
            true,
            Some(ConditionProcessStatus::Exited { code: 0 }),
        ),
        (
            ConditionProcessStatus::Interrupted,
            true,
            Some(ConditionProcessStatus::Signalled { signal: 15 }),
        ),
        (
            ConditionProcessStatus::LaunchFailed {
                message: "launch failed with incomplete cleanup knowledge".into(),
            },
            false,
            None,
        ),
    ] {
        let fixture = fixture();
        let mut evidence = supervision(&fixture);
        evidence.launched = launched;
        evidence.primary_exit = primary_exit;
        evidence.quiescent = false;
        let settled = prepare(&fixture)
            .unwrap()
            .begin_dispatch()
            .unwrap()
            .settle_observation(
                status,
                capture(b"", 16, true),
                capture(b"", 8, true),
                100,
                Some(evidence.clone()),
            )
            .unwrap();
        assert!(!settled.outcome_known);
        assert!(!settled.canonical_effect_resolved);
        assert_eq!(settled.verdict, None);
        let fixture = reopen(fixture);
        let snapshot = fixture.controller.snapshot().unwrap();
        assert!(snapshot.contract().has_unknown_effects());
        assert!(snapshot
            .contract()
            .condition_run(&fixture.run.run_id)
            .unwrap()
            .resolution
            .is_none());
        assert!(snapshot
            .contract()
            .current_condition(&fixture.run.condition_id)
            .is_none());
        assert!(prepare(&fixture).is_err());
        let state = fixture.controller.lock().unwrap();
        let restored = state
            .content
            .condition_result(settled.result.arguments())
            .unwrap()
            .unwrap();
        assert_eq!(restored, settled.result);
        assert_eq!(restored.supervision(), Some(&evidence));
    }
}

#[test]
fn quiescent_exit_with_incomplete_output_resolves_effect_but_keeps_condition_pending() {
    for (stdout_complete, stderr_complete) in [(false, true), (true, false), (false, false)] {
        let fixture = fixture();
        let evidence = supervision(&fixture);
        let settled = prepare(&fixture)
            .unwrap()
            .begin_dispatch()
            .unwrap()
            .settle_observation(
                ConditionProcessStatus::Exited { code: 0 },
                capture(b"observed stdout with suffix", 16, stdout_complete),
                capture(b"stderr", 8, stderr_complete),
                100,
                Some(evidence),
            )
            .unwrap();
        assert!(settled.outcome_known);
        assert!(settled.canonical_effect_resolved);
        assert_eq!(settled.verdict, None);
        let fixture = reopen(fixture);
        let snapshot = fixture.controller.snapshot().unwrap();
        assert!(!snapshot.contract().has_unknown_effects());
        assert!(snapshot
            .contract()
            .current_condition(&fixture.run.condition_id)
            .is_none());
        assert!(snapshot
            .contract()
            .condition_run(&fixture.run.run_id)
            .unwrap()
            .resolution
            .is_some());
        let state = fixture.controller.lock().unwrap();
        assert_eq!(
            state
                .content
                .condition_result(settled.result.arguments())
                .unwrap(),
            Some(settled.result)
        );
    }
}

#[test]
fn supervised_no_dispatch_requires_zero_output_and_retains_a_pending_condition() {
    for (stdout, stderr) in [
        (b"unexpected".as_slice(), b"".as_slice()),
        (b"".as_slice(), b"unexpected".as_slice()),
    ] {
        let fixture = fixture();
        let mut evidence = supervision(&fixture);
        evidence.launched = false;
        evidence.primary_exit = None;
        let started = prepare(&fixture).unwrap().begin_dispatch().unwrap();
        let arguments = started.arguments().clone();
        assert!(started
            .settle_observation(
                ConditionProcessStatus::NotDispatched,
                capture(stdout, 16, true),
                capture(stderr, 8, true),
                100,
                Some(evidence)
            )
            .is_err());
        assert!(fixture
            .controller
            .lock()
            .unwrap()
            .content
            .condition_result(&arguments)
            .unwrap()
            .is_none());
        let fixture = reopen(fixture);
        let snapshot = fixture.controller.snapshot().unwrap();
        assert!(snapshot.contract().has_unknown_effects());
        assert!(snapshot.contract().conditions().is_empty());
    }
    let fixture = fixture();
    let mut evidence = supervision(&fixture);
    evidence.launched = false;
    evidence.primary_exit = None;
    let settled = prepare(&fixture)
        .unwrap()
        .begin_dispatch()
        .unwrap()
        .settle_observation(
            ConditionProcessStatus::NotDispatched,
            capture(b"", 16, true),
            capture(b"", 8, true),
            100,
            Some(evidence),
        )
        .unwrap();
    assert!(settled.outcome_known);
    assert!(settled.canonical_effect_resolved);
    assert_eq!(settled.verdict, None);
    let fixture = reopen(fixture);
    let snapshot = fixture.controller.snapshot().unwrap();
    assert!(!snapshot.contract().has_unknown_effects());
    assert!(snapshot.contract().conditions().is_empty());
    assert!(matches!(
        snapshot
            .contract()
            .condition_run(&fixture.run.run_id)
            .unwrap()
            .resolution,
        Some(ConditionEffectResolution::NotDispatched { .. })
    ));
}

#[test]
fn retained_result_crash_prefix_reconciles_without_replay() {
    let fixture = fixture();
    let mut started = prepare(&fixture).unwrap().begin_dispatch().unwrap();
    let reference = {
        let mut state = fixture.controller.lock().unwrap();
        state
            .content
            .record_condition_result(
                started.arguments(),
                ConditionProcessStatus::Exited { code: 0 },
                capture(b"completed before process loss", 16, true),
                capture(b"", 8, true),
                100,
            )
            .unwrap()
            .reference()
            .clone()
    };
    // A process crash skips destructors. Remove only the live test handle; the
    // real disk state stops after acknowledged content, before authority settle.
    started.claim.take();
    drop(started);
    let fixture = reopen(fixture);
    let snapshot = fixture.controller.snapshot().unwrap();
    let observation = snapshot
        .contract()
        .current_condition(&fixture.run.condition_id)
        .unwrap();
    assert_eq!(observation.evidence, reference);
    assert_eq!(observation.outcome, ConditionOutcome::Passed);
    assert_eq!(
        snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    assert!(!snapshot.contract().has_unknown_effects());
    assert!(prepare(&fixture).is_err());
}

#[test]
fn bare_intent_crash_prefix_never_turns_absent_claim_into_no_dispatch() {
    let fixture = fixture();
    {
        let mut state = fixture.controller.lock().unwrap();
        let snapshot = state.canonical.snapshot(&fixture.run.turn_id).unwrap();
        let arguments = state
            .content
            .reserve_condition_arguments(&snapshot, &fixture.run, &fixture.repository)
            .unwrap();
        state
            .append(
                "crash-after-only-canonical-intent",
                TurnContractEvent::RecordConditionIntent {
                    run: fixture.run.clone(),
                    intent: arguments.reference().clone(),
                },
            )
            .unwrap();
    }
    let fixture = reopen(fixture);
    let snapshot = fixture.controller.snapshot().unwrap();
    assert!(snapshot.contract().has_unknown_effects());
    assert!(snapshot.contract().conditions().is_empty());
    assert!(prepare(&fixture).is_err());
    assert!(fixture
        .controller
        .lock()
        .unwrap()
        .authority
        .condition_call(&fixture.run.run_id)
        .unwrap()
        .is_none());
}

#[test]
fn old_epoch_result_is_retained_but_cannot_satisfy_new_condition_admission() {
    let fixture = fixture();
    let started = prepare(&fixture).unwrap().begin_dispatch().unwrap();
    {
        let mut state = fixture.controller.lock().unwrap();
        let result = state
            .content
            .record_condition_result(
                started.arguments(),
                ConditionProcessStatus::Exited { code: 0 },
                capture(b"old pass", 16, true),
                capture(b"", 8, true),
                100,
            )
            .unwrap();
        state
            .authority
            .settle_condition_run(started.claim.as_ref().unwrap(), &result)
            .unwrap();
        state
            .append(
                "effect-before-new-epoch",
                TurnContractEvent::ResolveConditionIntent {
                    run_id: fixture.run.run_id.clone(),
                    resolution: ConditionEffectResolution::OutcomeRecorded {
                        evidence: result.reference().clone(),
                    },
                },
            )
            .unwrap();
        state
            .append(
                "pause-for-new-check",
                TurnContractEvent::PauseEpoch {
                    epoch_id: fixture.run.epoch_id.clone(),
                },
            )
            .unwrap();
        state
            .append(
                "new-check-epoch",
                TurnContractEvent::Continue {
                    plan: ContinuationPlan {
                        source_epoch_id: fixture.run.epoch_id.clone(),
                        epoch_id: ExecutionEpochId::new("epoch-2").unwrap(),
                        selections: vec![ContinuationSelection::RetainAccepted {
                            activation: fixture.run.activations[0].clone(),
                        }],
                        condition_runs: vec![fixture.run.condition_id.clone()],
                    },
                },
            )
            .unwrap();
    }
    let authority_revision = fixture
        .controller
        .lock()
        .unwrap()
        .authority
        .revision()
        .unwrap();
    let canonical_revision = fixture.controller.snapshot().unwrap().contract().revision();
    drop(started);
    assert_eq!(
        fixture
            .controller
            .lock()
            .unwrap()
            .authority
            .revision()
            .unwrap(),
        authority_revision
    );
    assert_eq!(
        fixture.controller.snapshot().unwrap().contract().revision(),
        canonical_revision
    );
    let fixture = reopen(fixture);
    let snapshot = fixture.controller.snapshot().unwrap();
    assert!(snapshot
        .contract()
        .current_condition(&fixture.run.condition_id)
        .is_none());
    assert!(snapshot.contract().conditions().is_empty());
    assert!(!snapshot.contract().has_unknown_effects());
}

#[test]
fn superseded_accepted_generation_cannot_receive_a_recovered_old_pass() {
    let fixture = fixture();
    let mut started = prepare(&fixture).unwrap().begin_dispatch().unwrap();
    {
        let mut state = fixture.controller.lock().unwrap();
        let result = state
            .content
            .record_condition_result(
                started.arguments(),
                ConditionProcessStatus::Exited { code: 0 },
                capture(b"old accepted generation passed", 16, true),
                capture(b"", 8, true),
                100,
            )
            .unwrap();
        state
            .authority
            .settle_condition_run(started.claim.as_ref().unwrap(), &result)
            .unwrap();
        state
            .append(
                "resolved-before-revision",
                TurnContractEvent::ResolveConditionIntent {
                    run_id: fixture.run.run_id.clone(),
                    resolution: ConditionEffectResolution::OutcomeRecorded {
                        evidence: result.reference().clone(),
                    },
                },
            )
            .unwrap();
        let current = state.canonical.snapshot(&fixture.run.turn_id).unwrap();
        let accepted = &current.contract().activations()[0];
        let mut revised = accepted.input.clone();
        revised.manifest_id = InputManifestId::new("revised-input").unwrap();
        revised.activation.generation += 1;
        revised.activation.activation_id = ActivationId::new("revised-activation").unwrap();
        revised.revision_context = Some(RevisionContext {
            activation: accepted.activation.clone(),
            output: accepted.output.clone().unwrap(),
        });
        state
            .append(
                "revise-before-check-verdict",
                TurnContractEvent::ReviseAccepted {
                    previous: accepted.activation.clone(),
                    input: Box::new(revised),
                    invalidated_descendants: vec![],
                    evidence: result.reference().clone(),
                },
            )
            .unwrap();
    }
    started.claim.take();
    drop(started);
    let fixture = reopen(fixture);
    let snapshot = fixture.controller.snapshot().unwrap();
    assert_eq!(
        snapshot.contract().activations()[0].state,
        ActivationState::Superseded
    );
    assert!(snapshot.contract().conditions().is_empty());
    assert!(snapshot
        .contract()
        .current_condition(&fixture.run.condition_id)
        .is_none());
}

#[test]
fn check_read_projection_retains_distinct_process_outcomes_after_reopen() {
    let cases = [
        (ConditionProcessStatus::Exited { code: 0 }, "passed"),
        (ConditionProcessStatus::Exited { code: 7 }, "failed"),
        (ConditionProcessStatus::TimedOut, "timed_out"),
        (ConditionProcessStatus::Interrupted, "interrupted"),
        (
            ConditionProcessStatus::Signalled { signal: 15 },
            "signalled",
        ),
        (
            ConditionProcessStatus::LaunchFailed {
                message: "environment unavailable".into(),
            },
            "launch_failed",
        ),
        (
            ConditionProcessStatus::Uncertain {
                message: "transport lost".into(),
            },
            "outcome_unknown",
        ),
    ];
    for (process, expected) in cases {
        let fixture = fixture();
        let prepared = prepare(&fixture).unwrap();
        let definition = prepared.arguments().definition().clone();
        let result = prepared
            .begin_dispatch()
            .unwrap()
            .settle(
                process.clone(),
                capture(b"actual output", 16, true),
                capture(b"stderr", 8, true),
                100,
            )
            .unwrap()
            .result;
        let fixture = reopen(fixture);
        let state = fixture.controller.lock().unwrap();
        let snapshot = state.canonical.snapshot(&fixture.run.turn_id).unwrap();
        let check = axocoatl_session::turn_checks::project_check(
            &snapshot,
            &state.content,
            &fixture.run.condition_id,
            &definition,
        )
        .unwrap();
        assert_eq!(check.state, expected);
        assert_eq!(check.process_status, Some(process));
        assert_eq!(check.evidence.as_ref(), Some(result.reference()));
        assert_eq!(check.run_id, Some(fixture.run.run_id));
        assert_eq!(check.stdout, "actual output");
        assert_eq!(check.stderr, "stderr");
        assert_eq!(serde_json::to_value(&check).unwrap()["state"], expected);
    }
}

#[test]
fn check_read_projection_distinguishes_no_dispatch_from_missing_outcome() {
    for dispatched in [false, true] {
        let fixture = fixture();
        let prepared = prepare(&fixture).unwrap();
        let definition = prepared.arguments().definition().clone();
        if dispatched {
            drop(prepared.begin_dispatch().unwrap());
        } else {
            drop(prepared);
        }
        let fixture = reopen(fixture);
        let state = fixture.controller.lock().unwrap();
        let snapshot = state.canonical.snapshot(&fixture.run.turn_id).unwrap();
        let check = axocoatl_session::turn_checks::project_check(
            &snapshot,
            &state.content,
            &fixture.run.condition_id,
            &definition,
        )
        .unwrap();
        assert_eq!(
            check.state,
            if dispatched {
                "outcome_unknown"
            } else {
                "not_dispatched"
            }
        );
        assert_eq!(
            check.process_status,
            if dispatched {
                None
            } else {
                Some(ConditionProcessStatus::NotDispatched)
            }
        );
        assert_eq!(check.evidence.is_some(), !dispatched);
        assert_eq!(
            check.effect_disposition,
            Some(if dispatched {
                EffectDisposition::OutcomeUnknown
            } else {
                EffectDisposition::NotDispatched
            })
        );
    }
}
