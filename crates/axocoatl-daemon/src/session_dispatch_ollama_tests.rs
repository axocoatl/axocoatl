//! Native Ollama through the durable controller and actual Agent behavior.
use super::*;
use axocoatl_core::{MessageRole, ResponseFormat, SamplingConfig};
use axocoatl_llm_ollama::{NativeOllamaConfig, NativeOllamaProvider};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, Respond, ResponseTemplate,
};

const CONTEXT: usize = 2048;
const OUTPUT: usize = 128;
const ONE_CALL: u64 = (CONTEXT + OUTPUT) as u64;

fn native_config(base_url: String, model: &str) -> NativeOllamaConfig {
    NativeOllamaConfig {
        base_url,
        model: model.into(),
        context_tokens: CONTEXT,
        max_output_tokens: OUTPUT,
        max_response_bytes: 1024 * 1024,
    }
}

fn agent_config(model: &str, json: bool, tools: bool, output: usize) -> AgentConfig {
    AgentConfig {
        id: AgentId::new("conversation"),
        name: "Local verification".into(),
        provider: "ollama".into(),
        model: model.into(),
        tools: if tools { vec!["effect".into()] } else { vec![] },
        sampling: SamplingConfig {
            max_tokens: Some(output),
            temperature: Some(0.0),
            response_format: json.then_some(ResponseFormat::Json),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn local_fixture(config: AgentConfig, tokens: u64, input: &str) -> Fixture {
    fixture_with_config(
        GrantLimits {
            activations: 1,
            invocations: 8,
            tokens,
            cost_microunits: 0,
        },
        "in-process",
        config,
        input,
    )
}

fn local_resources(
    fixture: &Fixture,
    provider: NativeOllamaProvider,
    tool: Arc<CountingTool>,
) -> AutonomousActivationResources {
    let mut executor = axocoatl_tools::ToolExecutor::new();
    executor.register_builtin("effect", tool);
    AutonomousActivationResources {
        config: fixture.config.clone(),
        profile: fixture.profile.clone(),
        provider: Arc::new(provider),
        counter: Arc::new(Counter),
        tools: Arc::new(executor),
    }
}

struct NativeReply {
    tool: bool,
    json: bool,
}

impl Respond for NativeReply {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = request.body_json().unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["truncate"], false);
        assert_eq!(body["options"]["num_ctx"], CONTEXT);
        assert_eq!(body["options"]["num_predict"], OUTPUT);
        let has_tool_result = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool");
        // Like a real model, it cannot call a tool it was not offered.
        let offered = body["tools"].as_array().is_some_and(|tools| !tools.is_empty());
        let message = if self.tool && offered && !has_tool_result {
            serde_json::json!({"role":"assistant","content":"","tool_calls":[{"id":"call_native",
                "function":{"index":0,"name":"effect","arguments":{"value":"actual"}}
            }]})
        } else {
            serde_json::json!({"role":"assistant","content":if self.json {r#"{"ok":true}"#} else {"done"}})
        };
        let response = serde_json::json!({
            "model":"test-local", "message":message,
            "done":true, "done_reason":"stop", "prompt_eval_count":10, "eval_count":2
        });
        ResponseTemplate::new(200).set_body_raw(format!("{response}\n"), "application/x-ndjson")
    }
}

async fn server(tool: bool, json: bool) -> MockServer {
    server_replying(NativeReply { tool, json }).await
}

async fn server_replying(reply: impl Respond + 'static) -> MockServer {
    let server = MockServer::start().await;
    for (route, body) in [
        ("/api/version", serde_json::json!({"version": axocoatl_llm_ollama::AUDITED_OLLAMA_VERSION})),
        (
            "/api/status",
            serde_json::json!({"cloud":{"disabled":true,"source":"env"}}),
        ),
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "details":{"format":"gguf"}, "capabilities":["completion","tools","thinking"]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(reply)
        .mount(&server)
        .await;
    server
}

/// What qwen3-coder:30b on Ollama 0.40.8 wrote for a workbench turn: a call
/// without its opening `<tool_call>` token, which Ollama returned as content
/// ending in `stop`, with no structured call.
const TEXT_CALL: &str = "I'll help you fix the paging bug in `src/paginate.js`. Let me first locate this file and examine its contents to understand the issue.\n\n<function=glob>\n<parameter=pattern>\nsrc/paginate.js\n</parameter>\n</function>\n</tool_call>";

/// That text first, then a final answer once the call's result is in.
struct TextCallReply;

impl Respond for TextCallReply {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = request.body_json().unwrap();
        let has_tool_result = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool");
        let response = serde_json::json!({
            "model":"test-local",
            "message":{"role":"assistant","content":if has_tool_result {"done"} else {TEXT_CALL}},
            "done":true, "done_reason":"stop", "prompt_eval_count":10, "eval_count":2
        });
        ResponseTemplate::new(200).set_body_raw(format!("{response}\n"), "application/x-ndjson")
    }
}

/// A turn whose model writes its call as text runs the call and goes on; it
/// does not end on that text as its answer.
#[tokio::test]
async fn native_ollama_turn_runs_a_tool_call_the_model_wrote_as_text() {
    let server = server_replying(TextCallReply).await;
    let provider = NativeOllamaProvider::connect(native_config(server.uri(), "test-local"))
        .await
        .unwrap();
    let mut config = agent_config("test-local", false, true, OUTPUT);
    config.tools = vec!["glob".into()];
    let fixture = local_fixture(config, 2 * ONE_CALL, "Fix the paging bug in src/paginate.js");
    let tool = Arc::new(CountingTool::default());
    let mut executor = axocoatl_tools::ToolExecutor::new();
    executor.register_builtin("glob", tool.clone());
    let resources = AutonomousActivationResources {
        config: fixture.config.clone(),
        profile: fixture.profile.clone(),
        provider: Arc::new(provider),
        counter: Arc::new(Counter),
        tools: Arc::new(executor),
    };
    let settled = fixture
        .controller
        .prepare_autonomous_activation(fixture.activation.clone(), resources)
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    assert_eq!(tool.count.load(Ordering::SeqCst), 1);
    assert_eq!(settled.output.content().output.text, "done");
    let calls: Vec<serde_json::Value> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/api/chat")
        .map(|request| request.body_json().unwrap())
        .collect();
    assert_eq!(calls.len(), 2);
    let messages = calls[1]["messages"].as_array().unwrap();
    let call = messages
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    assert!(!call["content"].as_str().unwrap().contains("<function="));
    assert_eq!(call["tool_calls"][0]["function"]["name"], "glob");
    assert_eq!(
        call["tool_calls"][0]["function"]["arguments"],
        serde_json::json!({"pattern":"src/paginate.js"})
    );
    assert!(messages
        .iter()
        .any(|message| message["role"] == "tool" && message["tool_name"] == "glob"));
}

/// `text` until the request carries `rounds` tool results, then a final
/// answer.
struct TextCallsReply {
    text: &'static str,
    rounds: usize,
}

impl Respond for TextCallsReply {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = request.body_json().unwrap();
        let results = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .count();
        let response = serde_json::json!({
            "model":"test-local",
            "message":{"role":"assistant","content":if results >= self.rounds {"done"} else {self.text}},
            "done":true, "done_reason":"stop", "prompt_eval_count":10, "eval_count":2
        });
        ResponseTemplate::new(200).set_body_raw(format!("{response}\n"), "application/x-ndjson")
    }
}

async fn text_call_turn(reply: TextCallsReply) -> (Fixture, SettledActivation, Arc<CountingTool>, MockServer) {
    let server = server_replying(reply).await;
    let provider = NativeOllamaProvider::connect(native_config(server.uri(), "test-local"))
        .await
        .unwrap();
    let mut config = agent_config("test-local", false, true, OUTPUT);
    config.tools = vec!["glob".into()];
    let fixture = local_fixture(config, 3 * ONE_CALL, "Fix the paging bug in src/paginate.js");
    let tool = Arc::new(CountingTool::default());
    let mut executor = axocoatl_tools::ToolExecutor::new();
    executor.register_builtin("glob", tool.clone());
    let resources = AutonomousActivationResources {
        config: fixture.config.clone(),
        profile: fixture.profile.clone(),
        provider: Arc::new(provider),
        counter: Arc::new(Counter),
        tools: Arc::new(executor),
    };
    let settled = fixture
        .controller
        .prepare_autonomous_activation(fixture.activation.clone(), resources)
        .unwrap()
        .run()
        .await
        .unwrap();
    (fixture, settled, tool, server)
}

/// How each model response of the activation ended, as its stream records it.
fn recorded_responses(fixture: &Fixture) -> Vec<(String, usize, usize)> {
    use axocoatl_session::execution_content::ActivationStreamPayload;
    let state = fixture.controller.lock().unwrap();
    let snapshot = state.canonical.snapshot(&state.turn_id).unwrap();
    state
        .content
        .activation_stream(&snapshot, &fixture.activation)
        .unwrap()
        .into_iter()
        .filter_map(|view| match view.content.payload {
            ActivationStreamPayload::ProviderResponse {
                finish_reason,
                offered_tools,
                tool_calls,
            } => Some((finish_reason, offered_tools, tool_calls)),
            _ => None,
        })
        .collect()
}

/// Two calls the model writes as text in one Session take different ids, so
/// the history that carries both is valid and the turn goes on to its answer.
/// (Before 1.5.0's final round the second recovery could repeat the first
/// id, and every later request was refused.) The record says how each model
/// response ended.
#[tokio::test]
async fn two_text_calls_in_one_native_session_keep_their_ids_apart() {
    let (fixture, settled, tool, server) = text_call_turn(TextCallsReply {
        text: TEXT_CALL,
        rounds: 2,
    })
    .await;
    assert!(settled.accepted, "{:?}", settled.failure);
    assert_eq!(tool.count.load(Ordering::SeqCst), 2);
    assert_eq!(settled.output.content().output.text, "done");
    let last: serde_json::Value = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .rfind(|request| request.url.path() == "/api/chat")
        .unwrap()
        .body_json()
        .unwrap();
    let ids: Vec<&str> = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["tool_calls"][0]["id"].as_str())
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    assert_eq!(
        recorded_responses(&fixture),
        [
            ("tool_use".to_string(), 1, 1),
            ("tool_use".to_string(), 1, 1),
            ("stop".to_string(), 1, 0)
        ]
    );
}

/// A call the model writes as text that cannot run (here a parameter missing
/// its `</parameter>`) fails the activation with the reason, so the turn
/// needs attention instead of ending "done" having run nothing.
#[tokio::test]
async fn a_text_call_that_cannot_run_fails_the_activation_with_its_reason() {
    let (fixture, settled, tool, _server) = text_call_turn(TextCallsReply {
        text: "Looking.\n<function=glob>\n<parameter=pattern>\nsrc/paginate.js\n</function>",
        rounds: 1,
    })
    .await;
    assert!(!settled.accepted);
    let failure = settled.failure.unwrap();
    assert!(
        failure.contains("call to glob as answer text that Axocoatl cannot run")
            && failure.contains("done_reason \"stop\" and 1 tools offered"),
        "{failure}"
    );
    // The recorded output, which the workbench reads, says what happened and
    // offers Continue.
    let recorded = &settled.output.content().output.text;
    let view = axocoatl_session::execution_content::classify_activation_failure(recorded)
        .expect("a host-written activation failure");
    assert_eq!((view.class, view.next_step), ("provider_error", "continue"));
    assert!(
        view.explanation
            .starts_with("The model wrote a tool call as answer text that Axocoatl cannot run"),
        "{failure}: {}",
        view.explanation
    );
    assert_eq!(tool.count.load(Ordering::SeqCst), 0);
    let usage = fixture
        .controller
        .activation_provider_usage(&fixture.activation)
        .unwrap();
    assert_eq!(usage.calls, 1);
    assert_eq!(usage.unsettled_calls, 0);
    assert_eq!(usage.tokens.usage, TokenUsageStats::new(10, 2));
}

#[tokio::test]
async fn native_ollama_tool_roundtrip_settles_zero_cost_and_reopens_actual_checkpoint() {
    let server = server(true, false).await;
    let provider = NativeOllamaProvider::connect(native_config(server.uri(), "test-local"))
        .await
        .unwrap();
    let fixture = local_fixture(
        agent_config("test-local", false, true, OUTPUT),
        2 * ONE_CALL,
        "count once",
    );
    let tool = Arc::new(CountingTool::default());
    let settled = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            local_resources(&fixture, provider, tool.clone()),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    assert_eq!(tool.count.load(Ordering::SeqCst), 1);
    let usage = fixture
        .controller
        .activation_provider_usage(&fixture.activation)
        .unwrap();
    assert_eq!(usage.calls, 2);
    assert_eq!(usage.unsettled_calls, 0);
    assert_eq!(usage.tokens.usage, TokenUsageStats::new(20, 4));
    assert!(usage.tokens.complete);
    assert!(usage.cost_known);
    assert_eq!(usage.cost_microunits, 0);
    let requests = server.received_requests().await.unwrap();
    let calls: Vec<serde_json::Value> = requests
        .iter()
        .filter(|request| request.url.path() == "/api/chat")
        .map(|request| request.body_json().unwrap())
        .collect();
    assert_eq!(calls.len(), 2);
    assert!(calls[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["role"] == "tool" && message["tool_name"] == "effect"));

    let Fixture {
        _root,
        ownership,
        owner,
        controller,
        activation,
        ..
    } = fixture;
    drop(controller);
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    let reopened = SessionDispatchController::open(canonical, activation.turn_id.clone()).unwrap();
    assert_eq!(
        reopened.activation_provider_usage(&activation).unwrap(),
        usage
    );
    {
        let state = reopened.lock().unwrap();
        let checkpoint = state
            .memory
            .checkpoint(settled.checkpoint.as_ref().unwrap())
            .unwrap();
        assert_eq!(checkpoint.session_messages.last().unwrap().content, "done");
        assert!(checkpoint.cumulative_token_usage_known);
        assert!(checkpoint
            .session_messages
            .iter()
            .any(|message| message.role == MessageRole::Tool));
    }
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests.len()
    );
}

/// A zero-cost local model is still limited by the grant's tokens. With
/// room for one call only, that call cannot pay for a tool round and an
/// answer, so it goes without tools and the Agent answers; no second
/// inference is made.
#[tokio::test]
async fn native_ollama_budget_limits_inference_even_with_zero_api_cost() {
    let server = server(true, false).await;
    let provider = NativeOllamaProvider::connect(native_config(server.uri(), "test-local"))
        .await
        .unwrap();
    let fixture = local_fixture(
        agent_config("test-local", false, true, OUTPUT),
        ONE_CALL,
        "count once",
    );
    let tool = Arc::new(CountingTool::default());
    let settled = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            local_resources(&fixture, provider, tool.clone()),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    assert_eq!(settled.output.content().output.text, "done");
    assert_eq!(tool.count.load(Ordering::SeqCst), 0);
    let usage = fixture
        .controller
        .activation_provider_usage(&fixture.activation)
        .unwrap();
    assert_eq!(usage.calls, 1);
    assert!(usage.cost_known);
    assert_eq!(usage.cost_microunits, 0);
    let calls: Vec<serde_json::Value> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/api/chat")
        .map(|request| request.body_json().unwrap())
        .collect();
    assert_eq!(calls.len(), 1);
    assert!(calls[0]["tools"]
        .as_array()
        .is_none_or(|tools| tools.is_empty()));
}

#[tokio::test]
async fn native_ollama_structured_usage_stays_incomplete_in_accepted_checkpoint() {
    let server = server(false, true).await;
    let provider = NativeOllamaProvider::connect(native_config(server.uri(), "test-local"))
        .await
        .unwrap();
    let fixture = local_fixture(
        agent_config("test-local", true, false, OUTPUT),
        2 * ONE_CALL,
        "Return JSON",
    );
    let settled = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            local_resources(&fixture, provider, Arc::new(CountingTool::default())),
        )
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
    assert_eq!(usage.tokens.usage, TokenUsageStats::new(10, 2));
    assert!(!usage.tokens.complete);
    assert!(usage.cost_known);
    let state = fixture.controller.lock().unwrap();
    let checkpoint = state
        .memory
        .checkpoint(settled.checkpoint.as_ref().unwrap())
        .unwrap();
    assert!(!checkpoint.cumulative_token_usage_known);
    assert_eq!(
        checkpoint.session_messages.last().unwrap().content,
        r#"{"ok":true}"#
    );
}

