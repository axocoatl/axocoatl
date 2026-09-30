use axocoatl_session::turn_contract::*;
use serde_json::{json, Value};

fn node(id: &str) -> Value {
    json!({"node_id":id,"slot_id":format!("slot-{id}"),"definition":{"definition_id":"qa","snapshot":"definition-v1"},
        "conversation_id":format!("conversation-{id}"),"starting_savepoint":{"kind":"empty"},"required":true})
}
fn graph() -> Value {
    json!({"snapshot_id":"graph-1","revision":1,"nodes":[node("a"),node("b")],
        "dependencies":[{"parent":"a","child":"b"}],
        "conditions":[{"condition_id":"check-b","kind":{"kind":"repository_check","definition":"check-definition"},"nodes":["b"]}]})
}
fn event(revision: u64, value: Value) -> TurnContractEnvelope {
    TurnContractEnvelope::decode(&serde_json::to_vec(&json!({"schema_version":2,
        "command_id":format!("event-{revision}"),"expected_revision":revision,"session_id":"session-a","turn_id":"turn-a","event":value})).unwrap()).unwrap()
}
fn initial() -> (TurnContract, TurnContractEnvelope) {
    let mut fold = TurnContract::default();
    let begin = event(
        0,
        json!({"kind":"begin","epoch_id":"epoch-1","graph":graph()}),
    );
    fold.apply(&begin).unwrap();
    (fold, begin)
}
fn activation() -> Value {
    json!({"session_id":"session-a","turn_id":"turn-a","execution_epoch_id":"epoch-1","node_id":"a","generation":1,"activation_id":"activation-a-1"})
}
fn start() -> Value {
    json!({"kind":"start_activation","input":{"manifest_id":"input-a-1","activation":activation(),
        "definition":{"definition_id":"qa","snapshot":"definition-v1"},"conversation_id":"conversation-a",
        "starting_savepoint":{"kind":"empty"},"parents":[],"guidance":[],"attachments":[],"repository":{"kind":"unavailable"},"budget":"budget-a","grant":null}})
}
fn addition() -> Value {
    let mut next = graph();
    next["snapshot_id"] = "graph-2".into();
    next["revision"] = 2.into();
    next["nodes"].as_array_mut().unwrap().push(node("c"));
    next["dependencies"]
        .as_array_mut()
        .unwrap()
        .push(json!({"parent":"a","child":"c"}));
    json!({"kind":"revise_graph","epoch_id":"epoch-1","previous_graph":"graph-1","graph":next,
        "mutation":{"kind":"add","node_id":"c"},"admission_evidence":"approved-add-c"})
}
fn replacement() -> Value {
    let mut next = graph();
    next["snapshot_id"] = "graph-2".into();
    next["revision"] = 2.into();
    next["nodes"][0] = node("x");
    next["dependencies"][0]["parent"] = "x".into();
    json!({"kind":"revise_graph","epoch_id":"epoch-1","previous_graph":"graph-1","graph":next,
        "mutation":{"kind":"replace_future","previous":"a","replacement":"x","rewire_dependents":["b"]},"admission_evidence":"approved-replace-a"})
}
fn blocker() -> Value {
    json!({"kind":"open_blocker","blocker":{"schema_version":1,"blocker_id":"blocker-a","activation":activation(),
        "kind":{"kind":"human_approval","approval_request":"approval-request-a"},"command_id":"requested-command-a",
        "invocation_id":null,"grant":null,"parameters":"exact-parameters-a","safe_boundary":"safe-boundary-a","evidence":"wait-created-a"}})
}
fn response() -> Value {
    json!({"kind":"resolve_blocker","blocker_id":"blocker-a","activation":activation(),
        "response":{"kind":"human_approval","approval_request":"approval-request-a","approval_evidence":"authenticated-human-response"}})
}

