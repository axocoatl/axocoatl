//! `request_network_access` through real native actor admission: offered
//! only to a listed writer under `network: egress`, recorded as a proposal,
//! and answered only by a person's decision.
use super::*;
use crate::session_dispatch_browser::SessionEgressSource;
use crate::session_dispatch_network_tool::{RequestNetworkAccessTool, READ_ONLY_REFUSAL};
use crate::session_egress::tests::{FakeRecord, FakeResolver};
use crate::session_egress::{EgressPolicyConfig, SessionEgress};
use axocoatl_session::network_record::ProposalState;

struct Source(Arc<SessionEgress>);

#[async_trait]
impl SessionEgressSource for Source {
    async fn session_egress(&self, _: &str) -> std::result::Result<Arc<SessionEgress>, String> {
        Ok(self.0.clone())
    }
    async fn session_sidecar_ready(&self, _: &str) -> bool {
        true
    }
}

async fn decision_point(record: Arc<FakeRecord>) -> (Arc<SessionEgress>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let egress = SessionEgress::open(
        "input-session",
        EgressPolicyConfig {
            session_allow: vec![axocoatl_config::EgressAllowYaml::Preset("npm".into())],
            ..EgressPolicyConfig::default()
        },
        record,
        FakeResolver::with(&[("api.test", &["93.184.216.2"])]),
        Some(axocoatl_core::SecureDir::open(dir.path()).unwrap()),
    )
    .await
    .unwrap();
    (egress, dir)
}

#[tokio::test]
async fn a_listed_writer_asks_and_only_a_persons_approval_answers() {
    let record = Arc::new(FakeRecord::default());
    let (egress, _dir) = decision_point(record.clone()).await;
    let fixture = input_fixture_with_tools(false, &["request_network_access"]);
    fixture
        .controller
        .register_host_invocation_tool(Arc::new(RequestNetworkAccessTool::new(
            true,
            Arc::new(Source(egress.clone())),
        )))
        .unwrap();
    start_input(&fixture.controller, &fixture.parent);
    let provider = Arc::new(HostToolProvider::new(
        vec![(
            "request_network_access",
            serde_json::json!({"host": "api.test", "reason": "the API docs say so", "wait_secs": 60}),
        )],
        "asked",
    ));
    // The person, in the Network panel, approves what is waiting.
    let person = {
        let egress = egress.clone();
        tokio::spawn(async move {
            loop {
                let pending = egress
                    .proposals()
                    .into_iter()
                    .find(|view| view.state == ProposalState::Pending);
                if let Some(view) = pending {
                    return egress
                        .approve_proposal(&view.id, "human", "c-panel")
                        .await
                        .map(|_| view);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    };
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        run_with(&fixture.controller, &fixture.parent, provider.clone()),
    )
    .await
    .unwrap();
    assert!(result.accepted, "{:?}", result.failure);
    assert_eq!(provider.offered_first(), ["request_network_access"]);
    let view = person.await.unwrap().unwrap();
    // The proposal names the exact admitted call.
    assert_eq!(view.agent.as_deref(), Some("parent"));
    assert_eq!(
        view.activation_id.as_deref(),
        Some(fixture.parent.input.activation.activation_id.as_str())
    );
    let invocation = InvocationId::new(view.invocation_id.clone().unwrap()).unwrap();
    assert_eq!(
        audited_tool(&fixture.controller, &invocation),
        "request_network_access"
    );
    assert!(succeeded(&fixture.controller, &invocation));
    let results = provider.results();
    assert!(results[0].contains("\"decision\":\"approved\""), "{results:?}");
    assert!(results[0].contains(&view.id), "{results:?}");
    // The approval is the Session's own allow: api.test is in its policy.
    assert!(egress
        .policy(axocoatl_session::network_record::EgressScope::Session)
        .unwrap()
        .match_name("api.test", 443)
        .is_some());
}

#[tokio::test]
async fn it_is_not_offered_unlisted_or_outside_egress_and_a_read_only_helper_cannot_have_it() {
    let record = Arc::new(FakeRecord::default());
    let (egress, _dir) = decision_point(record.clone()).await;

    // Unlisted: never offered, even under egress.
    let fixture = input_fixture_with_tools(false, &["effect"]);
    fixture
        .controller
        .register_host_invocation_tool(Arc::new(RequestNetworkAccessTool::new(
            true,
            Arc::new(Source(egress.clone())),
        )))
        .unwrap();
    start_input(&fixture.controller, &fixture.parent);
    let provider = Arc::new(HostToolProvider::new(Vec::new(), "nothing"));
    let result = run_with(&fixture.controller, &fixture.parent, provider.clone()).await;
    assert!(result.accepted, "{:?}", result.failure);
    assert!(!provider
        .offered_first()
        .iter()
        .any(|tool| tool == "request_network_access"));

    // Listed outside egress: the Agent runs without it.
    let fixture = input_fixture_with_tools(false, &["request_network_access"]);
    fixture
        .controller
        .register_host_invocation_tool(Arc::new(RequestNetworkAccessTool::new(
            false,
            Arc::new(Source(egress.clone())),
        )))
        .unwrap();
    start_input(&fixture.controller, &fixture.parent);
    let provider = Arc::new(HostToolProvider::new(Vec::new(), "nothing"));
    let result = run_with(&fixture.controller, &fixture.parent, provider.clone()).await;
    assert!(result.accepted, "{:?}", result.failure);
    assert!(provider.offered_first().is_empty(), "{:?}", provider.offered_first());

    // A read-only helper that lists it is refused before any model call.
    let fixture = input_fixture_with_nodes(
        false,
        [(&["effect"], None), (&["request_network_access"], Some(Vec::new()))],
    );
    fixture
        .controller
        .register_host_invocation_tool(Arc::new(RequestNetworkAccessTool::new(
            true,
            Arc::new(Source(egress.clone())),
        )))
        .unwrap();
    start_input(&fixture.controller, &fixture.parent);
    let parent_provider = Arc::new(HostToolProvider::new(Vec::new(), "parent"));
    let parent = run_with(&fixture.controller, &fixture.parent, parent_provider).await;
    assert!(parent.accepted, "{:?}", parent.failure);
    let mut child = fixture.child.clone();
    child.input.parents = vec![accepted_parent(&parent)];
    start_input(&fixture.controller, &child);
    let provider = Arc::new(HostToolProvider::new(Vec::new(), "unused"));
    let refused = fixture
        .controller
        .prepare_autonomous_activation(
            child.input.activation.clone(),
            resources_with(&child, provider.clone()),
        )
        .err()
        .expect("a read-only helper cannot ask for hosts")
        .to_string();
    assert!(refused.contains(READ_ONLY_REFUSAL), "{refused}");
    assert_eq!(provider.round.load(Ordering::SeqCst), 0, "no model call");
    // Nothing was proposed by any of them.
    assert!(egress.proposals().is_empty());
}
