use super::*;
use crate::session_dispatch::RetainedSessionStores;
use axocoatl_core::{AgentId, SamplingConfig};
use axocoatl_memory::activation_state::ActivationStateStore;
use axocoatl_session::execution_content::ExecutionContentStore;
use axocoatl_session::execution_namespace::ExecutionComponent;
use axocoatl_session::execution_ownership::{LegacyFormatOwnership, UpgradedFormatOwnership};
use axocoatl_session::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

const DIGEST: &str = "a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72";

#[tokio::test]
async fn native_heterogeneous_team_keeps_each_selected_model_on_a_shared_provider() {
    let fixture = pending_fixture();
    let mut captured = Vec::new();
    let host = AxocoatlConfig::default();
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(
        axocoatl_llm_openai::OpenAiProvider::with_base_url(
            "fixture-key",
            "openai/gpt-4o-mini",
            "https://openrouter.ai/api/v1",
        )
        .with_provider_id("openrouter"),
    ));
    for model in [
        "meta-llama/llama-3.3-70b-instruct",
        "qwen/qwen-2.5-72b-instruct",
    ] {
        let agent = native_agent_config(
            &host,
            &registry,
            AgentConfig {
                provider: "openrouter".into(),
                model: model.into(),
                sampling: SamplingConfig {
                    max_tokens: Some(2048),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        let preparation = NativeDefinitionPreparation::new(
            agent,
            AgentDefinitionId::new(model.replace('/', "-")).unwrap(),
            1,
            GrantLimits {
                activations: 1,
                invocations: 8,
                tokens: 2_000_000,
                cost_microunits: 250_000,
            },
        )
        .unwrap();
        assert_eq!(preparation.model(), model);
        fixture.registry.with_session_team_stores(&fixture.team, |canonical, content, _| {
            let (definition, existing) = preparation.prepare_content(canonical, content).map_err(native_error)?;
            assert!(existing.is_none());
            let runtime = serde_json::from_value(serde_json::json!({
                "schema_version":2,"max_output_tokens":2048,"max_response_bytes":1048576,
                "initial_limits":{"activations":1,"invocations":8,"tokens":2000000,"cost_microunits":250000},
                "openrouter_observation":{
                    "schema_version":1,"base_url":"https://openrouter.ai/api/v1","model":model,
                    "endpoint_tag":"deepinfra/fp8","provider_name":"DeepInfra","context_tokens":32768,
                    "max_output_tokens":16384,"prompt_price_per_million":"0.36","completion_price_per_million":"0.40",
                    "supported_parameters":["max_tokens","tools"],
                    "non_reasoning_evidence":"openrouter-model-catalog:no-reasoning-contract-v1",
                    "observed_at_ms":1,"billing":"openrouter_credits"
                }
            })).unwrap();
            let first = preparation.capture(canonical, content, &definition, &runtime).map_err(native_error)?;
            let captured_profile = provider_profile(canonical, content, &first.definition);
            let (_, retained) = preparation.prepare_content(canonical, content).map_err(native_error)?;
            let second = preparation.capture(canonical, content, &definition, &retained.unwrap()).map_err(native_error)?;
            assert_eq!(first.definition, second.definition);
            assert_eq!(provider_profile(canonical, content, &second.definition), captured_profile);
            captured.push((definition.snapshot, model.to_owned()));
            Ok(())
        }).unwrap();
    }
    let default = native_agent_config(
        &host,
        &registry,
        AgentConfig {
            provider: "openrouter".into(),
            model: String::new(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(default.model, "openai/gpt-4o-mini");
    let cleanup = fixture
        .registry
        .prepare_session_cleanup(&fixture.session_id, Duration::from_secs(1))
        .await
        .unwrap();
    fixture.registry.complete_session_cleanup(&cleanup).unwrap();
    drop(cleanup);
    let canonical = SessionExecutionStore::open(fixture.ownership, fixture.owner).unwrap();
    let content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    for (definition, model) in captured {
        let (_, profile) = content
            .resolve_provider_profile(&canonical, &definition)
            .unwrap()
            .unwrap();
        assert_eq!(profile.provider(), "openrouter");
        let configuration: serde_json::Value =
            serde_json::from_str(profile.configuration()).unwrap();
        assert_eq!(configuration["openrouter_observation"]["model"], model);
    }
}

struct PendingFixture {
    _root: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    data: SecureDir,
    ownership: Arc<UpgradedFormatOwnership>,
    owner: ExecutionStoreOwner,
    registry: Arc<session_dispatch::SessionDispatchRegistry>,
    team: session_dispatch::SessionTeamToken,
    session_id: String,
}
/// The provider profile evidence retained for a captured definition.
fn provider_profile(
    canonical: &SessionExecutionStore,
    content: &ExecutionContentStore,
    definition: &axocoatl_session::turn_contract::DefinitionSnapshotRef,
) -> axocoatl_session::turn_contract::EvidenceRef {
    content
        .resolve_provider_profile(canonical, &definition.snapshot)
        .unwrap()
        .unwrap()
        .0
        .clone()
}
fn team_profile(
    registry: &session_dispatch::SessionDispatchRegistry,
    team: &session_dispatch::SessionTeamToken,
    definition: &axocoatl_session::turn_contract::DefinitionSnapshotRef,
) -> axocoatl_session::turn_contract::EvidenceRef {
    registry
        .with_session_team_stores(team, |canonical, content, _| {
            Ok(provider_profile(canonical, content, definition))
        })
        .unwrap()
}
fn pending_fixture() -> PendingFixture {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let data = SecureDir::open(root.path()).unwrap();
    let mut sessions = SessionStore::new_in_secure(&data, "sessions").unwrap();
    let ownership = Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    let (session, receipt) = sessions
        .create_native_with_environment(
            &ownership,
            "Native",
            "workspace",
            workspace.path(),
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
    let owner = receipt.owner().clone();
    let registry = Arc::new(session_dispatch::SessionDispatchRegistry::default());
    registry
        .retain_native_session(ownership.clone(), receipt)
        .unwrap();
    let team = registry.session_team_token(&session.id).unwrap();
    PendingFixture {
        _root: root,
        _workspace: workspace,
        data,
        ownership,
        owner,
        registry,
        team,
        session_id: session.id,
    }
}
fn preparation() -> NativeDefinitionPreparation {
    NativeDefinitionPreparation::new(
        AgentConfig {
            id: AgentId::new("conversation"),
            name: "Native fixture".into(),
            provider: "ollama".into(),
            model: "test-model".into(),
            sampling: SamplingConfig {
                max_tokens: Some(128),
                ..Default::default()
            },
            ..Default::default()
        },
        AgentDefinitionId::new("definition").unwrap(),
        1,
        GrantLimits {
            activations: 1,
            invocations: 0,
            tokens: 8192,
            cost_microunits: 0,
        },
    )
    .unwrap()
}
async fn server(
    close: Option<Arc<session_dispatch::SessionDispatchRegistry>>,
    delay: Option<Arc<tokio::sync::Notify>>,
) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/version"))
        .respond_with(move |_: &wiremock::Request| {
            if let Some(registry) = &close {
                registry.close_all_admission().unwrap();
            }
            let response =
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"version":"0.20.6"}));
            if let Some(started) = &delay {
                started.notify_one();
                response.set_delay(Duration::from_secs(1))
            } else {
                response
            }
        })
        .mount(&server)
        .await;
    for (verb, endpoint, value) in [
        (
            "GET",
            "/api/status",
            serde_json::json!({"cloud":{"disabled":true}}),
        ),
        (
            "POST",
            "/api/show",
            serde_json::json!({"details":{"format":"gguf"},"capabilities":["completion"]}),
        ),
        (
            "GET",
            "/api/tags",
            serde_json::json!({"models":[{"name":"test-model:latest","model":"test-model:latest","digest":DIGEST}]}),
        ),
        (
            "GET",
            "/api/ps",
            serde_json::json!({"models":[{"name":"test-model:latest","model":"test-model:latest","digest":DIGEST,"details":{"format":"gguf"},"context_length":2048}]}),
        ),
        (
            "POST",
            "/api/generate",
            serde_json::json!({"model":"test-model:latest","created_at":"2026-09-15T00:00:00Z","response":"","done":true,"done_reason":"load"}),
        ),
    ] {
        Mock::given(method(verb))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_json(value))
            .mount(&server)
            .await;
    }
    server
}
async fn count(server: &MockServer, endpoint: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == endpoint)
        .count()
}

