use axocoatl_session::control_authority::ProviderCallRecord;
use axocoatl_session::provider_run::*;
use axocoatl_session::turn_contract::{
    ActivationId, EvidenceRef, ExecutionEpochId, MAX_CONTRACT_ENVELOPE_BYTES,
};
use serde_json::json;

const RUN: &str = include_str!("fixtures/provider-run/run-v1.json");
const LEGACY_CAPABILITIES: &str = include_str!("fixtures/provider-run/legacy-capabilities-v1.json");
const NATIVE: &str = include_str!("fixtures/provider-run/native-call-v1.json");
fn run() -> ProviderRunRef {
    ProviderRunRef::decode(RUN.as_bytes()).unwrap()
}
fn unknown_provider() -> ProviderIdentity {
    ProviderIdentity {
        provider: RunFact::Unknown,
        model: RunFact::Unknown,
    }
}
fn unknown_external() -> ExternalRunIdentity {
    ExternalRunIdentity {
        session_id: RunFact::Unknown,
        run_id: RunFact::Unknown,
        request_id: RunFact::Unknown,
    }
}
fn native_executor() -> ExecutorIdentity {
    ExecutorIdentity {
        kind: ExecutorKind::Native,
        adapter: VersionedExecutorComponent {
            name: "native-fixture".into(),
            version: RunFact::Unknown,
        },
        protocol: RunFact::Unknown,
    }
}

#[test]
fn versioned_run_keeps_requested_observed_and_unknown_facts_distinct() {
    let value = run();
    assert_eq!(serde_json::to_string(&value).unwrap(), RUN.trim());
    assert_eq!(
        value.requested.model,
        RunFact::Known {
            value: "requested-alias".into()
        }
    );
    assert_eq!(
        value.observed.model,
        RunFact::Known {
            value: "resolved-model".into()
        }
    );
    assert_eq!(value.observed.provider, RunFact::Unknown);
    assert_eq!(value.external.request_id, RunFact::Unknown);
    value.validate_for(&value.activation).unwrap();
}

#[test]
fn native_projection_keeps_intent_as_requested_and_preserves_incurred_usage() {
    let record: ProviderCallRecord = serde_json::from_str(NATIVE).unwrap();
    let before = serde_json::to_string(&record).unwrap();
    assert_eq!(before, NATIVE.trim());
    let projected = record
        .provider_run_reference(
            native_executor(),
            unknown_provider(),
            unknown_external(),
            EvidenceRef::new("native-call-retained-evidence").unwrap(),
        )
        .unwrap();
    assert_eq!(projected.activation, record.activation);
    assert_eq!(projected.local_request_id, record.intent.call_id);
    assert_eq!(
        projected.requested.model,
        RunFact::Known {
            value: record.intent.model.clone()
        }
    );
    assert_eq!(projected.observed, unknown_provider());
    assert_eq!(projected.external, unknown_external());
    assert_eq!(serde_json::to_string(&record).unwrap(), before);
    assert!(!record.outcome.as_ref().unwrap().usage.complete);
    assert!(!record.outcome.as_ref().unwrap().cost_known);
    assert_eq!(
        record.outcome.as_ref().unwrap().usage.usage.output_tokens,
        3
    );
}

#[test]
fn multiple_requests_share_only_the_exact_parent_activation() {
    let first = run();
    let mut successor = first.clone();
    successor.local_request_id = "native-steering-successor".into();
    successor.external.request_id = RunFact::Known {
        value: ExternalIdentity::Integer(4),
    };
    successor.evidence = EvidenceRef::new("successor-evidence").unwrap();
    successor.validate_for(&first.activation).unwrap();
    assert_ne!(first.local_request_id, successor.local_request_id);
    let restored: Vec<ProviderRunRef> =
        serde_json::from_slice(&serde_json::to_vec(&[first.clone(), successor]).unwrap()).unwrap();
    assert_eq!(restored[0].activation, restored[1].activation);
    assert_eq!(restored[0].external.request_id, RunFact::Unknown);
    let mut different = first.activation.clone();
    different.generation += 1;
    different.activation_id = ActivationId::new("activation-a-2").unwrap();
    assert!(restored[1].validate_for(&different).is_err());
    different = first.activation.clone();
    different.execution_epoch_id = ExecutionEpochId::new("epoch-2").unwrap();
    assert!(restored[1].validate_for(&different).is_err());
}

