use super::*;
use axocoatl_session::turn_contract::{
    ActivationId, ExecutionEpochId, LogicalTurnId, SessionId, TurnNodeId,
};

fn activation() -> ActivationRef {
    ActivationRef {
        session_id: SessionId::new("session").unwrap(),
        turn_id: LogicalTurnId::new("turn").unwrap(),
        execution_epoch_id: ExecutionEpochId::new("epoch").unwrap(),
        node_id: TurnNodeId::new("node").unwrap(),
        generation: 1,
        activation_id: ActivationId::new("activation").unwrap(),
    }
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("external_harness_codex_fixture.json")).unwrap()
}

fn run(fixture: &Value, turn_field: &str) -> ProviderRunRef {
    ProviderRunRef::codex_fixture(
        activation(),
        fixture["thread_id"].as_str().unwrap().to_owned(),
        fixture[turn_field].as_str().unwrap().to_owned(),
    )
    .unwrap()
}

#[test]
fn captured_real_events_project_only_the_exact_run() {
    let fixture = fixture();
    assert_eq!(fixture["binary_sha256"], CODEX_FIXTURE_BINARY_SHA256);
    assert_eq!(fixture["schema_sha256"], CODEX_FIXTURE_SCHEMA_SHA256);
    let normal = run(&fixture, "normal_turn_id");
    let interrupted = run(&fixture, "interrupt_turn_id");
    let mut text = String::new();
    let mut normal_terminal = None;
    let mut interrupted_terminal = None;
    let mut measured_usage = false;
    for message in fixture["events"].as_array().unwrap() {
        let frame = serde_json::to_vec(message).unwrap();
        match normal.decode(&activation(), &frame).unwrap() {
            HarnessObservation::TextDelta { text: delta, .. } => text.push_str(&delta),
            HarnessObservation::Terminal(status) => normal_terminal = Some(status),
            HarnessObservation::Usage {
                input_tokens,
                output_tokens,
                cost_usd,
            } => {
                assert!(input_tokens.unwrap() > 0);
                assert!(output_tokens.unwrap() > 0);
                assert!(cost_usd.is_none());
                measured_usage = true;
            }
            _ => {}
        }
        if let HarnessObservation::Terminal(status) =
            interrupted.decode(&activation(), &frame).unwrap()
        {
            interrupted_terminal = Some(status);
        }
    }
    assert_eq!(text, "AXOCOATL_PROTOCOL_OK");
    assert_eq!(normal_terminal, Some(HarnessTerminal::Completed));
    assert_eq!(interrupted_terminal, Some(HarnessTerminal::Interrupted));
    assert!(measured_usage);
}

#[test]
fn observed_limits_remain_absent_and_schema_is_not_behavior() {
    let caps = codex_fixture_capabilities();
    assert_eq!(caps.whole_run_stop, CapabilityEvidence::Observed);
    assert_eq!(caps.native_steer, CapabilityEvidence::SchemaOnly);
    assert_eq!(caps.retained_history, CapabilityEvidence::Unavailable);
    assert_eq!(caps.child_stop, CapabilityEvidence::Unavailable);
    assert_eq!(caps.exact_retry, CapabilityEvidence::Unavailable);
    assert_eq!(caps.enforced_budget, CapabilityEvidence::Unavailable);
    let fixture = fixture();
    assert_eq!(fixture["history_response"]["error"]["code"], -32600);
    assert_eq!(fixture["stale_steer_response"]["error"]["code"], -32600);
    assert_eq!(fixture["tool_item_count"], 0);
}

#[test]
fn generation_epoch_and_external_identity_are_fenced() {
    let fixture = fixture();
    let run = run(&fixture, "normal_turn_id");
    let frame = serde_json::to_vec(&fixture["events"][0]).unwrap();
    let mut stale = activation();
    stale.generation += 1;
    assert!(matches!(
        run.decode(&stale, &frame),
        Err(HarnessProtocolError::StaleActivation)
    ));
    stale = activation();
    stale.execution_epoch_id = ExecutionEpochId::new("other").unwrap();
    assert!(matches!(
        run.decode(&stale, &frame),
        Err(HarnessProtocolError::StaleActivation)
    ));
    let mut foreign = fixture["events"][0].clone();
    foreign["params"]["threadId"] = json!("another-thread");
    assert_eq!(
        run.decode(&activation(), &serde_json::to_vec(&foreign).unwrap())
            .unwrap(),
        HarnessObservation::Unattributed
    );
}

