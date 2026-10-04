use super::*;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};
const MODEL: &str = "meta-llama/test-instruct";
const KEY: &str = "fixture-inference-key";

#[tokio::test]
async fn current_public_endpoint_metadata_admits_bounded_llama_and_qwen() {
    // Captured public endpoint fields, 2026-09-16; no account data or credentials.
    for fixture in [
        include_str!("fixtures/meta-llama--llama-3.3-70b-instruct.json"),
        include_str!("fixtures/qwen--qwen-2.5-72b-instruct.json"),
    ] {
        let endpoints: Value = serde_json::from_str(fixture).unwrap();
        let model = endpoints["data"]["id"].as_str().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{
                "id":model,"architecture":{"input_modalities":["text"],"output_modalities":["text"]},
                "supported_parameters":["max_tokens","tools"]
            }]})))
            .mount(&server).await;
        Mock::given(method("GET"))
            .and(path(format!("/models/{model}/endpoints")))
            .respond_with(ResponseTemplate::new(200).set_body_json(&endpoints))
            .mount(&server)
            .await;
        let profiles = observe_native_openrouter_profiles(&server.uri(), KEY, model)
            .await
            .unwrap();
        assert!(profiles
            .iter()
            .any(|p| p.endpoint_tag.starts_with("deepinfra/")));
        for profile in profiles {
            assert!(
                profile
                    .minimum_call_bounds(2048, None, 65536)
                    .unwrap()
                    .cost_microunits
                    > 0
            );
        }
    }
}
async fn metadata(server: &MockServer) {
    Mock::given(method("GET")).and(path("/models")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":MODEL,"architecture":{"input_modalities":["text"],"output_modalities":["text"]},"supported_parameters":["max_tokens","tools"]}]}))).mount(server).await;
    Mock::given(method("GET")).and(path(format!("/models/{MODEL}/endpoints"))).respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":{"id":MODEL,"endpoints":[{"model_id":MODEL,"provider_name":"FiniteProvider","tag":"finite/exact","context_length":2048,"max_completion_tokens":128,"supported_parameters":["max_tokens","tools"],"pricing":{"prompt":"0.000001","completion":"0.000002"},"status":0,"supports_implicit_caching":false}]}}))).mount(server).await;
}
fn reply(complete: bool) -> String {
    reply_with_cost(complete, json!(0.000008))
}
fn reply_with_cost(complete: bool, cost: Value) -> String {
    let first = json!({"id":"gen-fixture","model":MODEL,"provider":"FiniteProvider","choices":[{"index":0,"delta":{"content":"verified"},"finish_reason":"stop"}]});
    let usage = json!({"id":"gen-fixture","model":MODEL,"provider":"FiniteProvider","choices":[],"usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6,"cost":cost,"is_byok":false}});
    if complete {
        format!("data: {first}\n\ndata: {usage}\n\ndata: [DONE]\n\n")
    } else {
        format!("data: {first}\n\n")
    }
}
async fn provider(server: &MockServer) -> NativeOpenRouterProvider {
    let profile = observe_native_openrouter_profiles(&server.uri(), KEY, MODEL)
        .await
        .unwrap()
        .remove(0);
    NativeOpenRouterProvider::connect_observed(profile, KEY, 32, 65536, None)
        .await
        .unwrap()
}
#[tokio::test]
async fn exact_paid_credit_route_uses_wire_caps_and_reports_actual_cost() {
    let server = MockServer::start().await;
    metadata(&server).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(reply(true), "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let provider = provider(&server).await;
    let request = ChatRequest::simple("Answer once");
    let bound = provider.execution_bounds(&request).unwrap();
    assert_eq!(bound.token_limit, 2080);
    assert_eq!(bound.cost_microunits, 2112);
    let outcome = provider.chat_with_accounting(request).await;
    assert_eq!(outcome.response.unwrap().content, "verified");
    assert!(outcome.usage.complete);
    assert_eq!(outcome.cost_microunits, Some(8));
    let requests = server.received_requests().await.unwrap();
    let sent = requests
        .iter()
        .find(|r| r.method.as_str() == "POST")
        .unwrap();
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["provider"]["only"], json!(["finite/exact"]));
    assert_eq!(body["provider"]["allow_fallbacks"], false);
    assert_eq!(body["provider"]["require_parameters"], true);
    assert_eq!(body["provider"]["max_price"]["prompt"], "1");
    assert_eq!(body["max_tokens"], 32);
    // The account's default web plugin is turned off; a non-reasoning model
    // gets no reasoning parameter.
    assert_eq!(body["plugins"], json!([{"id":"web","enabled":false}]));
    assert!(body.get("reasoning").is_none());
    assert!(!String::from_utf8_lossy(&sent.body).contains(KEY));
}
#[tokio::test]
async fn unsupported_billing_profile_refuses_before_inference() {
    let server = MockServer::start().await;
    metadata(&server).await;
    let mut profile = observe_native_openrouter_profiles(&server.uri(), KEY, MODEL)
        .await
        .unwrap()
        .remove(0);
    profile.billing = "byok".into();
    assert!(
        NativeOpenRouterProvider::connect_observed(profile, KEY, 32, 65536, None)
            .await
            .is_err()
    );
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method.as_str() == "GET"));
}
#[tokio::test]
async fn unexpected_byok_preserves_tokens_without_claiming_credit_cost() {
    let server = MockServer::start().await;
    metadata(&server).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            reply(true).replace("\"is_byok\":false", "\"is_byok\":true"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let outcome = provider(&server)
        .await
        .chat_with_accounting(ChatRequest::simple("one call"))
        .await;
    assert!(outcome.response.is_err());
    assert_eq!(outcome.usage.usage.input_tokens, 4);
    assert!(!outcome.usage.complete);
    assert_eq!(outcome.cost_microunits, None);
}
#[tokio::test]
async fn lost_terminal_keeps_usage_unknown_and_never_retries() {
    let server = MockServer::start().await;
    metadata(&server).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(reply(false), "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let outcome = provider(&server)
        .await
        .chat_with_accounting(ChatRequest::simple("one call"))
        .await;
    assert!(outcome.response.is_err());
    assert!(!outcome.usage.complete);
    assert_eq!(outcome.cost_microunits, None);
}
#[tokio::test]
async fn rate_limit_has_no_fallback_and_endpoint_changes_refuse_before_request() {
    let server = MockServer::start().await;
    metadata(&server).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429))
        .expect(1)
        .mount(&server)
        .await;
    let mut provider = provider(&server).await;
    assert!(provider
        .chat(ChatRequest::simple("one call"))
        .await
        .is_err());
    provider.observation.endpoint_tag = "finite/changed".into();
    assert!(provider
        .chat(ChatRequest::simple("must not dispatch"))
        .await
        .is_err());
}
#[test]
fn money_bounds_use_exact_decimals_and_reject_unrepresentable_values() {
    assert_eq!(money::per_million_ceiling("1.35e-7").unwrap(), "0.135");
    assert_eq!(
        money::charge_bound(2048, 32, "0.135", "0.4", None).unwrap(),
        290
    );
    assert!(money::decimal_units("-1").is_err());
    assert!(money::decimal_units("0.0000000000000000001").is_err());
}

#[tokio::test]
async fn native_tool_output_retains_route_and_complete_arguments() {
    let server = MockServer::start().await;
    metadata(&server).await;
    let tool = json!({"id":"gen-fixture","model":MODEL,"provider":"FiniteProvider",
        "choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function",
        "function":{"name":"get_weather","arguments":"{\"location\":\"London\"}"}}]},"finish_reason":"tool_calls"}]});
    let terminal = reply(true).split_once("\n\n").unwrap().1.to_owned();
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(format!("data: {tool}\n\n{terminal}"), "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut request = ChatRequest::simple("Read the weather");
    request.tools.push(axocoatl_llm::ToolDefinition {
        name: "get_weather".into(), description: "Get weather".into(),
        parameters: json!({"type":"object","properties":{"location":{"type":"string"}},"required":["location"]}),
        concurrency: Default::default(),
    });
    let outcome = provider(&server).await.chat_with_accounting(request).await;
    let response = outcome.response.unwrap();
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
    assert_eq!(
        response.tool_calls[0].arguments,
        json!({"location":"London"})
    );
    assert_eq!(
        response.tool_calls[0].provider_metadata["axocoatl.openrouter.endpoint"],
        "finite/exact"
    );
    assert!(outcome.usage.complete);
}