#[test]
fn remote_string_integer_and_unknown_ids_never_collapse() {
    let mut text = run();
    text.external.request_id = RunFact::Known {
        value: ExternalIdentity::Text("4".into()),
    };
    let mut integer = text.clone();
    integer.external.request_id = RunFact::Known {
        value: ExternalIdentity::Integer(4),
    };
    assert_ne!(
        serde_json::to_value(&text).unwrap(),
        serde_json::to_value(&integer).unwrap()
    );
    for value in [text, integer] {
        assert_eq!(
            ProviderRunRef::decode(&serde_json::to_vec(&value).unwrap()).unwrap(),
            value
        );
    }
    let mut invalid = serde_json::to_value(run()).unwrap();
    invalid["external"]["request_id"] = json!({"state":"known","value":1.5});
    assert!(ProviderRunRef::decode(&serde_json::to_vec(&invalid).unwrap()).is_err());
}

#[test]
fn legacy_capability_bytes_survive_without_inventing_missing_steering_support() {
    let caps = ExecutorCapabilities::decode(LEGACY_CAPABILITIES.as_bytes()).unwrap();
    assert_eq!(
        serde_json::to_string(&caps).unwrap(),
        LEGACY_CAPABILITIES.trim()
    );
    assert_eq!(
        caps.steering().unwrap(),
        SteeringCapabilities {
            native: CapabilityEvidence::SchemaOnly,
            next_safe_boundary: CapabilityEvidence::Unavailable,
            interrupt_and_revise: CapabilityEvidence::Unavailable,
        }
    );
    assert_eq!(caps.whole_run_stop, CapabilityEvidence::Observed);
    assert_eq!(caps.child_stop, CapabilityEvidence::Unavailable);
    assert_eq!(caps.exact_retry, CapabilityEvidence::Unavailable);
}

#[test]
fn steering_dimensions_remain_independent_and_explicitly_unsupported_is_not_unknown() {
    let mut caps = ExecutorCapabilities::decode(LEGACY_CAPABILITIES.as_bytes()).unwrap();
    caps.next_safe_boundary_steer = Some(CapabilityEvidence::Observed);
    caps.interrupt_and_revise = Some(CapabilityEvidence::Unsupported);
    let encoded = serde_json::to_vec(&caps).unwrap();
    assert_eq!(ExecutorCapabilities::decode(&encoded).unwrap(), caps);
    let steering = caps.steering().unwrap();
    assert_eq!(steering.native, CapabilityEvidence::SchemaOnly);
    assert_eq!(steering.next_safe_boundary, CapabilityEvidence::Observed);
    assert_eq!(
        steering.interrupt_and_revise,
        CapabilityEvidence::Unsupported
    );
    assert_ne!(
        steering.interrupt_and_revise,
        CapabilityEvidence::Unavailable
    );
    // The envelope exposes observations only; no command/grant/dispatch claim is produced.
    assert_eq!(caps.exact_retry, CapabilityEvidence::Unavailable);
}

#[test]
fn existing_identity_and_envelope_bounds_are_checked_before_acceptance() {
    let mut value = run();
    value.local_request_id = "x".repeat(MAX_PROVIDER_IDENTITY_BYTES);
    value.validate().unwrap();
    value.local_request_id.push('x');
    assert!(matches!(value.validate(), Err(ProviderRunError::Identity)));
    for invalid in [
        "".to_owned(),
        "line\nbreak".to_owned(),
        "x".repeat(MAX_PROVIDER_IDENTITY_BYTES + 1),
    ] {
        let mut value = run();
        value.observed.model = RunFact::Known { value: invalid };
        assert!(value.validate().is_err());
    }
    let mut value = run();
    value.activation.generation = 0;
    assert!(value.validate().is_err());
    assert!(matches!(
        ProviderRunRef::decode(&vec![b' '; MAX_CONTRACT_ENVELOPE_BYTES + 1]),
        Err(ProviderRunError::Capacity)
    ));
    assert!(matches!(
        ExecutorCapabilities::decode(&vec![b' '; MAX_CONTRACT_ENVELOPE_BYTES + 1]),
        Err(ProviderRunError::Capacity)
    ));
}

#[test]
fn future_versions_unknown_fields_and_missing_facts_fail_closed() {
    assert!(matches!(
        ProviderRunRef::decode(br#"{"schema_version":99,"future":true}"#),
        Err(ProviderRunError::Version)
    ));
    assert!(matches!(
        ExecutorCapabilities::decode(br#"{"schema_version":99,"future":true}"#),
        Err(ProviderRunError::Version)
    ));
    let mut value = serde_json::to_value(run()).unwrap();
    value["observed"].as_object_mut().unwrap().remove("model");
    assert!(ProviderRunRef::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    let mut value = serde_json::to_value(run()).unwrap();
    value["ambient_permission"] = true.into();
    assert!(ProviderRunRef::decode(&serde_json::to_vec(&value).unwrap()).is_err());
}