#[test]
fn approvals_and_transport_loss_never_become_execution_success() {
    let fixture = fixture();
    let run = run(&fixture, "normal_turn_id");
    let request = json!({"id": 991, "method": "item/permissions/requestApproval", "params": {}});
    assert!(matches!(
        run.decode(&activation(), &serde_json::to_vec(&request).unwrap())
            .unwrap(),
        HarnessObservation::RequiresAuthority { .. }
    ));
    let lost = HarnessObservation::TransportLostOutcomeUnknown;
    assert_eq!(
        serde_json::from_value::<HarnessObservation>(serde_json::to_value(&lost).unwrap()).unwrap(),
        lost
    );
    let terminal = HarnessObservation::Terminal(HarnessTerminal::Completed);
    assert_eq!(
        serde_json::from_value::<HarnessObservation>(serde_json::to_value(&terminal).unwrap())
            .unwrap(),
        terminal
    );
    let request = run.interrupt_request(5);
    assert_eq!(request["params"]["threadId"], fixture["thread_id"]);
    assert_eq!(request["params"]["turnId"], fixture["normal_turn_id"]);
}

#[test]
fn oversized_frames_bad_pins_and_missing_usage_are_not_defaulted_to_success() {
    let fixture = fixture();
    let mut run = run(&fixture, "normal_turn_id");
    assert!(matches!(
        run.decode(&activation(), &vec![b' '; MAX_HARNESS_FRAME_BYTES + 1]),
        Err(HarnessProtocolError::FrameTooLarge)
    ));
    let event = json!({"method":"thread/tokenUsage/updated","params":{
        "threadId":run.provider_thread_id,"turnId":run.provider_turn_id,"tokenUsage":{}}});
    assert_eq!(
        run.decode(&activation(), &serde_json::to_vec(&event).unwrap())
            .unwrap(),
        HarnessObservation::Usage {
            input_tokens: None,
            output_tokens: None,
            cost_usd: None
        }
    );
    run.schema_sha256 = "changed".to_owned();
    assert!(matches!(
        run.decode(&activation(), b"{}"),
        Err(HarnessProtocolError::PinMismatch)
    ));
    assert!(verify_codex_fixture_pin(
        CODEX_FIXTURE_VERSION,
        b"not the executable",
        CODEX_FIXTURE_SCHEMA_SHA256
    )
    .is_err());
}

#[test]
fn existing_codex_run_projects_to_shared_contract_without_inferred_model_or_request() {
    use axocoatl_session::provider_run::{
        ExecutorKind, ExternalIdentity, ProviderIdentity, RunFact,
    };
    use axocoatl_session::turn_contract::EvidenceRef;
    let fixture = fixture();
    let run = run(&fixture, "normal_turn_id");
    let before = serde_json::to_vec(&run).unwrap();
    let projected = run
        .provider_run_reference(
            "host-request-1".into(),
            ProviderIdentity {
                provider: RunFact::Unknown,
                model: RunFact::Unknown,
            },
            ProviderIdentity {
                provider: RunFact::Unknown,
                model: RunFact::Unknown,
            },
            RunFact::Unknown,
            EvidenceRef::new("pinned-real-harness-transcript").unwrap(),
        )
        .unwrap();
    projected.validate_for(&activation()).unwrap();
    assert_eq!(projected.executor.kind, ExecutorKind::ExternalHarness);
    assert_eq!(
        projected.external.session_id,
        RunFact::Known {
            value: ExternalIdentity::Text(run.provider_thread_id.clone())
        }
    );
    assert_eq!(
        projected.external.run_id,
        RunFact::Known {
            value: ExternalIdentity::Text(run.provider_turn_id.clone())
        }
    );
    assert_eq!(projected.external.request_id, RunFact::Unknown);
    assert_eq!(projected.observed.model, RunFact::Unknown);
    assert_eq!(serde_json::to_vec(&run).unwrap(), before);
    let capabilities = codex_fixture_capabilities();
    let steering = capabilities.steering().unwrap();
    assert_eq!(steering.native, CapabilityEvidence::SchemaOnly);
    assert_eq!(steering.next_safe_boundary, CapabilityEvidence::Unavailable);
    assert_eq!(
        steering.interrupt_and_revise,
        CapabilityEvidence::Unavailable
    );
    assert_eq!(capabilities.child_stop, CapabilityEvidence::Unavailable);
    let restored: ProviderRunRef = serde_json::from_slice(&before).unwrap();
    assert_eq!(serde_json::to_vec(&restored).unwrap(), before);
    let mut changed_pin = run;
    changed_pin.schema_sha256 = "changed".into();
    assert!(changed_pin
        .provider_run_reference(
            "request-2".into(),
            projected.requested,
            projected.observed,
            RunFact::Unknown,
            projected.evidence
        )
        .is_err());
}