#[tokio::test]
async fn contract_breach_retains_observed_spend_and_refuses_acceptance() {
    let server = MockServer::start().await;
    metadata(&server).await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(reply_with_cost(true, json!(0.1)), "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let outcome = provider(&server)
        .await
        .chat_with_accounting(ChatRequest::simple("one call"))
        .await;
    assert!(outcome.response.is_err());
    assert_eq!(outcome.cost_microunits, Some(100_000));
    assert_eq!(outcome.usage.usage.input_tokens, 4);
    assert!(!outcome.usage.complete);
}

#[tokio::test]
async fn unsupported_request_never_reaches_inference() {
    let server = MockServer::start().await;
    metadata(&server).await;
    let provider = provider(&server).await;
    let mut request = ChatRequest::simple("one call");
    request.model_override = Some("other/model".into());
    assert!(provider.execution_bounds(&request).is_none());
    assert!(provider.chat(request).await.is_err());
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method.as_str() == "GET"));
}

// ---- Reasoning models, price ceilings and per-call reservations ----

const CLAUDE: &str = "anthropic/claude-sonnet-5.5";
const GPT: &str = "openai/gpt-5.6-sol";

/// Captured public catalog rows and endpoint metadata, 2026-10-03; no account
/// data or credentials.
fn captured(model: &str) -> (Value, Value) {
    let fixture: Value = serde_json::from_str(match model {
        CLAUDE => include_str!("fixtures/anthropic--claude-sonnet-5.5.json"),
        GPT => include_str!("fixtures/openai--gpt-5.6-sol.json"),
        _ => unreachable!(),
    })
    .unwrap();
    (fixture["model"].clone(), fixture["endpoints"].clone())
}

