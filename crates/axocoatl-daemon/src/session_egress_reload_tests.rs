//! The browser's scope in a Session's own decision point (gap 2) and live
//! configuration reloads (`axocoatl network reload`).
use super::tests::{attach_sidecar, open, sidecar_open, wait_for, FakeRecord, FakeResolver};
use super::*;
use axocoatl_config::{BrowserConfigYaml, EgressCidrYaml, EgressConfigYaml, EgressHostYaml};
use axocoatl_exec::egress::protocol::{DaemonFrame, SidecarFrame};
use axocoatl_isolation::egress::CloseOutcome as WireOutcome;

fn host(name: &str) -> EgressAllowYaml {
    EgressAllowYaml::Host(EgressHostYaml {
        host: name.into(),
        ports: None,
    })
}

fn daemon_config(allow: &[&str], browser: Option<&[&str]>) -> AxocoatlConfig {
    let mut config = AxocoatlConfig::default();
    config.sandbox.network = "egress".into();
    config.sandbox.egress = Some(EgressConfigYaml {
        allow: allow.iter().map(|name| host(name)).collect(),
        ..Default::default()
    });
    config.browser = browser.map(|hosts| BrowserConfigYaml {
        allow: hosts.iter().map(|name| host(name)).collect(),
        ..Default::default()
    });
    config
}

fn resolver() -> Arc<FakeResolver> {
    FakeResolver::with(&[
        ("a.test", &["93.184.216.1"]),
        ("b.test", &["93.184.216.2"]),
        ("c.test", &["93.184.216.3"]),
        ("docs.test", &["93.184.216.4"]),
        ("z.test", &["93.184.216.26"]),
        ("internal.test", &["10.1.2.3"]),
    ])
}

async fn opened(
    config: EgressPolicyConfig,
    record: Arc<FakeRecord>,
) -> (Arc<SessionEgress>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let egress = SessionEgress::open(
        "ses-1",
        config,
        record,
        resolver(),
        Some(SecureDir::open(dir.path()).unwrap()),
    )
    .await
    .unwrap();
    (egress, dir)
}

async fn hash_for(egress: &SessionEgress, spec: GrantSpec) -> (EgressGrant, String) {
    let grant = egress.grant(spec).await.unwrap();
    let url = match &grant.env_file {
        Some(path) => std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("HTTPS_PROXY=").map(str::to_string))
            .unwrap(),
        None => grant
            .proxy_url_for_stdin
            .as_ref()
            .unwrap()
            .expose()
            .to_string(),
    };
    let token = url
        .trim_start_matches("http://axo:")
        .trim_end_matches("@127.0.0.1:3128")
        .to_string();
    (grant, credential_hash(&token))
}

fn agent() -> GrantSpec {
    GrantSpec {
        invocation_id: Some("inv-1".into()),
        activation_id: Some("act-1".into()),
        agent: Some("writer".into()),
        ..GrantSpec::new(GrantKind::Agent)
    }
}

fn allowed(decision: &Decision) -> bool {
    matches!(decision, Decision::Allow { .. })
}

