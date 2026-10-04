//! Appending to a Session's network record stays cheap. This runs in a test
//! binary of its own: the library's tests make thousands of synced writes,
//! and on macOS a full sync stalls every other write to the volume, so a
//! timing taken beside them measures their syncs, not these appends.
#![cfg(unix)]

use std::sync::Arc;

use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::execution_ownership::LegacyFormatOwnership;
use axocoatl_session::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
use axocoatl_session::network_record::{
    BindingKind, ConnKind, Decision, EgressBinding, EgressScope, NetworkEvent, NetworkRecord,
};
use axocoatl_session::turn_contract::SessionId;

fn open_event(id: u64) -> NetworkEvent {
    NetworkEvent::Open {
        conn: format!("g1:{id}"),
        peer: None,
        decision: Decision::Allow,
        reason: None,
        status: None,
        rule: Some("preset:npm/registry.npmjs.org".into()),
        host: "registry.npmjs.org".into(),
        port: 443,
        conn_kind: ConnKind::Connect,
        method: None,
        path: None,
        addrs: vec!["104.16.0.1".into()],
        token: Some("0123456789abcdef".into()),
        binding: Some(EgressBinding {
            invocation_id: Some("inv-1".into()),
            activation_id: Some("act-1".into()),
            agent: Some("writer".into()),
            ..EgressBinding::new(BindingKind::Agent)
        }),
        scope: Some(EgressScope::Session),
        policy_revision: Some(1),
    }
}

#[test]
fn ten_thousand_appends_finish_quickly() {
    let root = tempfile::tempdir().unwrap();
    let ownership = Arc::new(
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
    let mut record = NetworkRecord::open(
        store
            .component_namespace(ExecutionComponent::NetworkRecord)
            .unwrap(),
    )
    .unwrap();
    let started = std::time::Instant::now();
    for id in 0..10_000 {
        record.append(id, open_event(id)).unwrap();
    }
    record.sync().unwrap();
    let elapsed = started.elapsed();
    eprintln!("network record: 10000 appends + 1 sync in {elapsed:?}");
    // The 2 s bound is for an optimized build (measured about 0.43 s on an
    // M-series Mac); unoptimized serialization alone takes about 2 s.
    let bound = if cfg!(debug_assertions) { 8 } else { 2 };
    assert!(
        elapsed < std::time::Duration::from_secs(bound),
        "{elapsed:?}"
    );
    let tail = record.read_after(Some(9_000), 1000).unwrap();
    assert_eq!(tail.len(), 1000);
    assert_eq!(tail.last().unwrap().seq, 10_000);
}