async fn catalog(server: &MockServer, row: Value, endpoints: Value) {
    let model = row["id"].as_str().unwrap().to_owned();
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[row]})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/models/{model}/endpoints")))
        .respond_with(ResponseTemplate::new(200).set_body_json(endpoints))
        .mount(server)
        .await;
}

async fn captured_profiles(model: &str) -> (MockServer, Vec<NativeOpenRouterObservation>) {
    let server = MockServer::start().await;
    let (row, endpoints) = captured(model);
    catalog(&server, row, endpoints).await;
    let profiles = observe_native_openrouter_profiles(&server.uri(), KEY, model)
        .await
        .unwrap();
    (server, profiles)
}

fn tagged(profiles: &[NativeOpenRouterObservation], tag: &str) -> NativeOpenRouterObservation {
    profiles
        .iter()
        .find(|profile| profile.endpoint_tag == tag)
        .unwrap_or_else(|| panic!("{tag} was not admitted"))
        .clone()
}

#[tokio::test]
async fn reasoning_models_are_admitted_with_every_price_component_and_tier() {
    let (_, claude) = captured_profiles(CLAUDE).await;
    let anthropic = tagged(&claude, "anthropic");
    let contract = anthropic.reasoning.clone().unwrap();
    assert!(contract.mandatory);
    assert_eq!(contract.default_effort.as_deref(), Some("high"));
    assert_eq!(
        contract.efforts,
        NativeOpenRouterEfforts::Listed(vec![
            "max".into(),
            "xhigh".into(),
            "high".into(),
            "medium".into(),
            "low".into()
        ])
    );
    // prompt 2, cache read 0.2, cache write 2.5, one-hour cache write 4.
    assert_eq!(anthropic.prompt_price_per_million, "4");
    assert_eq!(anthropic.completion_price_per_million, "10");
    assert_eq!(anthropic.unrequested_priced_features, ["web_search"]);
    assert_eq!(anthropic.request_price, None);
    // Regional endpoints at 2.2/11 sort after the 2/10 ones.
    assert!(
        claude
            .iter()
            .position(|p| p.endpoint_tag == "azure/us")
            .unwrap()
            > claude
                .iter()
                .position(|p| p.endpoint_tag == "anthropic")
                .unwrap()
    );

    let (_, gpt) = captured_profiles(GPT).await;
    // `openai` would also select openai/flex and openai/fast; Azure lists
    // only max_completion_tokens. The cheapest exact variant comes first.
    assert_eq!(
        gpt.iter()
            .map(|p| p.endpoint_tag.as_str())
            .collect::<Vec<_>>(),
        ["openai/flex", "openai/fast", "amazon-bedrock/us-east-1"]
    );
    let flex = &gpt[0];
    // Base 1/5, cache write 1.25, and the >272k-token tier 2/7.5 with cache
    // write 2.5: the highest tier wins.
    assert_eq!(flex.prompt_price_per_million, "2.5");
    assert_eq!(flex.completion_price_per_million, "7.5");
    assert_eq!(flex.max_prompt_tokens, Some(922_000));
    assert_eq!(flex.prompt_limit(), 922_000);
    let contract = flex.reasoning.clone().unwrap();
    assert!(!contract.mandatory && contract.enabled_by_default);
}