#[test]
fn additive_revision_retains_immutable_history_and_exact_replay() {
    let (mut fold, begin) = initial();
    let before = fold.graph().unwrap().clone();
    let edit = event(1, addition());
    fold.apply(&edit).unwrap();
    assert_eq!(fold.graph().unwrap().revision, 2);
    assert_eq!(fold.graph_history()[0].previous, before);
    assert_eq!(
        fold.graph_history()[0].admission_evidence.as_str(),
        "approved-add-c"
    );
    assert!(!fold.apply(&edit).unwrap());
    let mut restored = TurnContract::default();
    restored.apply(&begin).unwrap();
    restored.apply(&edit).unwrap();
    assert_eq!(fold, restored);
    let mut conflict = edit;
    conflict.event = TurnContractEvent::PauseEpoch {
        epoch_id: ExecutionEpochId::new("epoch-1").unwrap(),
    };
    assert!(matches!(
        fold.apply(&conflict),
        Err(TurnContractError::CommandConflict)
    ));
}

#[test]
fn malformed_or_stale_graph_changes_never_mutate_the_fold() {
    for case in 0..5 {
        let (mut fold, _) = initial();
        let before = fold.clone();
        let mut change = addition();
        match case {
            0 => change["previous_graph"] = "wrong-snapshot".into(),
            1 => change["graph"]["dependencies"]
                .as_array_mut()
                .unwrap()
                .push(json!({"parent":"c","child":"c"})),
            2 => change["graph"]["conditions"] = json!([]),
            3 => change["graph"]["nodes"][2]["conversation_id"] = "conversation-a".into(),
            _ => {
                change["graph"]["nodes"][0]["definition"]["snapshot"] =
                    "different-definition".into()
            }
        }
        assert!(fold.apply(&event(1, change)).is_err(), "case {case}");
        assert_eq!(fold, before);
    }
}

#[test]
fn future_replacement_retains_tombstone_and_requires_every_unstarted_dependent() {
    let (mut fold, _) = initial();
    fold.apply(&event(1, replacement())).unwrap();
    assert_eq!(fold.replaced_nodes()[0].previous.as_str(), "a");
    assert_eq!(fold.replaced_nodes()[0].replacement.as_str(), "x");
    assert_eq!(fold.graph().unwrap().dependencies[0].parent.as_str(), "x");
    assert_eq!(
        fold.graph_history()[0].previous.nodes[0].node_id.as_str(),
        "a"
    );
    let (mut started, _) = initial();
    started.apply(&event(1, start())).unwrap();
    let before = started.clone();
    assert!(started.apply(&event(2, replacement())).is_err());
    assert_eq!(started, before);
    let (mut omitted, _) = initial();
    let mut change = replacement();
    change["mutation"]["rewire_dependents"] = json!([]);
    assert!(omitted.apply(&event(1, change)).is_err());
}

#[test]
fn replacement_rebinds_existing_required_condition_without_waiving_it() {
    let (mut fold, _) = initial();
    let mut change = replacement();
    // Replace b instead: its required check must name the new node, preserving definition/id.
    change["mutation"] =
        json!({"kind":"replace_future","previous":"b","replacement":"x","rewire_dependents":[]});
    change["graph"]["nodes"] = json!([node("a"), node("x")]);
    change["graph"]["dependencies"] = json!([{"parent":"a","child":"x"}]);
    change["graph"]["conditions"][0]["nodes"] = json!(["x"]);
    fold.apply(&event(1, change)).unwrap();
    assert_eq!(
        fold.graph().unwrap().conditions[0].condition_id.as_str(),
        "check-b"
    );
    assert_eq!(fold.graph().unwrap().conditions[0].nodes[0].as_str(), "x");
    assert!(!fold.completion_satisfied());
}

