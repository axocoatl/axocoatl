//! Per-activation network evidence for the turn control plane.
//!
//! Each activation of a turn gets one `network` evidence item when its tool
//! calls' credentials opened, or were refused, connections: a one-line
//! summary such as `registry.npmjs.org:443 allowed ×3 (1.2 MB in) by node;
//! evil.test:443 refused (not_allowed) by curl` (the programs are named in
//! hardened egress Sessions, whose record holds each connection's `peer`),
//! followed by the requests made on
//! routes (`github.com POST /acme/app.git/git-receive-pack 200 (credential
//! github)`), and up to 200 of the recorded `open`, `close`, `request`,
//! `response` and `web` lines. Events are joined to an activation
//! through the tool call (`binding.invocation_id`, or a web event's
//! `invocation_id`) that the turn recorded for it. Session-level and
//! unattributed events (no credential, setup, terminals) appear only in
//! `GET /api/sessions/{id}/network`.
//!
//! The index folds each Session's record incrementally: a request reads only
//! the events appended since the last one.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use axocoatl_session::network_record::{
    Decision, NetworkEvent, NetworkLine, PeerIdentity, ResponseOutcome, MAX_READ_LIMIT,
};
use serde_json::{json, Value};

use crate::session_control_plane::{
    ControlPlaneActivationRef, ControlPlaneEvidence, EvidenceValue, SessionTurnControlPlane,
};
use crate::session_network::SessionNetworkRecords;

/// Most recorded lines kept as details for one activation.
pub const MAX_DETAIL_LINES: usize = 200;
/// Most destinations named in one summary line.
const MAX_SUMMARY_DESTINATIONS: usize = 8;
/// Most route requests named in one summary line.
const MAX_SUMMARY_REQUESTS: usize = 8;
/// Most route requests kept per tool call for the summary.
const MAX_KEPT_REQUESTS: usize = 64;
/// Most program names kept per destination for the summary.
const MAX_SUMMARY_PROGRAMS: usize = 4;

/// The file name of the program behind a connection, when its record names
/// one.
fn program_name(peer: Option<&PeerIdentity>) -> Option<String> {
    let exe = peer?.exe.as_deref()?;
    let name = exe.rsplit('/').next().filter(|name| !name.is_empty())?;
    Some(name.chars().take(64).collect())
}

/// Program names seen for one destination, bounded.
#[derive(Debug, Default, Clone)]
struct Programs {
    names: BTreeSet<String>,
    more: bool,
}

impl Programs {
    fn add(&mut self, name: String) {
        if self.names.contains(&name) {
            return;
        }
        if self.names.len() < MAX_SUMMARY_PROGRAMS {
            self.names.insert(name);
        } else {
            self.more = true;
        }
    }

    fn merge(&mut self, other: &Programs) {
        for name in &other.names {
            self.add(name.clone());
        }
        self.more |= other.more;
    }

    /// ` by curl, git` (or nothing).
    fn suffix(&self) -> String {
        if self.names.is_empty() {
            return String::new();
        }
        let mut names: Vec<&str> = self.names.iter().map(String::as_str).collect();
        if self.more {
            names.push("others");
        }
        format!(" by {}", names.join(", "))
    }
}

#[derive(Debug, Default, Clone)]
struct Allowed {
    count: u64,
    down: u64,
    up: u64,
}

#[derive(Debug, Default, Clone)]
struct InvocationNetwork {
    allowed: BTreeMap<String, Allowed>,
    refused: BTreeMap<(String, String), u64>,
    /// Programs that opened the allowed connections, per destination.
    allowed_programs: BTreeMap<String, Programs>,
    /// Programs whose connections were refused, per destination and reason.
    refused_programs: BTreeMap<(String, String), Programs>,
    web: u64,
    /// Route requests, in order, such as `github.com GET /x 200`.
    requests: Vec<String>,
    /// Route requests past [`MAX_KEPT_REQUESTS`].
    more_requests: u64,
    details: Vec<NetworkLine>,
    truncated: bool,
    last_ts_ms: u64,
}

impl InvocationNetwork {
    fn request(&mut self, text: String) {
        if self.requests.len() < MAX_KEPT_REQUESTS {
            self.requests.push(text);
        } else {
            self.more_requests += 1;
        }
    }

    fn keep(&mut self, line: &NetworkLine) {
        self.last_ts_ms = self.last_ts_ms.max(line.ts_ms);
        if self.details.len() < MAX_DETAIL_LINES {
            self.details.push(line.clone());
        } else {
            self.truncated = true;
        }
    }
}

