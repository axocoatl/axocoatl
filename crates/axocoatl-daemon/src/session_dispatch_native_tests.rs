//! Factory tests use a local HTTP fixture and real owned canonical journals.
//! They do not imply actual model inference or native live startup coverage.
use super::*;
use axocoatl_core::{AgentId, ChatMessage, SamplingConfig, SecureDir, TokenBudget};
use axocoatl_llm::ChatRequest;
use axocoatl_llm_ollama::observe_native_ollama_context;
use axocoatl_session::execution_content::ExecutionRequestContent;
use axocoatl_session::execution_ownership::{LegacyFormatOwnership, UpgradedFormatOwnership};
use axocoatl_session::execution_store::ExecutionStoreOwner;
use std::collections::BTreeMap;
use std::path::Path;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

const DIGEST: &str = "a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72";
struct Counter;
impl TokenCounter for Counter {
    fn count_text(&self, text: &str) -> usize {
        text.len() / 4 + 1
    }
    fn count_messages(&self, messages: &[ChatMessage]) -> usize {
        messages
            .iter()
            .map(|message| {
                message
                    .text_content()
                    .map_or(1, |text| self.count_text(text))
            })
            .sum()
    }
    fn count_tool_definition(&self, definition: &serde_json::Value) -> usize {
        self.count_text(&definition.to_string())
    }
}
fn config() -> AgentConfig {
    AgentConfig {
        id: AgentId::new("conversation"),
        name: "Retained local actor".into(),
        provider: "ollama".into(),
        model: "test-model".into(),
        sampling: SamplingConfig {
            max_tokens: Some(128),
            ..Default::default()
        },
        ..Default::default()
    }
}
fn limits(tokens: u64) -> GrantLimits {
    GrantLimits {
        activations: 2,
        invocations: 2,
        tokens,
        cost_microunits: 0,
    }
}

#[tokio::test]
async fn openrouter_billing_requires_explicit_credits_before_observation() {
    let mut config = config();
    config.provider = "openrouter".into();
    config.model = "meta-llama/llama-3.3-70b-instruct".into();
    let preparation = NativeDefinitionPreparation::new(
        config,
        AgentDefinitionId::new("credit-declaration").unwrap(),
        1,
        limits(200_000),
    )
    .unwrap();
    let credentials = NativeProviderCredentials {
        openrouter_api_key: Some("fixture-only-never-send".into()),
        ..Default::default()
    };
    let result = preparation.observe_configured(&credentials).await;
    assert!(matches!(result, Err(error) if error.to_string().contains("not BYOK provider keys")));
    let rejected_route = NativeProviderCredentials {
        openrouter_credits_only: true,
        openrouter_configuration_error: Some("configured fallback is unsupported".into()),
        ..credentials
    };
    assert!(
        matches!(preparation.observe_configured(&rejected_route).await,
        Err(error) if error.to_string().contains("configured fallback"))
    );
}
/// The native Ollama provider a Session runs waits as
/// `providers.ollama` says: its first-token and idle timeouts, else
/// Ollama's defaults.
#[tokio::test]
async fn a_native_ollama_provider_gets_the_configured_timeouts() {
    use axocoatl_llm::transport::StreamTimeouts;
    let (server, _) = metadata_server().await;
    let preparation = NativeDefinitionPreparation::new(
        config(),
        AgentDefinitionId::new("timeouts").unwrap(),
        1,
        limits(8192),
    )
    .unwrap();
    let configured = StreamTimeouts {
        first_token: std::time::Duration::from_secs(1801),
        idle: std::time::Duration::from_secs(91),
    };
    let credentials = NativeProviderCredentials {
        ollama_base_url: Some(server.uri()),
        ollama_timeouts: Some(configured),
        ..Default::default()
    };
    let runtime = preparation.observe_configured(&credentials).await.unwrap();
    let provider = runtime
        .verify_credentials(&credentials, None)
        .await
        .unwrap();
    assert_eq!(provider.stream_timeouts(), Some(configured));
    let unset = NativeProviderCredentials {
        ollama_timeouts: None,
        ..credentials
    };
    let provider = runtime.verify_credentials(&unset, None).await.unwrap();
    assert_eq!(provider.stream_timeouts(), Some(StreamTimeouts::LOCAL));
}

fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, path: &Path, output: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, output);
            } else {
                output.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    walk(root, root, &mut result);
    result
}
async fn metadata_server() -> (MockServer, Arc<Mutex<Option<SessionDispatchController>>>) {
    let server = MockServer::start().await;
    crate::ollama_contract_test_support::mount_contract_probes(&server).await;
    let revoke = Arc::new(Mutex::new(None::<SessionDispatchController>));
    let hook = revoke.clone();
    Mock::given(method("GET"))
        .and(path("/api/version"))
        .respond_with(move |_: &wiremock::Request| {
            if let Some(controller) = hook.lock().unwrap().take() {
                let state = controller.lock().unwrap();
                state
                    .authority
                    .revoke_grant("grant", state.authority.revision().unwrap())
                    .unwrap();
            }
            ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"version": axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION}),
            )
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
            serde_json::json!({"models":[{"name":"test-model:latest","model":"test-model:latest","digest":DIGEST,"details":{"format":"gguf"},"context_length":2048,"runner":"llamacpp"}]}),
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
    (server, revoke)
}
async fn calls(server: &MockServer, endpoint: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        // The contract probes are checks, not inference.
        .filter(|request| {
            request.url.path() == endpoint
                && !crate::ollama_contract_test_support::is_contract_probe(request)
        })
        .count()
}
struct NativeFixture {
    _root: tempfile::TempDir,
    ownership: Arc<UpgradedFormatOwnership>,
    owner: ExecutionStoreOwner,
    controller: SessionDispatchController,
    activation: ActivationRef,
}
fn factory_fixture(
    config: AgentConfig,
    observation: Option<NativeOllamaContextObservation>,
) -> NativeFixture {
    let limits = limits(8192);
    let input = "Use exactly this retained prompt";
    let root = crate::private_tempdir_test_support::private_tempdir();
    let ownership = Arc::new(
        LegacyFormatOwnership::acquire(root.path())
            .unwrap()
            .upgrade()
            .unwrap(),
    );
    let owner = ExecutionStoreOwner {
        workspace_id: "workspace".into(),
        session_id: SessionId::new("session").unwrap(),
    };
    let mut canonical = SessionExecutionStore::open(ownership.clone(), owner.clone()).unwrap();
    let mut content = ExecutionContentStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ExecutionContent)
            .unwrap(),
    )
    .unwrap();
    let turn = LogicalTurnId::new("turn").unwrap();
    let activation = ActivationRef {
        session_id: owner.session_id.clone(),
        turn_id: turn.clone(),
        execution_epoch_id: ExecutionEpochId::new("epoch").unwrap(),
        node_id: TurnNodeId::new("counter").unwrap(),
        generation: 1,
        activation_id: ActivationId::new("activation").unwrap(),
    };
    let profile = ExecutionProfile {
        definition: "counter".into(),
        provider: config.provider.clone(),
        model: config.model.clone(),
        isolation: "in-process".into(),
        tools: config.tools.clone(),
        write_scope: None,
    };
    let definition_id = AgentDefinitionId::new("counter").unwrap();
    let definition = content
        .retain_activation_evidence(ActivationEvidenceContent::Definition {
            definition_id: definition_id.clone(),
            revision: 1,
            profile: profile.clone(),
            configuration: serde_json::to_string(&config).unwrap(),
        })
        .unwrap();
    if let Some(observation) = observation {
        let preparation = NativeDefinitionPreparation::new(
            config.clone(),
            definition_id.clone(),
            1,
            limits.clone(),
        )
        .unwrap();
        let (exact, previous) = preparation
            .prepare_content(&canonical, &mut content)
            .unwrap();
        assert!(previous.is_none());
        assert_eq!(exact.snapshot, *definition.reference());
        let runtime = preparation.observed(observation).unwrap();
        preparation
            .capture(&canonical, &mut content, &exact, &runtime)
            .unwrap();
    }
    let approval = content
        .retain_activation_evidence(ActivationEvidenceContent::Guidance {
            text: "Host approved the bounded counting tool".into(),
        })
        .unwrap();
    let policy = AuthorityGrant {
        id: "grant".into(),
        revision: 1,
        issuer_evidence: approval.reference().clone(),
        holder: activation.node_id.clone(),
        descendants: vec![],
        allow_stop_descendants: false,
        delegation: None,
        conditions: vec![],
        profiles: vec![profile.clone()],
        limits: limits.clone(),
        expires_at_ms: now_ms().unwrap() + 3_600_000,
    };
    let grant = content
        .retain_activation_evidence(ActivationEvidenceContent::Grant {
            policy: policy.clone(),
        })
        .unwrap();
    let budget = content
        .retain_activation_evidence(ActivationEvidenceContent::Budget { limits })
        .unwrap();
    let request_content = ExecutionRequestContent {
        turn_id: turn.clone(),
        recorded_at_unix_ms: now_ms().unwrap(),
        display_input: input.into(),
        effective_input: input.into(),
        context: vec![],
        target_definition: Some(definition_id.clone()),
        model: None,
    };
    let request = content.retain_request(request_content).unwrap();
    let definition = DefinitionSnapshotRef {
        definition_id,
        snapshot: definition.reference().clone(),
    };
    canonical
        .begin_with_request(
            TurnContractEnvelope {
                schema_version: TURN_CONTRACT_SCHEMA_VERSION,
                command_id: CommandId::new("begin").unwrap(),
                expected_revision: 0,
                session_id: owner.session_id.clone(),
                turn_id: turn.clone(),
                event: TurnContractEvent::Begin {
                    epoch_id: activation.execution_epoch_id.clone(),
                    predecessor: None,
                    graph: TurnGraphSnapshot {
                        snapshot_id: GraphSnapshotId::new("graph").unwrap(),
                        revision: 1,
                        nodes: vec![GraphNode {
                            node_id: activation.node_id.clone(),
                            slot_id: SessionTeamSlotId::new("slot").unwrap(),
                            definition: definition.clone(),
                            conversation_id: NodeConversationId::new("conversation").unwrap(),
                            starting_savepoint: ConversationSavepoint::Empty,
                            required: true,
                        }],
                        dependencies: vec![],
                        conditions: vec![],
                    },
                },
            },
            &request,
        )
        .unwrap();
    canonical
        .append(TurnContractEnvelope {
            schema_version: TURN_CONTRACT_SCHEMA_VERSION,
            command_id: CommandId::new("start").unwrap(),
            expected_revision: 1,
            session_id: owner.session_id.clone(),
            turn_id: turn.clone(),
            event: TurnContractEvent::StartActivation {
                input: Box::new(ActivationInputManifest {
                    manifest_id: InputManifestId::new("input").unwrap(),
                    activation: activation.clone(),
                    definition,
                    conversation_id: NodeConversationId::new("conversation").unwrap(),
                    starting_savepoint: ConversationSavepoint::Empty,
                    parents: vec![],
                    guidance: vec![request.reference().clone()],
                    attachments: vec![],
                    repository: RepositoryInput::Unavailable,
                    budget: budget.reference().clone(),
                    grant: Some(GrantSnapshotRef {
                        grant_id: GrantId::new("grant").unwrap(),
                        revision: 1,
                        evidence: grant.reference().clone(),
                    }),
                    revision_context: None,
                }),
            },
        })
        .unwrap();
    let memory = ActivationStateStore::open_owned(
        canonical
            .component_namespace(ExecutionComponent::ActivationState)
            .unwrap(),
    )
    .unwrap();
    let controller = SessionDispatchController::open_retained(
        RetainedSessionStores {
            canonical,
            content,
            memory,
        },
        turn,
    )
    .map_err(|failure| failure.error)
    .unwrap();
    controller.install_grant(policy).unwrap();
    NativeFixture {
        _root: root,
        ownership,
        owner,
        controller,
        activation,
    }
}

fn manifest(fixture: &NativeFixture) -> ActivationInputManifest {
    fixture
        .controller
        .snapshot()
        .unwrap()
        .contract()
        .activations()
        .iter()
        .find(|record| record.activation == fixture.activation)
        .unwrap()
        .input
        .clone()
}
fn factory(fixture: &NativeFixture, url: &str) -> Arc<dyn AutonomousActivationFactory> {
    fixture
        .controller
        .native_ollama_factory(
            &SecureDir::open(fixture._root.path()).unwrap(),
            url,
            Arc::new(Counter),
        )
        .unwrap()
}

#[test]
fn explicit_output_is_preserved_or_refused_and_json_reserves_both_passes() {
    let mut config = config();
    assert_eq!(
        native_output_limit(&config, 2048, &limits(2176)).unwrap(),
        128
    );
    assert!(native_output_limit(&config, 2048, &limits(2175)).is_err());
    config.sampling.response_format = Some(ResponseFormat::Json);
    assert!(native_output_limit(&config, 2048, &limits(4351)).is_err());
    assert_eq!(
        native_output_limit(&config, 2048, &limits(4352)).unwrap(),
        128
    );
    config.sampling.max_tokens = Some(0);
    assert!(native_output_limit(&config, 2048, &limits(8192)).is_err());
}

fn legacy_openrouter_observation(model: &str) -> axocoatl_llm_openai::NativeOpenRouterObservation {
    axocoatl_llm_openai::NativeOpenRouterObservation {
        schema_version: 1,
        base_url: "https://openrouter.ai/api/v1".into(),
        model: model.into(),
        endpoint_tag: "deepinfra/fp8".into(),
        provider_name: "DeepInfra".into(),
        context_tokens: 32_768,
        max_output_tokens: 16_384,
        prompt_price_per_million: "0.36".into(),
        completion_price_per_million: "0.4".into(),
        supported_parameters: vec![
            "max_tokens".into(),
            "tools".into(),
            "response_format".into(),
        ],
        non_reasoning_evidence: Some("openrouter-model-catalog:no-reasoning-contract-v1".into()),
        max_prompt_tokens: None,
        request_price: None,
        reasoning: None,
        unrequested_priced_features: vec![],
        observed_at_ms: 1,
        billing: "openrouter_credits".into(),
    }
}