fn official_fixture() -> Value {
    serde_json::from_str(include_str!("external_harness_codex_official_fixture.json")).unwrap()
}

fn official_run(fixture: &Value, turn_field: &str) -> ProviderRunRef {
    ProviderRunRef::codex_official_fixture(
        activation(),
        fixture["thread_id"].as_str().unwrap().to_owned(),
        fixture[turn_field].as_str().unwrap().to_owned(),
    )
    .unwrap()
}

#[test]
fn official_release_keeps_original_pin_and_maps_real_normal_and_interrupt_events() {
    let fixture = official_fixture();
    assert_eq!(
        fixture["pin"]["binary_sha256"],
        CODEX_OFFICIAL_FIXTURE_BINARY_SHA256
    );
    assert_eq!(fixture["pin"]["schema_sha256"], CODEX_FIXTURE_SCHEMA_SHA256);
    assert_ne!(
        CODEX_FIXTURE_BINARY_SHA256,
        CODEX_OFFICIAL_FIXTURE_BINARY_SHA256
    );
    let normal = official_run(&fixture, "normal_turn_id");
    let interrupted = official_run(&fixture, "interrupt_turn_id");
    let mut text = String::new();
    let mut normal_terminal = None;
    let mut interrupted_terminal = None;
    let mut observed_usage = false;
    for event in fixture["events"].as_array().unwrap() {
        let frame = serde_json::to_vec(event).unwrap();
        match normal.decode(&activation(), &frame).unwrap() {
            HarnessObservation::TextDelta { text: delta, .. } => text.push_str(&delta),
            HarnessObservation::Terminal(status) => normal_terminal = Some(status),
            HarnessObservation::Usage {
                input_tokens,
                output_tokens,
                cost_usd,
            } => {
                assert!(input_tokens.is_some_and(|tokens| tokens > 0));
                assert!(output_tokens.is_some_and(|tokens| tokens > 0));
                assert!(cost_usd.is_none());
                observed_usage = true;
            }
            _ => {}
        }
        if let HarnessObservation::Terminal(status) =
            interrupted.decode(&activation(), &frame).unwrap()
        {
            interrupted_terminal = Some(status);
        }
    }
    assert_eq!(text, "AXOCOATL_RETAINED_PROTOCOL_OK");
    assert_eq!(normal_terminal, Some(HarnessTerminal::Completed));
    assert_eq!(interrupted_terminal, Some(HarnessTerminal::Interrupted));
    assert!(observed_usage);
    assert_eq!(fixture["tool_item_count"], 0);
    // Adding the new observed pin cannot change the old ephemeral proof.
    assert_eq!(
        codex_fixture_capabilities().retained_history,
        CapabilityEvidence::Unavailable
    );
    let capabilities = codex_official_fixture_capabilities();
    assert_eq!(capabilities.retained_history, CapabilityEvidence::Observed);
    assert_eq!(
        capabilities.cross_process_resume,
        CapabilityEvidence::Unavailable
    );
    assert_eq!(capabilities.child_stop, CapabilityEvidence::Unavailable);
    assert_eq!(capabilities.exact_retry, CapabilityEvidence::Unavailable);
    assert_eq!(
        capabilities.side_effect_rollback,
        CapabilityEvidence::Unavailable
    );
    assert_eq!(
        capabilities.enforced_budget,
        CapabilityEvidence::Unavailable
    );
    assert_eq!(capabilities.native_steer, CapabilityEvidence::SchemaOnly);
}