#[derive(Debug, Default)]
struct Folded {
    last_seq: Option<u64>,
    invocations: HashMap<String, InvocationNetwork>,
    /// Allowed connection → (invocation, destination), to attach its close.
    open: HashMap<String, (String, String)>,
    /// Allowed route request (conn, seq) → (invocation, `host METHOD path`,
    /// credential name), to attach its response.
    requests: HashMap<(String, u64), (String, String, Option<String>)>,
}

fn outcome_name(outcome: ResponseOutcome) -> &'static str {
    match outcome {
        ResponseOutcome::Completed => "completed",
        ResponseOutcome::UpstreamFailed => "upstream_failed",
        ResponseOutcome::CredentialReflected => "credential_reflected",
        ResponseOutcome::EncodedResponse => "encoded_response",
        ResponseOutcome::TooLarge => "too_large",
        ResponseOutcome::ClientClosed => "client_closed",
    }
}

impl Folded {
    fn fold(&mut self, line: &NetworkLine) {
        self.last_seq = Some(line.seq);
        match &line.event {
            NetworkEvent::Open {
                conn,
                peer,
                decision,
                reason,
                host,
                port,
                binding,
                ..
            } => {
                let Some(invocation) = binding.as_ref().and_then(|b| b.invocation_id.clone())
                else {
                    return;
                };
                let destination = format!("{host}:{port}");
                let program = program_name(peer.as_ref());
                let entry = self.invocations.entry(invocation.clone()).or_default();
                match decision {
                    Decision::Allow => {
                        entry.allowed.entry(destination.clone()).or_default().count += 1;
                        if let Some(program) = program {
                            entry
                                .allowed_programs
                                .entry(destination.clone())
                                .or_default()
                                .add(program);
                        }
                        self.open.insert(conn.clone(), (invocation, destination));
                    }
                    Decision::Deny => {
                        let key = (
                            destination,
                            reason.clone().unwrap_or_else(|| "refused".into()),
                        );
                        if let Some(program) = program {
                            entry
                                .refused_programs
                                .entry(key.clone())
                                .or_default()
                                .add(program);
                        }
                        *entry.refused.entry(key).or_default() += 1;
                    }
                }
                entry.keep(line);
            }
            NetworkEvent::Close { conn, up, down, .. } => {
                let Some((invocation, destination)) = self.open.remove(conn) else {
                    return;
                };
                self.requests
                    .retain(|(request_conn, _), _| request_conn != conn);
                let entry = self.invocations.entry(invocation).or_default();
                let allowed = entry.allowed.entry(destination).or_default();
                allowed.down += down;
                allowed.up += up;
                entry.keep(line);
            }
            NetworkEvent::Request {
                conn,
                seq_in_conn,
                method,
                path,
                host,
                decision,
                reason,
                credential,
                ..
            } => {
                let Some((invocation, _)) = self.open.get(conn) else {
                    return;
                };
                let invocation = invocation.clone();
                let text = format!("{host} {method} {path}");
                let entry = self.invocations.entry(invocation.clone()).or_default();
                match decision {
                    Decision::Allow => {
                        self.requests.insert(
                            (conn.clone(), *seq_in_conn),
                            (invocation, text, credential.clone()),
                        );
                    }
                    Decision::Deny => entry.request(format!(
                        "{text} refused ({})",
                        reason.as_deref().unwrap_or("refused")
                    )),
                }
                entry.keep(line);
            }
            NetworkEvent::Response {
                conn,
                seq_in_conn,
                status,
                outcome,
                ..
            } => {
                let Some((invocation, text, credential)) =
                    self.requests.remove(&(conn.clone(), *seq_in_conn))
                else {
                    return;
                };
                let mut summary = format!("{text} {status}");
                if *outcome != ResponseOutcome::Completed {
                    summary.push_str(&format!(" [{}]", outcome_name(*outcome)));
                }
                if let Some(credential) = credential {
                    summary.push_str(&format!(" (credential {credential})"));
                }
                let entry = self.invocations.entry(invocation).or_default();
                entry.request(summary);
                entry.keep(line);
            }
            NetworkEvent::Web { invocation_id, .. } => {
                let entry = self.invocations.entry(invocation_id.clone()).or_default();
                entry.web += 1;
                entry.keep(line);
            }
            _ => {}
        }
    }
}