#[tokio::test]
async fn exact_pending_preparation_reuses_bytes_across_close_reopen_and_rejects_stale_entry() {
    let fixture = pending_fixture();
    let server = server(None, None).await;
    let first = prepare_retained_native_definition(
        &fixture.registry,
        &fixture.team,
        &fixture.data,
        &server.uri(),
        preparation(),
    )
    .await
    .unwrap();
    let second = prepare_retained_native_definition(
        &fixture.registry,
        &fixture.team,
        &fixture.data,
        &server.uri(),
        preparation(),
    )
    .await
    .unwrap();
    assert_eq!(first.definition, second.definition);
    assert_eq!(
        team_profile(&fixture.registry, &fixture.team, &first.definition),
        team_profile(&fixture.registry, &fixture.team, &second.definition)
    );
    assert_eq!(first.profile, second.profile);
    assert_eq!(count(&server, "/api/generate").await, 1);
    assert_eq!(count(&server, "/api/chat").await, 0);
    assert_eq!(count(&server, "/api/pull").await, 0);
    let original_profile = team_profile(&fixture.registry, &fixture.team, &first.definition);
    let original = fixture
        .registry
        .with_session_team_stores(&fixture.team, |canonical, content, _| {
            assert!(canonical.records().map_err(native_error)?.is_empty());
            let (_, profile) = content
                .resolve_provider_profile(canonical, &first.definition.snapshot)
                .map_err(native_error)?
                .unwrap();
            Ok(profile.configuration().to_owned())
        })
        .unwrap();
    let cleanup = fixture
        .registry
        .prepare_session_cleanup(&fixture.session_id, Duration::from_secs(1))
        .await
        .unwrap();
    fixture.registry.complete_session_cleanup(&cleanup).unwrap();
    drop(cleanup);
    fixture
        .registry
        .reopen_session(&fixture.session_id)
        .unwrap();
    let canonical =
        SessionExecutionStore::open(fixture.ownership.clone(), fixture.owner.clone()).unwrap();
    let content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let memory = ActivationStateStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ActivationState)
            .unwrap(),
    )
    .unwrap();
    let mut stores = Some(RetainedSessionStores {
        canonical,
        content,
        memory,
    });
    fixture
        .registry
        .retain_existing_session(&mut stores)
        .unwrap();
    let fresh = fixture
        .registry
        .session_team_token(&fixture.session_id)
        .unwrap();
    let before = server.received_requests().await.unwrap().len();
    assert!(prepare_retained_native_definition(
        &fixture.registry,
        &fixture.team,
        &fixture.data,
        &server.uri(),
        preparation()
    )
    .await
    .is_err());
    assert_eq!(server.received_requests().await.unwrap().len(), before);
    let third = prepare_retained_native_definition(
        &fixture.registry,
        &fresh,
        &fixture.data,
        &server.uri(),
        preparation(),
    )
    .await
    .unwrap();
    assert_eq!(
        team_profile(&fixture.registry, &fresh, &third.definition),
        original_profile
    );
    fixture
        .registry
        .with_session_team_stores(&fresh, |canonical, content, _| {
            let (_, profile) = content
                .resolve_provider_profile(canonical, &first.definition.snapshot)
                .map_err(native_error)?
                .unwrap();
            assert_eq!(profile.configuration(), original);
            Ok(())
        })
        .unwrap();
    assert_eq!(count(&server, "/api/generate").await, 1);
}

