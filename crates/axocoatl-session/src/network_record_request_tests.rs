//! `request` and `response` events: wire form, bounds and storage.

use super::*;
use crate::execution_ownership::LegacyFormatOwnership;
use crate::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
use crate::turn_contract::SessionId;
use std::sync::Arc;

fn request(path: &str) -> NetworkEvent {
    NetworkEvent::Request {
        conn: "g1:7".into(),
        seq_in_conn: 1,
        method: "POST".into(),
        path: path.into(),
        host: "github.com".into(),
        rule: Some("route#0.rules[1]".into()),
        decision: Decision::Allow,
        reason: None,
        credential: Some("github".into()),
    }
}

fn response() -> NetworkEvent {
    NetworkEvent::Response {
        conn: "g1:7".into(),
        seq_in_conn: 1,
        status: 200,
        up: 512,
        down: 2048,
        ms: 31,
        outcome: ResponseOutcome::Completed,
        cookies_dropped: 0,
    }
}

#[test]
fn request_and_response_have_a_stable_wire_form() {
    let line = serde_json::to_value(request("/acme/app.git/git-receive-pack")).unwrap();
    assert_eq!(
        line,
        serde_json::json!({
            "kind": "request",
            "conn": "g1:7",
            "seq_in_conn": 1,
            "method": "POST",
            "path": "/acme/app.git/git-receive-pack",
            "host": "github.com",
            "rule": "route#0.rules[1]",
            "decision": "allow",
            "credential": "github",
        })
    );
    let refused = NetworkEvent::Request {
        conn: "g1:7".into(),
        seq_in_conn: 2,
        method: "DELETE".into(),
        path: "/acme/app.git".into(),
        host: "github.com".into(),
        rule: None,
        decision: Decision::Deny,
        reason: Some("route_denied".into()),
        credential: None,
    };
    assert_eq!(
        serde_json::to_value(&refused).unwrap(),
        serde_json::json!({
            "kind": "request", "conn": "g1:7", "seq_in_conn": 2, "method": "DELETE",
            "path": "/acme/app.git", "host": "github.com", "decision": "deny",
            "reason": "route_denied",
        })
    );
    assert_eq!(
        serde_json::to_value(response()).unwrap(),
        serde_json::json!({
            "kind": "response", "conn": "g1:7", "seq_in_conn": 1, "status": 200,
            "up": 512, "down": 2048, "ms": 31, "outcome": "completed",
        })
    );
    let mut dropped = response();
    if let NetworkEvent::Response {
        cookies_dropped, ..
    } = &mut dropped
    {
        *cookies_dropped = 2;
    }
    let line = serde_json::to_value(&dropped).unwrap();
    assert_eq!(line["cookies_dropped"], 2);
    assert_eq!(
        serde_json::from_value::<NetworkEvent>(line).unwrap(),
        dropped
    );
    for outcome in [
        (ResponseOutcome::UpstreamFailed, "upstream_failed"),
        (ResponseOutcome::CredentialReflected, "credential_reflected"),
        (ResponseOutcome::EncodedResponse, "encoded_response"),
        (ResponseOutcome::TooLarge, "too_large"),
        (ResponseOutcome::ClientClosed, "client_closed"),
    ] {
        assert_eq!(serde_json::to_value(outcome.0).unwrap(), outcome.1);
    }
    assert_eq!(request("/").kind(), "request");
    assert_eq!(response().kind(), "response");
    assert!(!request("/").is_control() && !response().is_control());
    // A value field is not part of the event: an unknown field is refused.
    assert!(serde_json::from_value::<NetworkEvent>(serde_json::json!({
        "kind": "request", "conn": "g1:7", "seq_in_conn": 1, "method": "GET", "path": "/",
        "host": "github.com", "decision": "allow", "authorization": "Bearer x",
    }))
    .is_err());
}

#[test]
fn request_events_are_bounded() {
    request("/a").validate().unwrap();
    response().validate().unwrap();
    let long_path = format!("/{}", "a".repeat(MAX_RECORDED_PATH_CHARS));
    assert!(request(&long_path).validate().is_err());
    let mut event = request("/a");
    if let NetworkEvent::Request { method, .. } = &mut event {
        *method = "M".repeat(MAX_RECORDED_METHOD_CHARS + 1);
    }
    assert!(event.validate().is_err());
    let mut event = request("/a");
    if let NetworkEvent::Request { host, .. } = &mut event {
        host.clear();
    }
    assert!(event.validate().is_err());
    let mut event = request("/a");
    if let NetworkEvent::Request { credential, .. } = &mut event {
        *credential = Some("c".repeat(MAX_RECORDED_REQUEST_LABEL_CHARS + 1));
    }
    assert!(event.validate().is_err());
    let mut event = response();
    if let NetworkEvent::Response { conn, .. } = &mut event {
        *conn = "c".repeat(33);
    }
    assert!(event.validate().is_err());
}

#[test]
fn request_events_are_stored_and_read_back_in_order() {
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
    let namespace = || {
        store
            .component_namespace(ExecutionComponent::NetworkRecord)
            .unwrap()
    };
    let mut record = NetworkRecord::open(namespace(), RecordLimits::default()).unwrap();
    record
        .append(1, request("/acme/app.git/git-receive-pack"))
        .unwrap();
    record.append(2, response()).unwrap();
    record.sync().unwrap();
    drop(record);
    let reopened = NetworkRecord::open(namespace(), RecordLimits::default()).unwrap();
    let lines = reopened.read_after(None, 10).unwrap();
    let kinds: Vec<&str> = lines.iter().map(|line| line.event.kind()).collect();
    assert_eq!(kinds, ["request", "response"]);
    assert_eq!(lines[0].event, request("/acme/app.git/git-receive-pack"));
    assert_eq!(lines[1].event, response());
}