fn megabytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// The summary line for one activation's network activity.
fn summarize(network: &InvocationNetwork) -> String {
    let mut parts = Vec::new();
    let programs = |found: Option<&Programs>| found.map(Programs::suffix).unwrap_or_default();
    for (destination, allowed) in &network.allowed {
        parts.push(format!(
            "{destination} allowed ×{} ({} in){}",
            allowed.count,
            megabytes(allowed.down),
            programs(network.allowed_programs.get(destination))
        ));
    }
    for (key, count) in &network.refused {
        let (destination, reason) = key;
        let by = programs(network.refused_programs.get(key));
        parts.push(if *count == 1 {
            format!("{destination} refused ({reason}){by}")
        } else {
            format!("{destination} refused ×{count} ({reason}){by}")
        });
    }
    let more = parts.len().saturating_sub(MAX_SUMMARY_DESTINATIONS);
    parts.truncate(MAX_SUMMARY_DESTINATIONS);
    if more > 0 {
        parts.push(format!("{more} more"));
    }
    parts.extend(network.requests.iter().take(MAX_SUMMARY_REQUESTS).cloned());
    let more_requests =
        network.requests.len().saturating_sub(MAX_SUMMARY_REQUESTS) as u64 + network.more_requests;
    if more_requests > 0 {
        parts.push(format!(
            "{more_requests} more request{}",
            if more_requests == 1 { "" } else { "s" }
        ));
    }
    if network.web > 0 {
        parts.push(format!(
            "{} web call{}",
            network.web,
            if network.web == 1 { "" } else { "s" }
        ));
    }
    parts.join("; ")
}

fn merge(into: &mut InvocationNetwork, from: &InvocationNetwork) {
    for (destination, allowed) in &from.allowed {
        let entry = into.allowed.entry(destination.clone()).or_default();
        entry.count += allowed.count;
        entry.down += allowed.down;
        entry.up += allowed.up;
    }
    for (key, count) in &from.refused {
        *into.refused.entry(key.clone()).or_default() += count;
    }
    for (destination, programs) in &from.allowed_programs {
        into.allowed_programs
            .entry(destination.clone())
            .or_default()
            .merge(programs);
    }
    for (key, programs) in &from.refused_programs {
        into.refused_programs
            .entry(key.clone())
            .or_default()
            .merge(programs);
    }
    into.web += from.web;
    for request in &from.requests {
        into.request(request.clone());
    }
    into.more_requests += from.more_requests;
    into.last_ts_ms = into.last_ts_ms.max(from.last_ts_ms);
    into.truncated |= from.truncated;
    for line in &from.details {
        if into.details.len() < MAX_DETAIL_LINES {
            into.details.push(line.clone());
        } else {
            into.truncated = true;
        }
    }
}

/// Folded network records per Session.
#[derive(Debug, Default)]
pub struct NetworkEvidenceIndex {
    sessions: tokio::sync::Mutex<HashMap<String, Folded>>,
}

impl NetworkEvidenceIndex {
    /// Drop a Session's folded record (its record closed or was deleted).
    pub async fn forget(&self, session: &str) {
        self.sessions.lock().await.remove(session);
    }

    /// Add one `network` evidence item to each exact activation in `view`
    /// whose tool calls have network activity in the Session's record.
    pub async fn attach(
        &self,
        records: &SessionNetworkRecords,
        session: &str,
        view: &mut SessionTurnControlPlane,
    ) {
        let EvidenceValue::Available { value: invocations } = &view.invocations else {
            return;
        };
        if invocations.is_empty() {
            return;
        }
        let invocations = invocations.clone();
        let mut sessions = self.sessions.lock().await;
        let folded = sessions.entry(session.to_string()).or_default();
        loop {
            let page = match records
                .read_after(session, folded.last_seq, MAX_READ_LIMIT)
                .await
            {
                Ok(page) => page,
                Err(error) => {
                    tracing::warn!(session, %error, "reading the network record for evidence failed");
                    return;
                }
            };
            for line in &page.events {
                folded.fold(line);
            }
            if page.events.len() < MAX_READ_LIMIT {
                break;
            }
        }
        let evidence = evidence_by_activation(folded, &invocations);
        drop(sessions);
        if evidence.is_empty() {
            return;
        }
        for node in &mut view.nodes {
            for item in &mut node.activations {
                let ControlPlaneActivationRef::Exact { activation } = &item.reference else {
                    continue;
                };
                if let Some(network) = evidence.get(activation.activation_id.as_str()) {
                    item.evidence.push(network.clone());
                }
            }
        }
    }
}