#[tokio::test]
async fn closing_during_metadata_prevents_capture_into_retired_pending_session() {
    let fixture = pending_fixture();
    let server = server(Some(fixture.registry.clone()), None).await;
    let (definition, _) = fixture
        .registry
        .with_session_team_stores(&fixture.team, |canonical, content, _| {
            preparation()
                .prepare_content(canonical, content)
                .map_err(native_error)
        })
        .unwrap();
    assert!(prepare_retained_native_definition(
        &fixture.registry,
        &fixture.team,
        &fixture.data,
        &server.uri(),
        preparation()
    )
    .await
    .is_err());
    assert!(fixture
        .registry
        .retains_session(&fixture.session_id)
        .unwrap());
    let cleanup = fixture
        .registry
        .prepare_session_cleanup(&fixture.session_id, Duration::from_secs(1))
        .await
        .unwrap();
    fixture.registry.complete_session_cleanup(&cleanup).unwrap();
    drop(cleanup);
    let canonical =
        SessionExecutionStore::open(fixture.ownership.clone(), fixture.owner.clone()).unwrap();
    let content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    assert!(canonical.records().unwrap().is_empty());
    assert!(content
        .resolve_provider_profile(&canonical, &definition.snapshot)
        .unwrap()
        .is_none());
    assert_eq!(count(&server, "/api/chat").await, 0);
}

#[tokio::test]
async fn cancelled_context_observation_retains_owned_stores_without_inventing_a_profile() {
    let fixture = pending_fixture();
    let started = Arc::new(tokio::sync::Notify::new());
    let server = server(None, Some(started.clone())).await;
    let url = server.uri();
    let mut request = Box::pin(prepare_retained_native_definition(
        &fixture.registry,
        &fixture.team,
        &fixture.data,
        &url,
        preparation(),
    ));
    tokio::select! {
        _=started.notified()=>{},
        _=&mut request=>panic!("metadata request should be waiting"),
    }
    drop(request);
    assert!(SessionExecutionStore::open(fixture.ownership.clone(), fixture.owner.clone()).is_err());
    fixture
        .registry
        .with_session_team_stores(&fixture.team, |canonical, content, _| {
            let (_, profile) = preparation()
                .prepare_content(canonical, content)
                .map_err(native_error)?;
            assert!(profile.is_none());
            assert!(canonical.records().map_err(native_error)?.is_empty());
            Ok(())
        })
        .unwrap();
    assert_eq!(count(&server, "/api/generate").await, 0);
    assert_eq!(count(&server, "/api/chat").await, 0);
}