/// The public catalog contract of anthropic/claude-sonnet-5.5 on its
/// first-party endpoint, 2026-10-03.
fn claude_observation() -> axocoatl_llm_openai::NativeOpenRouterObservation {
    axocoatl_llm_openai::NativeOpenRouterObservation {
        schema_version: 2,
        endpoint_tag: "anthropic".into(),
        provider_name: "Anthropic".into(),
        context_tokens: 1_000_000,
        max_output_tokens: 128_000,
        prompt_price_per_million: "4".into(),
        completion_price_per_million: "10".into(),
        supported_parameters: vec!["max_tokens".into(), "reasoning".into(), "tools".into()],
        non_reasoning_evidence: None,
        reasoning: Some(axocoatl_llm_openai::NativeOpenRouterReasoning {
            mandatory: true,
            enabled_by_default: true,
            efforts: axocoatl_llm_openai::NativeOpenRouterEfforts::Listed(
                ["max", "xhigh", "high", "medium", "low"]
                    .map(str::to_owned)
                    .to_vec(),
            ),
            default_effort: Some("high".into()),
        }),
        unrequested_priced_features: vec!["web_search".into()],
        ..legacy_openrouter_observation("anthropic/claude-sonnet-5.5")
    }
}

#[test]
fn openrouter_json_admission_uses_one_call_and_preserves_finite_limits() {
    let mut config = config();
    config.provider = "openrouter".into();
    config.model = "qwen/qwen-2.5-72b-instruct".into();
    config.sampling.max_tokens = Some(4096);
    config.sampling.response_format = Some(ResponseFormat::Json);
    config.token_budget = Some(TokenBudget {
        per_call: 40_000,
        per_execution: 40_000,
        overflow_policy: OverflowPolicy::Abort,
    });
    let observation = legacy_openrouter_observation(&config.model);
    let initial_limits = GrantLimits {
        cost_microunits: 1_000_000,
        ..limits(40_000)
    };
    let selected = openrouter_output_limit(&config, &observation, &initial_limits, None).unwrap();
    assert_eq!(selected, 4096);
    let mut runtime = NativeOpenRouterRuntimeConfiguration {
        schema_version: 2,
        openrouter_observation: observation,
        max_output_tokens: selected,
        max_response_bytes: NATIVE_RESPONSE_BYTES,
        initial_limits,
        reasoning: None,
        failover_observations: None,
    };
    runtime.validate(&config).unwrap();
    // The smallest call: the 4,096-token template allowance and the output,
    // not the 32,768-token context window.
    let bound = runtime
        .openrouter_observation
        .minimum_call_bounds(selected, None, NATIVE_RESPONSE_BYTES)
        .unwrap();
    assert_eq!(bound.token_limit, 8192);
    assert_eq!(bound.cost_microunits, 3113);

    // Admission and retained-profile validation accept the exact smallest
    // call without clamping the approved output or weakening any limit.
    runtime.initial_limits.tokens = bound.token_limit;
    runtime.initial_limits.cost_microunits = bound.cost_microunits;
    runtime.validate(&config).unwrap();
    runtime.initial_limits.tokens -= 1;
    assert!(runtime.validate(&config).is_err());
    runtime.initial_limits.tokens = bound.token_limit;
    runtime.initial_limits.cost_microunits -= 1;
    let costly = runtime.validate(&config).err().unwrap().to_string();
    assert!(costly.contains("can cost up to $0.003113"), "{costly}");
    runtime.initial_limits.cost_microunits = bound.cost_microunits;
    config.token_budget.as_mut().unwrap().per_call = bound.token_limit as usize - 1;
    assert!(runtime.validate(&config).is_err());
    config.token_budget.as_mut().unwrap().per_call = 40_000;
    config.token_budget.as_mut().unwrap().per_execution = bound.token_limit as usize - 1;
    assert!(runtime.validate(&config).is_err());
    config.token_budget.as_mut().unwrap().per_execution = 40_000;
    runtime.max_output_tokens -= 1;
    assert!(
        runtime.validate(&config).is_err(),
        "explicit output must remain exact"
    );

    // Reopen an older JSON profile whose output was implicitly derived:
    // 73,728 / 2 - 32,768 = 4,096. Half of the whole call now permits the
    // endpoint's 16,384, but recovery keeps the original request ceiling.
    config.sampling.max_tokens = None;
    config.token_budget.as_mut().unwrap().per_call = 73_728;
    config.token_budget.as_mut().unwrap().per_execution = 73_728;
    runtime.initial_limits.tokens = 73_728;
    runtime.max_output_tokens = 4096;
    let restored: NativeOpenRouterRuntimeConfiguration =
        serde_json::from_str(&serde_json::to_string(&runtime).unwrap()).unwrap();
    assert!(!serde_json::to_string(&restored)
        .unwrap()
        .contains("\"reasoning\":"));
    assert_eq!(
        openrouter_output_limit(
            &config,
            &restored.openrouter_observation,
            &restored.initial_limits,
            None,
        )
        .unwrap(),
        16_384
    );
    restored.validate(&config).unwrap();
    assert_eq!(restored.max_output_tokens, 4096);
    runtime.max_output_tokens = 16_385;
    assert!(runtime.validate(&config).is_err());
    runtime.max_output_tokens = 0;
    assert!(runtime.validate(&config).is_err());

    // An explicit output that consumes the entire context window must fail
    // admission, not be silently reduced beneath the actor's configured request.
    runtime.openrouter_observation.context_tokens = 2048;
    runtime.openrouter_observation.max_output_tokens = 2048;
    config.sampling.max_tokens = Some(2048);
    assert!(openrouter_output_limit(
        &config,
        &runtime.openrouter_observation,
        &runtime.initial_limits,
        None,
    )
    .is_err());
    config.sampling.max_tokens = Some(2047);
    assert_eq!(
        openrouter_output_limit(
            &config,
            &runtime.openrouter_observation,
            &runtime.initial_limits,
            None,
        )
        .unwrap(),
        2047
    );
}