/// One `network` evidence item per activation with network activity, joined
/// through the turn's recorded invocations (`invocation_id`, `activation`).
fn evidence_by_activation(
    folded: &Folded,
    invocations: &[Value],
) -> HashMap<String, ControlPlaneEvidence> {
    let mut activation_of: HashMap<&str, &str> = HashMap::new();
    for invocation in invocations {
        if let (Some(id), Some(activation)) = (
            invocation.get("invocation_id").and_then(Value::as_str),
            invocation
                .get("activation")
                .and_then(|activation| activation.get("activation_id"))
                .and_then(Value::as_str),
        ) {
            activation_of.insert(id, activation);
        }
    }
    let mut per_activation: BTreeMap<&str, InvocationNetwork> = BTreeMap::new();
    let mut ids: Vec<&String> = folded.invocations.keys().collect();
    ids.sort();
    for invocation in ids {
        if let Some(activation) = activation_of.get(invocation.as_str()) {
            merge(
                per_activation.entry(activation).or_default(),
                &folded.invocations[invocation],
            );
        }
    }
    per_activation
        .into_iter()
        .map(|(activation, network)| {
            (
                activation.to_string(),
                ControlPlaneEvidence {
                    kind: "network".into(),
                    reference: EvidenceValue::NotRecorded,
                    summary: EvidenceValue::Available {
                        value: summarize(&network),
                    },
                    recorded_at: EvidenceValue::Available {
                        value: network.last_ts_ms,
                    },
                    details: EvidenceValue::Available {
                        value: json!({
                            "events": network.details,
                            "truncated": network.truncated,
                        }),
                    },
                },
            )
        })
        .collect()
}