#[test]
fn typed_approval_cannot_be_resolved_as_machine_evidence_or_a_different_request() {
    let (mut fold, _) = initial();
    fold.apply(&event(1, start())).unwrap();
    fold.apply(&event(2, blocker())).unwrap();
    assert_eq!(fold.pending_blocker_count(), 1);
    let before = fold.clone();
    let mut wrong = response();
    wrong["response"] = json!({"kind":"machine_evidence","definition":"machine-definition","response_schema":"machine-schema","evidence":"made-up-approval"});
    assert!(fold.apply(&event(3, wrong)).is_err());
    assert_eq!(fold, before);
    let mut wrong = response();
    wrong["response"]["approval_request"] = "other-request".into();
    assert!(fold.apply(&event(3, wrong)).is_err());
    assert!(fold
        .apply(&event(
            3,
            json!({"kind":"record_intent","activation":activation(),"invocation_id":"new-tool"})
        ))
        .is_err());
    fold.apply(&event(3, response())).unwrap();
    assert_eq!(fold.pending_blocker_count(), 0);
    assert!(matches!(
        fold.blockers()[0].state,
        TurnBlockerState::Resolved { .. }
    ));
    fold.apply(&event(
        4,
        json!({"kind":"record_intent","activation":activation(),"invocation_id":"new-tool"}),
    ))
    .unwrap();
}

#[test]
fn epoch_loss_retires_the_wait_without_resurrecting_its_provider_future() {
    let (mut fold, _) = initial();
    fold.apply(&event(1, start())).unwrap();
    fold.apply(&event(2, blocker())).unwrap();
    fold.apply(&event(
        3,
        json!({"kind":"interrupt_epoch","epoch_id":"epoch-1"}),
    ))
    .unwrap();
    assert_eq!(fold.state(), Some(LogicalTurnState::NeedsAttention));
    assert_eq!(fold.pending_blocker_count(), 0);
    assert!(matches!(
        fold.blockers()[0].state,
        TurnBlockerState::Interrupted { .. }
    ));
    let before = fold.clone();
    assert!(fold.apply(&event(4, response())).is_err());
    assert_eq!(fold, before);
    assert_eq!(fold.activations()[0].state, ActivationState::Interrupted);
}

#[test]
fn legacy_serialization_omits_new_empty_projections_and_future_blocker_schema_fails() {
    let (mut fold, begin) = initial();
    let encoded = serde_json::to_value(&fold).unwrap();
    assert!(encoded.get("graph_history").is_none());
    assert!(encoded.get("blockers").is_none());
    assert!(encoded.get("replaced_nodes").is_none());
    let bytes = serde_json::to_vec(&begin).unwrap();
    let roundtrip = TurnContractEnvelope::decode(&bytes).unwrap();
    assert_eq!(bytes, serde_json::to_vec(&roundtrip).unwrap());
    fold.apply(&event(1, start())).unwrap();
    let mut future = blocker();
    future["blocker"]["schema_version"] = 2.into();
    let before = fold.clone();
    assert!(fold.apply(&event(2, future)).is_err());
    assert_eq!(fold, before);
}

#[test]
fn serialized_dynamic_fixture_replays_exactly_with_no_default_machine_authority() {
    let values: Vec<Value> = serde_json::from_str(include_str!(
        "fixtures/turn_contract/dynamic_graph_blocker_contract.json"
    ))
    .unwrap();
    let mut fold = TurnContract::default();
    for value in values {
        let event = TurnContractEnvelope::decode(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(&event).unwrap(), value);
        assert!(fold.apply(&event).unwrap());
        let after = fold.clone();
        assert!(!fold.apply(&event).unwrap());
        assert_eq!(fold, after);
    }
    assert_eq!(fold.graph().unwrap().revision, 2);
    assert_eq!(fold.pending_blocker_count(), 0);
    assert!(fold.has_dynamic_contract_history());
    // Fixture type/evidence names are data, never an installed registry or grant.
    assert!(fold.activations()[0].input.grant.is_none());
}