/// The provider a Session gets: the model's context observed (a load with
/// `shift:false`, then the contract probes on this server and model), then
/// connected from that observation.
async fn live_observed_provider(
    endpoint: &str,
    model: &str,
    output: usize,
) -> (
    NativeOllamaProvider,
    axocoatl_llm_ollama::NativeOllamaContextObservation,
) {
    let observation = axocoatl_llm_ollama::observe_native_ollama_context(endpoint, model)
        .await
        .unwrap();
    assert!(
        axocoatl_llm_ollama::ollama_version_accepted(&observation.server_version),
        "{observation:?}"
    );
    let provider =
        NativeOllamaProvider::connect_observed(observation.clone(), output, 1024 * 1024)
            .await
            .unwrap();
    (provider, observation)
}

/// Explicit local integration only: never starts a server or downloads a model.
/// Set AXOCOATL_TEST_OLLAMA_URL to a local-only, cloud-disabled Ollama
/// (0.40.6 or a later 0.40 release) and select an installed model with
/// AXOCOATL_TEST_OLLAMA_MODEL. The provider is the one a Session gets: the
/// observed context, after the contract probes. JSON exercises the native
/// structured-output path, whose aggregate token measurement remains
/// incomplete; AXOCOATL_TEST_OLLAMA_TOOLS=1 a tool round-trip.
#[tokio::test]
#[ignore = "requires an explicitly configured local-only Ollama server and installed model"]
async fn native_ollama_real_model_through_durable_controller() {
    let endpoint = std::env::var("AXOCOATL_TEST_OLLAMA_URL").expect("explicit local test endpoint");
    let model = std::env::var("AXOCOATL_TEST_OLLAMA_MODEL").expect("explicit installed test model");
    let json = std::env::var("AXOCOATL_TEST_OLLAMA_JSON").as_deref() == Ok("1");
    let use_tool = std::env::var("AXOCOATL_TEST_OLLAMA_TOOLS").as_deref() == Ok("1");
    assert!(!(json && use_tool), "select one live proof case");
    let output = 1024;
    let (provider, observation) = live_observed_provider(&endpoint, &model, output).await;
    let context = observation.context_tokens;
    let input = if use_tool {
        "Call the effect tool exactly once with {\"value\":\"actual\"}. After receiving its result, reply with the single word ready."
    } else if json {
        "Return one JSON object with exactly this content: {\"ok\":true}."
    } else {
        "Reply with the single word ready."
    };
    let bound = ((context + output) * if json || use_tool { 2 } else { 1 }) as u64;
    let fixture = local_fixture(agent_config(&model, json, use_tool, output), bound, input);
    let tool = Arc::new(CountingTool::default());
    let settled = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.activation.clone(),
            local_resources(&fixture, provider, tool.clone()),
        )
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(settled.accepted, "{:?}", settled.failure);
    let usage = fixture
        .controller
        .activation_provider_usage(&fixture.activation)
        .unwrap();
    let invocations = tool.count.load(Ordering::SeqCst);
    if !use_tool {
        assert_eq!(usage.calls, 1);
        assert_eq!(invocations, 0);
    }
    assert_eq!(usage.unsettled_calls, 0);
    assert!(usage.tokens.usage.input_tokens > 0);
    assert!(usage.tokens.usage.output_tokens > 0);
    assert_eq!(usage.tokens.complete, !json);
    assert_eq!(usage.cost_microunits, 0);
    assert!(usage.cost_known);
    let Fixture {
        _root,
        ownership,
        owner,
        controller,
        activation,
        ..
    } = fixture;
    drop(controller);
    let canonical = SessionExecutionStore::open(ownership, owner).unwrap();
    let reopened = SessionDispatchController::open(canonical, activation.turn_id.clone()).unwrap();
    assert_eq!(
        reopened.activation_provider_usage(&activation).unwrap(),
        usage
    );
    let state = reopened.lock().unwrap();
    let checkpoint = state
        .memory
        .checkpoint(settled.checkpoint.as_ref().unwrap())
        .unwrap();
    assert_eq!(checkpoint.cumulative_token_usage_known, !json);
    let answer = &checkpoint.session_messages.last().unwrap().content;
    assert!(!answer.trim().is_empty());
    let results: Vec<_> = checkpoint
        .session_messages
        .iter()
        .filter(|message| message.role == MessageRole::Tool)
        .collect();
    assert_eq!(!results.is_empty(), use_tool);
    if use_tool {
        // A model may call the tool more than once (qwen3-coder:30b did so in
        // 1 run of 4), so the proof is a call with the asked-for value, one
        // provider call per tool round plus the answer, and the answer.
        let rounds = checkpoint
            .session_messages
            .iter()
            .filter(|message| message.role == MessageRole::Assistant && !message.tool_calls.is_empty())
            .count();
        assert!(invocations >= 1);
        assert_eq!(results.len(), invocations);
        assert_eq!(usage.calls as usize, rounds + 1);
        assert!(
            results
                .iter()
                .any(|result| result.content.contains(r#""value":"actual""#)),
            "{results:?}"
        );
        assert!(answer.to_ascii_lowercase().contains("ready"), "{answer}");
    }
    if json {
        let value: serde_json::Value = serde_json::from_str(answer).unwrap();
        assert_eq!(value["ok"], true);
    }
    println!(
        "{}",
        serde_json::json!({
            "model":model,"json":json,"tools":use_tool,"reserved_tokens":bound,
            "ollama_version":observation.server_version,"observed_context":context,
            "model_digest":observation.model_digest,
            "observed_usage":usage.tokens,"provider_api_cost_microunits":usage.cost_microunits,
            "provider_api_cost_known":usage.cost_known,"calls":usage.calls,
            "accepted":settled.accepted,"checkpoint_reopened":true,"answer":answer,
            "tool_invocations":invocations,
        })
    );
}

/// Explicit local integration only, like the test above: the guarantees the
/// contract probes check, through the provider a Session gets. A prompt
/// longer than the observed context is refused before any generation, and a
/// call stops at its output limit.
#[tokio::test]
#[ignore = "requires an explicitly configured local-only Ollama server and installed model"]
async fn native_ollama_real_model_refuses_over_long_input_and_stops_at_its_limit() {
    use axocoatl_llm::LlmProvider;
    let endpoint = std::env::var("AXOCOATL_TEST_OLLAMA_URL").expect("explicit local test endpoint");
    let model = std::env::var("AXOCOATL_TEST_OLLAMA_MODEL").expect("explicit installed test model");
    let limit = 16;
    let (provider, observation) = live_observed_provider(&endpoint, &model, limit).await;
    let context = observation.context_tokens;

    let over_long = axocoatl_llm::ChatRequest::simple("x ".repeat(context * 2));
    let refused = provider.chat_with_accounting(over_long).await;
    let status = match &refused.response {
        Err(axocoatl_llm::ProviderError::ApiError { status, .. }) => *status,
        other => panic!("an over-long prompt must be refused, got {other:?}"),
    };
    assert!((400..500).contains(&status), "{status}");
    assert_eq!(refused.usage.usage.output_tokens, 0);

    let mut long_answer = axocoatl_llm::ChatRequest::simple(
        "Count from 1 to 300, one number per line, and nothing else.",
    );
    long_answer.max_tokens = Some(limit);
    let stopped = provider.chat_with_accounting(long_answer).await;
    // A thinking model may spend the whole limit on reasoning: the call then
    // ends without final text, refused, but its usage is still observed.
    assert!(
        stopped.usage.usage.output_tokens <= limit,
        "{:?}",
        stopped.usage
    );
    assert!(stopped.usage.usage.output_tokens > 0, "{:?}", stopped.usage);
    if let Ok(response) = &stopped.response {
        assert_eq!(response.finish_reason, axocoatl_llm::FinishReason::MaxTokens);
    }
    println!(
        "{}",
        serde_json::json!({
            "model":model,"ollama_version":observation.server_version,
            "observed_context":context,"over_long_status":status,
            "over_long_output_tokens":refused.usage.usage.output_tokens,
            "limit":limit,"stopped_output_tokens":stopped.usage.usage.output_tokens,
            "stopped_input_tokens":stopped.usage.usage.input_tokens,
            "stopped":stopped.response.as_ref().map(|r| format!("{:?}", r.finish_reason))
                .unwrap_or_else(|error| error.to_string()),
        })
    );
}
