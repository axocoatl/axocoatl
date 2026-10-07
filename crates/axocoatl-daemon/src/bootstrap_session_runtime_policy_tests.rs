//! 1.3 runtime policy through real containers: a required check that runs
//! past its own timeout is recorded `timed_out`, and a Session under
//! `network: egress` reaches an Ollama stand-in on this computer's loopback
//! only through `sandbox.egress.host_ollama`, every request recorded.
//!
//! Run with `CONTAINER_CONNECTION=axocoatl-ci-pr74` and
//! `AXO_SUPERVISOR_TEST_IMAGE` set to the prepared tools image (it has `git`).
use super::*;
use crate::session_dispatch::{AutonomousActivationFactory, AutonomousNodeInput};
use axocoatl_session::check_options::RequiredCheckOptions;

struct Factory {
    config: AgentConfig,
    profile: ExecutionProfile,
    provider: Arc<Provider>,
}
#[async_trait::async_trait]
impl AutonomousActivationFactory for Factory {
    async fn resources(
        &self,
        _: &ActivationInputManifest,
    ) -> std::result::Result<AutonomousActivationResources, String> {
        Ok(AutonomousActivationResources {
            config: self.config.clone(),
            profile: self.profile.clone(),
            provider: self.provider.clone(),
            counter: Arc::new(Counter),
            tools: Arc::new(axocoatl_tools::ToolExecutor::new()),
        })
    }
}

fn seed(r: &Run) -> AutonomousNodeInput {
    let snapshot = r.controller.snapshot().unwrap();
    let input = &snapshot.contract().activations()[0].input;
    AutonomousNodeInput {
        node_id: input.activation.node_id.clone(),
        guidance: input.guidance.clone(),
        attachments: input.attachments.clone(),
        repository: input.repository.clone(),
        budget: input.budget.clone(),
        grant: input.grant.clone(),
    }
}

/// A check with a two-second timeout that sleeps for thirty is stopped at
/// its timeout: its run is recorded `timed_out`, the turn needs attention,
/// and the turn does not wait the thirty seconds. A second check with the
/// default timeout passes in the same pass.
#[tokio::test]
#[ignore = "requires explicit AXO_SUPERVISOR_TEST_IMAGE and actual Podman with the rebuilt embedded helper"]
async fn actual_check_past_its_timeout_is_recorded_timed_out() {
    let mut f = fixture().await;
    let sandbox = actual_sandbox(&mut f).await;
    git_init(f._workspace.path());
    std::fs::write(f._workspace.path().join("notes.txt"), "draft\n").unwrap();
    let checks = vec![
        vec!["sh".into(), "-c".into(), "echo started; sleep 30".into()],
        vec!["sh".into(), "-c".into(), "echo quick".into()],
    ];
    let options = vec![
        RequiredCheckOptions {
            name: Some("slow".into()),
            timeout_ms: Some(2_000),
            report: None,
            egress: false,
        },
        RequiredCheckOptions::default(),
    ];
    let r = run_with_check_options(&mut f, &["bash"], true, None, &checks, &options);
    let authorized = r
        .controller
        .authorize_required_checks(r.resource.reference());
    let provider = Provider::new(vec![]);
    let factory = Arc::new(Factory {
        config: r.config.clone(),
        profile: r.profile.clone(),
        provider: provider.clone(),
    });
    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(180), async {
        r.controller
            .autonomous_turn_driver(vec![seed(&r)], factory.clone())?
            .run()
            .await
    })
    .await;
    let elapsed = started.elapsed();
    let plane = r.controller.control_plane();
    let idle = f.owner.execution_is_idle();
    sandbox.stop_checked().await.unwrap();

    authorized.unwrap();
    let outcome = outcome.unwrap().unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::NeedsAttention)
    );
    let plane = plane.unwrap();
    assert_eq!(plane.required_checks.len(), 2);
    let slow = &plane.required_checks[0];
    assert_eq!(slow.argv, checks[0]);
    assert_eq!(slow.state, "timed_out", "{slow:?}");
    assert_eq!(
        slow.process_status,
        Some(axocoatl_session::execution_content::ConditionProcessStatus::TimedOut)
    );
    assert_eq!(slow.stdout, "started\n");
    // The other check ran in the same pass and passed on its own.
    assert_eq!(plane.required_checks[1].state, "passed");
    assert_eq!(
        plane
            .required_check_readiness
            .as_ref()
            .map(|r| r.state.as_str()),
        Some("failed")
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "the check was stopped at its timeout, not run to the end: {elapsed:?}"
    );
    assert!(idle.unwrap());
}