#[tokio::test]
async fn reasoning_effort_follows_the_catalog_and_refuses_what_it_does_not_accept() {
    use axocoatl_core::ReasoningEffort as E;
    use NativeOpenRouterReasoningRequest as R;
    let (_, claude) = captured_profiles(CLAUDE).await;
    let claude = &claude[0];
    assert_eq!(
        claude.reasoning_request(None).unwrap(),
        Some(R::Effort(E::High))
    );
    assert_eq!(
        claude.reasoning_request(Some(E::Low)).unwrap(),
        Some(R::Effort(E::Low))
    );
    let none = claude
        .reasoning_request(Some(E::None))
        .unwrap_err()
        .to_string();
    assert!(none.contains("requires reasoning"), "{none}");
    let minimal = claude
        .reasoning_request(Some(E::Minimal))
        .unwrap_err()
        .to_string();
    assert!(
        minimal
            .contains("does not accept reasoning effort minimal (max, xhigh, high, medium, low)"),
        "{minimal}"
    );
    assert!(claude.accepts_reasoning(Some(R::Effort(E::Max))));
    assert!(!claude.accepts_reasoning(None));
    assert!(!claude.accepts_reasoning(Some(R::Disabled)));

    let (_, gpt) = captured_profiles(GPT).await;
    let gpt = &gpt[0];
    assert_eq!(
        gpt.reasoning_request(None).unwrap(),
        Some(R::Effort(E::Medium))
    );
    assert_eq!(
        gpt.reasoning_request(Some(E::None)).unwrap(),
        Some(R::Effort(E::None))
    );

    // A model whose catalog row has no reasoning object takes no effort.
    let server = MockServer::start().await;
    metadata(&server).await;
    let plain = observe_native_openrouter_profiles(&server.uri(), KEY, MODEL)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(plain.reasoning_request(None).unwrap(), None);
    let refused = plain
        .reasoning_request(Some(E::High))
        .unwrap_err()
        .to_string();
    assert!(refused.contains("is not a reasoning model"), "{refused}");

    // Catalog shapes without effort selection, or off by default.
    let mut shaped = plain.clone();
    shaped.schema_version = 2;
    shaped.non_reasoning_evidence = None;
    shaped.supported_parameters.push("reasoning".into());
    shaped.reasoning = Some(NativeOpenRouterReasoning {
        mandatory: true,
        enabled_by_default: true,
        efforts: NativeOpenRouterEfforts::Unavailable,
        default_effort: None,
    });
    assert_eq!(shaped.reasoning_request(None).unwrap(), Some(R::Enabled));
    assert!(shaped.reasoning_request(Some(E::High)).is_err());
    shaped.reasoning = Some(NativeOpenRouterReasoning {
        mandatory: false,
        enabled_by_default: false,
        efforts: NativeOpenRouterEfforts::Listed(vec!["high".into(), "low".into()]),
        default_effort: Some("low".into()),
    });
    assert_eq!(shaped.reasoning_request(None).unwrap(), None);
    assert_eq!(
        shaped.reasoning_request(Some(E::None)).unwrap(),
        Some(R::Disabled)
    );
    assert_eq!(
        shaped.reasoning_request(Some(E::High)).unwrap(),
        Some(R::Effort(E::High))
    );
}

#[test]
fn reasoning_allowance_follows_openrouter_effort_shares() {
    use axocoatl_core::ReasoningEffort as E;
    use NativeOpenRouterReasoningRequest as R;
    // max_tokens = output / (1 - share): high is 80%, so 4x the output.
    assert_eq!(R::Effort(E::High).allowance(4096), 16_384);
    assert_eq!(R::Effort(E::Max).allowance(4096), 77_824);
    assert_eq!(R::Effort(E::Medium).allowance(4096), 4096);
    assert_eq!(R::Enabled.allowance(4096), 4096);
    assert_eq!(R::Effort(E::Low).allowance(8192), 2048);
    // Never below OpenRouter's minimum reasoning budget while reasoning is on.
    assert_eq!(
        R::Effort(E::Low).allowance(100),
        MINIMUM_REASONING_ALLOWANCE
    );
    assert_eq!(
        R::Effort(E::Minimal).allowance(900),
        MINIMUM_REASONING_ALLOWANCE
    );
    assert_eq!(R::Effort(E::None).allowance(4096), 0);
    assert_eq!(R::Disabled.allowance(4096), 0);
}

const SONNET_PROVIDER: &str = "Anthropic";

fn frame(delta: Value, finish: Option<&str>, usage: Option<Value>) -> String {
    let mut chunk = json!({"id":"gen-reasoning","model":CLAUDE,"provider":SONNET_PROVIDER,
        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
    if let Some(usage) = usage {
        chunk["usage"] = usage;
    }
    format!("data: {chunk}\n\n")
}

/// A Claude-shaped stream: reasoning text in fragments, its signature, a tool
/// call, then terminal usage that counts the reasoning in completion tokens.
fn reasoning_tool_stream(usage: Value) -> String {
    let text = |t: &str| {
        json!({"reasoning":t,"reasoning_details":[
        {"type":"reasoning.text","text":t,"format":"anthropic-claude-v1","index":0}]})
    };
    [
        frame(json!({"role":"assistant","content":null}), None, None),
        frame(text("Oslo is "), None, None),
        frame(text("east of Lima."), None, None),
        frame(
            json!({"reasoning_details":[{"type":"reasoning.text","signature":"sig-1",
            "format":"anthropic-claude-v1","index":0}]}),
            None,
            None,
        ),
        ": OPENROUTER PROCESSING\n\n".into(),
        frame(
            json!({"tool_calls":[{"index":0,"id":"toolu_1","type":"function",
            "function":{"name":"get_time","arguments":""}}]}),
            None,
            None,
        ),
        frame(
            json!({"tool_calls":[{"index":0,"function":{"arguments":"{\"city\": \"Oslo\"}"}}]}),
            None,
            None,
        ),
        frame(json!({"content":""}), Some("tool_calls"), None),
        frame(json!({}), Some("tool_calls"), Some(usage)),
        "data: [DONE]\n\n".into(),
    ]
    .concat()
}

fn reasoning_usage() -> Value {
    json!({"prompt_tokens":432,"completion_tokens":492,"total_tokens":924,"cost":0.005784,
        "is_byok":false,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},
        "completion_tokens_details":{"reasoning_tokens":443}})
}

fn time_tool() -> axocoatl_llm::ToolDefinition {
    axocoatl_llm::ToolDefinition {
        name: "get_time".into(),
        description: "Get the local time of a city".into(),
        parameters: json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}),
        concurrency: Default::default(),
    }
}

