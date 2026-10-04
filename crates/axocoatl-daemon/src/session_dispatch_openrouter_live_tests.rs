//! Live native OpenRouter turns, ignored by default. Each runs one small
//! tool-calling activation on a current reasoning model through the actual
//! endpoint selection, provider, actor, tool execution and grant settlement,
//! and spends about a cent of OpenRouter credit. Run with the key in the
//! environment:
//!
//! ```text
//! OPENROUTER_API_KEY=... cargo test -p axocoatl-daemon --lib openrouter_live -- --ignored --nocapture
//! ```
use super::*;
use axocoatl_core::ReasoningEffort;
use axocoatl_llm_openai::{REASONING_DETAILS_METADATA, REASONING_TOKENS_METADATA};

/// Forwards to the real provider and keeps each request the actor sent, so
/// the test can see the reasoning the actor's history carried back.
struct Recording {
    inner: Arc<dyn LlmProvider>,
    requests: Mutex<Vec<ChatRequest>>,
}

#[async_trait]
impl LlmProvider for Recording {
    fn provider_id(&self) -> &str {
        self.inner.provider_id()
    }
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }
    fn capabilities_for(&self, request: &ChatRequest) -> ProviderCapabilities {
        self.inner.capabilities_for(request)
    }
    fn model_constraints_known(&self, request: &ChatRequest) -> bool {
        self.inner.model_constraints_known(request)
    }
    fn validate_request(&self, request: &ChatRequest) -> std::result::Result<(), ProviderError> {
        self.inner.validate_request(request)
    }
    fn execution_bounds(&self, request: &ChatRequest) -> Option<ProviderExecutionBounds> {
        self.inner.execution_bounds(request)
    }
    fn count_tokens(&self, request: &ChatRequest) -> usize {
        self.inner.count_tokens(request)
    }
    async fn chat(&self, request: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.chat(request).await
    }
    async fn chat_with_accounting(&self, request: ChatRequest) -> axocoatl_llm::AccountedChatOutcome {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.chat_with_accounting(request).await
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<
        Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    > {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.chat_stream(request).await
    }
}

const SYSTEM: &str = "You are checking a tool. Before calling it, reason step by step in your \
    thinking: find the smallest prime p greater than 1000 such that p + 2 is also prime. Then \
    call the `effect` tool exactly once with the arguments {\"value\": \"<p>\"}. When its \
    result arrives, reply with the single word: done.";