#[test]
fn pending_prepared_wait_cannot_be_bypassed_by_a_new_generation() {
    let (mut fold, _) = initial();
    fold.apply(&event(
        1,
        json!({"kind":"pause_epoch","epoch_id":"epoch-1"}),
    ))
    .unwrap();
    let mut input = start()["input"].clone();
    input["activation"]["execution_epoch_id"] = "epoch-2".into();
    fold.apply(&event(
        2,
        json!({"kind":"continue","plan":{
        "source_epoch_id":"epoch-1","epoch_id":"epoch-2","selections":[
            {"kind":"prepare_unmaterialized","input":input},
            {"kind":"await_dependencies","node_id":"b"}
        ]}}),
    ))
    .unwrap();
    let mut wait = blocker();
    wait["blocker"]["activation"] = input["activation"].clone();
    fold.apply(&event(3, wait)).unwrap();
    let before = fold.clone();
    assert!(fold
        .apply(&event(
            4,
            json!({"kind":"start_prepared_activation","activation":input["activation"]})
        ))
        .is_err());
    assert_eq!(fold, before);
    let mut retry = input.clone();
    retry["manifest_id"] = "input-a-2".into();
    retry["activation"]["generation"] = 2.into();
    retry["activation"]["activation_id"] = "activation-a-2".into();
    assert!(fold
        .apply(&event(4, json!({"kind":"start_activation","input":retry})))
        .is_err());
    assert_eq!(fold, before);
    assert_eq!(fold.activations().len(), 1);
    assert_eq!(fold.pending_blocker_count(), 1);
    // Affirmative resolution releases the exact prepared start, proving that the
    // refusal above was the pending wait rather than malformed continuation.
    let mut approved = response();
    approved["activation"] = input["activation"].clone();
    fold.apply(&event(4, approved)).unwrap();
    fold.apply(&event(
        5,
        json!({"kind":"start_prepared_activation","activation":input["activation"]}),
    ))
    .unwrap();
}

#[test]
fn human_decline_is_retained_without_approval_or_effect_settlement() {
    let (mut fold, _) = initial();
    fold.apply(&event(1, start())).unwrap();
    fold.apply(&event(
        2,
        json!({"kind":"record_intent","activation":activation(),"invocation_id":"denied-tool"}),
    ))
    .unwrap();
    let mut wait = blocker();
    wait["blocker"]["invocation_id"] = "denied-tool".into();
    fold.apply(&event(3, wait)).unwrap();
    let mut denied = response();
    denied["response"] = json!({"kind":"human_decline","approval_request":"approval-request-a","reason":"authenticated-human-denial"});
    let envelope = event(4, denied);
    assert_eq!(
        serde_json::to_value(&envelope).unwrap()["event"]["response"]["kind"],
        "human_decline"
    );
    fold.apply(&envelope).unwrap();
    assert!(
        matches!(&fold.blockers()[0].state, TurnBlockerState::Resolved {
        response: TurnBlockerResponse::HumanDecline { reason, .. }
    } if reason.as_str() == "authenticated-human-denial")
    );
    assert_eq!(fold.pending_blocker_count(), 0);
    assert_eq!(fold.activations()[0].state, ActivationState::Running);
    assert_eq!(fold.invocations()[0].evidence, InvocationEvidence::Intent);
    assert!(fold.has_unknown_effects());
    let before = fold.clone();
    assert!(fold.apply(&event(5, response())).is_err());
    assert_eq!(fold, before);
    let mut machine = response();
    machine["response"] = json!({"kind":"machine_evidence","definition":"machine-definition","response_schema":"machine-schema","evidence":"not-a-human-approval"});
    assert!(fold.apply(&event(5, machine)).is_err());
    assert_eq!(fold, before);
    let accept = json!({"kind":"accept_activation","activation":activation(),
        "checkpoint":{"checkpoint_id":"accepted-a","session_id":"session-a","conversation_id":"conversation-a",
            "source":{"kind":"accepted","activation":activation()}},"output":"output-a"});
    assert!(fold.apply(&event(5, accept)).is_err());
    assert_eq!(fold, before);
    // A negative response does not fail the entire actor. Another permitted
    // action may be proposed, while the denied exact intent remains unsettled
    // until a separate authoritative non-dispatch/outcome record is retained.
    fold.apply(&event(5, json!({"kind":"record_intent","activation":activation(),"invocation_id":"alternative-tool"}))).unwrap();
    assert_eq!(fold.invocations()[0].evidence, InvocationEvidence::Intent);
    fold.apply(&event(6, json!({"kind":"prove_not_dispatched","invocation_id":"denied-tool","evidence":"host-proved-denied-before-dispatch"}))).unwrap();
    assert!(matches!(
        fold.invocations()[0].evidence,
        InvocationEvidence::CancelledBeforeDispatch { .. }
    ));
    assert!(matches!(
        &fold.blockers()[0].state,
        TurnBlockerState::Resolved {
            response: TurnBlockerResponse::HumanDecline { .. }
        }
    ));
}