async fn claude_provider(server: &MockServer, output: usize) -> NativeOpenRouterProvider {
    let (row, endpoints) = captured(CLAUDE);
    catalog(server, row, endpoints).await;
    let profile = tagged(
        &observe_native_openrouter_profiles(&server.uri(), KEY, CLAUDE)
            .await
            .unwrap(),
        "anthropic",
    );
    let reasoning = profile.reasoning_request(None).unwrap();
    NativeOpenRouterProvider::connect_observed(profile, KEY, output, 65536, reasoning)
        .await
        .unwrap()
}

fn posted(requests: &[wiremock::Request]) -> Vec<(Value, usize)> {
    requests
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| (serde_json::from_slice(&r.body).unwrap(), r.body.len()))
        .collect()
}

#[tokio::test]
async fn a_reasoning_call_reserves_its_own_request_and_settles_reasoning_as_output() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            reasoning_tool_stream(reasoning_usage()),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let provider = claude_provider(&server, 1024).await;
    let mut request = ChatRequest::simple("Which is further east, Oslo or Lima? Get its time.");
    request.tools.push(time_tool());
    let bound = provider.execution_bounds(&request).unwrap();

    let mut stream = provider.chat_stream(request.clone()).await.unwrap();
    let mut reasoning_text = String::new();
    let mut metadata = None;
    let mut usage = None;
    let mut cost = None;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            StreamEvent::ReasoningDelta { delta } => reasoning_text.push_str(&delta),
            StreamEvent::ToolCallMetadata { metadata: m, .. } => metadata = Some(m),
            StreamEvent::UsageObservation(observed) => usage = Some(observed),
            StreamEvent::CostObservation { cost_microunits } => cost = Some(cost_microunits),
            _ => {}
        }
    }
    assert_eq!(reasoning_text, "Oslo is east of Lima.");
    let usage = usage.unwrap();
    assert!(usage.complete);
    assert_eq!(usage.usage.input_tokens, 432);
    assert_eq!(usage.usage.output_tokens, 49);
    assert_eq!(usage.usage.reasoning_tokens, Some(443));
    assert_eq!(usage.usage.total(), 924);
    assert_eq!(cost, Some(5784));

    // The wire request: the model's default effort, max_tokens with the
    // reasoning allowance, ceilings as max_price, the web plugin off.
    let (body, sent_bytes) = posted(&server.received_requests().await.unwrap()).remove(0);
    assert_eq!(body["reasoning"], json!({"effort":"high"}));
    assert_eq!(body["max_tokens"], 1024 + 4096);
    assert_eq!(body["provider"]["only"], json!(["anthropic"]));
    assert_eq!(body["provider"]["max_price"]["prompt"], "4");
    assert_eq!(body["provider"]["max_price"]["completion"], "10");
    assert_eq!(body["plugins"], json!([{"id":"web","enabled":false}]));
    assert!(body["messages"][0].get("reasoning_details").is_none());

    // The reservation is the request's own bytes plus the template allowance
    // and the call's max_tokens, never the 1,000,000-token context window.
    let prompt = (PROMPT_TEMPLATE_ALLOWANCE + sent_bytes) as u64;
    assert_eq!(bound.token_limit, prompt + 5120);
    assert_eq!(bound.cost_microunits, (prompt * 4 + 5120 * 10));
    assert!(bound.token_limit < 20_000, "{bound:?}");

    // The turn's reasoning rides on its first tool call, merged by block.
    let metadata = metadata.unwrap();
    let details: Value = serde_json::from_str(&metadata[REASONING_DETAILS_METADATA]).unwrap();
    assert_eq!(
        details,
        json!([{"type":"reasoning.text","text":"Oslo is east of Lima.","signature":"sig-1",
            "format":"anthropic-claude-v1","index":0}])
    );
    assert_eq!(metadata[REASONING_TOKENS_METADATA], "443");
}