/// The window a retained native profile states is the one its provider
/// fits each request to: for Ollama the observed context and the output
/// allowance; for OpenRouter the context less the reasoning allowance,
/// within the endpoint's own prompt limit. A profile that is not a native
/// one states none, and the reviewer's change is then fitted to the
/// smallest context onboarding configures.
#[tokio::test]
async fn a_retained_profile_states_the_window_its_requests_are_fitted_to() {
    let (server, _) = metadata_server().await;
    let observed = observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .unwrap();
    let preparation = NativeDefinitionPreparation::new(
        config(),
        AgentDefinitionId::new("counter").unwrap(),
        1,
        limits(200_000),
    )
    .unwrap();
    let runtime = preparation.observed(observed.clone()).unwrap();
    let NativeRuntimeConfiguration::Ollama(ollama) = &runtime else {
        panic!("an Ollama runtime");
    };
    assert_eq!(
        retained_request_window(&serde_json::to_string(&runtime).unwrap()),
        Some((observed.context_tokens, ollama.max_output_tokens))
    );
    let mut sonnet = config();
    sonnet.provider = "openrouter".into();
    sonnet.model = "anthropic/claude-sonnet-5.5".into();
    sonnet.sampling.max_tokens = Some(4096);
    let preparation = NativeDefinitionPreparation::new(
        sonnet,
        AgentDefinitionId::new("sonnet").unwrap(),
        1,
        GrantLimits {
            cost_microunits: 1_000_000,
            ..limits(200_000)
        },
    )
    .unwrap();
    let runtime = preparation
        .openrouter_runtime(vec![claude_observation()])
        .unwrap();
    // A 1,000,000-token context, 4,096 output tokens and 16,384 of
    // reasoning at the model's default effort.
    assert_eq!(
        retained_request_window(&serde_json::to_string(&runtime).unwrap()),
        Some((1_000_000 - 16_384, 4096))
    );
    assert_eq!(
        retained_request_window(r#"{"fixture":"finite local provider; no external model"}"#),
        None
    );
}

#[test]
fn a_reasoning_model_fits_a_modest_grant_and_retains_its_reasoning_setting() {
    use axocoatl_core::ReasoningEffort;
    use axocoatl_llm_openai::NativeOpenRouterReasoningRequest as Reasoning;
    let mut config = config();
    config.provider = "openrouter".into();
    config.model = "anthropic/claude-sonnet-5.5".into();
    config.sampling.max_tokens = Some(4096);
    // 200,000 tokens and $1: before, each call reserved the 1,000,000-token
    // window plus output (over $4 at these rates), so no call could start.
    let initial_limits = GrantLimits {
        cost_microunits: 1_000_000,
        ..limits(200_000)
    };
    let preparation = NativeDefinitionPreparation::new(
        config.clone(),
        AgentDefinitionId::new("sonnet").unwrap(),
        1,
        initial_limits.clone(),
    )
    .unwrap();
    let NativeRuntimeConfiguration::OpenRouter(runtime) = preparation
        .openrouter_runtime(vec![claude_observation()])
        .unwrap()
    else {
        panic!("an OpenRouter runtime");
    };
    // The model's default effort, high: 4x the output for reasoning.
    assert_eq!(
        runtime.reasoning,
        Some(Reasoning::Effort(ReasoningEffort::High))
    );
    assert_eq!(runtime.max_output_tokens, 4096);
    let bound = runtime
        .openrouter_observation
        .minimum_call_bounds(4096, runtime.reasoning, NATIVE_RESPONSE_BYTES)
        .unwrap();
    assert_eq!(bound.token_limit, 4096 + 4096 + 16_384);
    assert_eq!(bound.cost_microunits, 4096 * 4 + 20_480 * 10);
    let retained = serde_json::to_string(&runtime).unwrap();
    assert!(
        retained.contains(r#""reasoning":{"effort":"high"}"#),
        "{retained}"
    );

    // The Agent's own effort replaces the default and changes the allowance;
    // the retained profile no longer matches it.
    config.sampling.reasoning_effort = Some(ReasoningEffort::Low);
    assert!(runtime.validate(&config).is_err());
    let preparation = NativeDefinitionPreparation::new(
        config.clone(),
        AgentDefinitionId::new("sonnet").unwrap(),
        1,
        initial_limits.clone(),
    )
    .unwrap();
    let NativeRuntimeConfiguration::OpenRouter(low) = preparation
        .openrouter_runtime(vec![claude_observation()])
        .unwrap()
    else {
        panic!("an OpenRouter runtime");
    };
    assert_eq!(low.reasoning, Some(Reasoning::Effort(ReasoningEffort::Low)));
    low.validate(&config).unwrap();

    // Refusals name the reason: an effort the model does not accept, and a
    // response allowance the whole-call budget cannot hold.
    config.sampling.reasoning_effort = Some(ReasoningEffort::Minimal);
    let preparation = NativeDefinitionPreparation::new(
        config.clone(),
        AgentDefinitionId::new("sonnet").unwrap(),
        1,
        initial_limits.clone(),
    )
    .unwrap();
    let refused = preparation
        .openrouter_runtime(vec![claude_observation()])
        .err()
        .unwrap()
        .to_string();
    assert!(
        refused.contains("does not accept reasoning effort minimal"),
        "{refused}"
    );
    config.sampling.reasoning_effort = Some(ReasoningEffort::Max);
    config.token_budget = Some(TokenBudget {
        per_call: 40_000,
        per_execution: 400_000,
        overflow_policy: OverflowPolicy::Abort,
    });
    let preparation = NativeDefinitionPreparation::new(
        config.clone(),
        AgentDefinitionId::new("sonnet").unwrap(),
        1,
        initial_limits,
    )
    .unwrap();
    let refused = preparation
        .openrouter_runtime(vec![claude_observation()])
        .err()
        .unwrap()
        .to_string();
    assert!(
        refused.contains("may produce 81920 tokens (4096 of output plus 77824 of reasoning at reasoning effort max)"),
        "{refused}"
    );
    assert!(
        refused.contains("40000-token whole-call budget"),
        "{refused}"
    );

    // Without sampling.max_tokens, the response allowance takes at most half
    // of the whole call: 20,000 tokens at high effort is 4,000 of output.
    config.sampling.max_tokens = None;
    config.sampling.reasoning_effort = None;
    let budget = config.token_budget.as_mut().unwrap();
    budget.per_call = 40_000;
    let derived = openrouter_output_limit(
        &config,
        &claude_observation(),
        &limits(200_000),
        Some(Reasoning::Effort(ReasoningEffort::High)),
    )
    .unwrap();
    assert_eq!(derived, 4000);
}

#[test]
fn absent_sampling_uses_only_existing_abort_capacity_without_warn_default() {
    let mut config = config();
    config.sampling.max_tokens = None;
    assert!(native_output_limit(&config, 2048, &limits(8192)).is_err());
    config.token_budget = Some(TokenBudget {
        per_call: 5000,
        per_execution: 6000,
        overflow_policy: OverflowPolicy::Warn,
    });
    assert!(native_output_limit(&config, 2048, &limits(8192)).is_err());
    config.token_budget.as_mut().unwrap().overflow_policy = OverflowPolicy::Abort;
    assert_eq!(
        native_output_limit(&config, 2048, &limits(8192)).unwrap(),
        2952
    );
    assert_eq!(
        native_output_limit(&config, 2048, &limits(3000)).unwrap(),
        952
    );
    config.sampling.response_format = Some(ResponseFormat::Json);
    assert_eq!(
        native_output_limit(&config, 2048, &limits(8192)).unwrap(),
        452
    );
    config.token_budget.as_mut().unwrap().per_execution = 4096;
    assert!(native_output_limit(&config, 2048, &limits(8192)).is_err());
}

async fn run_owned_factory_actor(config: AgentConfig, expected_output: usize) {
    let (server, _) = metadata_server().await;
    let observed = observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .unwrap();
    let fixture = factory_fixture(config.clone(), Some(observed));
    let input = manifest(&fixture);
    let factory = factory(&fixture, &server.uri());
    let before = tree(fixture._root.path());
    let resources = factory.resources(&input).await.unwrap();
    assert_eq!(
        resources
            .provider
            .execution_bounds(&ChatRequest::simple("prompt"))
            .unwrap()
            .token_limit,
        (2048 + expected_output) as u64
    );
    assert_eq!(
        serde_json::to_vec(&resources.config).unwrap(),
        serde_json::to_vec(&config).unwrap()
    );
    assert_eq!(tree(fixture._root.path()), before);
    assert_eq!(calls(&server, "/api/generate").await, 1);
    assert_eq!(calls(&server, "/api/chat").await, 0);
    assert_eq!(calls(&server, "/api/pull").await, 0);
    Mock::given(method("POST")).and(path("/api/chat")).respond_with(move |request:&wiremock::Request| {
        let body:serde_json::Value=request.body_json().unwrap();
        assert_eq!(body["options"]["num_ctx"],2048);
        assert_eq!(body["options"]["num_predict"],expected_output);
        assert_eq!(body["truncate"],false);
        assert!(body["messages"].as_array().unwrap().iter().any(|message|message["content"].as_str()
            .is_some_and(|text|text.contains("Use exactly this retained prompt"))));
        let reply=serde_json::json!({"model":"test-model","message":{"role":"assistant","content":"Retained response"},
            "done":true,"done_reason":"stop","prompt_eval_count":3,"eval_count":4});
        ResponseTemplate::new(200).set_body_raw(format!("{reply}\n"),"application/x-ndjson")
    }).mount(&server).await;
    let settled = fixture
        .controller
        .prepare_autonomous_activation(fixture.activation.clone(), resources)
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    let usage = fixture
        .controller
        .activation_provider_usage(&fixture.activation)
        .unwrap();
    assert_eq!(usage.calls, 1);
    assert_eq!(usage.unsettled_calls, 0);
    assert!(usage.tokens.complete);
    assert_eq!(
        usage.tokens.usage,
        axocoatl_core::TokenUsageStats::new(3, 4)
    );
    assert_eq!(calls(&server, "/api/chat").await, 1);
    assert!(factory.resources(&input).await.is_err());
    assert_eq!(calls(&server, "/api/chat").await, 1);
}

#[tokio::test]
async fn actual_factory_reuses_owned_profile_without_inference_then_native_actor_reserves_bound() {
    run_owned_factory_actor(config(), 128).await;
}
#[tokio::test]
async fn abort_derived_output_uses_actual_native_context_and_whole_call_reservation() {
    let mut config = config();
    config.sampling.max_tokens = None;
    config.token_budget = Some(TokenBudget {
        per_call: 3000,
        per_execution: 6000,
        overflow_policy: OverflowPolicy::Abort,
    });
    run_owned_factory_actor(config, 952).await;
}

#[tokio::test]
async fn retained_profile_reopens_with_original_bytes_and_conflicting_capture_is_refused() {
    let (server, _) = metadata_server().await;
    let observed = observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .unwrap();
    let fixture = factory_fixture(config(), Some(observed));
    let input = manifest(&fixture);
    let NativeFixture {
        _root,
        ownership,
        owner,
        controller,
        activation,
    } = fixture;
    let original = {
        let mut state = controller.lock().unwrap();
        let DispatchState {
            canonical, content, ..
        } = &mut *state;
        let (reference, profile) = content
            .resolve_provider_profile(canonical, &input.definition.snapshot)
            .unwrap()
            .unwrap();
        let original = (reference.clone(), profile.configuration().to_owned());
        let retained = content
            .retain_provider_profile(
                canonical,
                &input.definition.snapshot,
                "ollama",
                original.1.clone(),
            )
            .unwrap();
        assert_eq!(retained.reference(), &original.0);
        assert!(content
            .retain_provider_profile(canonical, &input.definition.snapshot, "ollama", "{}".into())
            .is_err());
        original
    };
    drop(controller);
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    let reopened = SessionDispatchController::open(canonical, activation.turn_id).unwrap();
    let before = tree(_root.path());
    let factory = reopened
        .native_ollama_factory(
            &SecureDir::open(_root.path()).unwrap(),
            &server.uri(),
            Arc::new(Counter),
        )
        .unwrap();
    // Reopening interrupted this epoch. Retained provider bytes remain
    // readable, but the old activation cannot acquire execution resources.
    let requests_before = server.received_requests().await.unwrap().len();
    let failure = match factory.resources(&input).await {
        Ok(_) => panic!("an interrupted activation regained provider resources"),
        Err(failure) => failure,
    };
    assert!(failure.contains("not current and running"), "{failure}");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests_before
    );
    {
        let state = reopened.lock().unwrap();
        let (reference, profile) = state
            .content
            .resolve_provider_profile(&state.canonical, &input.definition.snapshot)
            .unwrap()
            .unwrap();
        assert_eq!(
            (reference.clone(), profile.configuration().to_owned()),
            original
        );
    }
    assert_eq!(tree(_root.path()), before);
    assert_eq!(calls(&server, "/api/generate").await, 1);
    assert_eq!(calls(&server, "/api/chat").await, 0);
}

#[tokio::test]
async fn revoked_during_metadata_and_forged_manifest_never_gain_resources() {
    let (server, revoke) = metadata_server().await;
    let observed = observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .unwrap();
    let fixture = factory_fixture(config(), Some(observed));
    let factory = factory(&fixture, &server.uri());
    let input = manifest(&fixture);
    let mut forged = input.clone();
    forged.manifest_id = InputManifestId::new("caller-invented").unwrap();
    let requests = server.received_requests().await.unwrap().len();
    assert!(factory.resources(&forged).await.is_err());
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    *revoke.lock().unwrap() = Some(fixture.controller.clone());
    assert!(factory.resources(&input).await.is_err());
    assert!(revoke.lock().unwrap().is_none());
    assert_eq!(calls(&server, "/api/chat").await, 0);
    assert_eq!(calls(&server, "/api/generate").await, 1);
    let state = fixture.controller.lock().unwrap();
    assert_eq!(state.authority.usage("grant").unwrap().activations, 0);
}

#[tokio::test]
async fn missing_before_begin_binding_and_retargeted_endpoint_are_refused_without_network() {
    let (server, _) = metadata_server().await;
    let missing = factory_fixture(config(), None);
    let input = manifest(&missing);
    let before = tree(missing._root.path());
    assert!(factory(&missing, &server.uri())
        .resources(&input)
        .await
        .is_err());
    {
        let mut state = missing.controller.lock().unwrap();
        let DispatchState {
            canonical, content, ..
        } = &mut *state;
        assert!(content
            .retain_provider_profile(canonical, &input.definition.snapshot, "ollama", "{}".into())
            .is_err());
    }
    assert_eq!(tree(missing._root.path()), before);
    assert!(server.received_requests().await.unwrap().is_empty());
    let observed = observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .unwrap();
    let retained = factory_fixture(config(), Some(observed));
    let requests = server.received_requests().await.unwrap().len();
    assert!(factory(&retained, "http://127.0.0.1:1")
        .resources(&manifest(&retained))
        .await
        .is_err());
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    let foreign = tempfile::tempdir().unwrap();
    assert!(retained
        .controller
        .native_ollama_factory(
            &SecureDir::open(foreign.path()).unwrap(),
            &server.uri(),
            Arc::new(Counter)
        )
        .is_err());
}

#[test]
fn identity_looking_standalone_content_cannot_capture_native_definition_or_profile() {
    let fixture = factory_fixture(config(), None);
    let path = crate::private_tempdir_test_support::private_tempdir();
    let state = fixture.controller.lock().unwrap();
    let mut copied =
        ExecutionContentStore::open(path.path(), state.canonical.identity().unwrap()).unwrap();
    let before = tree(path.path());
    let preparation = NativeDefinitionPreparation::new(
        config(),
        AgentDefinitionId::new("forged").unwrap(),
        1,
        limits(8192),
    )
    .unwrap();
    assert!(preparation
        .prepare_content(&state.canonical, &mut copied)
        .is_err());
    assert_eq!(tree(path.path()), before);
    assert!(copied
        .resolve_provider_profile(
            &state.canonical,
            &EvidenceRef::new("invented-profile").unwrap()
        )
        .is_err());
}

#[test]
fn openrouter_admission_refuses_unsent_parameters_and_unfit_responses_by_name() {
    use axocoatl_core::ReasoningEffort;
    let mut config = config();
    config.provider = "openrouter".into();
    config.model = "anthropic/claude-sonnet-5.5".into();
    config.sampling.max_tokens = Some(4096);
    config.sampling.temperature = Some(0.2);
    let initial_limits = GrantLimits {
        cost_microunits: 10_000_000,
        ..limits(1_000_000)
    };
    let prepare = |config: &AgentConfig| {
        NativeDefinitionPreparation::new(
            config.clone(),
            AgentDefinitionId::new("sonnet").unwrap(),
            1,
            initial_limits.clone(),
        )
        .unwrap()
    };
    // The cheapest endpoint does not list temperature: it is skipped for one
    // that does, and with none left the refusal names the parameter.
    let mut azure = claude_observation();
    azure.endpoint_tag = "azure/global".into();
    azure.provider_name = "Azure".into();
    azure.supported_parameters.push("temperature".into());
    azure.supported_parameters.sort();
    let selected = prepare(&config)
        .openrouter_runtime(vec![claude_observation(), azure])
        .unwrap();
    assert_eq!(selected.openrouter_endpoint(), Some("azure/global"));
    let refused = prepare(&config)
        .openrouter_runtime(vec![claude_observation()])
        .err()
        .unwrap()
        .to_string();
    assert!(
        refused.contains("anthropic/claude-sonnet-5.5 on OpenRouter endpoint anthropic does not accept sampling.temperature"),
        "{refused}"
    );
    config.sampling.temperature = None;

    // A response allowance past the endpoint's output limit is refused, not
    // clamped below sampling.max_tokens.
    config.sampling.max_tokens = Some(8192);
    config.sampling.reasoning_effort = Some(ReasoningEffort::Xhigh);
    let refused = prepare(&config)
        .openrouter_runtime(vec![claude_observation()])
        .err()
        .unwrap()
        .to_string();
    assert!(
        refused.contains("sampling.max_tokens 8192 at reasoning effort xhigh")
            && refused.contains("max_tokens 163840")
            && refused.contains("128000 output tokens"),
        "{refused}"
    );
    // One the endpoint allows but whose streamed text can pass the response
    // bytes a Session keeps of one call.
    config.sampling.max_tokens = Some(5300);
    config.sampling.reasoning_effort = Some(ReasoningEffort::Max);
    let refused = prepare(&config)
        .openrouter_runtime(vec![claude_observation()])
        .err()
        .unwrap()
        .to_string();
    assert!(refused.contains("may stream 106000 tokens"), "{refused}");

    // A derived output stops where the response still fits both limits.
    config.sampling.max_tokens = None;
    config.token_budget = Some(TokenBudget {
        per_call: 1_000_000,
        per_execution: 1_000_000,
        overflow_policy: OverflowPolicy::Abort,
    });
    let observation = claude_observation();
    let reasoning = observation
        .reasoning_request(config.sampling.reasoning_effort)
        .unwrap();
    let derived =
        openrouter_output_limit(&config, &observation, &initial_limits, reasoning).unwrap();
    assert_eq!(derived, 5242);
    assert_eq!(observation.response_allowance(derived, reasoning), 104_840);
}

// ------------------------------------------------ OpenRouter endpoint failover

fn openrouter_config(model: &str, max_tokens: usize) -> AgentConfig {
    let mut config = config();
    config.provider = "openrouter".into();
    config.model = model.into();
    config.sampling.max_tokens = Some(max_tokens);
    config
}

/// Team & budget retains, after the cheapest endpoint that fits, up to three
/// more of the model's endpoints, cheapest first, that can take over each of
/// its calls: at least its context and prompt limit, the same output and
/// reasoning setting, and the smallest call within the approved budget.
/// Retained-profile validation refuses any other list.
#[test]
fn openrouter_runtime_retains_up_to_three_endpoints_that_can_take_over_each_call() {
    let config = openrouter_config("qwen/qwen-2.5-72b-instruct", 4096);
    let initial_limits = GrantLimits {
        cost_microunits: 10_000,
        ..limits(40_000)
    };
    let endpoint = |tag: &str, context: usize, prompt: &str| {
        axocoatl_llm_openai::NativeOpenRouterObservation {
            endpoint_tag: tag.into(),
            provider_name: tag.to_uppercase(),
            context_tokens: context,
            prompt_price_per_million: prompt.into(),
            ..legacy_openrouter_observation(&config.model)
        }
    };
    let prepare = || {
        NativeDefinitionPreparation::new(
            config.clone(),
            AgentDefinitionId::new("writer").unwrap(),
            1,
            initial_limits.clone(),
        )
        .unwrap()
    };
    let runtime = prepare()
        .openrouter_runtime(vec![
            endpoint("a/fp8", 32_768, "0.36"),
            // A smaller window cannot take every call the first can.
            endpoint("b/fp8", 16_384, "0.36"),
            // Its smallest call costs more than the approved $0.01.
            endpoint("c/fp8", 32_768, "5"),
            endpoint("d/fp8", 32_768, "0.5"),
            endpoint("e/fp8", 65_536, "0.6"),
            endpoint("f/fp8", 32_768, "0.7"),
            endpoint("g/fp8", 32_768, "0.8"),
        ])
        .unwrap();
    assert_eq!(runtime.openrouter_endpoint(), Some("a/fp8"));
    assert_eq!(
        runtime.openrouter_failover_endpoints(),
        ["d/fp8", "e/fp8", "f/fp8"]
    );
    runtime.validate(&config).unwrap();
    let json = serde_json::to_string(&runtime).unwrap();
    let NativeRuntimeConfiguration::OpenRouter(retained) =
        serde_json::from_str::<NativeRuntimeConfiguration>(&json).unwrap()
    else {
        panic!("{json}");
    };
    assert_eq!(serde_json::to_string(&retained).unwrap(), json);

    // A retained list that names an endpoint twice, one that cannot take
    // over each call, more than three, another model's or another base URL's
    // is refused.
    let tampered = |change: &dyn Fn(&mut Vec<axocoatl_llm_openai::NativeOpenRouterObservation>)| {
        let mut runtime = retained.clone();
        change(runtime.failover_observations.as_mut().unwrap());
        runtime.validate(&config).err().unwrap().to_string()
    };
    assert!(tampered(&|list| list[1] = list[0].clone()).contains("twice"));
    assert!(
        tampered(&|list| list[2] = endpoint("b/fp8", 16_384, "0.36"))
            .contains("cannot take over every call of a/fp8")
    );
    assert!(
        tampered(&|list| list.push(endpoint("g/fp8", 32_768, "0.8")))
            .contains("differs from its admitted definition")
    );
    assert!(tampered(&|list| list[0].model = "qwen/other".into())
        .contains("differs from its admitted definition"));
    assert!(
        tampered(&|list| list[0].base_url = "https://example.test/api/v1".into())
            .contains("cannot take over every call")
    );
    assert!(
        tampered(&|list| list[0].prompt_price_per_million = "5".into())
            .contains("more than the approved $0.010000 cost budget")
    );

    // With no other endpoint that fits, the profile says so: an empty list.
    // An activation adds what the catalog lists then.
    let alone = prepare()
        .openrouter_runtime(vec![
            endpoint("a/fp8", 32_768, "0.36"),
            endpoint("b/fp8", 16_384, "0.36"),
        ])
        .unwrap();
    assert!(alone.openrouter_failover_endpoints().is_empty());
    assert!(serde_json::to_string(&alone)
        .unwrap()
        .contains("\"failover_observations\":[]"));
}

const FAILOVER_MODEL: &str = "meta-llama/test-instruct";

/// A catalog listing `endpoints` (tag, provider, price scale, context) for
/// [`FAILOVER_MODEL`].
async fn failover_catalog(endpoints: &[(&str, &str, u32, usize)]) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":[{
                "id": FAILOVER_MODEL,
                "architecture": {"input_modalities":["text"],"output_modalities":["text"]},
                "supported_parameters": ["max_tokens","tools"]
            }]})),
        )
        .mount(&server)
        .await;
    let rows: Vec<serde_json::Value> = endpoints
        .iter()
        .map(|(tag, provider, scale, context)| {
            serde_json::json!({
                "model_id": FAILOVER_MODEL, "provider_name": provider, "tag": tag,
                "context_length": context, "max_completion_tokens": 128,
                "supported_parameters": ["max_tokens","tools"],
                "pricing": {"prompt": format!("0.00000{scale}"), "completion": format!("0.00000{}", 2 * scale)},
                "status": 0, "supports_implicit_caching": false
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path(format!("/models/{FAILOVER_MODEL}/endpoints")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"data":{"id": FAILOVER_MODEL, "endpoints": rows}}),
            ),
        )
        .mount(&server)
        .await;
    server
}