/// Under `network: egress` an Agent's shell reaches the Ollama stand-in on
/// this computer's loopback through `https://ollama.host.axocoatl.internal`:
/// its environment names the route in `OLLAMA_HOST`, git trusts the
/// Session's authority, the request reaches the stand-in addressed to
/// itself, and the record holds the connection, the request and the
/// response. Another reserved name is refused, and nothing is resolved.
#[tokio::test]
#[ignore = "requires Podman (CONTAINER_CONNECTION), AXO_SUPERVISOR_TEST_IMAGE and the egress-capable embedded helper"]
async fn actual_egress_reaches_host_ollama_only_through_its_route() {
    use crate::session_egress::host_ollama_tests::FakeOllama;
    use crate::session_egress::tests::{FakeRecord, FakeResolver};
    use crate::session_egress::{EgressPolicyConfig, RouteSettings, SessionEgress};
    use axocoatl_session::network_record::{Decision as Recorded, NetworkEvent};
    const OLLAMA: &str = axocoatl_config::egress_host_ollama::HOST_OLLAMA_ROUTE_HOST;
    let upstream_label = EgressUpstream::start();
    let ollama = FakeOllama::start().await;
    let port = ollama.addr.port();
    let mut f = fixture().await;
    let record = Arc::new(FakeRecord::default());
    let resolver = FakeResolver::with(&[]);
    let env_dir = f.owner.inner.data_root.child("egress-env").unwrap();
    let egress = SessionEgress::open_session(
        f.owner.metadata().session_id.clone(),
        EgressPolicyConfig {
            host_ollama: Some(axocoatl_config::HostOllamaRouteYaml {
                port,
                bindings: None,
            }),
            ..EgressPolicyConfig::default()
        },
        record.clone(),
        resolver.clone(),
        Some(env_dir),
        axocoatl_core::netaddr::classify,
        RouteSettings::default(),
    )
    .await
    .unwrap();
    let sandbox = actual_egress_sandbox(&mut f, &upstream_label, egress.clone()).await;
    git_init(f._workspace.path());
    let r = run(&mut f, &["bash"], true);
    let command = "echo \"ollama=$OLLAMA_HOST\"; \
         git ls-remote https://ollama.host.axocoatl.internal/api/tags.git 2>&1 | tail -1; \
         echo \"rc=$?\"; \
         git ls-remote https://other.axocoatl.internal/x.git 2>&1 | tail -1; true";
    let provider = Provider::new(vec![("bash", serde_json::json!({ "command": command }))]);
    let result = tokio::time::timeout(Duration::from_secs(180), async {
        r.controller
            .prepare_repository_activation(
                r.activation.clone(),
                r.resources(provider.clone()),
                r.resource.clone(),
            )
            .unwrap()
            .run()
            .await
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let idle = f.owner.execution_is_idle();
    sandbox.stop_checked().await.unwrap();
    let settled = result.unwrap().unwrap();
    assert!(idle.unwrap());
    assert!(settled.accepted, "{:?}", settled.failure);
    let shown: Vec<String> = provider.requests.lock().unwrap()[1]
        .iter()
        .filter_map(ChatMessage::text_content)
        .map(str::to_string)
        .collect();
    assert!(
        provider.saw(1, "ollama=https://ollama.host.axocoatl.internal:443"),
        "{shown:?}"
    );

    // The stand-in saw git's request, sent to it as to itself.
    let seen = ollama.seen();
    assert!(
        seen.iter().any(|request| request.method == "GET"
            && request.path == "/api/tags.git/info/refs"
            && request.host == format!("127.0.0.1:{port}")),
        "{seen:?}"
    );
    assert!(resolver.queries().is_empty(), "{:?}", resolver.queries());

    let events = record.events();
    for event in &events {
        event.validate().unwrap();
    }
    assert!(
        events
            .iter()
            .any(|event| matches!(event, NetworkEvent::Open {
            decision: Recorded::Allow, rule: Some(rule), host, addrs, ..
        } if rule == "host_ollama" && host == OLLAMA && addrs == &["127.0.0.1"])),
        "{events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, NetworkEvent::Request {
            decision: Recorded::Allow, rule: Some(rule), path, ..
        } if rule == "host_ollama.access=full" && path == "/api/tags.git/info/refs")),
        "{events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, NetworkEvent::Response { status: 404, .. })),
        "{events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, NetworkEvent::Open {
            decision: Recorded::Deny, reason: Some(reason), host, ..
        } if reason == "reserved_host" && host == "other.axocoatl.internal")),
        "{events:#?}"
    );
}

