use super::*;
use axocoatl_config::{EgressCidrYaml, EgressHostYaml};
use axocoatl_exec::egress::protocol::{DaemonFrame, SidecarFrame};
use axocoatl_isolation::egress::CloseOutcome as WireOutcome;
use axocoatl_isolation::egress_control::{self, ControlTiming};
use std::collections::BTreeMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

#[derive(Debug, Default)]
pub(crate) struct FakeRecord {
    lines: Mutex<Vec<NetworkLine>>,
    /// Ordinary appends fail with this once set.
    fail: Mutex<Option<RecordFailure>>,
}

impl FakeRecord {
    pub(crate) fn events(&self) -> Vec<NetworkEvent> {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .map(|line| line.event.clone())
            .collect()
    }

    fn push(&self, event: NetworkEvent) -> u64 {
        let mut lines = self.lines.lock().unwrap();
        let seq = lines.len() as u64 + 1;
        lines.push(NetworkLine {
            v: 1,
            seq,
            ts_ms: 1,
            event,
        });
        seq
    }

    fn opens(&self) -> Vec<NetworkEvent> {
        self.events()
            .into_iter()
            .filter(|event| matches!(event, NetworkEvent::Open { .. }))
            .collect()
    }
}

#[async_trait::async_trait]
impl EgressRecordSink for FakeRecord {
    async fn append(&self, event: NetworkEvent) -> Result<u64, RecordFailure> {
        if let Some(failure) = self.fail.lock().unwrap().clone() {
            return Err(failure);
        }
        Ok(self.push(event))
    }

    async fn append_control(&self, event: NetworkEvent) -> Result<u64, RecordFailure> {
        Ok(self.push(event))
    }

    async fn history(&self) -> Result<Vec<NetworkLine>, RecordFailure> {
        Ok(self.lines.lock().unwrap().clone())
    }
}

/// Maps names to fixed answers and logs every query it receives.
#[derive(Debug, Default)]
pub(crate) struct FakeResolver {
    answers: BTreeMap<String, Vec<IpAddr>>,
    queries: Mutex<Vec<String>>,
}