#[test]
fn replaced_initial_checkpoint_identity_cannot_be_reused_by_another_owner() {
    let mut initial_graph = graph();
    initial_graph["nodes"][0]["starting_savepoint"] = json!({"kind":"checkpoint","checkpoint":{
        "checkpoint_id":"retained-initial-a","session_id":"session-a","conversation_id":"conversation-a",
        "source":{"kind":"committed","evidence":"original-committed-a"}}});
    let mut fold = TurnContract::default();
    fold.apply(&event(
        0,
        json!({"kind":"begin","epoch_id":"epoch-1","graph":initial_graph}),
    ))
    .unwrap();
    fold.apply(&event(1, replacement())).unwrap();
    assert!(fold.activations().is_empty());
    assert_eq!(
        fold.graph_history()[0].previous.nodes[0].starting_savepoint,
        serde_json::from_value::<ConversationSavepoint>(
            initial_graph["nodes"][0]["starting_savepoint"].clone()
        )
        .unwrap()
    );
    let mut input = start()["input"].clone();
    input["manifest_id"] = "input-x-1".into();
    input["activation"]["node_id"] = "x".into();
    input["activation"]["activation_id"] = "activation-x-1".into();
    input["conversation_id"] = "conversation-x".into();
    fold.apply(&event(2, json!({"kind":"start_activation","input":input})))
        .unwrap();
    let mut accept = json!({"kind":"accept_activation","activation":input["activation"],
        "checkpoint":{"checkpoint_id":"retained-initial-a","session_id":"session-a","conversation_id":"conversation-x",
            "source":{"kind":"accepted","activation":input["activation"]}},"output":"output-x"});
    let before = fold.clone();
    assert!(matches!(
        fold.apply(&event(3, accept.clone())),
        Err(TurnContractError::InvalidTransition(
            "checkpoint identity conflicts with a retained graph savepoint"
        ))
    ));
    assert_eq!(fold, before);
    accept["checkpoint"]["checkpoint_id"] = "fresh-accepted-x".into();
    fold.apply(&event(3, accept)).unwrap();
}

