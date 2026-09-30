use axocoatl_session::turn_contract::{
    ActivationState, EffectDisposition, EpochState, InvocationEvidence, LogicalTurnState,
    TurnContract, TurnContractEnvelope, TurnContractError,
};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    envelope: Value,
    error: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    name: String,
    steps: Vec<Step>,
    expected_revision: u64,
    expected_state: LogicalTurnState,
    expected_epochs: Vec<EpochState>,
}

fn decode(value: &Value) -> TurnContractEnvelope {
    TurnContractEnvelope::decode(&serde_json::to_vec(value).unwrap()).unwrap()
}

fn fixture_scenarios() -> Vec<Scenario> {
    [
        include_str!("fixtures/turn_contract/interrupted_epoch_preserves_acceptance.json"),
        include_str!("fixtures/turn_contract/unknown_effect_blocks_recovery.json"),
        include_str!("fixtures/turn_contract/independent_node_identity.json"),
        include_str!("fixtures/turn_contract/partial_finish_is_not_success.json"),
        include_str!("fixtures/turn_contract/complete_plan_retains_blocked_and_unstarted.json"),
        include_str!(
            "fixtures/turn_contract/graph_revision_invalidates_descendants_and_conditions.json"
        ),
        include_str!("fixtures/turn_contract/recovery_covers_unmaterialized_graph_nodes.json"),
        include_str!(
            "fixtures/turn_contract/check_only_recovery_preserves_accepted_generations.json"
        ),
    ]
    .into_iter()
    .map(|text| serde_json::from_str(text).unwrap())
    .collect()
}

#[test]
fn serialized_recovery_scenarios_enforce_transitions_without_partial_mutation() {
    for scenario in fixture_scenarios() {
        let mut fold = TurnContract::default();
        for step in &scenario.steps {
            let envelope = decode(&step.envelope);
            assert_eq!(serde_json::to_value(&envelope).unwrap(), step.envelope);
            let before = fold.clone();
            match &step.error {
                Some(expected) => {
                    let error = fold.apply(&envelope).unwrap_err();
                    assert!(
                        error.to_string().contains(expected),
                        "{}: {error}",
                        scenario.name
                    );
                    assert_eq!(fold, before, "{}: rejection mutated state", scenario.name);
                }
                None => {
                    assert!(fold.apply(&envelope).unwrap(), "{}", scenario.name);
                    let applied = fold.clone();
                    assert!(
                        !fold.apply(&envelope).unwrap(),
                        "exact replay must be inert"
                    );
                    assert_eq!(fold, applied);
                }
            }
        }
        assert_eq!(
            fold.state(),
            Some(scenario.expected_state),
            "{}",
            scenario.name
        );
        assert_eq!(
            fold.revision(),
            scenario.expected_revision,
            "{}",
            scenario.name
        );
        assert_eq!(
            fold.epochs()
                .iter()
                .map(|epoch| epoch.state)
                .collect::<Vec<_>>(),
            scenario.expected_epochs,
            "{}",
            scenario.name,
        );
        if scenario.name == "interrupted_epoch_preserves_acceptance" {
            let accepted = &fold.activations()[0];
            assert_eq!(accepted.state, ActivationState::Accepted);
            assert_eq!(accepted.activation.execution_epoch_id.as_str(), "epoch-1");
            assert_eq!(
                accepted.checkpoint.as_ref().unwrap().checkpoint_id.as_str(),
                "checkpoint-a"
            );
            assert_eq!(fold.activations()[1].state, ActivationState::Interrupted);
            assert_eq!(fold.activations()[2].activation.generation, 2);
        }
    }
}

#[test]
fn tool_recovery_evidence_never_equates_unknown_or_failure_with_no_effect() {
    #[derive(Deserialize)]
    struct Case {
        evidence: InvocationEvidence,
        expected: EffectDisposition,
    }
    let cases: Vec<Case> =
        serde_json::from_str(include_str!("fixtures/turn_contract/effect_evidence.json")).unwrap();
    for case in cases {
        assert_eq!(case.evidence.disposition(), case.expected);
    }
}

#[test]
fn unknown_versions_fail_before_payload_interpretation_and_cannot_bypass_apply() {
    for version in [0, 1, 3, u32::MAX] {
        let value = json!({"schema_version": version, "event": {"kind": "future_unknown_event"}});
        assert!(
            matches!(TurnContractEnvelope::decode(&serde_json::to_vec(&value).unwrap()),
            Err(TurnContractError::UnsupportedVersion(actual)) if actual == version)
        );
        let mut envelope = decode(&fixture_scenarios()[0].steps[0].envelope);
        envelope.schema_version = version;
        let mut fold = TurnContract::default();
        assert!(
            matches!(fold.apply(&envelope), Err(TurnContractError::UnsupportedVersion(actual)) if actual == version)
        );
        assert_eq!(fold, TurnContract::default());
    }
}

