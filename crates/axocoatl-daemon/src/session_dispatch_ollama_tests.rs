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
    let server = MockServer::start().await;
    for (route, body) in [
        ("/api/version", serde_json::json!({"version":"0.20.6"})),
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
        .respond_with(NativeReply { tool, json })
        .mount(&server)
        .await;
    server
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

/// Explicit local integration only: never starts a server or downloads a model.
/// Set AXOCOATL_TEST_OLLAMA_URL to a local-only 0.20.6 server and select an
/// installed model with AXOCOATL_TEST_OLLAMA_MODEL. JSON exercises the native
/// structured-output path, whose aggregate token measurement remains incomplete.
#[tokio::test]
#[ignore = "requires an explicitly configured local-only Ollama server and installed model"]
async fn native_ollama_real_model_through_durable_controller() {
    let endpoint = std::env::var("AXOCOATL_TEST_OLLAMA_URL").expect("explicit local test endpoint");
    let model = std::env::var("AXOCOATL_TEST_OLLAMA_MODEL").expect("explicit installed test model");
    let json = std::env::var("AXOCOATL_TEST_OLLAMA_JSON").as_deref() == Ok("1");
    let use_tool = std::env::var("AXOCOATL_TEST_OLLAMA_TOOLS").as_deref() == Ok("1");
    assert!(!(json && use_tool), "select one live proof case");
    let context = 4096;
    let output = 1024;
    let provider = NativeOllamaProvider::connect(NativeOllamaConfig {
        context_tokens: context,
        max_output_tokens: output,
        ..native_config(endpoint, &model)
    })
    .await
    .unwrap();
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
    assert_eq!(usage.calls, if use_tool { 2 } else { 1 });
    assert_eq!(tool.count.load(Ordering::SeqCst), usize::from(use_tool));
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
    assert_eq!(
        checkpoint
            .session_messages
            .iter()
            .any(|message| message.role == MessageRole::Tool),
        use_tool,
    );
    if json {
        let value: serde_json::Value = serde_json::from_str(answer).unwrap();
        assert_eq!(value["ok"], true);
    }
    println!(
        "{}",
        serde_json::json!({
            "model":model,"json":json,"tools":use_tool,"reserved_tokens":bound,
            "observed_usage":usage.tokens,"provider_api_cost_microunits":usage.cost_microunits,
            "provider_api_cost_known":usage.cost_known,"calls":usage.calls,
            "accepted":settled.accepted,"checkpoint_reopened":true,"answer":answer,
            "tool_invocations":tool.count.load(Ordering::SeqCst),
        })
    );
}