fn reloads(record: &FakeRecord) -> Vec<(EgressScope, u64, Option<String>)> {
    record
        .events()
        .into_iter()
        .filter_map(|event| match event {
            NetworkEvent::Policy {
                scope,
                revision,
                source: PolicySource::ConfigReload,
                actor,
                ..
            } => Some((scope, revision, actor)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn an_egress_sessions_decision_point_holds_the_browser_scope_apart_from_its_own() {
    // With `browser:` configured, the Session's decision point compiles three
    // scopes and records each.
    let config = EgressPolicyConfig::from_config(&daemon_config(&["a.test"], Some(&["docs.test"])));
    assert_eq!(
        config.browser,
        Some((vec![host("docs.test")], Vec::<String>::new()))
    );
    let record = Arc::new(FakeRecord::default());
    let (egress, _dir) = opened(config, record.clone()).await;
    let mut scopes: Vec<&str> = egress
        .policy_views()
        .iter()
        .map(|view| match view.scope.as_str() {
            "browser" => "browser",
            "session" => "session",
            "provisioning" => "provisioning",
            other => panic!("{other}"),
        })
        .collect();
    scopes.sort_unstable();
    assert_eq!(scopes, ["browser", "provisioning", "session"]);
    let recorded: Vec<EgressScope> = record
        .events()
        .iter()
        .filter_map(|event| match event {
            NetworkEvent::Policy {
                scope,
                source: PolicySource::Config,
                ..
            } => Some(*scope),
            _ => None,
        })
        .collect();
    assert_eq!(
        recorded,
        [
            EgressScope::Session,
            EgressScope::Provisioning,
            EgressScope::Browser
        ]
    );

    // A browser credential matches only browser rules, an Agent's only the
    // Session's.
    let (_browser, browser) = hash_for(
        &egress,
        GrantSpec {
            invocation_id: Some("inv-b".into()),
            ..GrantSpec::new(GrantKind::Browser)
        },
    )
    .await;
    let (_agent, agent_hash) = hash_for(&egress, agent()).await;
    for (id, hash, target, expected) in [
        (1, &browser, "docs.test", true),
        (2, &browser, "a.test", false),
        (3, &agent_hash, "a.test", true),
        (4, &agent_hash, "docs.test", false),
    ] {
        let decision = egress.decide(open(id, target, 443, Some(hash))).await;
        assert_eq!(allowed(&decision), expected, "{target}: {decision:?}");
    }
    type Opened = (
        String,
        Option<EgressScope>,
        Option<BindingKind>,
        Option<u64>,
    );
    let opens: Vec<Opened> = record
        .events()
        .into_iter()
        .filter_map(|event| match event {
            NetworkEvent::Open {
                host,
                scope,
                binding,
                policy_revision,
                ..
            } => Some((
                host,
                scope,
                binding.map(|binding| binding.kind),
                policy_revision,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        opens[0],
        (
            "docs.test".to_string(),
            Some(EgressScope::Browser),
            Some(BindingKind::Browser),
            Some(1)
        )
    );
    assert_eq!(opens[3].1, Some(EgressScope::Session));

    // A person's allow for the browser does not widen the Session's list.
    egress
        .allow(EgressScope::Browser, "b.test", None, "human", "c-browser")
        .await
        .unwrap();
    assert!(!allowed(
        &egress
            .decide(open(5, "b.test", 443, Some(&agent_hash)))
            .await
    ));
    assert!(allowed(
        &egress.decide(open(6, "b.test", 443, Some(&browser))).await
    ));

    // Without `browser:` there is no browser scope, and a browser credential
    // cannot be minted.
    let without = EgressPolicyConfig::from_config(&daemon_config(&["a.test"], None));
    assert_eq!(without.browser, None);
    let (plain, _dir) = opened(without, Arc::new(FakeRecord::default())).await;
    assert!(plain.policy(EgressScope::Browser).is_none());
    assert!(plain
        .grant(GrantSpec::new(GrantKind::Browser))
        .await
        .is_err());
}

#[tokio::test]
async fn a_reload_adds_a_host_and_new_connections_use_it_at_once() {
    let record = Arc::new(FakeRecord::default());
    let started = daemon_config(&["a.test"], Some(&["docs.test"]));
    let (egress, _dir) = opened(EgressPolicyConfig::from_config(&started), record.clone()).await;
    let (_grant, hash) = hash_for(&egress, agent()).await;
    assert!(!allowed(
        &egress.decide(open(1, "b.test", 443, Some(&hash))).await
    ));

    let next = EgressPolicyConfig::from_config(&daemon_config(
        &["a.test", "b.test"],
        Some(&["docs.test"]),
    ));
    let reloaded = egress
        .reload_config(next.clone(), "human")
        .await
        .unwrap()
        .changed;
    assert_eq!(reloaded.len(), 1, "only the session scope changed");
    assert_eq!(
        (reloaded[0].scope, reloaded[0].revision, reloaded[0].closed),
        (EgressScope::Session, 2, 0)
    );
    assert_eq!(
        reloaded[0].digest,
        egress.policy(EgressScope::Session).unwrap().digest()
    );
    assert_eq!(egress.policy_config(), next);
    assert!(allowed(
        &egress.decide(open(2, "b.test", 443, Some(&hash))).await
    ));
    assert_eq!(
        reloads(&record),
        [(EgressScope::Session, 2, Some("human".to_string()))]
    );
    // The same lists again change nothing and record nothing.
    assert_eq!(
        egress.reload_config(next, "human").await.unwrap(),
        ConfigReload::default()
    );
    assert_eq!(reloads(&record).len(), 1);
    // A person's allow after the reload compiles against the new lists.
    let (revision, _) = egress
        .allow(EgressScope::Session, "c.test", None, "human", "c-1")
        .await
        .unwrap();
    assert_eq!(revision, 3);
    let session = egress.policy(EgressScope::Session).unwrap();
    assert!(session.match_name("b.test", 443).is_some());
    assert!(session.match_name("c.test", 443).is_some());
}

#[tokio::test]
async fn a_reload_that_removes_a_host_closes_its_open_connections() {
    let record = Arc::new(FakeRecord::default());
    let started = daemon_config(&["a.test", "b.test"], None);
    let (egress, _dir) = opened(EgressPolicyConfig::from_config(&started), record.clone()).await;
    let (mut sidecar, _task) = attach_sidecar(&egress, 4).await;
    let (_grant, hash) = hash_for(&egress, agent()).await;
    egress
        .allow(EgressScope::Session, "c.test", None, "human", "c-1")
        .await
        .unwrap();
    for (id, target) in [(1, "a.test"), (2, "b.test"), (3, "c.test")] {
        sidecar.send(sidecar_open(id, target, 443, &hash)).await;
        assert!(
            matches!(sidecar.frame().await, DaemonFrame::Allow { id: answered, .. } if answered == id),
            "{target}"
        );
    }

    // b.test leaves the list: its connection is closed. a.test keeps its
    // rule, and this Session's own allow of c.test stays.
    let reloaded = egress
        .reload_config(
            EgressPolicyConfig::from_config(&daemon_config(&["a.test"], None)),
            "human",
        )
        .await
        .unwrap()
        .changed;
    assert_eq!(
        (reloaded[0].scope, reloaded[0].revision, reloaded[0].closed),
        (EgressScope::Session, 3, 1)
    );
    assert_eq!(sidecar.frame().await, DaemonFrame::Revoke { ids: vec![2] });
    sidecar
        .send(SidecarFrame::Close {
            id: 2,
            ip: Some("93.184.216.2".parse().unwrap()),
            up: 10,
            down: 20,
            ms: 30,
            outcome: WireOutcome::Revoked,
            error: None,
        })
        .await;
    wait_for(|| {
        record.events().iter().any(|event| {
            matches!(event, NetworkEvent::Close { conn, outcome: CloseOutcome::Revoked, .. } if conn == "g4:2")
        })
    })
    .await;
    // The record says why: the reload's policy comes before the close.
    let events = record.events();
    let reload = events
        .iter()
        .position(|event| {
            matches!(
                event,
                NetworkEvent::Policy {
                    source: PolicySource::ConfigReload,
                    ..
                }
            )
        })
        .unwrap();
    let close = events
        .iter()
        .position(|event| matches!(event, NetworkEvent::Close { conn, .. } if conn == "g4:2"))
        .unwrap();
    assert!(reload < close);
    // New connections follow the new list.
    sidecar.send(sidecar_open(4, "b.test", 443, &hash)).await;
    assert!(matches!(
        sidecar.frame().await,
        DaemonFrame::Deny { id: 4, .. }
    ));
    sidecar.send(sidecar_open(5, "c.test", 443, &hash)).await;
    assert!(matches!(
        sidecar.frame().await,
        DaemonFrame::Allow { id: 5, .. }
    ));

    // A rule whose position moves gets another id. Its connections are
    // closed too, fail-closed, and reconnect under the new rule.
    let reloaded = egress
        .reload_config(
            EgressPolicyConfig::from_config(&daemon_config(&["z.test", "a.test"], None)),
            "human",
        )
        .await
        .unwrap()
        .changed;
    assert_eq!(reloaded[0].closed, 1);
    assert_eq!(sidecar.frame().await, DaemonFrame::Revoke { ids: vec![1] });
}

#[tokio::test]
async fn removing_a_private_range_closes_the_scopes_open_connections() {
    let record = Arc::new(FakeRecord::default());
    let mut started = daemon_config(&["internal.test", "a.test"], None);
    started
        .sandbox
        .egress
        .as_mut()
        .unwrap()
        .private_destinations = vec!["10.0.0.0/8".into()];
    let (egress, _dir) = opened(EgressPolicyConfig::from_config(&started), record.clone()).await;
    let (mut sidecar, _task) = attach_sidecar(&egress, 2).await;
    let (_grant, hash) = hash_for(&egress, agent()).await;
    sidecar
        .send(sidecar_open(1, "internal.test", 443, &hash))
        .await;
    assert!(matches!(
        sidecar.frame().await,
        DaemonFrame::Allow { id: 1, .. }
    ));
    sidecar.send(sidecar_open(2, "a.test", 443, &hash)).await;
    assert!(matches!(
        sidecar.frame().await,
        DaemonFrame::Allow { id: 2, .. }
    ));
    let mut next = started.clone();
    next.sandbox
        .egress
        .as_mut()
        .unwrap()
        .private_destinations
        .clear();
    let reloaded = egress
        .reload_config(EgressPolicyConfig::from_config(&next), "human")
        .await
        .unwrap()
        .changed;
    assert_eq!(reloaded[0].closed, 2);
    assert_eq!(
        sidecar.frame().await,
        DaemonFrame::Revoke { ids: vec![1, 2] }
    );
    sidecar
        .send(sidecar_open(3, "internal.test", 443, &hash))
        .await;
    assert!(matches!(
        sidecar.frame().await,
        DaemonFrame::Deny { id: 3, .. }
    ));
}

#[tokio::test]
async fn an_invalid_list_changes_nothing() {
    let record = Arc::new(FakeRecord::default());
    let started =
        EgressPolicyConfig::from_config(&daemon_config(&["a.test"], Some(&["docs.test"])));
    let (egress, _dir) = opened(started.clone(), record.clone()).await;
    let before = record.events().len();
    let digest = egress
        .policy(EgressScope::Session)
        .unwrap()
        .digest()
        .to_string();
    for invalid in [
        EgressPolicyConfig {
            session_allow: vec![host("*")],
            ..started.clone()
        },
        EgressPolicyConfig {
            session_allow: vec![EgressAllowYaml::Cidr(EgressCidrYaml {
                cidr: "10.0.0.0/8".into(),
                ports: None,
            })],
            ..started.clone()
        },
        EgressPolicyConfig {
            browser: Some((
                vec![EgressAllowYaml::Preset("no-such-preset".into())],
                Vec::new(),
            )),
            ..started.clone()
        },
    ] {
        let error = egress.reload_config(invalid, "human").await.unwrap_err();
        assert!(matches!(error, EgressPolicyError::Invalid(_)), "{error:?}");
    }
    assert_eq!(record.events().len(), before);
    assert_eq!(egress.policy_config(), started);
    assert_eq!(
        egress.policy(EgressScope::Session).unwrap().digest(),
        digest
    );
}

#[tokio::test]
async fn reopening_with_the_reloaded_file_reproduces_the_digest() {
    let record = Arc::new(FakeRecord::default());
    let started = daemon_config(&["a.test"], Some(&["docs.test"]));
    let (egress, _dir) = opened(EgressPolicyConfig::from_config(&started), record.clone()).await;
    egress
        .allow(
            EgressScope::Session,
            "c.test",
            Some(vec![443, 8443]),
            "human",
            "c-1",
        )
        .await
        .unwrap();
    let next = daemon_config(&["a.test", "b.test"], Some(&[]));
    let reloaded = egress
        .reload_config(EgressPolicyConfig::from_config(&next), "human")
        .await
        .unwrap()
        .changed;
    assert_eq!(reloaded.len(), 2);
    let views = egress.policy_views();
    let lines = record.events().len();
    drop(egress);

    // A daemon restarted with the edited file replays the record: the same
    // policies, revisions and digests, and nothing new recorded.
    let (reopened, _dir) = opened(EgressPolicyConfig::from_config(&next), record.clone()).await;
    assert_eq!(reopened.policy_views(), views);
    assert_eq!(record.events().len(), lines);
    let session = reopened.policy(EgressScope::Session).unwrap();
    assert!(
        session.match_name("c.test", 8443).is_some(),
        "the Session's allow is replayed"
    );

    // One restarted with the old file records the old policy again, as a
    // configuration change.
    let (old, _dir) = opened(EgressPolicyConfig::from_config(&started), record.clone()).await;
    let revisions: HashMap<String, u64> = old
        .policy_views()
        .into_iter()
        .map(|view| (view.scope, view.revision))
        .collect();
    assert_eq!(revisions["session"], 4);
    assert_eq!(revisions["browser"], 3);
    assert_eq!(revisions["provisioning"], 1);
}

/// Resolves only after the test lets it, so a reload can land between the
/// allowlist check and the connection's registration.
#[derive(Debug)]
struct GatedResolver {
    gate: tokio::sync::Semaphore,
    asked: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl EgressResolver for GatedResolver {
    async fn resolve(&self, _host: &str, _port: u16) -> Result<Vec<IpAddr>, String> {
        self.asked.notify_one();
        let _permit = self
            .gate
            .acquire()
            .await
            .map_err(|error| error.to_string())?;
        Ok(vec!["93.184.216.1".parse().unwrap()])
    }
}

#[tokio::test]
async fn a_rule_id_that_a_reload_gave_another_host_does_not_admit_a_connection_in_flight() {
    let record = Arc::new(FakeRecord::default());
    let resolver = Arc::new(GatedResolver {
        gate: tokio::sync::Semaphore::new(0),
        asked: tokio::sync::Notify::new(),
    });
    let dir = tempfile::tempdir().unwrap();
    let egress = SessionEgress::open(
        "ses-1",
        EgressPolicyConfig::from_config(&daemon_config(&["a.test"], None)),
        record.clone(),
        resolver.clone(),
        Some(SecureDir::open(dir.path()).unwrap()),
    )
    .await
    .unwrap();
    let (_grant, hash) = hash_for(&egress, agent()).await;
    let deciding = {
        let egress = egress.clone();
        let hash = hash.clone();
        tokio::spawn(async move { egress.decide(open(1, "a.test", 443, Some(&hash))).await })
    };
    resolver.asked.notified().await;
    // config#0 now names another host while a.test resolves.
    egress
        .reload_config(
            EgressPolicyConfig::from_config(&daemon_config(&["z.test"], None)),
            "human",
        )
        .await
        .unwrap();
    resolver.gate.add_permits(1);
    let decision = deciding.await.unwrap();
    assert!(
        matches!(&decision, Decision::Deny { reason, .. } if reason == "not_allowed"),
        "{decision:?}"
    );
    assert!(egress.state().open.is_empty());
}

/// A record whose `config_reload` policy lines for one scope fail while
/// `fail` is set.
#[derive(Debug, Default)]
struct ScopeFailingRecord {
    inner: FakeRecord,
    fail: Mutex<Option<EgressScope>>,
}

impl ScopeFailingRecord {
    fn fail(&self, scope: Option<EgressScope>) {
        *self.fail.lock().unwrap() = scope;
    }
}

#[async_trait::async_trait]
impl EgressRecordSink for ScopeFailingRecord {
    async fn append(&self, event: NetworkEvent) -> Result<u64, RecordFailure> {
        self.inner.append(event).await
    }

    async fn append_control(&self, event: NetworkEvent) -> Result<u64, RecordFailure> {
        if let NetworkEvent::Policy {
            scope,
            source: PolicySource::ConfigReload,
            ..
        } = &event
        {
            if *self.fail.lock().unwrap() == Some(*scope) {
                return Err(RecordFailure::Unavailable("disk full".into()));
            }
        }
        self.inner.append_control(event).await
    }

    async fn history(&self) -> Result<Vec<NetworkLine>, RecordFailure> {
        self.inner.history().await
    }
}

async fn opened_with(
    config: EgressPolicyConfig,
    record: Arc<ScopeFailingRecord>,
) -> (Arc<SessionEgress>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let egress = SessionEgress::open(
        "ses-1",
        config,
        record,
        resolver(),
        Some(SecureDir::open(dir.path()).unwrap()),
    )
    .await
    .unwrap();
    (egress, dir)
}

/// A scope whose reloaded policy cannot be recorded keeps its policy, its
/// lists and the connections they admitted. A later per-Session allow does
/// not slip the unrecorded lists in, and a reload of the same lists tries
/// that scope again.
#[tokio::test]
async fn a_scope_whose_reload_was_not_recorded_keeps_its_lists_until_a_reload_succeeds() {
    let record = Arc::new(ScopeFailingRecord::default());
    let started = EgressPolicyConfig::from_config(&daemon_config(
        &["a.test", "b.test"],
        Some(&["docs.test"]),
    ));
    let (egress, _dir) = opened_with(started.clone(), record.clone()).await;
    let (mut sidecar, _task) = attach_sidecar(&egress, 3).await;
    let (_grant, hash) = hash_for(&egress, agent()).await;
    sidecar.send(sidecar_open(1, "b.test", 443, &hash)).await;
    assert!(matches!(
        sidecar.frame().await,
        DaemonFrame::Allow { id: 1, .. }
    ));

    // b.test leaves the Session's list and z.test joins the browser's; only
    // the browser's policy can be recorded.
    let next = EgressPolicyConfig::from_config(&daemon_config(
        &["a.test"],
        Some(&["docs.test", "z.test"]),
    ));
    record.fail(Some(EgressScope::Session));
    let reload = egress.reload_config(next.clone(), "human").await.unwrap();
    assert_eq!(
        reload
            .changed
            .iter()
            .map(|scope| scope.scope)
            .collect::<Vec<_>>(),
        [EgressScope::Browser]
    );
    assert_eq!(reload.failed.len(), 1);
    assert_eq!(reload.failed[0].scope, EgressScope::Session);
    assert!(
        reload.failed[0].error.contains("session"),
        "{:?}",
        reload.failed
    );
    // The session scope keeps its lists and its open connection; the
    // browser's took the new ones.
    let applied = egress.policy_config();
    assert_eq!(applied.session_allow, started.session_allow);
    assert_eq!(applied.browser, next.browser);
    assert!(egress
        .policy(EgressScope::Session)
        .unwrap()
        .match_name("b.test", 443)
        .is_some());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), sidecar.frame())
            .await
            .is_err(),
        "nothing is revoked"
    );

    // A person's allow compiles from the lists the scope still has, so it
    // neither drops b.test nor leaves its connection open under a policy
    // that no longer lists it.
    egress
        .allow(EgressScope::Session, "c.test", None, "human", "c-1")
        .await
        .unwrap();
    let session = egress.policy(EgressScope::Session).unwrap();
    assert!(session.match_name("b.test", 443).is_some());
    assert!(session.match_name("c.test", 443).is_some());
    assert_eq!(egress.policy_config().session_allow, started.session_allow);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), sidecar.frame())
            .await
            .is_err(),
        "the allow revokes nothing"
    );

    // The same lists again: the session scope is tried again and closes the
    // connection b.test admitted; the browser's is left alone.
    record.fail(None);
    let retry = egress.reload_config(next.clone(), "human").await.unwrap();
    assert!(retry.failed.is_empty(), "{:?}", retry.failed);
    assert_eq!(retry.changed.len(), 1);
    assert_eq!(
        (retry.changed[0].scope, retry.changed[0].closed),
        (EgressScope::Session, 1)
    );
    assert_eq!(sidecar.frame().await, DaemonFrame::Revoke { ids: vec![1] });
    assert_eq!(egress.policy_config(), next);
    let session = egress.policy(EgressScope::Session).unwrap();
    assert!(session.match_name("b.test", 443).is_none());
    assert!(
        session.match_name("c.test", 443).is_some(),
        "the allow stays"
    );
}

/// A daemon reload compares what each decision point has applied with the
/// new lists: one that an earlier reload could not record is reloaded by
/// the next reload of the same file, and only it.
#[tokio::test]
async fn a_reload_of_the_same_file_retries_the_decision_points_that_failed() {
    use crate::session_network_reload::{reload_points, PointsReload};
    let started = EgressPolicyConfig::from_config(&daemon_config(&["a.test"], None));
    let next = EgressPolicyConfig::from_config(&daemon_config(&["a.test", "b.test"], None));
    let failing = Arc::new(ScopeFailingRecord::default());
    let healthy = Arc::new(ScopeFailingRecord::default());
    let (first, _first_dir) = opened_with(started.clone(), failing.clone()).await;
    let (second, _second_dir) = opened_with(started.clone(), healthy.clone()).await;
    let points = || {
        vec![
            ("ses-1".to_string(), first.clone(), next.clone()),
            ("ses-2".to_string(), second.clone(), next.clone()),
        ]
    };

    failing.fail(Some(EgressScope::Session));
    let reloaded = reload_points(points(), "human").await;
    assert_eq!(reloaded.lagging, ["sandbox.egress.allow"]);
    assert_eq!(
        reloaded
            .revisions
            .iter()
            .map(|revision| (revision.session_id.as_str(), revision.scope.as_str()))
            .collect::<Vec<_>>(),
        [("ses-2", "session")]
    );
    assert_eq!(reloaded.failed.len(), 1);
    assert_eq!(reloaded.failed[0].session_id, "ses-1");
    assert_eq!(reloaded.failed[0].scope.as_deref(), Some("session"));

    // Still failing: tried again, still reported.
    let reloaded = reload_points(points(), "human").await;
    assert_eq!(reloaded.lagging, ["sandbox.egress.allow"]);
    assert!(reloaded.revisions.is_empty());
    assert_eq!(reloaded.failed.len(), 1);

    failing.fail(None);
    let healthy_lines = healthy.inner.events().len();
    let reloaded = reload_points(points(), "human").await;
    assert!(reloaded.failed.is_empty(), "{:?}", reloaded.failed);
    assert_eq!(
        reloaded
            .revisions
            .iter()
            .map(|revision| (revision.session_id.as_str(), revision.revision))
            .collect::<Vec<_>>(),
        [("ses-1", 2)]
    );
    assert_eq!(healthy.inner.events().len(), healthy_lines);
    assert!(first
        .policy(EgressScope::Session)
        .unwrap()
        .match_name("b.test", 443)
        .is_some());
    assert_eq!(
        reload_points(points(), "human").await,
        PointsReload::default()
    );
}