#[tokio::test]
async fn reasoning_goes_back_unmodified_with_tool_results_and_raises_the_prompt_bound() {
    let server = MockServer::start().await;
    let answer = [
        frame(json!({"content":"It is noon in Oslo."}), Some("stop"), None),
        frame(
            json!({}),
            Some("stop"),
            Some(json!({"prompt_tokens":932,"completion_tokens":12,
            "total_tokens":944,"cost":0.001984,"is_byok":false})),
        ),
        "data: [DONE]\n\n".into(),
    ]
    .concat();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = calls.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            let body = if counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                reasoning_tool_stream(reasoning_usage())
            } else {
                answer.clone()
            };
            ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
        })
        .expect(2)
        .mount(&server)
        .await;
    let provider = claude_provider(&server, 1024).await;
    let mut request = ChatRequest::simple("Which is further east, Oslo or Lima? Get its time.");
    request.tools.push(time_tool());
    let first = provider
        .chat_with_accounting(request.clone())
        .await
        .response
        .unwrap();
    assert_eq!(first.finish_reason, FinishReason::ToolUse);
    request
        .messages
        .push(axocoatl_core::ChatMessage::assistant_with_tool_calls(
            first.content.clone(),
            first.tool_calls.clone(),
        ));
    let mut result = axocoatl_core::ChatMessage::tool("12:00");
    result.tool_call_id = Some(first.tool_calls[0].id.clone());
    request.messages.push(result);
    let bound = provider.execution_bounds(&request).unwrap();
    let outcome = provider.chat_with_accounting(request).await;
    assert_eq!(outcome.response.unwrap().content, "It is noon in Oslo.");
    assert!(outcome.usage.complete);
    assert_eq!(outcome.cost_microunits, Some(1984));

    let (body, sent_bytes) = posted(&server.received_requests().await.unwrap()).remove(1);
    assert_eq!(
        body["messages"][1]["reasoning_details"],
        json!([{"type":"reasoning.text","text":"Oslo is east of Lima.","signature":"sig-1",
            "format":"anthropic-claude-v1","index":0}])
    );
    assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "toolu_1");
    // Replayed reasoning can be billed as input at its full 443 tokens.
    let prompt = (PROMPT_TEMPLATE_ALLOWANCE + sent_bytes + 443) as u64;
    assert_eq!(bound.token_limit, prompt + 5120);

    // Replayed reasoning without its recorded token count is refused.
    let mut tampered = first.tool_calls.clone();
    tampered[0]
        .provider_metadata
        .remove(REASONING_TOKENS_METADATA);
    let mut request = ChatRequest::simple("again");
    request
        .messages
        .push(axocoatl_core::ChatMessage::assistant_with_tool_calls(
            "", tampered,
        ));
    assert!(provider.execution_bounds(&request).is_none());
}