#[cfg(test)]
#[path = "session_network_evidence_request_tests.rs"]
mod request_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_session::network_record::{BindingKind, CloseOutcome, ConnKind, EgressBinding};

    fn binding(invocation: &str) -> EgressBinding {
        EgressBinding {
            invocation_id: Some(invocation.into()),
            activation_id: Some("act".into()),
            ..EgressBinding::new(BindingKind::Agent)
        }
    }

    fn open(
        seq: u64,
        conn: &str,
        host: &str,
        allow: bool,
        invocation: Option<&str>,
    ) -> NetworkLine {
        NetworkLine {
            v: 1,
            seq,
            ts_ms: 1000 + seq,
            event: NetworkEvent::Open {
                conn: conn.into(),
                peer: None,
                decision: if allow {
                    Decision::Allow
                } else {
                    Decision::Deny
                },
                reason: (!allow).then(|| "not_allowed".into()),
                status: (!allow).then_some(403),
                rule: None,
                host: host.into(),
                port: 443,
                conn_kind: ConnKind::Connect,
                method: None,
                path: None,
                addrs: Vec::new(),
                token: None,
                binding: invocation.map(binding),
                scope: None,
                policy_revision: None,
            },
        }
    }

    fn close(seq: u64, conn: &str, down: u64) -> NetworkLine {
        NetworkLine {
            v: 1,
            seq,
            ts_ms: 1000 + seq,
            event: NetworkEvent::Close {
                conn: conn.into(),
                ip: None,
                up: 10,
                down,
                ms: 5,
                outcome: CloseOutcome::Closed,
                error: None,
            },
        }
    }

    #[test]
    fn opens_and_closes_fold_per_tool_call_and_summarize() {
        let mut folded = Folded::default();
        for line in [
            open(1, "g1:1", "registry.npmjs.org", true, Some("tool-a")),
            close(2, "g1:1", 1_258_291),
            open(3, "g1:2", "registry.npmjs.org", true, Some("tool-a")),
            open(4, "g1:3", "evil.test", false, Some("tool-a")),
            // Unattributed: no credential.
            open(5, "g1:4", "other.test", false, None),
            close(6, "g1:2", 10),
            open(7, "g1:5", "registry.npmjs.org", true, Some("tool-b")),
        ] {
            folded.fold(&line);
        }
        assert_eq!(folded.last_seq, Some(7));
        let a = &folded.invocations["tool-a"];
        assert_eq!(
            summarize(a),
            "registry.npmjs.org:443 allowed ×2 (1.2 MB in); evil.test:443 refused (not_allowed)"
        );
        assert_eq!(a.details.len(), 5);
        assert!(!folded.invocations.contains_key(""));
        assert_eq!(folded.invocations.len(), 2);
        let mut merged = InvocationNetwork::default();
        merge(&mut merged, a);
        merge(&mut merged, &folded.invocations["tool-b"]);
        assert!(summarize(&merged).starts_with("registry.npmjs.org:443 allowed ×3"));
    }

    #[test]
    fn evidence_joins_tool_calls_to_their_activations_only() {
        let mut folded = Folded::default();
        for line in [
            open(1, "g1:1", "registry.npmjs.org", true, Some("tool-a")),
            close(2, "g1:1", 2048),
            open(3, "g1:2", "evil.test", false, Some("tool-b")),
            open(4, "g1:3", "elsewhere.test", false, Some("tool-other-turn")),
        ] {
            folded.fold(&line);
        }
        let invocations = vec![
            json!({"invocation_id": "tool-a", "activation": {"activation_id": "act-1"}}),
            json!({"invocation_id": "tool-b", "activation": {"activation_id": "act-1"}}),
            json!({"invocation_id": "tool-c", "activation": {"activation_id": "act-2"}}),
        ];
        let evidence = evidence_by_activation(&folded, &invocations);
        assert_eq!(evidence.len(), 1);
        let item = &evidence["act-1"];
        assert_eq!(item.kind, "network");
        assert_eq!(
            item.summary,
            EvidenceValue::Available {
                value: "registry.npmjs.org:443 allowed ×1 (2.0 KB in); evil.test:443 refused (not_allowed)"
                    .into()
            }
        );
        let EvidenceValue::Available { value: details } = &item.details else {
            panic!()
        };
        assert_eq!(details["events"].as_array().unwrap().len(), 3);
        assert_eq!(details["truncated"], false);
    }

    /// In a hardened egress Session the summary names the programs behind
    /// each destination, bounded; identities without a path add nothing.
    #[test]
    fn summaries_name_the_programs_behind_connections() {
        let with_peer = |mut line: NetworkLine, exe: Option<&str>| {
            if let NetworkEvent::Open { peer, .. } = &mut line.event {
                *peer = Some(PeerIdentity {
                    pid: Some(7),
                    uid: Some(1000),
                    exe: exe.map(str::to_string),
                    error: exe.is_none().then(|| "foreign_namespace".into()),
                    ..PeerIdentity::default()
                });
            }
            line
        };
        let mut folded = Folded::default();
        for line in [
            with_peer(
                open(1, "g1:1", "registry.npmjs.org", true, Some("tool-a")),
                Some("/usr/local/bin/node"),
            ),
            close(2, "g1:1", 2048),
            with_peer(
                open(3, "g1:2", "registry.npmjs.org", true, Some("tool-a")),
                Some("/usr/local/bin/node"),
            ),
            with_peer(
                open(4, "g1:3", "registry.npmjs.org", true, Some("tool-a")),
                Some("/usr/bin/curl"),
            ),
            with_peer(
                open(5, "g1:4", "registry.npmjs.org", true, Some("tool-a")),
                None,
            ),
            with_peer(
                open(6, "g1:5", "evil.test", false, Some("tool-a")),
                Some("/usr/bin/curl"),
            ),
            open(7, "g1:6", "plain.test", false, Some("tool-a")),
        ] {
            folded.fold(&line);
        }
        assert_eq!(
            summarize(&folded.invocations["tool-a"]),
            "registry.npmjs.org:443 allowed ×4 (2.0 KB in) by curl, node; \
             evil.test:443 refused (not_allowed) by curl; plain.test:443 refused (not_allowed)"
        );
        let mut many = Programs::default();
        for name in ["a", "b", "c", "d", "e", "a"] {
            many.add(name.into());
        }
        assert_eq!(many.suffix(), " by a, b, c, d, others");
        let mut merged = InvocationNetwork::default();
        merge(&mut merged, &folded.invocations["tool-a"]);
        merge(&mut merged, &folded.invocations["tool-a"]);
        assert!(
            summarize(&merged)
                .starts_with("registry.npmjs.org:443 allowed ×8 (4.0 KB in) by curl, node;"),
            "{}",
            summarize(&merged)
        );
        assert_eq!(program_name(None), None);
        assert_eq!(
            program_name(Some(&PeerIdentity {
                exe: Some("/".into()),
                ..PeerIdentity::default()
            })),
            None
        );
    }

    #[test]
    fn details_are_bounded() {
        let mut folded = Folded::default();
        for seq in 1..=(MAX_DETAIL_LINES as u64 + 10) {
            folded.fold(&open(
                seq,
                &format!("g1:{seq}"),
                "a.test",
                false,
                Some("tool-a"),
            ));
        }
        let a = &folded.invocations["tool-a"];
        assert_eq!(a.details.len(), MAX_DETAIL_LINES);
        assert!(a.truncated);
        assert_eq!(summarize(a), "a.test:443 refused ×210 (not_allowed)");
    }
}
