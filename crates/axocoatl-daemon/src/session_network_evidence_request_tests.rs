//! Route requests in an activation's network evidence.

use super::*;
use axocoatl_session::network_record::{BindingKind, ConnKind, EgressBinding};

fn line(seq: u64, event: NetworkEvent) -> NetworkLine {
    NetworkLine {
        v: 1,
        seq,
        ts_ms: 1000 + seq,
        event,
    }
}

fn route_open(seq: u64, conn: &str, invocation: &str) -> NetworkLine {
    line(
        seq,
        NetworkEvent::Open {
            conn: conn.into(),
            decision: Decision::Allow,
            reason: None,
            status: None,
            rule: Some("route#0".into()),
            host: "github.com".into(),
            port: 443,
            conn_kind: ConnKind::Connect,
            method: None,
            path: None,
            addrs: vec!["140.82.112.3".into()],
            token: None,
            binding: Some(EgressBinding {
                invocation_id: Some(invocation.into()),
                activation_id: Some("act".into()),
                ..EgressBinding::new(BindingKind::Agent)
            }),
            scope: None,
            policy_revision: Some(1),
        },
    )
}

fn request(seq: u64, conn: &str, n: u64, method: &str, path: &str, allow: bool) -> NetworkLine {
    line(
        seq,
        NetworkEvent::Request {
            conn: conn.into(),
            seq_in_conn: n,
            method: method.into(),
            path: path.into(),
            host: "github.com".into(),
            rule: allow.then(|| "route#0.rules[1]".into()),
            decision: if allow {
                Decision::Allow
            } else {
                Decision::Deny
            },
            reason: (!allow).then(|| "route_denied".into()),
            credential: allow.then(|| "github".into()),
        },
    )
}

fn response(seq: u64, conn: &str, n: u64, status: u16, outcome: ResponseOutcome) -> NetworkLine {
    line(
        seq,
        NetworkEvent::Response {
            conn: conn.into(),
            seq_in_conn: n,
            status,
            up: 100,
            down: 2000,
            ms: 40,
            outcome,
        },
    )
}

#[test]
fn route_requests_are_summarized_per_tool_call() {
    let mut folded = Folded::default();
    for event in [
        route_open(1, "g1:1", "tool-a"),
        request(2, "g1:1", 1, "GET", "/acme/app.git/info/refs", true),
        response(3, "g1:1", 1, 200, ResponseOutcome::Completed),
        request(4, "g1:1", 2, "POST", "/acme/app.git/git-receive-pack", true),
        response(5, "g1:1", 2, 200, ResponseOutcome::Completed),
        request(6, "g1:1", 3, "DELETE", "/acme/app.git", false),
        request(7, "g1:1", 4, "GET", "/acme/app.git/archive", true),
        response(8, "g1:1", 4, 502, ResponseOutcome::CredentialReflected),
        // A request on a connection the index never saw opened.
        request(9, "g9:9", 1, "GET", "/elsewhere", true),
    ] {
        folded.fold(&event);
    }
    let a = &folded.invocations["tool-a"];
    assert_eq!(
        summarize(a),
        "github.com:443 allowed ×1 (0 B in); \
         github.com GET /acme/app.git/info/refs 200 (credential github); \
         github.com POST /acme/app.git/git-receive-pack 200 (credential github); \
         github.com DELETE /acme/app.git refused (route_denied); \
         github.com GET /acme/app.git/archive 502 [credential_reflected] (credential github)"
    );
    // Every request and response line is kept as a detail.
    assert_eq!(a.details.len(), 8);
    assert_eq!(folded.invocations.len(), 1);
}

#[test]
fn many_route_requests_are_counted_not_listed() {
    let mut folded = Folded::default();
    folded.fold(&route_open(1, "g1:1", "tool-a"));
    let mut seq = 1;
    for n in 1..=(MAX_KEPT_REQUESTS as u64 + 5) {
        seq += 1;
        folded.fold(&request(seq, "g1:1", n, "GET", "/x", true));
        seq += 1;
        folded.fold(&response(seq, "g1:1", n, 200, ResponseOutcome::Completed));
    }
    let a = &folded.invocations["tool-a"];
    let summary = summarize(a);
    assert_eq!(
        summary.matches("github.com GET /x 200").count(),
        MAX_SUMMARY_REQUESTS
    );
    let more = MAX_KEPT_REQUESTS - MAX_SUMMARY_REQUESTS + 5;
    assert!(
        summary.ends_with(&format!("{more} more requests")),
        "{summary}"
    );
    let mut merged = InvocationNetwork::default();
    merge(&mut merged, a);
    assert_eq!(summarize(&merged), summary);
}