/// Upgrade compatibility: a native OpenRouter profile as Axocoatl 1.4.0
/// and the 1.5.0 builds before endpoint failover retained it (one endpoint,
/// no failover list) still loads, validates and keeps its exact bytes. Its
/// activation opens with the endpoints of the model that OpenRouter lists
/// healthy now and that pass the checks Team & budget applies to a failover
/// endpoint, so a rate-limited endpoint does not end its runs either. Every
/// activation does so: one of a profile retained with a failover list (even
/// an empty one) also holds every other endpoint that fits now, each once,
/// however many there are; the profile alone connects as retained.
#[tokio::test]
async fn a_profile_retained_before_failover_still_loads_and_its_activation_can_fail_over() {
    let server = failover_catalog(&[
        ("finite/exact", "FiniteProvider", 1, 2048),
        ("other/exact", "OtherProvider", 2, 2048),
        ("small/exact", "SmallProvider", 3, 1024),
    ])
    .await;
    let config = openrouter_config(FAILOVER_MODEL, 32);
    let endpoints_of = |provider: &Arc<dyn LlmProvider>| {
        provider
            .endpoint_failover()
            .map_or(1, |failover| failover.endpoint_count())
    };
    let profiles = axocoatl_llm_openai::observe_native_openrouter_profiles(
        &server.uri(),
        "fixture-inference-key",
        FAILOVER_MODEL,
    )
    .await
    .unwrap();
    let selected = serde_json::to_string(&profiles[0]).unwrap();
    // The fields and their order of 1.4.0's NativeOpenRouterRuntimeConfiguration.
    let legacy = format!(
        r#"{{"schema_version":2,"openrouter_observation":{selected},"max_output_tokens":32,"max_response_bytes":1048576,"initial_limits":{{"activations":2,"invocations":2,"tokens":40000,"cost_microunits":1000000}}}}"#
    );
    let runtime: NativeRuntimeConfiguration = serde_json::from_str(&legacy).unwrap();
    runtime.validate(&config).unwrap();
    assert_eq!(serde_json::to_string(&runtime).unwrap(), legacy);
    assert!(runtime.openrouter_failover_endpoints().is_empty());
    assert!(retained_call_reservation(&legacy, &config).is_some());
    assert!(!retained_profile_refused(&legacy));

    let credentials = NativeProviderCredentials {
        openrouter_api_key: Some("fixture-inference-key".into()),
        openrouter_credits_only: true,
        ..Default::default()
    };
    // An activation of the definition: the model's other endpoint that can
    // take over each call joins; the smaller one does not.
    let provider = runtime
        .verify_credentials(&credentials, Some(&config))
        .await
        .unwrap();
    let failover = provider.endpoint_failover().expect("a second endpoint");
    assert_eq!(failover.endpoint_count(), 2);
    assert_eq!(failover.current_endpoint(), 0);
    // The profile alone, without its definition, connects as retained.
    let provider = runtime
        .verify_credentials(&credentials, None)
        .await
        .unwrap();
    assert!(provider.endpoint_failover().is_none());
    connect_retained_profile(&legacy, &credentials)
        .await
        .unwrap();

    // A profile retained with its (here empty) failover list: its
    // activation holds the other endpoint that fits now too; the profile
    // alone holds what it lists.
    let preparation = NativeDefinitionPreparation::new(
        config.clone(),
        AgentDefinitionId::new("writer").unwrap(),
        1,
        GrantLimits {
            cost_microunits: 1_000_000,
            ..limits(40_000)
        },
    )
    .unwrap();
    let alone = preparation
        .openrouter_runtime(vec![profiles[0].clone()])
        .unwrap();
    assert!(alone.openrouter_failover_endpoints().is_empty());
    let provider = alone
        .verify_credentials(&credentials, Some(&config))
        .await
        .unwrap();
    assert_eq!(endpoints_of(&provider), 2);
    let provider = alone.verify_credentials(&credentials, None).await.unwrap();
    assert_eq!(endpoints_of(&provider), 1);
    let retained = preparation.openrouter_runtime(profiles).unwrap();
    assert_eq!(retained.openrouter_failover_endpoints(), ["other/exact"]);
    let provider = retained
        .verify_credentials(&credentials, Some(&config))
        .await
        .unwrap();
    assert_eq!(endpoints_of(&provider), 2, "each endpoint once");

    // More endpoints fit than a profile retains: an activation holds all.
    let many = failover_catalog(&[
        ("a/exact", "A", 1, 2048),
        ("b/exact", "B", 1, 2048),
        ("c/exact", "C", 2, 2048),
        ("d/exact", "D", 2, 2048),
        ("e/exact", "E", 3, 2048),
        ("f/exact", "F", 3, 2048),
    ])
    .await;
    let profiles = axocoatl_llm_openai::observe_native_openrouter_profiles(
        &many.uri(),
        "fixture-inference-key",
        FAILOVER_MODEL,
    )
    .await
    .unwrap();
    let retained = preparation.openrouter_runtime(profiles).unwrap();
    assert_eq!(
        retained.openrouter_failover_endpoints(),
        ["b/exact", "c/exact", "d/exact"]
    );
    let provider = retained
        .verify_credentials(&credentials, Some(&config))
        .await
        .unwrap();
    assert_eq!(endpoints_of(&provider), 6);
}

