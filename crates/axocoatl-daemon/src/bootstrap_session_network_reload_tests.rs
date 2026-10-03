//! `reload_network_policy` and proposal decisions on a real daemon: the
//! configuration file it was started with is read again, validated, and
//! its allowlists reach running and new Sessions; everything else is
//! reported as needing a restart.
use super::*;
use crate::session_network::NetworkAllowRequest;
use crate::session_network_proposals::{NetworkProposalDecisionRequest, ProposalRequest};
use crate::session_network_reload::ListChange;
use axocoatl_session::execution_ownership::DataRootFormatOwnership;
use axocoatl_session::network_record::{NetworkEvent, PolicySource, ProposalState};
use std::os::unix::fs::PermissionsExt;

async fn native_session(daemon: &AxocoatlDaemon, work: &std::path::Path, name: &str) -> String {
    let workspace = daemon
        .workspace_store
        .lock()
        .await
        .register(work, Some(name))
        .unwrap();
    let DataRootFormatOwnership::Upgraded(ownership) = &daemon._data_dir_lease.ownership else {
        panic!("fresh data roots must use native ownership");
    };
    let (session, receipt) = daemon
        .session_store
        .lock()
        .await
        .create_native_with_environment(
            ownership,
            name,
            &workspace.id,
            &workspace.canonical_path,
            SessionMode::SingleAgent {
                agent_id: "conversation".into(),
            },
            vec![],
            vec![],
            None,
            None,
            false,
            true,
        )
        .unwrap();
    let _token = daemon
        .session_dispatch_lifecycles
        .retain_native_session(ownership.clone(), receipt)
        .unwrap();
    session.id.clone()
}