#[tokio::test]
async fn summary_and_encrypted_reasoning_blocks_merge_by_index() {
    let server = MockServer::start().await;
    let gpt = |delta: Value, finish: Option<&str>, usage: Option<Value>| {
        let mut chunk = json!({"id":"gen-gpt","model":GPT,"provider":"OpenAI",
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
        if let Some(usage) = usage {
            chunk["usage"] = usage;
        }
        format!("data: {chunk}\n\n")
    };
    let summary = |t: &str| {
        json!({"reasoning":t,"reasoning_details":[
        {"type":"reasoning.summary","summary":t,"format":"openai-responses-v1","index":0}]})
    };
    let body = [
        gpt(summary("**Plan**\n\nCheck"), None, None),
        gpt(summary(" the time."), None, None),
        gpt(
            json!({"reasoning_details":[{"type":"reasoning.encrypted","data":"gAAAA-opaque",
            "id":"rs_1","format":"openai-responses-v1","index":1}]}),
            None,
            None,
        ),
        gpt(
            json!({"tool_calls":[{"index":0,"id":"call_1","type":"function",
            "function":{"name":"get_time","arguments":"{\"city\":\"Oslo\"}"}}]}),
            None,
            None,
        ),
        gpt(
            json!({}),
            Some("tool_calls"),
            Some(json!({"prompt_tokens":97,"completion_tokens":105,
            "total_tokens":202,"cost":0.001244,"is_byok":false,
            "completion_tokens_details":{"reasoning_tokens":84}})),
        ),
        "data: [DONE]\n\n".into(),
    ]
    .concat();
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let (row, endpoints) = captured(GPT);
    catalog(&server, row, endpoints).await;
    let profile = observe_native_openrouter_profiles(&server.uri(), KEY, GPT)
        .await
        .unwrap()
        .remove(0);
    let reasoning = profile.reasoning_request(None).unwrap();
    let provider = NativeOpenRouterProvider::connect_observed(profile, KEY, 2048, 65536, reasoning)
        .await
        .unwrap();
    let mut request = ChatRequest::simple("What time is it in Oslo?");
    request.tools.push(time_tool());
    let outcome = provider.chat_with_accounting(request).await;
    let response = outcome.response.unwrap();
    let details: Value =
        serde_json::from_str(&response.tool_calls[0].provider_metadata[REASONING_DETAILS_METADATA])
            .unwrap();
    assert_eq!(
        details,
        json!([
            {"type":"reasoning.summary","summary":"**Plan**\n\nCheck the time.",
                "format":"openai-responses-v1","index":0},
            {"type":"reasoning.encrypted","data":"gAAAA-opaque","id":"rs_1",
                "format":"openai-responses-v1","index":1}
        ])
    );
    assert_eq!(response.usage.reasoning_tokens, Some(84));
    assert_eq!(response.usage.output_tokens, 21);
    let (body, _) = posted(&server.received_requests().await.unwrap()).remove(0);
    assert_eq!(body["reasoning"], json!({"effort":"medium"}));
    // Medium doubles the output: 2,048 visible plus 2,048 of reasoning.
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["provider"]["only"], json!(["openai/flex"]));
}

async fn breach(usage: Value, extra_delta: Option<Value>) -> AccountedChatOutcome {
    let server = MockServer::start().await;
    let mut body = reasoning_tool_stream(usage);
    if let Some(delta) = extra_delta {
        body = format!("{}{body}", frame(delta, None, None));
    }
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let provider = claude_provider(&server, 1024).await;
    let mut request = ChatRequest::simple("Get the time in Oslo.");
    request.tools.push(time_tool());
    provider.chat_with_accounting(request).await
}

#[tokio::test]
async fn usage_beyond_the_call_bound_is_a_breach_that_keeps_the_observed_spend() {
    // More completion (reasoning included) than the call's max_tokens.
    let mut over = reasoning_usage();
    over["completion_tokens"] = json!(5121);
    over["total_tokens"] = json!(432 + 5121);
    over["completion_tokens_details"]["reasoning_tokens"] = json!(5000);
    let outcome = breach(over, None).await;
    assert!(outcome.response.is_err());
    assert!(!outcome.usage.complete);
    assert_eq!(outcome.usage.usage.reasoning_tokens, Some(5000));
    assert_eq!(outcome.cost_microunits, Some(5784));

    // A prompt larger than the request's bound.
    let mut over = reasoning_usage();
    over["prompt_tokens"] = json!(900_000);
    over["total_tokens"] = json!(900_000 + 492);
    let outcome = breach(over, None).await;
    assert!(outcome.response.is_err());
    assert_eq!(outcome.usage.usage.input_tokens, 900_000);

    // A server tool that ran, though none was offered.
    let mut tool = reasoning_usage();
    tool["server_tool_use_details"] = json!({"web_search_requests":1});
    let outcome = breach(tool, None).await;
    let error = outcome.response.unwrap_err().to_string();
    assert!(error.contains("server tool"), "{error}");
    assert_eq!(outcome.cost_microunits, Some(5784));

    // Web citations in the stream.
    let outcome = breach(
        reasoning_usage(),
        Some(json!({"annotations":[{"type":"url_citation","url_citation":{"url":"https://example.com"}}]})),
    )
    .await;
    let error = outcome.response.unwrap_err().to_string();
    assert!(error.contains("web citations"), "{error}");
}

#[tokio::test]
async fn unbounded_or_ambiguous_endpoints_are_refused_by_name() {
    let (row, mut endpoints) = captured(CLAUDE);
    let rows = endpoints["data"]["endpoints"].as_array_mut().unwrap();
    // An unknown charged component, a discount above 1, and a base tag that
    // would also select a regional variant.
    rows[0]["pricing"]["per_page"] = json!("0.01");
    rows[1]["pricing"]["discount"] = json!(1.5);
    let mut regional = rows[4].clone();
    regional["tag"] = json!("anthropic/eu");
    rows.push(regional);
    let server = MockServer::start().await;
    catalog(&server, row, endpoints).await;
    let profiles = observe_native_openrouter_profiles(&server.uri(), KEY, CLAUDE)
        .await
        .unwrap();
    let tags = profiles
        .iter()
        .map(|p| p.endpoint_tag.as_str())
        .collect::<Vec<_>>();
    assert!(!tags.contains(&"google-vertex/global"), "{tags:?}");
    assert!(!tags.contains(&"amazon-bedrock"), "{tags:?}");
    assert!(!tags.contains(&"anthropic"), "{tags:?}");
    assert!(tags.contains(&"anthropic/eu"), "{tags:?}");

    // Every endpoint refused: the error names each reason.
    let (row, mut endpoints) = captured(CLAUDE);
    for endpoint in endpoints["data"]["endpoints"].as_array_mut().unwrap() {
        endpoint["pricing"]["per_page"] = json!("0.01");
    }
    let server = MockServer::start().await;
    catalog(&server, row, endpoints).await;
    let error = observe_native_openrouter_profiles(&server.uri(), KEY, CLAUDE)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(
            "anthropic: the endpoint charges for `per_page`, which Axocoatl cannot bound"
        ),
        "{error}"
    );

    // A model that outputs images, or has no tools, or a web-search variant.
    for (change, expected) in [
        (
            json!({"architecture":{"input_modalities":["text"],"output_modalities":["text","image"]}}),
            "does not read and write text only",
        ),
        (
            json!({"supported_parameters":["max_tokens","reasoning"]}),
            "does not support tool calling",
        ),
    ] {
        let (mut row, endpoints) = captured(CLAUDE);
        for (key, value) in change.as_object().unwrap() {
            row[key] = value.clone();
        }
        let server = MockServer::start().await;
        catalog(&server, row, endpoints).await;
        let error = observe_native_openrouter_profiles(&server.uri(), KEY, CLAUDE)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{error}");
    }
    let online =
        observe_native_openrouter_profiles("http://127.0.0.1:9", KEY, "openai/gpt-5.6-sol:online")
            .await
            .unwrap_err()
            .to_string();
    assert!(online.contains(":online"), "{online}");
}