/// Each native OpenRouter provider of a Session records, in the Session's
/// log and by model, the endpoint that answered each of its calls once the
/// answer names that endpoint's provider: what a loadout run's Outcome,
/// notes and receipt name.
#[tokio::test]
async fn the_endpoint_that_answered_each_call_is_recorded_for_the_session() {
    let server = failover_catalog(&[("finite/exact", "FiniteProvider", 1, 2048)]).await;
    let first = serde_json::json!({"id":"gen-fixture","model":FAILOVER_MODEL,"provider":"FiniteProvider","choices":[{"index":0,"delta":{"content":"verified"},"finish_reason":"stop"}]});
    let usage = serde_json::json!({"id":"gen-fixture","model":FAILOVER_MODEL,"provider":"FiniteProvider","choices":[],"usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6,"cost":0.000008,"is_byok":false}});
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!("data: {first}\n\ndata: {usage}\n\ndata: [DONE]\n\n"),
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let config = openrouter_config(FAILOVER_MODEL, 32);
    let profiles = axocoatl_llm_openai::observe_native_openrouter_profiles(
        &server.uri(),
        "fixture-inference-key",
        FAILOVER_MODEL,
    )
    .await
    .unwrap();
    let runtime = NativeDefinitionPreparation::new(
        config.clone(),
        AgentDefinitionId::new("writer").unwrap(),
        1,
        GrantLimits {
            cost_microunits: 1_000_000,
            ..limits(40_000)
        },
    )
    .unwrap()
    .openrouter_runtime(profiles)
    .unwrap();
    let log = Arc::new(OpenRouterServedLog::default());
    let credentials = NativeProviderCredentials {
        openrouter_api_key: Some("fixture-inference-key".into()),
        openrouter_credits_only: true,
        openrouter_served: Some(log.clone()),
        ..Default::default()
    };
    assert!(log.endpoints().is_empty());
    let provider = runtime
        .verify_credentials(&credentials, Some(&config))
        .await
        .unwrap();
    assert!(log.endpoints().is_empty(), "opening answers no call");
    for _ in 0..2 {
        provider
            .chat(ChatRequest::simple("one call"))
            .await
            .unwrap();
    }
    assert_eq!(
        log.endpoints(),
        [(
            FAILOVER_MODEL.to_string(),
            vec![axocoatl_llm_openai::ServedOpenRouterEndpoint {
                endpoint: "finite/exact".into(),
                provider: "FiniteProvider".into(),
                calls: 2,
                moved: 0,
            }]
        )]
        .into_iter()
        .collect()
    );
}

/// Whether a retained profile's model changed is asked for only as long as
/// Team & budget waits: an Ollama or an OpenRouter that accepts the
/// connection and does not answer is not taken for a change, and Preview
/// does not wait for its own timeouts (two minutes a request).
#[tokio::test]
async fn a_provider_that_does_not_answer_is_not_taken_for_a_change() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let (server, _) = metadata_server().await;
    let observed = observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .unwrap();
    let mut ollama = serde_json::to_value(NativeRuntimeConfiguration::Ollama(
        NativeOllamaRuntimeConfiguration {
            schema_version: 1,
            observation: observed,
            max_output_tokens: 128,
            max_response_bytes: NATIVE_RESPONSE_BYTES,
            initial_limits: limits(100_000),
        },
    ))
    .unwrap();
    ollama["observation"]["base_url"] = serde_json::json!(silent);
    let mut observation = legacy_openrouter_observation("meta-llama/test-instruct");
    observation.base_url = silent.clone();
    let openrouter = serde_json::to_value(NativeRuntimeConfiguration::OpenRouter(
        NativeOpenRouterRuntimeConfiguration {
            schema_version: 2,
            openrouter_observation: observation,
            max_output_tokens: 128,
            max_response_bytes: NATIVE_RESPONSE_BYTES,
            initial_limits: limits(100_000),
            reasoning: None,
            failover_observations: None,
        },
    ))
    .unwrap();
    let credentials = NativeProviderCredentials {
        ollama_base_url: Some(silent),
        ollama_timeouts: None,
        ollama_served: None,
        openrouter_api_key: Some("fixture-key".into()),
        openrouter_credits_only: true,
        openrouter_configuration_error: None,
        openrouter_timeouts: None,
        openrouter_served: None,
    };
    for configuration in [ollama, openrouter] {
        let started = std::time::Instant::now();
        assert_eq!(
            retained_profile_observation_within(
                &configuration.to_string(),
                &credentials,
                std::time::Duration::from_millis(300),
            )
            .await,
            None
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }
}