fn yaml(network: &str, allow: &[&str], browser: &[&str]) -> String {
    let list = |hosts: &[&str]| {
        if hosts.is_empty() {
            "[]".to_string()
        } else {
            format!(
                "[{}]",
                hosts
                    .iter()
                    .map(|host| format!("{{host: {host}}}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    };
    format!(
        "agents: []\nconsolidation:\n  enabled: false\nsandbox:\n  network: {network}\n  egress:\n    allow: {}\nbrowser:\n  allow: {}\n",
        list(allow),
        list(browser)
    )
}

fn policy_hosts(view: &crate::session_network::SessionNetworkView, scope: &str) -> Vec<String> {
    view.policies
        .iter()
        .find(|policy| policy.scope == scope)
        .map(|policy| policy.rules.iter().map(|rule| rule.text.clone()).collect())
        .unwrap_or_default()
}

async fn reload_child_body() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("axocoatl.yaml");
    std::fs::write(
        &path,
        yaml("egress", &["a.example.com"], &["docs.example.com"]),
    )
    .unwrap();
    let config = axocoatl_config::load_config(&path).await.unwrap();
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    // A daemon that was not started from a file has nothing to reload.
    let refused = daemon.reload_network_policy().await.unwrap_err();
    assert!(
        matches!(refused, DaemonError::InvalidRequest(_)),
        "{refused}"
    );
    daemon.set_config_path(&path);

    let work = tempfile::tempdir().unwrap();
    let running = native_session(&daemon, work.path(), "Running").await;
    // Opening the decision point compiles the Session's list and, apart from
    // it, the browser's declared hosts (gap 2).
    daemon.session_egress(&running).await.unwrap();
    let view = daemon.session_network(&running, None, None).await.unwrap();
    assert_eq!(
        policy_hosts(&view, "session"),
        ["a.example.com:443 (config)"]
    );
    assert_eq!(
        policy_hosts(&view, "browser"),
        ["docs.example.com:443 (config)"]
    );
    // The browser's scope takes a person's allow under egress now.
    daemon
        .allow_session_network_host(
            &running,
            NetworkAllowRequest {
                command_id: "c-browser".into(),
                scope: "browser".into(),
                host: "fonts.example.com".into(),
                ports: None,
            },
        )
        .await
        .unwrap();

    // Unchanged: nothing applied, nothing recorded.
    let report = daemon.reload_network_policy().await.unwrap();
    assert!(report.applied.is_empty(), "{report:?}");
    assert_eq!(report.unchanged.len(), 6);
    assert!(report.restart_required.is_empty() && report.revisions.is_empty());

    // A host added to each list applies to the running Session at once.
    std::fs::write(
        &path,
        yaml("egress", &["a.example.com", "b.example.com"], &[]),
    )
    .unwrap();
    let report = daemon.reload_network_policy().await.unwrap();
    assert_eq!(report.applied, ["sandbox.egress.allow", "browser.allow"]);
    assert!(report.restart_required.is_empty(), "{report:?}");
    // What each list gains and loses is named, entry by entry.
    assert_eq!(
        report.changes,
        [
            ListChange {
                key: "sandbox.egress.allow".into(),
                added: vec!["b.example.com:443 (config)".into()],
                removed: Vec::new(),
            },
            ListChange {
                key: "browser.allow".into(),
                added: Vec::new(),
                removed: vec!["docs.example.com:443 (config)".into()],
            },
        ]
    );
    let mut changed: Vec<(String, String, u64)> = report
        .revisions
        .iter()
        .map(|revision| {
            (
                revision.session_id.clone(),
                revision.scope.clone(),
                revision.revision,
            )
        })
        .collect();
    changed.sort();
    assert_eq!(
        changed,
        [
            (running.clone(), "browser".to_string(), 3),
            (running.clone(), "session".to_string(), 2),
        ]
    );
    let view = daemon.session_network(&running, None, None).await.unwrap();
    assert_eq!(
        policy_hosts(&view, "session"),
        ["a.example.com:443 (config)", "b.example.com:443 (config)"]
    );
    assert_eq!(
        policy_hosts(&view, "browser"),
        ["fonts.example.com:443 (allowed for this Session)"],
        "the Session's own allow stays"
    );
    let wire = serde_json::to_value(&view).unwrap();
    let reloads = wire["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|line| line["event"]["source"] == "config_reload")
        .count();
    assert_eq!(reloads, 2);
    // A Session opened after the reload starts from the new lists.
    let later_work = tempfile::tempdir().unwrap();
    let later = native_session(&daemon, later_work.path(), "Later").await;
    daemon.session_egress(&later).await.unwrap();
    let view = daemon.session_network(&later, None, None).await.unwrap();
    assert_eq!(policy_hosts(&view, "session").len(), 2);
    assert!(policy_hosts(&view, "browser").is_empty());

    // A changed mode is reported and not applied; the lists still are.
    std::fs::write(&path, yaml("bridge", &["c.example.com"], &[])).unwrap();
    let report = daemon.reload_network_policy().await.unwrap();
    assert_eq!(report.restart_required, ["sandbox.network"]);
    assert_eq!(report.applied, ["sandbox.egress.allow"]);
    assert_eq!(daemon.config.sandbox.network, "egress");

    // An invalid file changes nothing and says why.
    let before = daemon.network_policy.current();
    let revision = |view: &crate::session_network::SessionNetworkView| {
        view.policies
            .iter()
            .find(|policy| policy.scope == "session")
            .unwrap()
            .revision
    };
    let previous = revision(&daemon.session_network(&running, None, None).await.unwrap());
    for invalid in [
        "sandbox:\n  network: egress\n  egress:\n    allow: [{host: '*'}]\n",
        "sandbox:\n  network: everything\n",
        "agents: [\n",
    ] {
        std::fs::write(&path, invalid).unwrap();
        let error = daemon.reload_network_policy().await.unwrap_err();
        assert!(
            matches!(&error, DaemonError::InvalidRequest(message) if message.contains("nothing was changed")),
            "{error}"
        );
    }
    assert_eq!(daemon.network_policy.current(), before);
    assert_eq!(
        revision(&daemon.session_network(&running, None, None).await.unwrap()),
        previous
    );

    // A file inside a Session's Workspace, where its Agents can edit it, is
    // not reloaded while the daemon runs.
    std::fs::write(
        &path,
        yaml("egress", &["c.example.com", "upload.example.com"], &[]),
    )
    .unwrap();
    let inside = native_session(&daemon, dir.path(), "Holds the config").await;
    let refused = daemon.reload_network_policy().await.unwrap_err();
    assert!(
        matches!(&refused, DaemonError::InvalidRequest(message)
            if message.contains("inside the Workspace") && message.contains(&inside) && message.contains("nothing was changed")),
        "{refused}"
    );
    assert_eq!(daemon.network_policy.current(), before);
    assert_eq!(
        revision(&daemon.session_network(&running, None, None).await.unwrap()),
        previous
    );
    daemon.delete_session(&inside).await.unwrap();
    let report = daemon.reload_network_policy().await.unwrap();
    assert_eq!(report.applied, ["sandbox.egress.allow"]);
    assert_eq!(report.changes[0].added, ["upload.example.com:443 (config)"]);

    // Proposals: an Agent's request waits; a person decides it.
    let egress = daemon.session_egress(&running).await.unwrap();
    let request = |host: &str| ProposalRequest {
        host: host.into(),
        ports: vec![443],
        reason: "docs".into(),
        agent: "writer".into(),
        invocation_id: "inv-1".into(),
        activation_id: "act-1".into(),
    };
    let approved = egress.propose(request("api.example.com")).await.unwrap();
    let rejected = egress.propose(request("cdn.example.com")).await.unwrap();
    let view = daemon.session_network(&running, None, None).await.unwrap();
    assert_eq!(view.proposals.len(), 2);
    assert!(view
        .proposals
        .iter()
        .all(|proposal| proposal.state == ProposalState::Pending));
    let wire = serde_json::to_value(&view).unwrap();
    assert_eq!(wire["proposals"][0]["state"], "pending");
    let decision = |command: &str| NetworkProposalDecisionRequest {
        command_id: command.into(),
    };
    let decided = daemon
        .approve_session_network_proposal(&running, &approved.view.id, decision("c-approve"))
        .await
        .unwrap();
    assert_eq!(decided.state, ProposalState::Approved);
    assert!(decided.revision.is_some() && decided.digest.is_some());
    assert_eq!(*approved.outcome.borrow(), ProposalState::Approved);
    let decided = daemon
        .reject_session_network_proposal(&running, &rejected.view.id, decision("c-reject"))
        .await
        .unwrap();
    assert_eq!(decided.state, ProposalState::Rejected);
    let resent = daemon
        .approve_session_network_proposal(&running, &approved.view.id, decision("c-approve"))
        .await
        .unwrap_err();
    assert!(
        matches!(resent, DaemonError::SessionConflict(_)),
        "{resent}"
    );
    let unknown = daemon
        .approve_session_network_proposal(&running, "prop_0000000000000000", decision("c-x"))
        .await
        .unwrap_err();
    assert!(
        matches!(&unknown, DaemonError::Session(message) if message == "proposal 'prop_0000000000000000' not found"),
        "{unknown}"
    );
    let malformed = daemon
        .reject_session_network_proposal(&running, "../../x", decision("c-y"))
        .await
        .unwrap_err();
    assert!(
        matches!(malformed, DaemonError::InvalidRequest(_)),
        "{malformed}"
    );
    let missing = daemon
        .approve_session_network_proposal("missing-session", &rejected.view.id, decision("c-z"))
        .await
        .unwrap_err()
        .to_string();
    assert!(missing.contains("not found"), "{missing}");
    let view = daemon.session_network(&running, None, None).await.unwrap();
    assert!(view.events.iter().any(|line| matches!(
        &line.event,
        NetworkEvent::Policy { source: PolicySource::SessionAllow, change: Some(change), actor: Some(actor), .. }
            if change.proposal_id.as_deref() == Some(approved.view.id.as_str()) && actor == "human"
    )));
    assert!(policy_hosts(&view, "session")
        .iter()
        .any(|rule| rule.starts_with("api.example.com:443")));
    daemon.delete_session(&running).await.unwrap();
    daemon.delete_session(&later).await.unwrap();
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_reload_applies_the_files_allowlists_and_reports_what_needs_a_restart() {
    const CHILD: &str = "AXOCOATL_TEST_NETWORK_RELOAD_CHILD";
    if std::env::var_os(CHILD).is_some() {
        reload_child_body().await;
        return;
    }
    run_child(
        CHILD,
        "bootstrap::session_network_reload_tests::a_reload_applies_the_files_allowlists_and_reports_what_needs_a_restart",
    )
    .await;
}

fn routes_yaml(rule_path: &str, variable: &str) -> String {
    format!(
        "agents: []\nconsolidation:\n  enabled: false\ncredentials:\n  api: {{env: {variable}}}\n\
         sandbox:\n  network: egress\n  egress:\n    allow: [{{host: a.example.com}}]\n    routes:\n      \
         - {{host: api.example.com, credential: api, inject: {{header: Authorization, format: 'Bearer {{}}'}}, \
         rules: [{{methods: [GET], path: '{rule_path}'}}]}}\n"
    )
}

async fn routes_reload_child_body() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("axocoatl.yaml");
    std::fs::write(&path, routes_yaml("/v1/repos", "AXO_ROUTE_RELOAD_A")).unwrap();
    let config = axocoatl_config::load_config(&path).await.unwrap();
    let daemon = AxocoatlDaemon::bootstrap_headless(config).await.unwrap();
    daemon.set_config_path(&path);
    let work = tempfile::tempdir().unwrap();
    let running = native_session(&daemon, work.path(), "Routed").await;
    // The daemon's decision point has the configured route, an authority
    // for it, and lists it in the Session policy.
    let egress = daemon.session_egress(&running).await.unwrap();
    let files = egress.trust_files().unwrap().unwrap();
    assert_eq!(files.len(), 2);
    assert!(egress.authority_pem().is_some());
    let view = daemon.session_network(&running, None, None).await.unwrap();
    let session = view
        .policies
        .iter()
        .find(|policy| policy.scope == "session")
        .unwrap();
    let route = session
        .rules
        .iter()
        .find(|rule| rule.source == "route")
        .unwrap();
    assert_eq!(route.id, "route#0");
    assert_eq!(
        route.text,
        "api.example.com:443 (route#0: 1 rule, credential api, for agent)"
    );
    let revision = session.revision;

    // A changed rule and a changed credential source apply with no restart.
    std::fs::write(&path, routes_yaml("/v1/**", "AXO_ROUTE_RELOAD_B")).unwrap();
    let report = daemon.reload_network_policy().await.unwrap();
    assert_eq!(report.applied, ["sandbox.egress.routes", "credentials"]);
    assert!(report.restart_required.is_empty(), "{report:?}");
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(report.revisions.len(), 1, "{report:?}");
    assert_eq!(report.revisions[0].scope, "session");
    assert_eq!(report.revisions[0].revision, revision + 1);
    let keys: Vec<&str> = report
        .changes
        .iter()
        .map(|change| change.key.as_str())
        .collect();
    assert_eq!(keys, ["sandbox.egress.routes", "credentials"]);
    assert_eq!(report.changes[1].added, ["api: env AXO_ROUTE_RELOAD_B"]);
    assert_eq!(report.changes[1].removed, ["api: env AXO_ROUTE_RELOAD_A"]);
    // The same authority serves the changed route.
    assert!(Arc::ptr_eq(&files, &egress.trust_files().unwrap().unwrap()));
    // The record holds the reloaded policy; nothing holds a value.
    let records = daemon
        .session_network(&running, None, None)
        .await
        .unwrap()
        .events;
    assert!(records.iter().any(|line| matches!(&line.event,
        NetworkEvent::Policy { source: PolicySource::ConfigReload, rules, .. }
            if rules.iter().any(|rule| rule.starts_with("api.example.com:443 (route#0")))));

    // Without routes the Session has no trust files to mount.
    std::fs::write(
        &path,
        yaml("egress", &["a.example.com"], &[]).replace("browser:\n  allow: []\n", ""),
    )
    .unwrap();
    let report = daemon.reload_network_policy().await.unwrap();
    assert!(
        report
            .applied
            .contains(&"sandbox.egress.routes".to_string()),
        "{report:?}"
    );
    assert!(egress.trust_files().unwrap().is_none());
}

#[tokio::test]
async fn routes_and_credentials_reload_into_a_running_sessions_policy() {
    const CHILD: &str = "AXOCOATL_TEST_ROUTES_RELOAD_CHILD";
    if std::env::var_os(CHILD).is_some() {
        routes_reload_child_body().await;
        return;
    }
    run_child(
        CHILD,
        "bootstrap::session_network_reload_tests::routes_and_credentials_reload_into_a_running_sessions_policy",
    )
    .await;
}

/// Run one test body in a child process with its own data root and a fake
/// Podman, because bootstrap reads the process environment.
async fn run_child(child: &str, name: &str) {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let podman = bin.join("podman");
    std::fs::write(
        &podman,
        r#"#!/bin/sh
case "$*" in
  --version) printf 'podman version 5.0.0\n' ;;
  'machine list --format json') printf '[{"Running":true}]\n' ;;
  'info --format json') printf '{}\n' ;;
  'ps '*) ;;
  'rm '*|'volume rm '*|'network rm '*) ;;
  *) printf 'unexpected Podman command: %s\n' "$*" >&2; exit 1 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(child, "1")
            .env("AXOCOATL_DATA_DIR", root.path().join("data"))
            .env("AXOCOATL_SOCKET_PATH", root.path().join("ipc/daemon.sock"))
            .env("PATH", bin)
            .current_dir(root.path())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        result.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