/// Under `network: egress` a required check admitted with `egress` (as an
/// e2e check is) gets an egress credential for its own process and reaches
/// the Session's route; a check admitted without it, in the same pass, is
/// refused by the proxy as before 1.3. Both checks assert what they saw, so
/// the turn completes only if each got exactly its own network.
#[tokio::test]
#[ignore = "requires Podman (CONTAINER_CONNECTION), AXO_SUPERVISOR_TEST_IMAGE and the egress-capable embedded helper"]
async fn actual_only_a_check_admitted_with_egress_reaches_the_sessions_route() {
    use crate::session_egress::host_ollama_tests::FakeOllama;
    use crate::session_egress::tests::{FakeRecord, FakeResolver};
    use crate::session_egress::{EgressPolicyConfig, RouteSettings, SessionEgress};
    use axocoatl_session::network_record::{Decision as Recorded, NetworkEvent};
    let upstream_label = EgressUpstream::start();
    let ollama = FakeOllama::start().await;
    let port = ollama.addr.port();
    let mut f = fixture().await;
    let record = Arc::new(FakeRecord::default());
    let resolver = FakeResolver::with(&[]);
    let env_dir = f.owner.inner.data_root.child("egress-env").unwrap();
    let egress = SessionEgress::open_session(
        f.owner.metadata().session_id.clone(),
        EgressPolicyConfig {
            host_ollama: Some(axocoatl_config::HostOllamaRouteYaml {
                port,
                bindings: None,
            }),
            ..EgressPolicyConfig::default()
        },
        record.clone(),
        resolver.clone(),
        Some(env_dir),
        axocoatl_core::netaddr::classify,
        RouteSettings::default(),
    )
    .await
    .unwrap();
    let sandbox = actual_egress_sandbox(&mut f, &upstream_label, egress.clone()).await;
    git_init(f._workspace.path());
    // git's answer to the stand-in's 404 says the request got there; a
    // refused connection says something else.
    let probe = "out=$(git ls-remote https://ollama.host.axocoatl.internal/{path}.git 2>&1); \
                 echo \"$out\"; case \"$out\" in *404*|*'not found'*) reached=yes;; *) reached=no;; esac";
    let checks = vec![
        vec![
            "sh".into(),
            "-c".into(),
            format!(
                "{}; test \"$reached\" = yes",
                probe.replace("{path}", "with-egress")
            ),
        ],
        vec![
            "sh".into(),
            "-c".into(),
            format!(
                "{}; test \"$reached\" = no",
                probe.replace("{path}", "without-egress")
            ),
        ],
    ];
    let options = vec![
        RequiredCheckOptions {
            name: Some("e2e".into()),
            egress: true,
            ..RequiredCheckOptions::default()
        },
        RequiredCheckOptions {
            name: Some("tests".into()),
            ..RequiredCheckOptions::default()
        },
    ];
    let r = run_with_check_options(&mut f, &["bash"], true, None, &checks, &options);
    let authorized = r
        .controller
        .authorize_required_checks(r.resource.reference());
    let provider = Provider::new(vec![]);
    let factory = Arc::new(Factory {
        config: r.config.clone(),
        profile: r.profile.clone(),
        provider: provider.clone(),
    });
    let outcome = tokio::time::timeout(Duration::from_secs(240), async {
        r.controller
            .autonomous_turn_driver(vec![seed(&r)], factory.clone())?
            .run()
            .await
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let plane = r.controller.control_plane();
    let idle = f.owner.execution_is_idle();
    sandbox.stop_checked().await.unwrap();

    authorized.unwrap();
    let outcome = outcome.unwrap().unwrap();
    let plane = plane.unwrap();
    assert_eq!(
        outcome.snapshot.contract().state(),
        Some(LogicalTurnState::Completed),
        "{:#?}",
        plane.required_checks
    );
    assert_eq!(plane.required_checks[0].state, "passed");
    assert_eq!(plane.required_checks[1].state, "passed");
    assert!(idle.unwrap());
    // Only the egress check's request reached the stand-in, and the record
    // holds it.
    let seen = ollama.seen();
    assert!(
        seen.iter()
            .any(|request| request.path == "/with-egress.git/info/refs"),
        "{seen:?}"
    );
    assert!(
        !seen.iter().any(|request| request.path.contains("without")),
        "{seen:?}"
    );
    let events = record.events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, NetworkEvent::Request {
            decision: Recorded::Allow, path, ..
        } if path == "/with-egress.git/info/refs")),
        "{events:#?}"
    );
    assert!(resolver.queries().is_empty(), "{:?}", resolver.queries());
}