#[tokio::test]
async fn a_retained_one_point_two_profile_still_verifies_and_runs() {
    let server = MockServer::start().await;
    metadata(&server).await;
    let current = observe_native_openrouter_profiles(&server.uri(), KEY, MODEL)
        .await
        .unwrap()
        .remove(0);
    // The exact shape 1.2.0 retained for this endpoint.
    let legacy: NativeOpenRouterObservation = serde_json::from_value(json!({
        "schema_version":1,"base_url":server.uri(),"model":MODEL,"endpoint_tag":"finite/exact",
        "provider_name":"FiniteProvider","context_tokens":2048,"max_output_tokens":128,
        "prompt_price_per_million":"1","completion_price_per_million":"2",
        "supported_parameters":["max_tokens","tools"],
        "non_reasoning_evidence":"openrouter-model-catalog:no-reasoning-contract-v1",
        "observed_at_ms":1,"billing":"openrouter_credits"
    }))
    .unwrap();
    assert!(legacy.same_contract(&current));
    // Its serialized bytes are unchanged by the new optional fields.
    assert!(!serde_json::to_string(&legacy)
        .unwrap()
        .contains("reasoning\":"));
    NativeOpenRouterProvider::connect_observed(legacy.clone(), KEY, 32, 65536, None)
        .await
        .unwrap();
    // A reasoning setting is refused for a profile that has none.
    assert!(NativeOpenRouterProvider::connect_observed(
        legacy.clone(),
        KEY,
        32,
        65536,
        Some(NativeOpenRouterReasoningRequest::Enabled)
    )
    .await
    .is_err());
    // A price that rose no longer matches the retained contract.
    let mut raised = current.clone();
    raised.completion_price_per_million = "3".into();
    assert!(!legacy.same_contract(&raised));
}

#[test]
fn price_ceilings_round_up_and_include_request_fees() {
    // 22 decimals: rounded up to the next 10^-18, never down.
    assert_eq!(
        money::ceiling_units("0.0000000416666666666667").unwrap(),
        41_666_666_667
    );
    assert!(money::decimal_units("0.0000000416666666666667").is_err());
    // 1,000 prompt tokens at $3/M, 100 output at $15/M and a $0.002 fee.
    assert_eq!(
        money::charge_bound(1000, 100, "3", "15", Some("0.002")).unwrap(),
        3000 + 1500 + 2000
    );
}

#[test]
fn streamed_reasoning_blocks_merge_until_signed_and_refuse_unknown_types() {
    let mut details = stream::ReasoningDetails::default();
    let text = |t: &str| json!({"type":"reasoning.text","text":t,"format":"anthropic-claude-v1","index":0});
    details.push(&text("first ")).unwrap();
    details.push(&text("block")).unwrap();
    details
        .push(&json!({"type":"reasoning.text","signature":"sig-a","index":0}))
        .unwrap();
    // Text under the same index after its signature is the next block.
    details.push(&text("second")).unwrap();
    details
        .push(&json!({"type":"reasoning.text","signature":"sig-b","index":0}))
        .unwrap();
    let merged: Value = serde_json::from_str(&details.encoded().unwrap().unwrap()).unwrap();
    assert_eq!(
        merged,
        json!([
            {"type":"reasoning.text","text":"first block","format":"anthropic-claude-v1","index":0,"signature":"sig-a"},
            {"type":"reasoning.text","text":"second","format":"anthropic-claude-v1","index":0,"signature":"sig-b"}
        ])
    );
    // A changed field of one block, and a server tool block, are refused.
    assert!(details
        .push(&json!({"type":"reasoning.text","format":"other","index":0}))
        .is_err());
    assert!(details
        .push(&json!({"type":"reasoning.server_tool_call","index":3}))
        .is_err());
    assert_eq!(stream::ReasoningDetails::default().encoded().unwrap(), None);
}
