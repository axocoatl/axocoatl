//! Wire form and bounds of proposals and configuration reloads.
use super::*;

fn proposal(state: ProposalState) -> NetworkEvent {
    decided(state, None, None, None)
}

fn decided(
    state: ProposalState,
    actor: Option<&str>,
    command_id: Option<&str>,
    revision: Option<u64>,
) -> NetworkEvent {
    NetworkEvent::Proposal {
        id: "prop_0123456789abcdef".into(),
        state,
        host: "api.example.com".into(),
        ports: vec![443],
        reason: Some("the build downloads its schema from here".into()),
        agent: Some("writer".into()),
        invocation_id: Some("inv-1".into()),
        activation_id: Some("act-1".into()),
        actor: actor.map(str::to_string),
        command_id: command_id.map(str::to_string),
        revision,
    }
}

#[test]
fn proposals_and_reloads_round_trip_with_their_wire_names() {
    let pending = proposal(ProposalState::Pending);
    let wire = serde_json::to_value(&pending).unwrap();
    assert_eq!(wire["kind"], "proposal");
    assert_eq!(wire["state"], "pending");
    assert!(wire.get("actor").is_none());
    assert_eq!(
        serde_json::from_value::<NetworkEvent>(wire).unwrap(),
        pending
    );
    assert_eq!(pending.kind(), "proposal");
    assert!(pending.validate().is_ok());

    let approved = decided(ProposalState::Approved, Some("human"), Some("c-1"), Some(4));
    let wire = serde_json::to_value(&approved).unwrap();
    assert_eq!(wire["state"], "approved");
    assert_eq!(wire["revision"], 4);
    assert_eq!(
        serde_json::from_value::<NetworkEvent>(wire).unwrap(),
        approved
    );

    let reload = NetworkEvent::Policy {
        scope: EgressScope::Browser,
        revision: 2,
        digest: "cd".repeat(32),
        source: PolicySource::ConfigReload,
        rules: vec!["fonts.gstatic.com:443 (config)".into()],
        change: None,
        actor: Some("human".into()),
    };
    let wire = serde_json::to_value(&reload).unwrap();
    assert_eq!(wire["source"], "config_reload");
    assert_eq!(
        serde_json::from_value::<NetworkEvent>(wire).unwrap(),
        reload
    );

    // An approval's allow names the proposal; an ordinary allow omits it,
    // and a line written before proposals existed still reads.
    let change = PolicyChange {
        op: PolicyOp::Allow,
        host: "api.example.com".into(),
        ports: vec![443],
        command_id: Some("c-1".into()),
        proposal_id: Some("prop_0123456789abcdef".into()),
    };
    let wire = serde_json::to_value(&change).unwrap();
    assert_eq!(wire["proposal_id"], "prop_0123456789abcdef");
    let old: PolicyChange =
        serde_json::from_str(r#"{"op":"allow","host":"a.example","ports":[443],"command_id":"c"}"#)
            .unwrap();
    assert_eq!(old.proposal_id, None);
    assert!(serde_json::to_value(&old)
        .unwrap()
        .get("proposal_id")
        .is_none());
}

#[test]
fn only_decided_proposals_use_the_control_headroom() {
    // An Agent can make pending proposals; they must not fill the headroom
    // that explains why the record stopped. A person's decision may use it.
    assert!(!proposal(ProposalState::Pending).is_control());
    assert!(proposal(ProposalState::Approved).is_control());
    assert!(proposal(ProposalState::Rejected).is_control());
}

#[test]
fn proposal_bounds_are_enforced() {
    let bad = |event: NetworkEvent| event.validate().is_err();
    let with = |change: &dyn Fn(&mut NetworkEvent)| {
        let mut event = proposal(ProposalState::Pending);
        change(&mut event);
        event
    };
    assert!(bad(with(&|event| {
        if let NetworkEvent::Proposal { id, .. } = event {
            *id = "prop_XYZ".into();
        }
    })));
    assert!(bad(with(&|event| {
        if let NetworkEvent::Proposal { host, .. } = event {
            host.clear();
        }
    })));
    assert!(bad(with(&|event| {
        if let NetworkEvent::Proposal { ports, .. } = event {
            ports.clear();
        }
    })));
    assert!(bad(with(&|event| {
        if let NetworkEvent::Proposal { ports, .. } = event {
            *ports = (1..=17).collect();
        }
    })));
    assert!(bad(with(&|event| {
        if let NetworkEvent::Proposal { reason, .. } = event {
            *reason = Some("x".repeat(MAX_PROPOSAL_REASON_BYTES + 1));
        }
    })));
    assert!(!bad(with(&|event| {
        if let NetworkEvent::Proposal { reason, .. } = event {
            *reason = Some("x".repeat(MAX_PROPOSAL_REASON_BYTES));
        }
    })));
    assert!(bad(with(&|event| {
        if let NetworkEvent::Proposal { agent, .. } = event {
            *agent = Some("a".repeat(129));
        }
    })));
    let policy = |proposal_id: &str| NetworkEvent::Policy {
        scope: EgressScope::Session,
        revision: 2,
        digest: "ab".repeat(32),
        source: PolicySource::SessionAllow,
        rules: Vec::new(),
        change: Some(PolicyChange {
            op: PolicyOp::Allow,
            host: "api.example.com".into(),
            ports: vec![443],
            command_id: Some("c".into()),
            proposal_id: Some(proposal_id.into()),
        }),
        actor: Some("human".into()),
    };
    assert!(policy("prop_0123456789abcdef").validate().is_ok());
    assert!(bad(policy("0123456789abcdef")));
    assert!(is_proposal_id("prop_ffffffffffffffff"));
    assert!(!is_proposal_id("prop_FFFFFFFFFFFFFFFF"));
    assert!(!is_proposal_id("prop_0123456789abcde"));
}

#[cfg(unix)]
#[test]
fn a_record_keeps_proposals_in_order_and_reads_them_back() {
    use crate::execution_ownership::LegacyFormatOwnership;
    use crate::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
    use crate::turn_contract::SessionId;
    let root = tempfile::tempdir().unwrap();
    let ownership = std::sync::Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    let store = SessionExecutionStore::open(
        ownership,
        ExecutionStoreOwner {
            workspace_id: "workspace".into(),
            session_id: SessionId::new("session").unwrap(),
        },
    )
    .unwrap();
    let limits = RecordLimits {
        max_events: 1,
        max_bytes: DEFAULT_MAX_BYTES,
    };
    let mut record = NetworkRecord::open(
        store
            .component_namespace(ExecutionComponent::NetworkRecord)
            .unwrap(),
        limits,
    )
    .unwrap();
    record.append(1, proposal(ProposalState::Pending)).unwrap();
    // Past the cap an Agent's next proposal is refused, while a person's
    // decision still fits in the control headroom.
    assert!(matches!(
        record.append(2, proposal(ProposalState::Pending)),
        Err(NetworkRecordError::Full)
    ));
    assert!(matches!(
        record.append_control(2, proposal(ProposalState::Pending)),
        Err(NetworkRecordError::NotControl)
    ));
    record
        .append_control(
            3,
            decided(ProposalState::Rejected, Some("human"), Some("c-2"), None),
        )
        .unwrap();
    let lines = record.read_after(None, 10).unwrap();
    assert_eq!(lines.len(), 2);
    assert!(matches!(
        &lines[1].event,
        NetworkEvent::Proposal { state: ProposalState::Rejected, command_id: Some(command), .. } if command == "c-2"
    ));
}