async fn live_turn(model: &str, effort: ReasoningEffort) {
    let key = std::env::var("OPENROUTER_API_KEY")
        .expect("the live OpenRouter test needs OPENROUTER_API_KEY in its environment");
    // A modest grant: 100,000 tokens and $0.50. Reserving the 1,000,000-token
    // context window for each call, as 1.2.0 did, could not start one here.
    let limits = GrantLimits {
        activations: 16,
        invocations: 32,
        tokens: 100_000,
        cost_microunits: 500_000,
    };
    let fixture = input_fixture_configured(
        false,
        [(&["effect"], None), (&["effect"], None)],
        &|config| {
            config.provider = "openrouter".into();
            config.model = model.into();
            config.system_prompt = Some(SYSTEM.into());
            config.sampling.max_tokens = Some(512);
            config.sampling.reasoning_effort = Some(effort);
        },
        limits.clone(),
    );
    let node = &fixture.parent;

    // Endpoint selection and the retained profile, as Team & budget does it.
    let profiles = axocoatl_llm_openai::observe_native_openrouter_profiles(
        "https://openrouter.ai/api/v1",
        &key,
        model,
    )
    .await
    .unwrap();
    let preparation = NativeDefinitionPreparation::new(
        node.config.clone(),
        node.input.definition.definition_id.clone(),
        1,
        limits.clone(),
    )
    .unwrap();
    let runtime = preparation.openrouter_runtime(profiles).unwrap();
    let endpoint = runtime.openrouter_endpoint().unwrap().to_owned();
    let credentials = NativeProviderCredentials {
        openrouter_api_key: Some(key),
        openrouter_credits_only: true,
        ..Default::default()
    };
    let provider = Arc::new(Recording {
        inner: runtime.verify_credentials(&credentials).await.unwrap(),
        requests: Mutex::new(Vec::new()),
    });

    start_input(&fixture.controller, node);
    let mut resources = input_resources(node, InputProvider::new("unused", false, false));
    resources.provider = provider.clone();
    let result = fixture
        .controller
        .prepare_autonomous_activation(node.input.activation.clone(), resources)
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(result.accepted, "{model}: {:?}", result.failure);

    let usage = fixture
        .controller
        .activation_provider_usage(&node.input.activation)
        .unwrap();
    let state = fixture.controller.lock().unwrap();
    let grant = state.authority.usage("input-grant").unwrap();
    let largest = state
        .authority
        .largest_provider_reservation(&node.input.activation)
        .unwrap()
        .unwrap();
    let tools = state
        .canonical
        .records()
        .unwrap()
        .iter()
        .filter_map(|record| match &record.event {
            TurnContractEvent::RecordOutcome {
                invocation_id,
                outcome: InvocationOutcome::Succeeded,
                ..
            } => Some(
                state
                    .audit
                    .invocation(invocation_id)
                    .unwrap()
                    .unwrap()
                    .intent
                    .tool_name
                    .clone(),
            ),
            _ => None,
        })
        .collect::<Vec<_>>();
    drop(state);
    println!(
        "{}",
        serde_json::json!({
            "model": model,
            "endpoint": endpoint,
            "effort": effort.as_str(),
            "provider_calls": usage.calls,
            "input_tokens": usage.tokens.usage.input_tokens,
            "output_tokens": usage.tokens.usage.output_tokens,
            "reasoning_tokens": usage.tokens.usage.reasoning_tokens.unwrap_or(0),
            "usage_complete": usage.tokens.complete,
            "cost_microusd": usage.cost_microunits,
            "grant_tokens_charged": grant.tokens,
            "grant_cost_charged_microusd": grant.cost_microunits,
            "largest_reservation_tokens": largest.tokens,
            "largest_reservation_microusd": largest.cost_microunits,
            "succeeded_tools": tools,
        })
    );

    // The answer's request carried the tool turn's reasoning back: the
    // actor kept it on the tool call, with the tokens it reported.
    let requests = provider.requests.lock().unwrap();
    let replayed = requests
        .last()
        .unwrap()
        .messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .find(|call| call.provider_metadata.contains_key(REASONING_DETAILS_METADATA))
        .unwrap_or_else(|| panic!("{model}: the tool turn's reasoning was not sent back"));
    assert!(
        replayed.provider_metadata[REASONING_TOKENS_METADATA]
            .parse::<usize>()
            .unwrap()
            > 0
    );
    drop(requests);

    // One tool call, then the answer: at least two settled provider calls.
    assert_eq!(tools, ["effect"], "{model}");
    assert!(usage.calls >= 2, "{model}: {usage:?}");
    assert_eq!(usage.unsettled_calls, 0, "{model}");
    assert!(usage.tokens.complete && usage.cost_known, "{model}: {usage:?}");
    assert!(usage.cost_microunits > 0, "{model}");
    // The model reasoned; that reasoning went back with the tool result
    // (or the second call would have been refused) and was billed as output.
    assert!(
        usage.tokens.usage.reasoning_tokens.unwrap_or(0) > 0,
        "{model}: {usage:?}"
    );
    // Every call settled to what OpenRouter reported: the grant is charged
    // exactly the measured tokens (reasoning included) and the billed cost.
    assert_eq!(grant.tokens, usage.tokens.usage.total() as u64, "{model}");
    assert_eq!(grant.cost_microunits, usage.cost_microunits, "{model}");
    // Each reservation was sized to its request, far below the window.
    assert!(largest.tokens < 30_000, "{model}: {largest:?}");
    assert!(largest.cost_microunits < limits.cost_microunits / 2, "{model}: {largest:?}");
}

#[tokio::test]
#[ignore = "live: needs OPENROUTER_API_KEY and spends OpenRouter credit"]
async fn openrouter_live_claude_sonnet_reasoning_tool_turn_settles_the_grant() {
    live_turn("anthropic/claude-sonnet-5.5", ReasoningEffort::High).await;
}

#[tokio::test]
#[ignore = "live: needs OPENROUTER_API_KEY and spends OpenRouter credit"]
async fn openrouter_live_gpt_sol_reasoning_tool_turn_settles_the_grant() {
    live_turn("openai/gpt-5.6-sol", ReasoningEffort::Low).await;
}