impl FakeResolver {
    pub(crate) fn with(answers: &[(&str, &[&str])]) -> Arc<Self> {
        Arc::new(Self {
            answers: answers
                .iter()
                .map(|(name, addrs)| {
                    (
                        (*name).to_string(),
                        addrs.iter().map(|addr| addr.parse().unwrap()).collect(),
                    )
                })
                .collect(),
            queries: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn queries(&self) -> Vec<String> {
        self.queries.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl EgressResolver for FakeResolver {
    async fn resolve(&self, host: &str, _port: u16) -> Result<Vec<IpAddr>, String> {
        self.queries.lock().unwrap().push(host.to_string());
        self.answers
            .get(host)
            .cloned()
            .ok_or_else(|| "NXDOMAIN".to_string())
    }
}

fn host(name: &str, ports: Option<Vec<u16>>) -> EgressAllowYaml {
    EgressAllowYaml::Host(EgressHostYaml {
        host: name.into(),
        ports,
    })
}

fn cidr(range: &str, ports: Vec<u16>) -> EgressAllowYaml {
    EgressAllowYaml::Cidr(EgressCidrYaml {
        cidr: range.into(),
        ports: Some(ports),
    })
}

fn config() -> EgressPolicyConfig {
    EgressPolicyConfig {
        session_allow: vec![
            EgressAllowYaml::Preset("npm".into()),
            host("allowed.test", Some(vec![443, 8080])),
            host("*.wild.test", None),
            host("internal.test", None),
            cidr("10.20.0.0/16", vec![8000]),
        ],
        session_private: vec!["10.0.0.0/8".into()],
        browser: Some((vec![host("docs.test", None)], Vec::new())),
    }
}

struct Fixture {
    egress: Arc<SessionEgress>,
    record: Arc<FakeRecord>,
    resolver: Arc<FakeResolver>,
    dir: tempfile::TempDir,
}

async fn fixture_with(config: EgressPolicyConfig, resolver: Arc<FakeResolver>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let env_dir = SecureDir::open(dir.path()).unwrap();
    let record = Arc::new(FakeRecord::default());
    let egress = SessionEgress::open(
        "ses-1",
        config,
        record.clone(),
        resolver.clone(),
        Some(env_dir),
    )
    .await
    .unwrap();
    Fixture {
        egress,
        record,
        resolver,
        dir,
    }
}

async fn fixture() -> Fixture {
    fixture_with(
        config(),
        FakeResolver::with(&[
            ("registry.npmjs.org", &["104.16.0.35"]),
            ("allowed.test", &["93.184.216.34", "2606:2800:220:1::1"]),
            ("a.wild.test", &["93.184.216.35"]),
            ("internal.test", &["10.1.2.3"]),
            ("loop.test", &["127.0.0.1"]),
        ]),
    )
    .await
}

fn open(id: u64, host: &str, port: u16, auth: Option<&str>) -> OpenRequest {
    OpenRequest {
        generation: 1,
        id,
        kind: RequestKind::Connect,
        host: host.into(),
        port,
        auth: auth.map(str::to_string),
        method: None,
        path: None,
    }
}

fn agent_spec() -> GrantSpec {
    GrantSpec {
        invocation_id: Some("inv-7".into()),
        activation_id: Some("act-3".into()),
        agent: Some("writer".into()),
        ..GrantSpec::new(GrantKind::Agent)
    }
}

/// The token a supervised process would read from its env file.
fn token_of(grant: &EgressGrant) -> String {
    let contents = std::fs::read_to_string(grant.env_file.as_ref().unwrap()).unwrap();
    let line = contents
        .lines()
        .find(|line| line.starts_with("HTTPS_PROXY="))
        .unwrap();
    line.trim_start_matches("HTTPS_PROXY=http://axo:")
        .trim_end_matches("@127.0.0.1:3128")
        .to_string()
}

fn reason(decision: &Decision) -> (u16, String) {
    match decision {
        Decision::Deny { status, reason, .. } => (*status, reason.clone()),
        Decision::Allow { .. } => (200, "allow".into()),
    }
}

async fn granted(fixture: &Fixture, spec: GrantSpec) -> (EgressGrant, String) {
    let grant = fixture.egress.grant(spec).await.unwrap();
    let hash = if grant.env_file.is_some() {
        credential_hash(&token_of(&grant))
    } else {
        let url = grant
            .proxy_url_for_stdin
            .as_ref()
            .unwrap()
            .expose()
            .to_string();
        let token = url
            .trim_start_matches("http://axo:")
            .trim_end_matches("@127.0.0.1:3128")
            .to_string();
        credential_hash(&token)
    };
    (grant, hash)
}

#[tokio::test]
async fn a_denied_name_is_refused_before_anything_resolves_it() {
    let fixture = fixture().await;
    let (_grant, hash) = granted(&fixture, agent_spec()).await;
    let decision = fixture
        .egress
        .decide(open(1, "data.attacker.test", 443, Some(&hash)))
        .await;
    assert_eq!(reason(&decision), (403, "not_allowed".into()));
    let Decision::Deny { hint, .. } = &decision else {
        unreachable!()
    };
    assert!(hint.contains("data.attacker.test:443 is not in this Session's egress allowlist"));
    // An allowed name on an unlisted port is refused the same way.
    let decision = fixture
        .egress
        .decide(open(2, "allowed.test", 22, Some(&hash)))
        .await;
    assert_eq!(reason(&decision), (403, "not_allowed".into()));
    assert!(fixture.resolver.queries().is_empty());
    let opens = fixture.record.opens();
    assert_eq!(opens.len(), 2);
    let NetworkEvent::Open {
        decision,
        reason,
        status,
        token,
        binding,
        scope,
        conn,
        addrs,
        ..
    } = &opens[0]
    else {
        unreachable!()
    };
    assert_eq!(*decision, RecordDecision::Deny);
    assert_eq!(reason.as_deref(), Some("not_allowed"));
    assert_eq!(*status, Some(403));
    assert_eq!(token.as_deref(), Some(credential_tag(&hash).as_str()));
    assert_eq!(
        binding.as_ref().unwrap().invocation_id.as_deref(),
        Some("inv-7")
    );
    assert_eq!(*scope, Some(EgressScope::Session));
    assert_eq!(conn, "g1:1");
    assert!(addrs.is_empty());
}

#[tokio::test]
async fn allowed_names_resolve_on_the_host_and_are_recorded_before_the_answer() {
    let fixture = fixture().await;
    let (_grant, hash) = granted(&fixture, agent_spec()).await;
    let decision = fixture
        .egress
        .decide(open(1, "Allowed.Test.", 8080, Some(&hash)))
        .await;
    assert_eq!(
        decision,
        Decision::Allow {
            addrs: vec![
                "93.184.216.34".parse().unwrap(),
                "2606:2800:220:1::1".parse().unwrap()
            ]
        }
    );
    assert_eq!(fixture.resolver.queries(), ["allowed.test"]);
    let decision = fixture
        .egress
        .decide(open(2, "a.wild.test", 443, Some(&hash)))
        .await;
    assert!(matches!(decision, Decision::Allow { .. }));
    let decision = fixture
        .egress
        .decide(open(3, "registry.npmjs.org", 443, Some(&hash)))
        .await;
    assert!(matches!(decision, Decision::Allow { .. }));
    let opens = fixture.record.opens();
    let rules: Vec<Option<String>> = opens
        .iter()
        .map(|event| match event {
            NetworkEvent::Open { rule, .. } => rule.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(
        rules,
        [
            Some("config#1".into()),
            Some("config#2".into()),
            Some("preset:npm/registry.npmjs.org".into())
        ]
    );
    let NetworkEvent::Open {
        addrs,
        policy_revision,
        host,
        ..
    } = &opens[0]
    else {
        unreachable!()
    };
    assert_eq!(addrs, &["93.184.216.34", "2606:2800:220:1::1"]);
    assert_eq!(*policy_revision, Some(1));
    assert_eq!(host, "Allowed.Test.");
}

#[tokio::test]
async fn special_and_private_answers_are_refused_all_or_nothing() {
    let cases: &[(&[&str], &str)] = &[
        (&["127.0.0.1"], "forbidden_destination"),
        (&["::ffff:127.0.0.1"], "forbidden_destination"),
        (&["169.254.169.254"], "forbidden_destination"),
        (&["192.168.127.254"], "private_destination"),
        (&["93.184.216.34", "10.9.9.9"], "allow"),
        (&["93.184.216.34", "192.168.1.1"], "private_destination"),
        (&["93.184.216.34", "127.0.0.1"], "forbidden_destination"),
        (&["fd00::1"], "private_destination"),
        (&["64:ff9b::a9fe:a9fe"], "forbidden_destination"),
    ];
    for (answer, expected) in cases {
        let fixture = fixture_with(config(), FakeResolver::with(&[("allowed.test", answer)])).await;
        let (_grant, hash) = granted(&fixture, agent_spec()).await;
        let decision = fixture
            .egress
            .decide(open(1, "allowed.test", 443, Some(&hash)))
            .await;
        assert_eq!(reason(&decision).1, *expected, "{answer:?}");
        // The record names every address the name resolved to.
        let NetworkEvent::Open { addrs, .. } = &fixture.record.opens()[0] else {
            unreachable!()
        };
        assert_eq!(addrs.len(), answer.len());
    }
}

#[tokio::test]
async fn private_destinations_never_open_forbidden_ranges() {
    let fixture = fixture().await;
    let (_grant, hash) = granted(&fixture, agent_spec()).await;
    // Listed private range: allowed.
    let decision = fixture
        .egress
        .decide(open(1, "internal.test", 443, Some(&hash)))
        .await;
    assert!(matches!(decision, Decision::Allow { .. }), "{decision:?}");
    // The same name answering loopback is still refused.
    let fixture = fixture_with(
        config(),
        FakeResolver::with(&[("internal.test", &["127.0.0.1"])]),
    )
    .await;
    let (_grant, hash) = granted(&fixture, agent_spec()).await;
    let decision = fixture
        .egress
        .decide(open(1, "internal.test", 443, Some(&hash)))
        .await;
    assert_eq!(reason(&decision).1, "forbidden_destination");
}

#[tokio::test]
async fn ip_literals_use_only_range_rules_and_never_resolve() {
    let fixture = fixture().await;
    let (_grant, hash) = granted(&fixture, agent_spec()).await;
    let cases: &[(&str, u16, (u16, &str))] = &[
        ("10.20.0.5", 8000, (200, "allow")),
        ("10.20.0.5", 443, (403, "not_allowed")),
        ("10.21.0.5", 8000, (403, "not_allowed")),
        ("1.1.1.1", 443, (403, "not_allowed")),
        ("[::ffff:127.0.0.1]", 8080, (403, "forbidden_destination")),
        ("169.254.1.2", 8080, (403, "forbidden_destination")),
        ("192.168.127.254", 8080, (403, "not_allowed")),
        ("2130706433", 80, (400, "invalid_host")),
        ("0x7f.1", 80, (400, "invalid_host")),
        ("bad_host.test", 443, (400, "invalid_host")),
    ];
    for (index, (target, port, expected)) in cases.iter().enumerate() {
        let decision = fixture
            .egress
            .decide(open(index as u64, target, *port, Some(&hash)))
            .await;
        let got = reason(&decision);
        assert_eq!((got.0, got.1.as_str()), *expected, "{target}:{port}");
    }
    assert!(fixture.resolver.queries().is_empty());
}

#[tokio::test]
async fn credentials_absent_unknown_ended_and_live() {
    let fixture = fixture().await;
    let decision = fixture
        .egress
        .decide(open(1, "allowed.test", 443, None))
        .await;
    assert_eq!(reason(&decision), (407, "no_credential".into()));
    let unknown = credential_hash("axe_leftover_from_setup");
    let decision = fixture
        .egress
        .decide(open(2, "allowed.test", 443, Some(&unknown)))
        .await;
    assert_eq!(reason(&decision), (407, "unknown_credential".into()));
    let NetworkEvent::Open { token, binding, .. } = &fixture.record.opens()[1] else {
        unreachable!()
    };
    assert_eq!(token.as_deref(), Some(credential_tag(&unknown).as_str()));
    assert!(binding.is_none());

    let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = alive.clone();
    let (_terminal, hash) = granted(
        &fixture,
        GrantSpec {
            terminal_id: Some("pty-1".into()),
            liveness: Some(Arc::new(move || {
                flag.load(std::sync::atomic::Ordering::SeqCst)
            })),
            ..GrantSpec::new(GrantKind::Terminal)
        },
    )
    .await;
    let decision = fixture
        .egress
        .decide(open(3, "allowed.test", 443, Some(&hash)))
        .await;
    assert!(matches!(decision, Decision::Allow { .. }));
    alive.store(false, std::sync::atomic::Ordering::SeqCst);
    let decision = fixture
        .egress
        .decide(open(4, "allowed.test", 443, Some(&hash)))
        .await;
    assert_eq!(reason(&decision), (407, "binding_ended".into()));
    let decision = fixture
        .egress
        .decide(open(5, "allowed.test", 443, Some(&hash)))
        .await;
    assert_eq!(reason(&decision), (407, "unknown_credential".into()));
    wait_for(|| {
        fixture.record.events().iter().any(|event| {
            matches!(
                event,
                NetworkEvent::Unbind {
                    reason: UnbindReason::TerminalClosed,
                    ..
                }
            )
        })
    })
    .await;
}

async fn wait_for(mut condition: impl FnMut() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}

#[tokio::test]
async fn scopes_are_separate() {
    let fixture = fixture_with(
        config(),
        FakeResolver::with(&[
            ("allowed.test", &["93.184.216.34"]),
            ("docs.test", &["93.184.216.36"]),
            ("deb.debian.org", &["151.101.2.132"]),
        ]),
    )
    .await;
    let (_browser, browser) = granted(
        &fixture,
        GrantSpec {
            invocation_id: Some("inv-b".into()),
            ..GrantSpec::new(GrantKind::Browser)
        },
    )
    .await;
    let (_provisioning, provisioning) =
        granted(&fixture, GrantSpec::new(GrantKind::Provisioning)).await;
    let (_agent, agent) = granted(&fixture, agent_spec()).await;
    for (hash, target, port, allowed) in [
        (&browser, "allowed.test", 443, false),
        (&browser, "docs.test", 443, true),
        (&agent, "docs.test", 443, false),
        (&provisioning, "deb.debian.org", 80, true),
        (&provisioning, "allowed.test", 443, false),
        (&agent, "deb.debian.org", 80, false),
    ] {
        let decision = fixture
            .egress
            .decide(open(1, target, port, Some(hash)))
            .await;
        assert_eq!(
            matches!(decision, Decision::Allow { .. }),
            allowed,
            "{target} {decision:?}"
        );
    }
    let scopes: Vec<Option<EgressScope>> = fixture
        .record
        .opens()
        .iter()
        .map(|event| match event {
            NetworkEvent::Open { scope, .. } => *scope,
            _ => None,
        })
        .collect();
    assert_eq!(
        scopes,
        [
            Some(EgressScope::Browser),
            Some(EgressScope::Browser),
            Some(EgressScope::Session),
            Some(EgressScope::Provisioning),
            Some(EgressScope::Provisioning),
            Some(EgressScope::Session),
        ]
    );
    // A browser credential is never written to an env file.
    let browser_grant = fixture
        .egress
        .grant(GrantSpec::new(GrantKind::Browser))
        .await
        .unwrap();
    assert!(browser_grant.env_file.is_none());
    assert!(format!("{browser_grant:?}").contains("redacted"));
}

#[tokio::test]
async fn record_failure_refuses_new_connections() {
    let fixture = fixture().await;
    let (_grant, hash) = granted(&fixture, agent_spec()).await;
    *fixture.record.fail.lock().unwrap() = Some(RecordFailure::Full);
    for id in 0..3 {
        let decision = fixture
            .egress
            .decide(open(id, "allowed.test", 443, Some(&hash)))
            .await;
        assert_eq!(reason(&decision), (503, "record_unavailable".into()));
    }
    let limits = fixture
        .record
        .events()
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                NetworkEvent::Limit {
                    what: LimitKind::RecordFull,
                    ..
                }
            )
        })
        .count();
    assert_eq!(limits, 1);
    // A refusal stays a refusal even when it cannot be recorded.
    *fixture.record.fail.lock().unwrap() = Some(RecordFailure::Unavailable("disk".into()));
    let decision = fixture
        .egress
        .decide(open(9, "evil.test", 443, Some(&hash)))
        .await;
    assert_eq!(reason(&decision), (403, "not_allowed".into()));
    // A grant needs its bind recorded, and leaves nothing behind without it.
    let live = fixture.egress.live_bindings();
    assert!(fixture.egress.grant(agent_spec()).await.is_err());
    assert_eq!(fixture.egress.live_bindings(), live);
    let leftovers: Vec<_> = std::fs::read_dir(fixture.dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(leftovers.len(), 1, "{leftovers:?}");
}

#[tokio::test]
async fn grants_write_env_files_and_unbind_when_dropped() {
    let fixture = fixture().await;
    let grant = fixture.egress.grant(agent_spec()).await.unwrap();
    let path = grant.env_file.clone().unwrap();
    assert!(path.starts_with(fixture.dir.path()));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let contents = std::fs::read_to_string(&path).unwrap();
    let token = token_of(&grant);
    assert!(
        token.starts_with("axe_") && token.len() == 4 + 43,
        "{token}"
    );
    for name in [
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "https_proxy",
        "http_proxy",
        "npm_config_proxy",
        "npm_config_https_proxy",
    ] {
        assert!(
            contents.contains(&format!("{name}=http://axo:{token}@127.0.0.1:3128\n")),
            "{name}"
        );
    }
    assert!(contents.contains("NO_PROXY=localhost,127.0.0.1,::1\n"));
    assert!(contents.contains("NODE_USE_ENV_PROXY=1\n"));
    // Nothing secret reaches the record.
    let recorded = serde_json::to_string(&fixture.record.events()).unwrap();
    assert!(!recorded.contains(&token));
    assert!(!recorded.contains(&credential_hash(&token)));
    assert!(recorded.contains(&grant.token_tag));
    let NetworkEvent::Bind { binding, scope, .. } = fixture.record.events().last().unwrap().clone()
    else {
        panic!("expected bind")
    };
    assert_eq!(scope, EgressScope::Session);
    assert_eq!(binding.kind, BindingKind::Agent);
    assert_eq!(binding.agent.as_deref(), Some("writer"));
    assert_eq!(fixture.egress.live_bindings(), 1);
    let hash = credential_hash(&token);
    drop(grant);
    assert!(!path.exists());
    assert_eq!(fixture.egress.live_bindings(), 0);
    let decision = fixture
        .egress
        .decide(open(1, "allowed.test", 443, Some(&hash)))
        .await;
    assert_eq!(reason(&decision), (407, "unknown_credential".into()));
    wait_for(|| {
        fixture.record.events().iter().any(|event| {
            matches!(
                event,
                NetworkEvent::Unbind {
                    reason: UnbindReason::Settled,
                    ..
                }
            )
        })
    })
    .await;
}

#[tokio::test]
async fn policy_events_replay_and_reproduce_the_digest() {
    let fixture = fixture().await;
    let initial: Vec<NetworkEvent> = fixture.record.events();
    let scopes: Vec<EgressScope> = initial
        .iter()
        .filter_map(|event| match event {
            NetworkEvent::Policy { scope, source, .. } => {
                assert_eq!(*source, PolicySource::Config);
                Some(*scope)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        scopes,
        [
            EgressScope::Session,
            EgressScope::Provisioning,
            EgressScope::Browser
        ]
    );
    let (revision, digest) = fixture
        .egress
        .allow(
            EgressScope::Session,
            "Extra.Test",
            Some(vec![443]),
            "human",
            "cmd-1",
        )
        .await
        .unwrap();
    assert_eq!(revision, 2);
    let (_, with_other) = fixture
        .egress
        .allow(EgressScope::Session, "other.test", None, "human", "cmd-2")
        .await
        .unwrap();
    assert_ne!(digest, with_other);
    let (_, after_revoke) = fixture
        .egress
        .revoke(EgressScope::Session, "other.test", "human", "cmd-3")
        .await
        .unwrap();
    // Revoking the only change since revision 2 restores that policy.
    assert_eq!(digest, after_revoke);
    let views = fixture.egress.policy_views();
    let session = views.iter().find(|view| view.scope == "session").unwrap();
    assert_eq!(
        (session.revision, session.digest.as_str()),
        (4, after_revoke.as_str())
    );
    assert!(session
        .rules
        .iter()
        .any(|rule| rule.id == "session#rev2" && rule.source == "session"));
    assert!(!session
        .rules
        .iter()
        .any(|rule| rule.text.starts_with("other.test")));

    // A new decision point over the same record replays to the same policy
    // and records nothing new.
    let before = fixture.record.events().len();
    let replayed = SessionEgress::open(
        "ses-1",
        config(),
        fixture.record.clone(),
        fixture.resolver.clone(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(fixture.record.events().len(), before);
    assert_eq!(replayed.policy_views(), fixture.egress.policy_views());
    // A config change is recorded as a new config policy.
    let mut changed = config();
    changed.session_allow.push(host("new.test", None));
    let reconfigured = SessionEgress::open(
        "ses-1",
        changed,
        fixture.record.clone(),
        fixture.resolver.clone(),
        None,
    )
    .await
    .unwrap();
    let NetworkEvent::Policy {
        source, revision, ..
    } = fixture.record.events().last().unwrap().clone()
    else {
        panic!("expected a policy event")
    };
    assert_eq!((source, revision), (PolicySource::Config, 5));
    let session = reconfigured.policy(EgressScope::Session).unwrap();
    assert!(session.match_name("extra.test", 443).is_some());
    assert!(session.match_name("new.test", 443).is_some());
}

#[tokio::test]
async fn per_session_allows_apply_to_new_connections_immediately() {
    let fixture = fixture_with(
        config(),
        FakeResolver::with(&[("late.test", &["93.184.216.40"])]),
    )
    .await;
    let (_grant, hash) = granted(&fixture, agent_spec()).await;
    let decision = fixture
        .egress
        .decide(open(1, "late.test", 443, Some(&hash)))
        .await;
    assert_eq!(reason(&decision).1, "not_allowed");
    fixture
        .egress
        .allow(EgressScope::Session, "late.test", None, "human", "cmd-1")
        .await
        .unwrap();
    let decision = fixture
        .egress
        .decide(open(2, "late.test", 443, Some(&hash)))
        .await;
    assert!(matches!(decision, Decision::Allow { .. }));
    let NetworkEvent::Policy { change, actor, .. } = fixture
        .record
        .events()
        .into_iter()
        .rfind(|event| matches!(event, NetworkEvent::Policy { .. }))
        .unwrap()
    else {
        unreachable!()
    };
    assert_eq!(
        change,
        Some(PolicyChange {
            op: PolicyOp::Allow,
            host: "late.test".into(),
            ports: vec![443]
        })
    );
    assert_eq!(actor.as_deref(), Some("human"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_policy_changes_are_serialized() {
    let fixture = fixture().await;
    let mut tasks = Vec::new();
    for index in 0..16 {
        let egress = fixture.egress.clone();
        tasks.push(tokio::spawn(async move {
            egress
                .allow(
                    EgressScope::Session,
                    &format!("host{index}.test"),
                    None,
                    "human",
                    &format!("cmd-{index}"),
                )
                .await
                .unwrap()
                .0
        }));
    }
    let mut revisions = Vec::new();
    for task in tasks {
        revisions.push(task.await.unwrap());
    }
    revisions.sort_unstable();
    assert_eq!(revisions, (2..=17).collect::<Vec<u64>>());
    let replayed = SessionEgress::open(
        "ses-1",
        config(),
        fixture.record.clone(),
        fixture.resolver.clone(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(replayed.policy_views(), fixture.egress.policy_views());
}

#[tokio::test]
async fn invalid_policy_changes_are_refused() {
    let fixture = fixture().await;
    for (host, expected) in [
        ("*.example.com", "wildcards"),
        ("10.0.0.1", "IP address"),
        ("bad_host", "letters, digits or hyphens"),
    ] {
        let error = fixture
            .egress
            .allow(EgressScope::Session, host, None, "human", host)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, EgressPolicyError::Invalid(message) if message.contains(expected)),
            "{host}: {error}"
        );
    }
    fixture
        .egress
        .allow(EgressScope::Session, "a.test", None, "human", "same")
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .egress
            .allow(EgressScope::Session, "b.test", None, "human", "same")
            .await,
        Err(EgressPolicyError::Conflict(_))
    ));
    assert!(matches!(
        fixture
            .egress
            .revoke(EgressScope::Session, "registry.npmjs.org", "human", "r1")
            .await,
        Err(EgressPolicyError::Invalid(_))
    ));
    assert!(matches!(
        fixture
            .egress
            .allow(EgressScope::Provisioning, "a.test", None, "human", "p1")
            .await,
        Err(EgressPolicyError::Invalid(_))
    ));
    assert!(matches!(
        fixture
            .egress
            .allow(EgressScope::Session, "a.test", Some(vec![0]), "human", "z1")
            .await,
        Err(EgressPolicyError::Invalid(_))
    ));
    // Without a browser block there is no browser scope to change.
    let no_browser = fixture_with(
        EgressPolicyConfig {
            browser: None,
            ..config()
        },
        FakeResolver::with(&[]),
    )
    .await;
    assert!(no_browser
        .egress
        .allow(EgressScope::Browser, "a.test", None, "human", "b1")
        .await
        .is_err());
    assert!(no_browser
        .egress
        .grant(GrantSpec::new(GrantKind::Browser))
        .await
        .is_err());
}

struct FakeSidecar {
    to_daemon: tokio::io::DuplexStream,
    from_daemon: tokio::io::BufReader<tokio::io::DuplexStream>,
}

impl FakeSidecar {
    async fn frame(&mut self) -> DaemonFrame {
        loop {
            let mut line = Vec::new();
            tokio::time::timeout(
                Duration::from_secs(5),
                self.from_daemon.read_until(b'\n', &mut line),
            )
            .await
            .unwrap()
            .unwrap();
            let frame = axocoatl_exec::egress::protocol::decode_daemon(&line).unwrap();
            if frame != DaemonFrame::Ping {
                return frame;
            }
        }
    }

    async fn send(&mut self, frame: SidecarFrame) {
        self.to_daemon
            .write_all(&axocoatl_exec::egress::protocol::encode_sidecar(&frame).unwrap())
            .await
            .unwrap();
    }
}

async fn attach_sidecar(
    egress: &Arc<SessionEgress>,
    generation: u32,
) -> (
    FakeSidecar,
    tokio::task::JoinHandle<egress_control::ControlEnd>,
) {
    let (daemon_read, sidecar_write) = tokio::io::duplex(1 << 16);
    let (sidecar_read, daemon_write) = tokio::io::duplex(1 << 16);
    let mut sidecar = FakeSidecar {
        to_daemon: sidecar_write,
        from_daemon: tokio::io::BufReader::new(sidecar_read),
    };
    sidecar
        .send(SidecarFrame::Hello {
            protocol: 1,
            version: "test".into(),
            max_connections: 8,
        })
        .await;
    let (handle, task) = egress_control::start(
        generation,
        daemon_read,
        daemon_write,
        egress.clone(),
        ControlTiming::default(),
    )
    .await
    .unwrap();
    egress.attach_control(handle);
    assert_eq!(sidecar.frame().await, DaemonFrame::HelloAck { protocol: 1 });
    (sidecar, task)
}

fn sidecar_open(id: u64, host: &str, port: u16, hash: &str) -> SidecarFrame {
    SidecarFrame::Open {
        id,
        kind: RequestKind::Connect,
        host: host.into(),
        port,
        auth: Some(hash.into()),
        method: None,
        path: None,
    }
}

#[tokio::test]
async fn revoking_a_host_or_dropping_a_grant_closes_its_connections() {
    let fixture = fixture_with(
        config(),
        FakeResolver::with(&[
            ("late.test", &["93.184.216.40"]),
            ("allowed.test", &["93.184.216.34"]),
        ]),
    )
    .await;
    let (mut sidecar, task) = attach_sidecar(&fixture.egress, 3).await;
    fixture
        .egress
        .allow(EgressScope::Session, "late.test", None, "human", "c1")
        .await
        .unwrap();
    let (grant, hash) = granted(&fixture, agent_spec()).await;
    sidecar.send(sidecar_open(1, "late.test", 443, &hash)).await;
    assert!(matches!(
        sidecar.frame().await,
        DaemonFrame::Allow { id: 1, .. }
    ));
    sidecar
        .send(sidecar_open(2, "allowed.test", 443, &hash))
        .await;
    assert!(matches!(
        sidecar.frame().await,
        DaemonFrame::Allow { id: 2, .. }
    ));
    fixture
        .egress
        .revoke(EgressScope::Session, "late.test", "human", "c2")
        .await
        .unwrap();
    assert_eq!(sidecar.frame().await, DaemonFrame::Revoke { ids: vec![1] });
    sidecar
        .send(SidecarFrame::Close {
            id: 1,
            ip: Some("93.184.216.40".parse().unwrap()),
            up: 5,
            down: 7,
            ms: 12,
            outcome: WireOutcome::Revoked,
            error: None,
        })
        .await;
    wait_for(|| {
        fixture
            .record
            .events()
            .iter()
            .any(|event| matches!(event, NetworkEvent::Close { conn, .. } if conn == "g3:1"))
    })
    .await;
    drop(grant);
    assert_eq!(sidecar.frame().await, DaemonFrame::Revoke { ids: vec![2] });
    sidecar
        .send(SidecarFrame::Close {
            id: 2,
            ip: Some("93.184.216.34".parse().unwrap()),
            up: 1,
            down: 2,
            ms: 3,
            outcome: WireOutcome::Revoked,
            error: None,
        })
        .await;
    wait_for(|| {
        fixture
            .record
            .events()
            .iter()
            .filter(|event| matches!(event, NetworkEvent::Close { .. }))
            .count()
            == 2
    })
    .await;
    let closes: Vec<(String, CloseOutcome, u64)> = fixture
        .record
        .events()
        .into_iter()
        .filter_map(|event| match event {
            NetworkEvent::Close {
                conn, outcome, up, ..
            } => Some((conn, outcome, up)),
            _ => None,
        })
        .collect();
    assert_eq!(
        closes,
        [
            ("g3:1".to_string(), CloseOutcome::Revoked, 5),
            ("g3:2".to_string(), CloseOutcome::Revoked, 1)
        ]
    );
    drop(sidecar);
    task.await.unwrap();
    assert!(matches!(
        fixture.record.events().last(),
        Some(NetworkEvent::Sidecar {
            state: SidecarState::ChannelLost,
            generation: 3,
            ..
        })
    ));
}

#[tokio::test]
async fn a_lost_channel_records_open_connections_as_interrupted() {
    let fixture = fixture().await;
    let (mut sidecar, task) = attach_sidecar(&fixture.egress, 2).await;
    let (_grant, hash) = granted(&fixture, agent_spec()).await;
    sidecar
        .send(sidecar_open(5, "allowed.test", 443, &hash))
        .await;
    assert!(matches!(
        sidecar.frame().await,
        DaemonFrame::Allow { id: 5, .. }
    ));
    drop(sidecar);
    task.await.unwrap();
    let events = fixture.record.events();
    let tail: Vec<&str> = events
        .iter()
        .rev()
        .take(2)
        .map(NetworkEvent::kind)
        .collect();
    assert_eq!(tail, ["sidecar", "close"]);
    assert!(events.iter().any(|event| matches!(
        event,
        NetworkEvent::Close {
            outcome: CloseOutcome::Interrupted,
            conn,
            ..
        } if conn == "g2:5"
    )));
}

/// The real proxy from `axocoatl-exec`, in-process, behind the real control
/// loop and this decision point: an allowed tunnel carries bytes and is
/// recorded as bind, open and close; a refused one never connects.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_proxy_control_loop_and_decision_point_work_end_to_end() {
    use std::io::{Read, Write};
    let upstream = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = upstream.local_addr().unwrap().port();
    let echo = std::thread::spawn(move || {
        let (mut stream, _) = upstream.accept().unwrap();
        let mut buffer = [0u8; 1024];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => stream.write_all(&buffer[..read]).unwrap(),
            }
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let record = Arc::new(FakeRecord::default());
    let resolver = FakeResolver::with(&[("app.test", &["127.0.0.1"])]);
    // Loopback stands in for a public upstream in this test only.
    fn loopback_is_public(ip: IpAddr) -> AddrClass {
        if ip.is_loopback() {
            AddrClass::Public
        } else {
            netaddr::classify(ip)
        }
    }
    let egress = SessionEgress::open_with_classifier(
        "ses-e2e",
        EgressPolicyConfig {
            session_allow: vec![host("app.test", Some(vec![port]))],
            ..EgressPolicyConfig::default()
        },
        record.clone(),
        resolver.clone(),
        Some(SecureDir::open(dir.path()).unwrap()),
        loopback_is_public,
    )
    .await
    .unwrap();

    let socket = dir.path().join("proxy.sock");
    let listener = axocoatl_exec::egress::proxy::bind_unix_listener(&socket).unwrap();
    let (proxy_end, daemon_end) = std::os::unix::net::UnixStream::pair().unwrap();
    let proxy_in = proxy_end.try_clone().unwrap();
    let mut proxy_config = axocoatl_exec::egress::proxy::ProxyConfig::new(8);
    proxy_config.never = |_| false;
    let proxy = std::thread::spawn(move || {
        axocoatl_exec::egress::proxy::run(
            proxy_config,
            listener,
            Box::new(proxy_in),
            Box::new(proxy_end),
        )
    });
    daemon_end.set_nonblocking(true).unwrap();
    let (read_half, write_half) = tokio::net::UnixStream::from_std(daemon_end)
        .unwrap()
        .into_split();
    let (control, task) = egress_control::start(
        1,
        read_half,
        write_half,
        egress.clone(),
        ControlTiming::default(),
    )
    .await
    .unwrap();
    egress.attach_control(control.clone());

    let grant = egress.grant(agent_spec()).await.unwrap();
    let token = token_of(&grant);
    let basic = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(format!("axo:{token}"))
    };
    let client_socket = socket.clone();
    let tunnel = tokio::task::spawn_blocking(move || {
        let mut client = std::os::unix::net::UnixStream::connect(&client_socket).unwrap();
        write!(
            client,
            "CONNECT app.test:{port} HTTP/1.1\r\nProxy-Authorization: Basic {basic}\r\n\r\n"
        )
        .unwrap();
        let mut established = [0u8; 39];
        client.read_exact(&mut established).unwrap();
        assert_eq!(&established, b"HTTP/1.1 200 Connection Established\r\n\r\n");
        client.write_all(b"hello egress").unwrap();
        let mut echoed = [0u8; 12];
        client.read_exact(&mut echoed).unwrap();
        assert_eq!(&echoed, b"hello egress");
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).unwrap();
    });
    tunnel.await.unwrap();
    echo.join().unwrap();

    let refused_socket = socket.clone();
    let refused = tokio::task::spawn_blocking(move || {
        let mut client = std::os::unix::net::UnixStream::connect(&refused_socket).unwrap();
        client
            .write_all(b"CONNECT data.attacker.test:443 HTTP/1.1\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        response
    })
    .await
    .unwrap();
    assert!(refused.starts_with("HTTP/1.1 407 "), "{refused}");
    assert!(refused.contains("no_credential"));

    wait_for(|| {
        record
            .events()
            .iter()
            .any(|event| matches!(event, NetworkEvent::Close { .. }))
    })
    .await;
    let kinds: Vec<&str> = record
        .events()
        .iter()
        .map(NetworkEvent::kind)
        .filter(|kind| *kind != "policy")
        .collect();
    assert_eq!(&kinds[..3], ["bind", "open", "close"]);
    let opens = record.opens();
    let NetworkEvent::Open {
        decision,
        binding,
        addrs,
        ..
    } = &opens[0]
    else {
        unreachable!()
    };
    assert_eq!(*decision, RecordDecision::Allow);
    assert_eq!(
        binding.as_ref().unwrap().invocation_id.as_deref(),
        Some("inv-7")
    );
    assert_eq!(addrs, &["127.0.0.1"]);
    let close = record
        .events()
        .into_iter()
        .find(|event| matches!(event, NetworkEvent::Close { .. }))
        .unwrap();
    let NetworkEvent::Close {
        up,
        down,
        outcome,
        ip,
        ..
    } = close
    else {
        unreachable!()
    };
    assert_eq!((up, down, outcome), (12, 12, CloseOutcome::Closed));
    assert_eq!(ip.as_deref(), Some("127.0.0.1"));
    assert_eq!(resolver.queries(), ["app.test"]);

    drop(grant);
    assert!(control.shutdown());
    assert_eq!(
        proxy.join().unwrap(),
        axocoatl_exec::egress::proxy::ProxyExit::Shutdown
    );
    assert_eq!(task.await.unwrap(), egress_control::ControlEnd::Shutdown);
}