#[test]
fn actual_abrupt_loss_retains_unknown_outcome_and_read_only_history_is_not_acceptance() {
    let fixture = official_fixture();
    let lost = official_run(&fixture, "loss_turn_id");
    let mut partial = String::new();
    let mut terminal = None;
    for event in fixture["events"].as_array().unwrap() {
        match lost
            .decode(&activation(), &serde_json::to_vec(event).unwrap())
            .unwrap()
        {
            HarnessObservation::TextDelta { text, .. } => partial.push_str(&text),
            HarnessObservation::Terminal(status) => terminal = Some(status),
            _ => {}
        }
    }
    assert_eq!(partial, "1");
    assert!(
        terminal.is_none(),
        "no terminal was observed before owned SIGKILL"
    );
    assert_eq!(fixture["abrupt_exit"], -9);
    assert_eq!(fixture["retained_prefix_unchanged"], true);
    let lost_observation = lost.transport_lost(&activation()).unwrap();
    assert_eq!(
        lost_observation,
        HarnessObservation::TransportLostOutcomeUnknown
    );
    assert_eq!(
        serde_json::from_value::<HarnessObservation>(
            serde_json::to_value(&lost_observation).unwrap()
        )
        .unwrap(),
        lost_observation
    );
    let loss = fixture["local_journal"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["kind"] == "transport_lost")
        .unwrap();
    assert_eq!(loss["turn_id"], fixture["loss_turn_id"]);
    assert_eq!(loss["outcome"], "unknown");
    assert_eq!(loss["usage"], "unknown");
    assert_eq!(loss["automatic_replay"], false);
    let normal_id = &fixture["normal_turn_id"];
    let live = fixture["live_history"]["thread"]["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|turn| &turn["id"] == normal_id)
        .unwrap();
    let reloaded = fixture["reloaded_history"]["thread"]["turns"]
        .as_array()
        .unwrap();
    assert_eq!(
        reloaded
            .iter()
            .find(|turn| &turn["id"] == normal_id)
            .unwrap(),
        live
    );
    let historical_loss = reloaded
        .iter()
        .find(|turn| turn["id"] == fixture["loss_turn_id"])
        .unwrap();
    assert_eq!(historical_loss["status"], "interrupted");
    assert!(
        historical_loss["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["type"] != "agentMessage"),
        "the final streamed prefix was not persisted by the external harness"
    );
    let response = json!({"id":2,"result":fixture["reloaded_history"]});
    assert_eq!(
        lost.decode(&activation(), &serde_json::to_vec(&response).unwrap())
            .unwrap(),
        HarnessObservation::Unattributed,
        "a retained response is not an attributed live terminal or effect-settlement proof"
    );
    let methods: Vec<_> = fixture["after_loss_requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|request| request["method"].as_str().unwrap())
        .collect();
    assert_eq!(methods, ["initialize", "initialized", "thread/read"]);
}

#[test]
fn official_pin_and_transport_observations_remain_exact_activation_bound() {
    let fixture = official_fixture();
    let mut run = official_run(&fixture, "loss_turn_id");
    let mut stale = activation();
    stale.generation += 1;
    assert!(matches!(
        run.transport_lost(&stale),
        Err(HarnessProtocolError::StaleActivation)
    ));
    let bytes = serde_json::to_vec(&run).unwrap();
    assert_eq!(
        serde_json::from_slice::<ProviderRunRef>(&bytes).unwrap(),
        run
    );
    run.executable_sha256 = "unobserved-official-version".into();
    assert!(matches!(
        run.decode(&activation(), b"{}"),
        Err(HarnessProtocolError::PinMismatch)
    ));
    assert!(matches!(
        run.transport_lost(&activation()),
        Err(HarnessProtocolError::PinMismatch)
    ));
    assert!(verify_codex_official_fixture_pin(
        CODEX_FIXTURE_VERSION,
        b"wrong bytes",
        CODEX_FIXTURE_SCHEMA_SHA256
    )
    .is_err());
}