#[test]
fn replacement_reserves_cumulative_node_capacity_before_accepting_the_graph() {
    for count in [MAX_CONTRACT_NODES - 1, MAX_CONTRACT_NODES] {
        let nodes = (0..count)
            .map(|i| node(&format!("n{i}")))
            .collect::<Vec<_>>();
        let mut wide_graph = json!({"snapshot_id":"wide-1","revision":1,"nodes":nodes,"dependencies":[],
            "conditions":[{"condition_id":"last-check","kind":{"kind":"repository_check","definition":"check-definition"},"nodes":[format!("n{}",count-1)]}]});
        let mut fold = TurnContract::default();
        fold.apply(&event(
            0,
            json!({"kind":"begin","epoch_id":"epoch-1","graph":wide_graph}),
        ))
        .unwrap();
        fold.apply(&event(
            1,
            json!({"kind":"pause_epoch","epoch_id":"epoch-1"}),
        ))
        .unwrap();
        let prepared = (0..count)
            .map(|i| {
                let mut input = start()["input"].clone();
                input["manifest_id"] = format!("input-n{i}").into();
                input["activation"]["node_id"] = format!("n{i}").into();
                input["activation"]["activation_id"] = format!("activation-n{i}").into();
                input["activation"]["execution_epoch_id"] = "epoch-2".into();
                input["conversation_id"] = format!("conversation-n{i}").into();
                json!({"kind":"prepare_unmaterialized","input":input})
            })
            .collect::<Vec<_>>();
        fold.apply(&event(2, json!({"kind":"continue","plan":{"source_epoch_id":"epoch-1","epoch_id":"epoch-2","selections":prepared}}))).unwrap();
        assert_eq!(fold.activations().len(), count);
        wide_graph["snapshot_id"] = "wide-2".into();
        wide_graph["revision"] = 2.into();
        wide_graph["nodes"][0] = node("fresh");
        let change = event(
            3,
            json!({"kind":"revise_graph","epoch_id":"epoch-2","previous_graph":"wide-1","graph":wide_graph,
            "mutation":{"kind":"replace_future","previous":"n0","replacement":"fresh","rewire_dependents":[]},"admission_evidence":"replace-proof"}),
        );
        let before = fold.clone();
        if count == MAX_CONTRACT_NODES {
            assert!(matches!(
                fold.apply(&change),
                Err(TurnContractError::LimitExceeded(
                    "retained and declared graph node count"
                ))
            ));
            assert_eq!(fold, before);
        } else {
            fold.apply(&change).unwrap();
            let mut input = start()["input"].clone();
            input["manifest_id"] = "input-fresh".into();
            input["activation"]["node_id"] = "fresh".into();
            input["activation"]["activation_id"] = "activation-fresh".into();
            input["activation"]["execution_epoch_id"] = "epoch-2".into();
            input["conversation_id"] = "conversation-fresh".into();
            fold.apply(&event(4, json!({"kind":"start_activation","input":input})))
                .unwrap();
            assert_eq!(fold.activations().len(), MAX_CONTRACT_NODES);
        }
    }
}

fn retain_failed_generations(fold: &mut TurnContract, count: usize) {
    for generation in 1..=count {
        let mut input = start()["input"].clone();
        input["manifest_id"] = format!("input-a-{generation}").into();
        input["activation"]["generation"] = json!(generation);
        input["activation"]["activation_id"] = format!("activation-a-{generation}").into();
        fold.apply(&event(
            fold.revision(),
            json!({"kind":"start_activation","input":input}),
        ))
        .unwrap();
        fold.apply(&event(fold.revision(), json!({"kind":"fail_activation","activation":input["activation"],"evidence":"failed-generation"}))).unwrap();
    }
}
fn independent_addition() -> Value {
    let mut change = addition();
    change["graph"]["dependencies"] = graph()["dependencies"].clone();
    change
}
fn retry_input(generation: usize) -> Value {
    let mut input = start()["input"].clone();
    input["manifest_id"] = format!("input-a-{generation}").into();
    input["activation"]["generation"] = json!(generation);
    input["activation"]["activation_id"] = format!("activation-a-{generation}").into();
    input
}