#[test]
fn malformed_identity_and_unknown_event_fields_are_rejected() {
    let begin = fixture_scenarios()[0].steps[0].envelope.clone();
    for identity in [
        "",
        " ",
        "wrong/session",
        "../other",
        "has\ncontrol",
        &"x".repeat(129),
    ] {
        let mut value = begin.clone();
        value["session_id"] = json!(identity);
        assert!(TurnContractEnvelope::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    for field in ["event", "envelope"] {
        let mut value = begin.clone();
        if field == "event" {
            value["event"]["silently_ignored_authority"] = json!(true);
        } else {
            value["silently_ignored_authority"] = json!(true);
        }
        assert!(TurnContractEnvelope::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    }
}

#[test]
fn command_conflicts_stale_revisions_and_cross_turn_references_are_inert() {
    let scenario = &fixture_scenarios()[0];
    let mut fold = TurnContract::default();
    let begin = decode(&scenario.steps[0].envelope);
    fold.apply(&begin).unwrap();
    let before = fold.clone();
    let mut conflict = begin.clone();
    conflict.expected_revision = fold.revision();
    assert!(matches!(
        fold.apply(&conflict),
        Err(TurnContractError::CommandConflict)
    ));
    let mut stale = decode(&scenario.steps[1].envelope);
    stale.expected_revision = 0;
    assert!(matches!(
        fold.apply(&stale),
        Err(TurnContractError::StaleRevision { .. })
    ));
    for key in ["session_id", "turn_id", "execution_epoch_id"] {
        let mut value = scenario.steps[1].envelope.clone();
        value["event"]["input"]["activation"][key] = json!("other-owner");
        assert!(fold.apply(&decode(&value)).is_err());
    }
    assert_eq!(fold, before);
}

#[test]
fn every_closure_is_immutable_but_exact_prior_receipts_remain_replayable() {
    let scenario = &fixture_scenarios()[0];
    for closure in ["completed", "cancelled", "finished"] {
        let mut fold = TurnContract::default();
        let mut begin = scenario.steps[0].envelope.clone();
        begin["event"]["graph"]["nodes"][1]["required"] = json!(false);
        fold.apply(&decode(&begin)).unwrap();
        for step in &scenario.steps[1..3] {
            fold.apply(&decode(&step.envelope)).unwrap();
        }
        let mut close = scenario.steps[0].envelope.clone();
        close["command_id"] = json!("close");
        close["expected_revision"] = json!(fold.revision());
        close["event"] = json!({"kind":"close", "closure":closure});
        fold.apply(&decode(&close)).unwrap();
        let before = fold.clone();
        for event in [
            fixture_scenarios()[1].steps[5].envelope["event"].clone(),
            begin["event"].clone(),
        ] {
            let mut next = close.clone();
            next["command_id"] = json!("new-command");
            next["expected_revision"] = json!(fold.revision());
            next["event"] = event;
            assert!(matches!(
                fold.apply(&decode(&next)),
                Err(TurnContractError::InvalidTransition(
                    "closed turns are immutable"
                ))
            ));
        }
        assert!(!fold.apply(&decode(&begin)).unwrap());
        assert_eq!(fold, before);
    }
}

#[test]
fn successor_keeps_an_exact_closed_reference_without_reopening_history() {
    let scenario = &fixture_scenarios()[0];
    let mut prior = TurnContract::default();
    assert!(prior.closed_reference().is_err());
    for step in &scenario.steps {
        if step.error.is_none() {
            prior.apply(&decode(&step.envelope)).unwrap();
        }
    }
    let before = prior.clone();
    let reference = prior.closed_reference().unwrap();
    let mut successor = scenario.steps[0].envelope.clone();
    successor["turn_id"] = json!("successor-turn");
    successor["event"]["predecessor"] = serde_json::to_value(&reference).unwrap();
    let mut next = TurnContract::default();
    next.apply(&decode(&successor)).unwrap();
    assert_eq!(next.predecessor(), Some(&reference));
    assert_eq!(next.state(), Some(LogicalTurnState::Running));
    assert_eq!(prior, before);
    for (field, value) in [
        ("turn_id", json!("successor-turn")),
        ("session_id", json!("another-session")),
        ("closure_revision", json!(0)),
    ] {
        let mut invalid = successor.clone();
        invalid["event"]["predecessor"][field] = value;
        let mut rejected = TurnContract::default();
        assert!(rejected.apply(&decode(&invalid)).is_err());
        assert_eq!(rejected, TurnContract::default());
    }
}

#[test]
fn proven_pre_dispatch_cancellation_can_clear_unknown_without_inventing_outcome() {
    let scenario = &fixture_scenarios()[1];
    let mut fold = TurnContract::default();
    for step in &scenario.steps[..5] {
        if step.error.is_none() {
            fold.apply(&decode(&step.envelope)).unwrap();
        }
    }
    assert_eq!(
        fold.invocations()[0].evidence.disposition(),
        EffectDisposition::OutcomeUnknown
    );
    let mut proof = scenario.steps[5].envelope.clone();
    proof["event"] = json!({
        "kind":"prove_not_dispatched", "invocation_id":"invocation-a",
        "evidence":"durable-pre-dispatch-cancellation",
    });
    fold.apply(&decode(&proof)).unwrap();
    assert_eq!(
        fold.invocations()[0].evidence.disposition(),
        EffectDisposition::NotDispatched
    );
    let mut continuation = scenario.steps[5].envelope.clone();
    continuation["command_id"] = json!("continue-after-proof");
    continuation["expected_revision"] = json!(fold.revision());
    fold.apply(&decode(&continuation)).unwrap();
    assert_eq!(fold.state(), Some(LogicalTurnState::Running));
}

fn interrupted_fold_and_plan() -> (TurnContract, Value) {
    let scenario = &fixture_scenarios()[0];
    let mut fold = TurnContract::default();
    for step in &scenario.steps[..5] {
        fold.apply(&decode(&step.envelope)).unwrap();
    }
    (fold, scenario.steps[7].envelope.clone())
}

#[test]
fn atomic_continuation_preserves_inputs_and_rejects_incomplete_or_changed_plans() {
    let (original, continuation) = interrupted_fold_and_plan();
    for mutation in [
        "omit_accepted",
        "omit_retry",
        "duplicate",
        "source_epoch",
        "previous_generation",
        "starting_savepoint",
        "parents",
        "guidance",
        "attachments",
        "repository",
        "definition",
        "budget",
        "grant",
        "conversation_id",
        "manifest_id",
        "generation",
        "activation_owner",
    ] {
        let mut invalid = continuation.clone();
        let plan = &mut invalid["event"]["plan"];
        match mutation {
            "omit_accepted" => {
                plan["selections"].as_array_mut().unwrap().remove(0);
            }
            "omit_retry" => {
                plan["selections"].as_array_mut().unwrap().remove(1);
            }
            "duplicate" => {
                let repeated = plan["selections"][0].clone();
                plan["selections"].as_array_mut().unwrap().push(repeated);
            }
            "source_epoch" => plan["source_epoch_id"] = json!("stale-epoch"),
            "previous_generation" => plan["selections"][1]["previous"]["generation"] = json!(99),
            "starting_savepoint" => {
                plan["selections"][1]["input"]["starting_savepoint"] = json!({"kind":"empty"})
            }
            "parents" | "guidance" | "attachments" => {
                plan["selections"][1]["input"][mutation] = json!([])
            }
            "repository" => {
                plan["selections"][1]["input"][mutation] = json!({"kind":"unavailable"})
            }
            "definition" => {
                plan["selections"][1]["input"][mutation]["snapshot"] = json!("changed-definition")
            }
            "budget" => plan["selections"][1]["input"][mutation] = json!("reset-budget"),
            "grant" => plan["selections"][1]["input"][mutation]["revision"] = json!(2),
            "conversation_id" => {
                plan["selections"][1]["input"][mutation] = json!("new-conversation")
            }
            "manifest_id" => plan["selections"][1]["input"][mutation] = json!("input-node-b-1"),
            "generation" => plan["selections"][1]["input"]["activation"][mutation] = json!(1),
            "activation_owner" => {
                plan["selections"][1]["input"]["activation"]["turn_id"] = json!("other-turn")
            }
            _ => unreachable!(),
        }
        let mut rejected = original.clone();
        assert!(
            rejected.apply(&decode(&invalid)).is_err(),
            "accepted mutation {mutation}"
        );
        assert_eq!(rejected, original, "partial recovery mutation: {mutation}");
    }
    let mut continued = original.clone();
    continued.apply(&decode(&continuation)).unwrap();
    assert_eq!(continued.revision(), original.revision() + 1);
    assert_eq!(continued.activations()[0], original.activations()[0]);
    assert_eq!(continued.activations()[1], original.activations()[1]);
    assert_eq!(continued.activations()[2].state, ActivationState::Unstarted);
    assert_eq!(
        continued.activations()[2].input.starting_savepoint,
        original.activations()[1].input.starting_savepoint
    );
    assert_eq!(
        continued.activations()[2].input.parents,
        original.activations()[1].input.parents
    );
    assert!(continued.epochs().last().unwrap().continuation.is_some());
}

#[test]
fn failed_later_retry_rolls_back_earlier_preparation_and_epoch_in_same_event() {
    let scenario = &fixture_scenarios()[4];
    let mut original = TurnContract::default();
    for step in &scenario.steps[..7] {
        original.apply(&decode(&step.envelope)).unwrap();
    }
    let mut plan = scenario.steps[7].envelope.clone();
    let source = &scenario.steps[3].envelope["event"]["input"];
    let mut retry_b = source.clone();
    retry_b["manifest_id"] = json!("input-node-b-2");
    retry_b["activation"]["generation"] = json!(2);
    retry_b["activation"]["activation_id"] = json!("node-b-2");
    retry_b["activation"]["execution_epoch_id"] = json!("epoch-2");
    plan["event"]["plan"]["selections"][1] = json!({
        "kind":"retry", "previous":source["activation"], "input":retry_b,
    });
    // Both inputs pass the initial plan check. The later generation is rejected
    // during admission, after the first was prepared on the transactional clone.
    plan["event"]["plan"]["selections"][2]["input"]["activation"]["generation"] = json!(99);
    let mut rejected = original.clone();
    assert!(rejected.apply(&decode(&plan)).is_err());
    assert_eq!(rejected, original);
    plan["event"]["plan"]["selections"][2]["input"]["activation"]["generation"] = json!(2);
    rejected.apply(&decode(&plan)).unwrap();
    assert_eq!(
        rejected.activations().len(),
        original.activations().len() + 2
    );
    assert!(rejected.activations()[3..]
        .iter()
        .all(|item| item.state == ActivationState::Unstarted));
}

#[test]
fn epoch_only_continuation_and_implicit_cross_epoch_retries_are_rejected() {
    let (original, mut value) = interrupted_fold_and_plan();
    value["event"] = json!({"kind":"continue","epoch_id":"epoch-2"});
    assert!(TurnContractEnvelope::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    let scenario = &fixture_scenarios()[4];
    let mut fold = TurnContract::default();
    for step in &scenario.steps {
        fold.apply(&decode(&step.envelope)).unwrap();
    }
    let mut bypass = scenario.steps[3].envelope.clone();
    bypass["command_id"] = json!("bypass-blocked-selection");
    bypass["expected_revision"] = json!(fold.revision());
    bypass["event"]["input"]["manifest_id"] = json!("bypassed-input");
    bypass["event"]["input"]["activation"]["activation_id"] = json!("bypassed-activation");
    bypass["event"]["input"]["activation"]["execution_epoch_id"] = json!("epoch-2");
    bypass["event"]["input"]["activation"]["generation"] = json!(2);
    let before = fold.clone();
    assert!(matches!(
        fold.apply(&decode(&bypass)),
        Err(TurnContractError::InvalidTransition(
            "recovery explicitly left this node blocked"
        ))
    ));
    assert_eq!(fold, before);
    assert_eq!(original.state(), Some(LogicalTurnState::NeedsAttention));
}

#[test]
fn checkpoint_and_parent_references_cannot_forge_accepted_state() {
    let scenario = &fixture_scenarios()[0];
    let mut fold = TurnContract::default();
    for step in &scenario.steps[..3] {
        fold.apply(&decode(&step.envelope)).unwrap();
    }
    let before = fold.clone();
    for field in ["output", "checkpoint", "generation"] {
        let mut child = scenario.steps[3].envelope.clone();
        let parent = &mut child["event"]["input"]["parents"][0];
        match field {
            "output" => parent["output"] = json!("not-accepted-output"),
            "checkpoint" => parent["checkpoint"]["checkpoint_id"] = json!("private-candidate"),
            "generation" => parent["activation"]["generation"] = json!(99),
            _ => unreachable!(),
        }
        assert!(fold.apply(&decode(&child)).is_err());
        assert_eq!(fold, before);
    }
    let mut alias = scenario.steps[3].envelope.clone();
    alias["event"]["input"]["starting_savepoint"]["checkpoint"]["checkpoint_id"] =
        json!("baseline-node-a");
    assert!(fold.apply(&decode(&alias)).is_err());
    let mut forged = scenario.steps[3].envelope.clone();
    forged["event"]["input"]["starting_savepoint"]["checkpoint"] =
        scenario.steps[2].envelope["event"]["checkpoint"].clone();
    assert!(fold.apply(&decode(&forged)).is_err());
    assert_eq!(fold, before);
}

#[test]
fn input_and_envelope_limits_reject_before_state_mutation() {
    use axocoatl_session::turn_contract::{MAX_CONTRACT_ENVELOPE_BYTES, MAX_INPUT_REFERENCES};
    assert!(matches!(
        TurnContractEnvelope::decode(&vec![b' '; MAX_CONTRACT_ENVELOPE_BYTES + 1]),
        Err(TurnContractError::LimitExceeded("envelope bytes"))
    ));
    let scenario = &fixture_scenarios()[0];
    let mut fold = TurnContract::default();
    fold.apply(&decode(&scenario.steps[0].envelope)).unwrap();
    let before = fold.clone();
    let mut oversized = scenario.steps[1].envelope.clone();
    oversized["event"]["input"]["guidance"] = json!((0..=MAX_INPUT_REFERENCES)
        .map(|i| format!("guidance-{i}"))
        .collect::<Vec<_>>());
    assert!(matches!(
        TurnContractEnvelope::decode(&serde_json::to_vec(&oversized).unwrap()),
        Err(TurnContractError::LimitExceeded("input references"))
    ));
    let directly_deserialized: TurnContractEnvelope = serde_json::from_value(oversized).unwrap();
    assert!(matches!(
        fold.apply(&directly_deserialized),
        Err(TurnContractError::LimitExceeded("input references"))
    ));
    assert_eq!(fold, before);
}

#[test]
fn immutable_input_requires_savepoint_snapshot_and_unambiguous_evidence() {
    let scenario = &fixture_scenarios()[0];
    let mut fold = TurnContract::default();
    fold.apply(&decode(&scenario.steps[0].envelope)).unwrap();
    let before = fold.clone();
    for field in [
        "starting_savepoint",
        "definition",
        "repository",
        "budget",
        "parents",
    ] {
        let mut absent = scenario.steps[1].envelope.clone();
        absent["event"]["input"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert!(
            TurnContractEnvelope::decode(&serde_json::to_vec(&absent).unwrap()).is_err(),
            "missing {field}"
        );
    }
    for mutation in ["duplicate_guidance", "zero_grant", "private_candidate"] {
        let mut invalid = scenario.steps[1].envelope.clone();
        let input = &mut invalid["event"]["input"];
        match mutation {
            "duplicate_guidance" => input["guidance"] = json!(["same-reference", "same-reference"]),
            "zero_grant" => input["grant"]["revision"] = json!(0),
            "private_candidate" => {
                input["starting_savepoint"]["checkpoint"]["source"] = json!({
                    "kind":"accepted", "activation":input["activation"],
                });
            }
            _ => unreachable!(),
        }
        assert!(
            fold.apply(&decode(&invalid)).is_err(),
            "accepted {mutation}"
        );
        assert_eq!(fold, before);
    }
}

#[test]
fn graph_capacity_and_undeclared_nodes_reject_without_losing_existing_work() {
    use axocoatl_session::turn_contract::MAX_CONTRACT_NODES;
    let scenario = &fixture_scenarios()[0];
    let mut begin = scenario.steps[0].envelope.clone();
    let node = begin["event"]["graph"]["nodes"][0].clone();
    begin["event"]["graph"]["nodes"] = json!(vec![node; MAX_CONTRACT_NODES + 1]);
    assert!(matches!(
        TurnContractEnvelope::decode(&serde_json::to_vec(&begin).unwrap()),
        Err(TurnContractError::LimitExceeded("graph declarations"))
    ));
    let mut fold = TurnContract::default();
    fold.apply(&decode(&scenario.steps[0].envelope)).unwrap();
    let before = fold.clone();
    let mut start = scenario.steps[1].envelope.clone();
    start["event"]["input"]["activation"]["node_id"] = json!("undeclared-node");
    assert!(fold.apply(&decode(&start)).is_err());
    assert_eq!(fold, before);
}

#[test]
fn retry_can_bind_a_new_narrowed_grant_snapshot_without_resetting_inputs_or_budget() {
    let (original, continuation) = interrupted_fold_and_plan();
    let mut narrowed = continuation.clone();
    narrowed["event"]["plan"]["selections"][1]["input"]["grant"] = json!({
        "grant_id":"supervisor-grant", "revision":2, "evidence":"narrowed-grant-snapshot-2",
    });
    let mut continued = original.clone();
    continued.apply(&decode(&narrowed)).unwrap();
    assert_eq!(
        continued.activations()[2]
            .input
            .grant
            .as_ref()
            .unwrap()
            .revision,
        2
    );
    assert_eq!(
        continued.activations()[2].input.budget,
        original.activations()[1].input.budget
    );
    assert_eq!(
        continued.activations()[2].input.starting_savepoint,
        original.activations()[1].input.starting_savepoint
    );
    for grant in [
        json!(null),
        json!({"grant_id":"new-grant", "revision":2, "evidence":"expanded-authority"}),
        json!({"grant_id":"supervisor-grant", "revision":2, "evidence":"grant-snapshot-1"}),
        json!({"grant_id":"supervisor-grant", "revision":1, "evidence":"changed-under-same-revision"}),
    ] {
        let mut invalid = continuation.clone();
        invalid["event"]["plan"]["selections"][1]["input"]["grant"] = grant;
        let mut rejected = original.clone();
        assert!(rejected.apply(&decode(&invalid)).is_err());
        assert_eq!(rejected, original);
    }
}

#[test]
fn canonical_graph_rejects_cycles_aliases_dangling_edges_and_ambiguous_conditions() {
    let begin = fixture_scenarios()[0].steps[0].envelope.clone();
    for mutation in [
        "cycle",
        "dangling",
        "duplicate_edge",
        "duplicate_node",
        "slot_alias",
        "conversation_alias",
        "revision",
        "savepoint_owner",
        "check_scope",
        "no_requirements",
    ] {
        let mut invalid = begin.clone();
        let graph = &mut invalid["event"]["graph"];
        match mutation {
            "cycle" => graph["dependencies"]
                .as_array_mut()
                .unwrap()
                .push(json!({"parent":"node-b","child":"node-a"})),
            "dangling" => graph["dependencies"][0]["parent"] = json!("missing"),
            "duplicate_edge" => {
                let edge = graph["dependencies"][0].clone();
                graph["dependencies"].as_array_mut().unwrap().push(edge);
            }
            "duplicate_node" => {
                let node = graph["nodes"][0].clone();
                graph["nodes"].as_array_mut().unwrap().push(node);
            }
            "slot_alias" => graph["nodes"][1]["slot_id"] = graph["nodes"][0]["slot_id"].clone(),
            "conversation_alias" => {
                graph["nodes"][1]["conversation_id"] = graph["nodes"][0]["conversation_id"].clone()
            }
            "revision" => graph["revision"] = json!(2),
            "savepoint_owner" => {
                graph["nodes"][0]["starting_savepoint"]["checkpoint"]["session_id"] =
                    json!("other-session")
            }
            "check_scope" => {
                graph["conditions"] = json!([{"condition_id":"check", "kind":{"kind":"review","criterion":"rubric"},"nodes":["missing"]}])
            }
            "no_requirements" => {
                for node in graph["nodes"].as_array_mut().unwrap() {
                    node["required"] = json!(false);
                }
            }
            _ => unreachable!(),
        }
        let mut fold = TurnContract::default();
        assert!(
            fold.apply(&decode(&invalid)).is_err(),
            "accepted {mutation}"
        );
        assert_eq!(fold, TurnContract::default());
    }
}

#[test]
fn exact_direct_parent_set_is_required_even_when_other_accepted_nodes_exist() {
    let scenario = &fixture_scenarios()[0];
    let mut fold = TurnContract::default();
    for step in &scenario.steps[..3] {
        fold.apply(&decode(&step.envelope)).unwrap();
    }
    let before = fold.clone();
    let mut missing = scenario.steps[3].envelope.clone();
    missing["event"]["input"]["parents"] = json!([]);
    assert!(fold.apply(&decode(&missing)).is_err());
    let mut duplicated = scenario.steps[3].envelope.clone();
    let parent = duplicated["event"]["input"]["parents"][0].clone();
    duplicated["event"]["input"]["parents"]
        .as_array_mut()
        .unwrap()
        .push(parent);
    assert!(fold.apply(&decode(&duplicated)).is_err());
    assert_eq!(fold, before);
}

#[test]
fn invalidation_preserves_historical_outputs_but_removes_promotion_and_check_eligibility() {
    let scenario = &fixture_scenarios()[5];
    let mut fold = TurnContract::default();
    let condition = axocoatl_session::turn_contract::ConditionId::new("repository-check").unwrap();
    let mut saw_revision = false;
    for step in &scenario.steps {
        if step.error.is_some() {
            continue;
        }
        fold.apply(&decode(&step.envelope)).unwrap();
        if step.envelope["event"]["kind"] == "revise_accepted" {
            saw_revision = true;
            assert!(fold.current_accepted_activations().is_empty());
            assert!(fold.current_condition(&condition).is_none());
            assert!(!fold.completion_satisfied());
            for old in &fold.activations()[..3] {
                assert_eq!(old.state, ActivationState::Superseded);
                assert!(old.checkpoint.is_some());
                assert!(old.output.is_some());
            }
        }
    }
    assert!(saw_revision);
    assert!(fold.condition_satisfied(&condition));
    assert!(fold.completion_satisfied());
    assert_eq!(fold.current_accepted_activations().len(), 3);
    assert!(fold
        .current_accepted_activations()
        .iter()
        .all(|item| item.activation.generation == 2));
    let closed = fold.closed_reference().unwrap();
    assert_eq!(closed.session_id().as_str(), "session-a");
    assert_eq!(closed.turn_id().as_str(), "turn-a");
    assert_eq!(closed.closure_revision(), fold.revision());
}

#[test]
fn revision_waits_for_running_descendants_and_unknown_effects_without_partial_invalidation() {
    let scenario = &fixture_scenarios()[0];
    let mut fold = TurnContract::default();
    for step in &scenario.steps[..4] {
        fold.apply(&decode(&step.envelope)).unwrap();
    }
    let parent = scenario.steps[1].envelope["event"]["input"].clone();
    let child = scenario.steps[3].envelope["event"]["input"]["activation"].clone();
    let mut revised = parent.clone();
    revised["manifest_id"] = json!("revised-parent-input");
    revised["activation"]["activation_id"] = json!("revised-parent");
    revised["activation"]["generation"] = json!(2);
    revised["guidance"] = json!(["new-guidance"]);
    let mut envelope = scenario.steps[0].envelope.clone();
    envelope["command_id"] = json!("revise-parent");
    envelope["expected_revision"] = json!(fold.revision());
    envelope["event"] = json!({"kind":"revise_accepted", "previous":parent["activation"], "input":revised,
        "invalidated_descendants":[child], "evidence":"user-revision"});
    let before = fold.clone();
    assert!(fold.apply(&decode(&envelope)).is_err());
    assert_eq!(fold, before);
    let mut intent = envelope.clone();
    intent["command_id"] = json!("child-intent");
    intent["event"] =
        json!({"kind":"record_intent", "invocation_id":"child-tool", "activation":child});
    fold.apply(&decode(&intent)).unwrap();
    let mut failed = envelope.clone();
    failed["command_id"] = json!("child-failed");
    failed["expected_revision"] = json!(fold.revision());
    failed["event"] =
        json!({"kind":"fail_activation", "activation":child, "evidence":"executor-failed"});
    fold.apply(&decode(&failed)).unwrap();
    envelope["expected_revision"] = json!(fold.revision());
    let before = fold.clone();
    assert!(fold.apply(&decode(&envelope)).is_err());
    assert_eq!(fold, before);
    let mut outcome = envelope.clone();
    outcome["command_id"] = json!("settled-child-tool");
    outcome["event"] = json!({"kind":"record_outcome", "invocation_id":"child-tool", "outcome":"failed", "evidence":"authoritative-failure"});
    fold.apply(&decode(&outcome)).unwrap();
    envelope["expected_revision"] = json!(fold.revision());
    fold.apply(&decode(&envelope)).unwrap();
    assert_eq!(fold.activations()[0].state, ActivationState::Superseded);
    assert_eq!(fold.activations()[1].state, ActivationState::Superseded);
    assert_eq!(fold.activations()[2].state, ActivationState::Unstarted);
}

#[test]
fn check_only_recovery_renews_observation_capacity_without_replaying_accepted_nodes() {
    let scenario = &fixture_scenarios()[7];
    let condition = axocoatl_session::turn_contract::ConditionId::new("review").unwrap();
    let mut fold = TurnContract::default();
    for step in &scenario.steps {
        if step.error.is_some() {
            continue;
        }
        fold.apply(&decode(&step.envelope)).unwrap();
        if step.envelope["event"]["kind"] == "continue" {
            assert!(fold.current_condition(&condition).is_none());
            assert_eq!(fold.activations().len(), 1);
            assert_eq!(fold.activations()[0].state, ActivationState::Accepted);
            assert_eq!(fold.activations()[0].activation.generation, 1);
        }
    }
    assert!(fold.condition_satisfied(&condition));
    assert_eq!(
        fold.current_condition(&condition)
            .unwrap()
            .epoch_id
            .as_str(),
        "epoch-2"
    );
}

#[test]
fn rebasing_a_revised_descendant_drops_obsolete_answer_context_but_retains_history() {
    fn envelope(fold: &TurnContract, event: Value) -> TurnContractEnvelope {
        decode(
            &json!({"schema_version":2, "command_id":format!("context-command-{}",fold.revision()),
            "expected_revision":fold.revision(),"session_id":"session-a","turn_id":"turn-a","event":event}),
        )
    }
    fn next_input(old: &Value, generation: u32) -> Value {
        let mut next = old.clone();
        let node = old["activation"]["node_id"].as_str().unwrap();
        next["manifest_id"] = json!(format!("context-input-{node}-{generation}"));
        next["activation"]["activation_id"] =
            json!(format!("context-activation-{node}-{generation}"));
        next["activation"]["generation"] = json!(generation);
        next
    }
    fn accept_prepared(fold: &mut TurnContract, input: &Value) {
        fold.apply(&envelope(
            fold,
            json!({"kind":"start_prepared_activation","activation":input["activation"]}),
        ))
        .unwrap();
        let id = input["activation"]["activation_id"].as_str().unwrap();
        fold.apply(&envelope(fold,json!({"kind":"accept_activation","activation":input["activation"],
            "checkpoint":{"checkpoint_id":format!("checkpoint-{id}"),"session_id":"session-a", "conversation_id":input["conversation_id"],
                "source":{"kind":"accepted","activation":input["activation"]}},"output":format!("output-{id}")}))).unwrap();
    }
    let scenario = &fixture_scenarios()[5];
    let mut fold = TurnContract::default();
    for step in &scenario.steps {
        if step.envelope["event"]["kind"] == "record_condition" {
            break;
        }
        if step.error.is_none() {
            fold.apply(&decode(&step.envelope)).unwrap();
        }
    }
    let a1 = serde_json::to_value(&fold.activations()[0].input).unwrap();
    let b1 = serde_json::to_value(&fold.activations()[1].input).unwrap();
    let c1 = serde_json::to_value(&fold.activations()[2].activation).unwrap();
    let mut b2 = next_input(&b1, 2);
    b2["guidance"] = json!(["child-specific-revision"]);
    b2["revision_context"] = json!({"activation":b1["activation"],"output":"output-b-1"});
    fold.apply(&envelope(
        &fold,
        json!({"kind":"revise_accepted","previous":b1["activation"],"input":b2,
        "invalidated_descendants":[c1],"evidence":"revise-child"}),
    ))
    .unwrap();
    accept_prepared(&mut fold, &b2);
    let mut a2 = next_input(&a1, 2);
    a2["guidance"] = json!(["parent-specific-revision"]);
    fold.apply(&envelope(
        &fold,
        json!({"kind":"revise_accepted","previous":a1["activation"],"input":a2,
        "invalidated_descendants":[b2["activation"],c1],"evidence":"revise-parent"}),
    ))
    .unwrap();
    accept_prepared(&mut fold, &a2);
    let accepted_a = fold
        .current_accepted_activations()
        .into_iter()
        .find(|item| item.activation.node_id.as_str() == "a")
        .unwrap();
    let mut b3 = next_input(&b2, 3);
    b3["parents"] = json!([{"activation":accepted_a.activation,"checkpoint":accepted_a.checkpoint,"output":accepted_a.output}]);
    let before = fold.clone();
    assert!(fold
        .apply(&envelope(
            &fold,
            json!({"kind":"rebase_activation","previous":b2["activation"],"input":b3})
        ))
        .is_err());
    assert_eq!(fold, before);
    b3.as_object_mut().unwrap().remove("revision_context");
    fold.apply(&envelope(
        &fold,
        json!({"kind":"rebase_activation","previous":b2["activation"],"input":b3}),
    ))
    .unwrap();
    let rebased = fold.activations().last().unwrap();
    assert!(rebased.input.revision_context.is_none());
    assert_eq!(
        rebased.input.guidance[0].as_str(),
        "child-specific-revision"
    );
    assert_eq!(
        serde_json::to_value(&rebased.input.starting_savepoint).unwrap(),
        b1["starting_savepoint"]
    );
    assert!(fold
        .activations()
        .iter()
        .find(|item| item.activation.generation == 2 && item.activation.node_id.as_str() == "b")
        .unwrap()
        .input
        .revision_context
        .is_some());
}

fn condition_ready_fold() -> (TurnContract, Value) {
    let scenario = &fixture_scenarios()[7];
    let mut fold = TurnContract::default();
    for step in &scenario.steps[..3] {
        fold.apply(&decode(&step.envelope)).unwrap();
    }
    let run = json!({
        "session_id":"session-a", "turn_id":"turn-a", "epoch_id":"epoch-1",
        "condition_id":"review", "run_id":"check-run-1",
        "activations":[scenario.steps[1].envelope["event"]["input"]["activation"].clone()]
    });
    (fold, run)
}

fn condition_event(fold: &TurnContract, id: &str, event: Value) -> TurnContractEnvelope {
    decode(&json!({
        "schema_version":2, "command_id":id, "expected_revision":fold.revision(),
        "session_id":"session-a", "turn_id":"turn-a", "event":event
    }))
}

#[test]
fn condition_intent_requires_exact_live_accepted_scope_and_immutable_run_identity() {
    let (mut fold, run) = condition_ready_fold();
    let mut unaccepted = TurnContract::default();
    for step in &fixture_scenarios()[7].steps[..2] {
        unaccepted.apply(&decode(&step.envelope)).unwrap();
    }
    let early = condition_event(
        &unaccepted,
        "early",
        json!({
            "kind":"record_condition_intent", "run":run, "intent":"protected-arguments"
        }),
    );
    assert!(unaccepted.apply(&early).is_err());
    let mut paused = fold.clone();
    let pause = condition_event(
        &paused,
        "pause",
        json!({"kind":"pause_epoch", "epoch_id":"epoch-1"}),
    );
    paused.apply(&pause).unwrap();
    let late_admission = condition_event(
        &paused,
        "late-admission",
        json!({
            "kind":"record_condition_intent", "run":run, "intent":"protected-arguments"
        }),
    );
    assert!(paused.apply(&late_admission).is_err());
    let mut malformed = Vec::new();
    for field in ["session_id", "turn_id", "epoch_id", "condition_id"] {
        let mut changed = run.clone();
        changed[field] = json!("foreign");
        malformed.push(changed);
    }
    for field in [
        "session_id",
        "turn_id",
        "execution_epoch_id",
        "activation_id",
        "node_id",
    ] {
        let mut changed = run.clone();
        changed["activations"][0][field] = json!("foreign");
        malformed.push(changed);
    }
    let mut empty = run.clone();
    empty["activations"] = json!([]);
    malformed.push(empty);
    let mut duplicated = run.clone();
    duplicated["activations"]
        .as_array_mut()
        .unwrap()
        .push(run["activations"][0].clone());
    malformed.push(duplicated);
    for bad in malformed {
        let before = fold.clone();
        let event = condition_event(
            &fold,
            "bad-intent",
            json!({
                "kind":"record_condition_intent", "run":bad, "intent":"protected-arguments"
            }),
        );
        assert!(fold.apply(&event).is_err());
        assert_eq!(fold, before);
    }
    let intent = condition_event(
        &fold,
        "intent",
        json!({
            "kind":"record_condition_intent", "run":run, "intent":"protected-arguments"
        }),
    );
    assert!(fold.apply(&intent).unwrap());
    assert!(fold.has_unknown_effects());
    assert!(!fold.apply(&intent).unwrap());
    let before = fold.clone();
    let mut conflict = intent.clone();
    conflict.expected_revision = fold.revision();
    assert!(matches!(
        fold.apply(&conflict),
        Err(TurnContractError::CommandConflict)
    ));
    let duplicate = condition_event(
        &fold,
        "another-intent-command",
        json!({
            "kind":"record_condition_intent", "run":run, "intent":"changed-arguments"
        }),
    );
    assert!(fold.apply(&duplicate).is_err());
    let mut concurrent = run.clone();
    concurrent["run_id"] = json!("check-run-2");
    let event = condition_event(
        &fold,
        "concurrent",
        json!({
            "kind":"record_condition_intent", "run":concurrent, "intent":"other-arguments"
        }),
    );
    assert!(fold.apply(&event).is_err());
    assert_eq!(fold, before);
}

#[test]
fn condition_unknown_blocks_observation_completion_revision_and_continuation() {
    let (mut fold, run) = condition_ready_fold();
    let intent = condition_event(
        &fold,
        "intent",
        json!({
            "kind":"record_condition_intent", "run":run, "intent":"protected-arguments"
        }),
    );
    fold.apply(&intent).unwrap();
    let mut revision_input = fixture_scenarios()[7].steps[1].envelope["event"]["input"].clone();
    revision_input["manifest_id"] = json!("input-a-2");
    revision_input["activation"]["activation_id"] = json!("a-2");
    revision_input["activation"]["generation"] = json!(2);
    revision_input["guidance"] = json!(["new-guidance"]);
    for event in [
        json!({"kind":"record_condition", "epoch_id":"epoch-1", "condition_id":"review", "activations":run["activations"], "outcome":"passed", "evidence":"unsupported-verdict"}),
        json!({"kind":"close", "closure":"completed"}),
        json!({"kind":"revise_accepted", "previous":run["activations"][0], "input":revision_input, "invalidated_descendants":[], "evidence":"revision-request"}),
    ] {
        let before = fold.clone();
        let envelope = condition_event(&fold, "refused", event);
        assert!(fold.apply(&envelope).is_err());
        assert_eq!(fold, before);
    }
    let interrupt = condition_event(
        &fold,
        "interrupt",
        json!({"kind":"interrupt_epoch", "epoch_id":"epoch-1"}),
    );
    fold.apply(&interrupt).unwrap();
    let continuation = json!({
        "kind":"continue", "plan":{
            "source_epoch_id":"epoch-1", "epoch_id":"epoch-2",
            "selections":[{"kind":"retain_accepted", "activation":run["activations"][0]}],
            "condition_runs":["review"]
        }
    });
    let event = condition_event(&fold, "continue", continuation.clone());
    assert!(fold.apply(&event).is_err());
    let resolve = condition_event(
        &fold,
        "resolve",
        json!({
            "kind":"resolve_condition_intent", "run_id":"check-run-1",
            "resolution":{"kind":"outcome_recorded", "evidence":"late-known-exit"}
        }),
    );
    fold.apply(&resolve).unwrap();
    assert!(!fold.has_unknown_effects());
    assert!(fold.conditions().is_empty());
    assert!(!fold.completion_satisfied());
    let event = condition_event(&fold, "continue", continuation);
    fold.apply(&event).unwrap();
    assert_eq!(fold.activations().len(), 1);
    assert_eq!(fold.activations()[0].activation.generation, 1);
    let mut next_run = run.clone();
    next_run["run_id"] = json!("check-run-2");
    next_run["epoch_id"] = json!("epoch-2");
    let event = condition_event(
        &fold,
        "next-intent",
        json!({"kind":"record_condition_intent", "run":next_run, "intent":"new-protected-arguments"}),
    );
    fold.apply(&event).unwrap();
    let before = fold.clone();
    let stale = condition_event(
        &fold,
        "late-old-observation",
        json!({
            "kind":"record_condition", "epoch_id":"epoch-1", "condition_id":"review",
            "activations":run["activations"], "outcome":"passed", "evidence":"late-known-exit"
        }),
    );
    assert!(fold.apply(&stale).is_err());
    assert_eq!(fold, before);
    assert!(fold.has_unknown_effects());
}

#[test]
fn condition_resolution_is_not_a_verdict_and_fresh_failed_checks_require_explicit_recovery() {
    use axocoatl_session::turn_contract::ConditionRunId;
    for resolution in ["outcome_recorded", "not_dispatched"] {
        let (mut fold, run) = condition_ready_fold();
        let intent = condition_event(
            &fold,
            "intent",
            json!({"kind":"record_condition_intent", "run":run, "intent":"arguments"}),
        );
        fold.apply(&intent).unwrap();
        let settlement = condition_event(
            &fold,
            "settlement",
            json!({
                "kind":"resolve_condition_intent", "run_id":"check-run-1",
                "resolution":{"kind":resolution, "evidence":"executor-evidence"}
            }),
        );
        fold.apply(&settlement).unwrap();
        assert!(!fold.has_unknown_effects());
        assert!(fold.conditions().is_empty());
        assert!(!fold.completion_satisfied());
        let disposition = fold
            .condition_run(&ConditionRunId::new("check-run-1").unwrap())
            .unwrap()
            .disposition();
        assert_eq!(
            disposition,
            if resolution == "outcome_recorded" {
                EffectDisposition::OutcomeRecorded
            } else {
                EffectDisposition::NotDispatched
            }
        );
        assert!(!fold.apply(&settlement).unwrap());
        let conflicting = condition_event(
            &fold,
            "other-settlement",
            json!({
                "kind":"resolve_condition_intent", "run_id":"check-run-1",
                "resolution":{"kind":"outcome_recorded", "evidence":"different-evidence"}
            }),
        );
        assert!(fold.apply(&conflicting).is_err());
        if resolution == "outcome_recorded" {
            let mut successful = fold.clone();
            let passed = condition_event(
                &successful,
                "passed",
                json!({
                    "kind":"record_condition", "epoch_id":"epoch-1", "condition_id":"review",
                    "activations":run["activations"], "outcome":"passed", "evidence":"actual-passing-check"
                }),
            );
            successful.apply(&passed).unwrap();
            let close = condition_event(
                &successful,
                "complete",
                json!({"kind":"close", "closure":"completed"}),
            );
            successful.apply(&close).unwrap();
            assert_eq!(successful.state(), Some(LogicalTurnState::Completed));
            let observation = condition_event(
                &fold,
                "actual-verdict",
                json!({
                    "kind":"record_condition", "epoch_id":"epoch-1", "condition_id":"review",
                    "activations":run["activations"], "outcome":"failed", "evidence":"actual-failed-check"
                }),
            );
            fold.apply(&observation).unwrap();
            let mut next = run.clone();
            next["run_id"] = json!("check-run-2");
            let retry = condition_event(
                &fold,
                "implicit-retry",
                json!({"kind":"record_condition_intent", "run":next, "intent":"arguments"}),
            );
            assert!(fold.apply(&retry).is_err());
        }
    }
}

#[test]
fn condition_unknown_survives_serialized_prefixes_and_explicit_partial_closure() {
    for closure in ["cancelled", "finished"] {
        let scenario = &fixture_scenarios()[7];
        let (mut fold, run) = condition_ready_fold();
        let mut records = scenario.steps[..3]
            .iter()
            .map(|step| decode(&step.envelope))
            .collect::<Vec<_>>();
        let intent = condition_event(
            &fold,
            "intent",
            json!({"kind":"record_condition_intent", "run":run, "intent":"protected-arguments"}),
        );
        fold.apply(&intent).unwrap();
        records.push(intent.clone());
        let close = condition_event(&fold, "close", json!({"kind":"close", "closure":closure}));
        fold.apply(&close).unwrap();
        records.push(close);
        for cut in 0..=records.len() {
            let mut recovered = TurnContract::default();
            for record in &records[..cut] {
                let serialized = serde_json::to_vec(record).unwrap();
                recovered
                    .apply(&TurnContractEnvelope::decode(&serialized).unwrap())
                    .unwrap();
            }
            for record in &records[cut..] {
                recovered.apply(record).unwrap();
            }
            assert_eq!(recovered, fold);
        }
        assert!(fold.has_unknown_effects());
        let before = fold.clone();
        let late = condition_event(
            &fold,
            "late-result",
            json!({"kind":"resolve_condition_intent", "run_id":"check-run-1", "resolution":{"kind":"outcome_recorded", "evidence":"actual-late-outcome"}}),
        );
        assert!(fold.apply(&late).is_err());
        assert!(!fold.apply(&intent).unwrap());
        assert_eq!(fold, before);
    }
}

#[test]
fn oversized_condition_scope_and_unknown_resolution_fail_before_mutation() {
    let (mut fold, mut run) = condition_ready_fold();
    run["activations"] = json!(vec![
        run["activations"][0].clone();
        axocoatl_session::turn_contract::MAX_CONTRACT_NODES + 1
    ]);
    let oversized = json!({"schema_version":2, "command_id":"oversized", "expected_revision":fold.revision(), "session_id":"session-a", "turn_id":"turn-a", "event":{"kind":"record_condition_intent", "run":run, "intent":"arguments"}});
    assert!(TurnContractEnvelope::decode(&serde_json::to_vec(&oversized).unwrap()).is_err());
    let typed: TurnContractEnvelope = serde_json::from_value(oversized).unwrap();
    let before = fold.clone();
    assert!(fold.apply(&typed).is_err());
    let missing = condition_event(
        &fold,
        "missing",
        json!({"kind":"resolve_condition_intent", "run_id":"unknown", "resolution":{"kind":"not_dispatched", "evidence":"unbound-proof"}}),
    );
    assert!(fold.apply(&missing).is_err());
    assert_eq!(fold, before);
}

#[test]
fn explicit_check_continuation_refreshes_selected_capture_and_keeps_unselected_pass() {
    let fixture: Value = serde_json::from_str(include_str!(
        "fixtures/turn_contract/check_only_recovery_preserves_accepted_generations.json"
    ))
    .unwrap();
    let mut fold = TurnContract::default();
    let mut begin = fixture["steps"][0]["envelope"].clone();
    for id in ["capture", "unselected"] {
        let mut condition = begin["event"]["graph"]["conditions"][0].clone();
        condition["condition_id"] = json!(id);
        begin["event"]["graph"]["conditions"]
            .as_array_mut()
            .unwrap()
            .push(condition);
    }
    fold.apply(&decode(&begin)).unwrap();
    for index in 1..=3 {
        fold.apply(&decode(&fixture["steps"][index]["envelope"]))
            .unwrap();
    }
    for id in ["capture", "unselected"] {
        let mut pass = fixture["steps"][3]["envelope"].clone();
        pass["command_id"] = json!(format!("pass-{id}"));
        pass["expected_revision"] = json!(fold.revision());
        pass["event"]["condition_id"] = json!(id);
        pass["event"]["outcome"] = json!("passed");
        pass["event"]["evidence"] = json!(format!("evidence-{id}"));
        fold.apply(&decode(&pass)).unwrap();
    }
    let mut interrupt = fixture["steps"][4]["envelope"].clone();
    interrupt["expected_revision"] = json!(fold.revision());
    fold.apply(&decode(&interrupt)).unwrap();
    let mut resume = fixture["steps"][6]["envelope"].clone();
    resume["expected_revision"] = json!(fold.revision());
    resume["event"]["plan"]["condition_runs"] = json!(["review", "capture"]);
    fold.apply(&decode(&resume)).unwrap();
    assert!(fold
        .current_condition(&axocoatl_session::turn_contract::ConditionId::new("capture").unwrap())
        .is_none());
    assert!(fold.condition_satisfied(
        &axocoatl_session::turn_contract::ConditionId::new("unselected").unwrap()
    ));
    assert_eq!(
        fold.epochs()
            .last()
            .unwrap()
            .continuation
            .as_ref()
            .unwrap()
            .condition_runs
            .len(),
        2
    );
    assert_eq!(fold.activations().len(), 1);
    assert_eq!(fold.activations()[0].activation.generation, 1);
}

#[test]
fn revision_forecast_leaves_actual_effects_unknown_and_refuses_unrelated_intents() {
    use axocoatl_session::turn_contract::{CommandId, InvocationId, TurnContractEvent};
    let fixture: Value = serde_json::from_str(include_str!(
        "fixtures/turn_contract/graph_revision_invalidates_descendants_and_conditions.json"
    ))
    .unwrap();
    let mut contract = TurnContract::default();
    for index in [0, 2, 3, 4] {
        let mut envelope = decode(&fixture["steps"][index]["envelope"]);
        envelope.expected_revision = contract.revision();
        match &mut envelope.event {
            TurnContractEvent::Begin { graph, .. } => graph.dependencies.clear(),
            TurnContractEvent::StartActivation { input } => input.parents.clear(),
            _ => {}
        }
        contract.apply(&envelope).unwrap();
    }
    let source = contract.activations().last().unwrap().activation.clone();
    let own = InvocationId::new("current-control-tool").unwrap();
    let mut invocation = decode(&fixture["steps"][4]["envelope"]);
    invocation.command_id = CommandId::new("own-control-intent").unwrap();
    invocation.expected_revision = contract.revision();
    invocation.event = TurnContractEvent::RecordIntent {
        invocation_id: own.clone(),
        activation: source.clone(),
    };
    contract.apply(&invocation).unwrap();
    let mut revision = decode(&fixture["steps"][11]["envelope"]);
    revision.expected_revision = contract.revision();
    if let TurnContractEvent::ReviseAccepted {
        invalidated_descendants,
        ..
    } = &mut revision.event
    {
        invalidated_descendants.clear();
    }
    let before = contract.clone();
    contract
        .preview_revision_after_invocation_settles(&revision, &own)
        .unwrap();
    assert_eq!(
        contract, before,
        "forecast changes no invocation or accepted state"
    );
    assert!(
        contract.apply(&revision).is_err(),
        "real application still requires actual settlement"
    );
    invocation.command_id = CommandId::new("unrelated-tool-intent").unwrap();
    invocation.expected_revision = contract.revision();
    invocation.event = TurnContractEvent::RecordIntent {
        invocation_id: InvocationId::new("unknown-external-effect").unwrap(),
        activation: source,
    };
    contract.apply(&invocation).unwrap();
    revision.expected_revision = contract.revision();
    let before = contract.clone();
    assert!(contract
        .preview_revision_after_invocation_settles(&revision, &own)
        .is_err());
    assert_eq!(contract, before);
}