#[test]
fn graph_capacity_includes_required_ancestors_but_not_unrelated_optional_retries() {
    // a is optional in isolation, but b is required and depends on it.
    let mut declared = graph();
    declared["nodes"][0]["required"] = false.into();
    let mut fold = TurnContract::default();
    fold.apply(&event(
        0,
        json!({"kind":"begin","epoch_id":"epoch-1","graph":declared}),
    ))
    .unwrap();
    retain_failed_generations(&mut fold, MAX_CONTRACT_ACTIVATIONS - 2);
    let mut change = independent_addition();
    change["graph"]["nodes"][0]["required"] = false.into();
    let before = fold.clone();
    // Two initial nodes alone fit, but b's failed ancestor a also needs a
    // fresh generation. Do not accept this known-impossible required graph.
    assert!(matches!(
        fold.apply(&event(fold.revision(), change)),
        Err(TurnContractError::LimitExceeded(
            "declared graph activation capacity"
        ))
    ));
    assert_eq!(fold, before);

    // The same failed optional a does not need an unsolicited retry when it
    // is unrelated to required b/c. Reserve only their two first generations.
    let mut unrelated = TurnContract::default();
    declared["dependencies"] = json!([]);
    unrelated
        .apply(&event(
            0,
            json!({"kind":"begin","epoch_id":"epoch-1","graph":declared}),
        ))
        .unwrap();
    retain_failed_generations(&mut unrelated, MAX_CONTRACT_ACTIVATIONS - 2);
    let mut change = independent_addition();
    change["graph"]["nodes"][0]["required"] = false.into();
    change["graph"]["dependencies"] = json!([]);
    // A required completion check also makes its selected optional node part
    // of the minimum work, even with no ordinary dependency edge.
    let mut condition_bound = unrelated.clone();
    let mut checked_optional = change.clone();
    checked_optional["graph"]["conditions"].as_array_mut().unwrap().push(json!({
        "condition_id":"check-a-c","kind":{"kind":"repository_check","definition":"check-a-c"},"nodes":["a","c"]}));
    assert!(matches!(
        condition_bound.apply(&event(condition_bound.revision(), checked_optional)),
        Err(TurnContractError::LimitExceeded(
            "declared graph activation capacity"
        ))
    ));
    assert_eq!(condition_bound, unrelated);
    unrelated
        .apply(&event(unrelated.revision(), change))
        .unwrap();
    let before = unrelated.clone();
    // An optional retry may not spend the capacity already needed by b/c.
    assert!(matches!(
        unrelated.apply(&event(
            unrelated.revision(),
            json!({"kind":"start_activation","input":retry_input(MAX_CONTRACT_ACTIVATIONS - 1)})
        )),
        Err(TurnContractError::LimitExceeded(
            "declared graph activation capacity"
        ))
    ));
    assert_eq!(unrelated, before);
    let mut input = start()["input"].clone();
    input["manifest_id"] = "input-c".into();
    input["activation"]["node_id"] = "c".into();
    input["activation"]["activation_id"] = "activation-c".into();
    input["conversation_id"] = "conversation-c".into();
    unrelated
        .apply(&event(
            unrelated.revision(),
            json!({"kind":"start_activation","input":input}),
        ))
        .unwrap();
}

#[test]
fn known_capacity_admission_never_rejects_later_observed_failure_or_closure() {
    let (mut fold, _) = initial();
    retain_failed_generations(&mut fold, MAX_CONTRACT_ACTIVATIONS - 3);
    // One required a retry plus b/c initial generations fit exactly.
    fold.apply(&event(fold.revision(), independent_addition()))
        .unwrap();
    let input = retry_input(MAX_CONTRACT_ACTIVATIONS - 2);
    fold.apply(&event(
        fold.revision(),
        json!({"kind":"start_activation","input":input}),
    ))
    .unwrap();
    // No promise was made that another failure would leave retry capacity.
    // Its truthful terminal evidence must still be recordable.
    fold.apply(&event(fold.revision(), json!({"kind":"fail_activation","activation":input["activation"],"evidence":"another-observed-failure"}))).unwrap();
    let before = fold.clone();
    assert!(matches!(
        fold.apply(&event(
            fold.revision(),
            json!({"kind":"start_activation","input":retry_input(MAX_CONTRACT_ACTIVATIONS - 1)})
        )),
        Err(TurnContractError::LimitExceeded(
            "declared graph activation capacity"
        ))
    ));
    assert_eq!(fold, before);
    fold.apply(&event(
        fold.revision(),
        json!({"kind":"interrupt_epoch","epoch_id":"epoch-1"}),
    ))
    .unwrap();
    fold.apply(&event(
        fold.revision(),
        json!({"kind":"close","closure":"finished"}),
    ))
    .unwrap();
    assert_eq!(fold.state(), Some(LogicalTurnState::Finished));
}
