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
    // The account's default web plugin and context compression are turned
    // off; a non-reasoning model gets no reasoning parameter.
    assert_eq!(
        body["plugins"],
        json!([{"id":"web","enabled":false},{"id":"context-compression","enabled":false}])
    );
    assert_eq!(body["transforms"], json!([]));
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
    provider.endpoints[0].endpoint_tag = "finite/changed".into();
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
    // openai/flex and openai/fast are service tiers a request must opt into:
    // never selected, and a base slug never matches them, so `openai`
    // qualifies. Azure lists only max_completion_tokens.
    assert_eq!(
        gpt.iter()
            .map(|p| p.endpoint_tag.as_str())
            .collect::<Vec<_>>(),
        ["openai", "amazon-bedrock/us-east-1"]
    );
    let openai = &gpt[0];
    // Base 2/10, cache write 2.5, and the >272k-token tier 4/15 with cache
    // write 5: the highest tier wins.
    assert_eq!(openai.prompt_price_per_million, "5");
    assert_eq!(openai.completion_price_per_million, "15");
    assert_eq!(openai.max_prompt_tokens, Some(922_000));
    assert_eq!(openai.prompt_limit(), 922_000);
    let contract = openai.reasoning.clone().unwrap();
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

/// Admission reads the smallest call from the public model catalog before
/// a Session exists: the same prompt allowance and reasoning allowance as
/// the endpoint observation's smallest call, which Team & budget checks.
#[tokio::test]
async fn the_catalog_floor_is_the_observed_smallest_call() {
    use axocoatl_core::ReasoningEffort as E;
    for (model, effort, reasoning) in [
        // Reasoning mandatory, default effort high: 4x the output.
        (CLAUDE, None, 4 * 8192),
        (CLAUDE, Some(E::Low), 2048),
        // Reasoning on by default at medium: once the output.
        (GPT, None, 8192),
        (GPT, Some(E::None), 0),
    ] {
        let (row, _) = captured(model);
        let catalog = json!({"data": [row]});
        let floor = catalog_call_floor(&catalog, model, 8192, effort).unwrap();
        assert_eq!(floor.prompt_tokens, PROMPT_TEMPLATE_ALLOWANCE, "{model}");
        assert_eq!(floor.reasoning_tokens, reasoning, "{model} {effort:?}");
        assert_eq!(floor.tokens(8192), 4096 + 8192 + reasoning);
        let (_, profiles) = captured_profiles(model).await;
        for profile in &profiles {
            let request = profile.reasoning_request(effort).unwrap();
            assert_eq!(request, floor.reasoning);
            let bounds = profile
                .minimum_call_bounds(8192, request, 1024 * 1024)
                .unwrap();
            assert_eq!(bounds.token_limit, floor.tokens(8192) as u64, "{model}");
        }
    }
    let (row, _) = captured(GPT);
    assert_eq!(
        catalog_call_floor(&json!({"data": [row.clone()]}), GPT, 8192, None)
            .unwrap()
            .context_tokens,
        1_050_000
    );
    // The catalog cannot say: not listed, listed twice, an effort the model
    // refuses (its observation then refuses it by name).
    assert!(catalog_call_floor(&json!({"data": [row.clone()]}), CLAUDE, 8192, None).is_none());
    assert!(catalog_call_floor(
        &json!({"data": [row.clone(), row.clone()]}),
        GPT,
        8192,
        None
    )
    .is_none());
    let (claude, _) = captured(CLAUDE);
    assert!(catalog_call_floor(&json!({"data": [claude]}), CLAUDE, 8192, Some(E::None)).is_none());
    assert!(catalog_call_floor(&json!({}), GPT, 8192, None).is_none());
    // A small context bounds the prompt allowance.
    let mut small = row;
    small["context_length"] = json!(3000);
    assert_eq!(
        catalog_call_floor(&json!({"data": [small]}), GPT, 1024, None)
            .unwrap()
            .prompt_tokens,
        3000
    );
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
    let mut reasoning_deltas = 0;
    let mut metadata = None;
    let mut usage = None;
    let mut cost = None;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            StreamEvent::ReasoningDelta { delta } => {
                reasoning_deltas += 1;
                reasoning_text.push_str(&delta);
            }
            StreamEvent::ToolCallMetadata { metadata: m, .. } => metadata = Some(m),
            StreamEvent::UsageObservation(observed) => usage = Some(observed),
            StreamEvent::CostObservation { cost_microunits } => cost = Some(cost_microunits),
            _ => {}
        }
    }
    assert_eq!(reasoning_text, "Oslo is east of Lima.");
    // Fragments are merged before they are yielded, flushed by the tool call.
    assert_eq!(reasoning_deltas, 1);
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
    assert_eq!(
        body["provider"]["max_price"],
        json!({"prompt":"4","completion":"10","request":"0"})
    );
    assert_eq!(
        body["plugins"],
        json!([{"id":"web","enabled":false},{"id":"context-compression","enabled":false}])
    );
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

/// After a call moves to another endpoint of the model, the reasoning an
/// earlier tool-calling turn returned from the first endpoint goes back to
/// the new one unchanged, as OpenRouter documents for tool use and as its
/// own load balancing sends a conversation's turns to several providers;
/// the new endpoint's bound includes the replayed reasoning tokens.
#[tokio::test]
async fn reasoning_goes_back_unchanged_to_the_endpoint_a_call_moved_to() {
    let server = MockServer::start().await;
    let (row, endpoints) = captured(CLAUDE);
    catalog(&server, row, endpoints).await;
    let profiles = observe_native_openrouter_profiles(&server.uri(), KEY, CLAUDE)
        .await
        .unwrap();
    let endpoints = vec![
        tagged(&profiles, "anthropic"),
        tagged(&profiles, "amazon-bedrock"),
    ];
    let reasoning = endpoints[0].reasoning_request(None).unwrap();
    Mock::given(method("POST"))
        .and(wiremock::matchers::body_partial_json(
            json!({"provider":{"only":["anthropic"]}}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            reasoning_tool_stream(reasoning_usage()),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&server)
        .await;
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
    .concat()
    .replace(SONNET_PROVIDER, "Amazon Bedrock");
    Mock::given(method("POST"))
        .and(wiremock::matchers::body_partial_json(
            json!({"provider":{"only":["amazon-bedrock"]}}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(answer, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let provider = NativeOpenRouterProvider::connect_observed_endpoints(
        endpoints, KEY, 1024, 65536, reasoning,
    )
    .await
    .unwrap();
    let mut request = ChatRequest::simple("Which is further east, Oslo or Lima? Get its time.");
    request.tools.push(time_tool());
    let first = provider
        .chat_with_accounting(request.clone())
        .await
        .response
        .unwrap();
    request
        .messages
        .push(axocoatl_core::ChatMessage::assistant_with_tool_calls(
            first.content.clone(),
            first.tool_calls.clone(),
        ));
    let mut result = axocoatl_core::ChatMessage::tool("12:00");
    result.tool_call_id = Some(first.tool_calls[0].id.clone());
    request.messages.push(result);
    provider.endpoint_failover().unwrap().move_to(1, true);
    let bound = provider.execution_bounds(&request).unwrap();
    let outcome = provider.chat_with_accounting(request).await;
    assert_eq!(outcome.response.unwrap().content, "It is noon in Oslo.");
    let (body, sent_bytes) = posted(&server.received_requests().await.unwrap()).remove(1);
    assert_eq!(body["provider"]["only"], json!(["amazon-bedrock"]));
    assert_eq!(
        body["messages"][1]["reasoning_details"],
        json!([{"type":"reasoning.text","text":"Oslo is east of Lima.","signature":"sig-1",
            "format":"anthropic-claude-v1","index":0}])
    );
    let prompt = (PROMPT_TEMPLATE_ALLOWANCE + sent_bytes + 443) as u64;
    assert_eq!(bound.token_limit, prompt + 5120);
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
    assert_eq!(body["provider"]["only"], json!(["openai"]));
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

#[tokio::test]
async fn service_tier_endpoints_are_never_selected_and_never_shadow_a_base_tag() {
    // A provider that adds a tier endpoint later leaves its base tag usable.
    let (row, mut endpoints) = captured(CLAUDE);
    let rows = endpoints["data"]["endpoints"].as_array_mut().unwrap();
    let mut fast = rows[4].clone();
    assert_eq!(fast["tag"], "anthropic");
    fast["tag"] = json!("anthropic/fast");
    fast["pricing"]["prompt"] = json!("0.000001");
    rows.push(fast);
    let server = MockServer::start().await;
    catalog(&server, row, endpoints).await;
    let profiles = observe_native_openrouter_profiles(&server.uri(), KEY, CLAUDE)
        .await
        .unwrap();
    assert!(profiles.iter().all(|p| p.endpoint_tag != "anthropic/fast"));
    let (_, before) = captured_profiles(CLAUDE).await;
    let mut retained = tagged(&before, "anthropic");
    let after = tagged(&profiles, "anthropic");
    retained.base_url = after.base_url.clone();
    assert!(retained.same_contract(&after));

    // Only tier endpoints: refused by name.
    let (row, mut endpoints) = captured(GPT);
    endpoints["data"]["endpoints"]
        .as_array_mut()
        .unwrap()
        .retain(|endpoint| endpoint["tag"] == "openai/flex" || endpoint["tag"] == "openai/fast");
    let server = MockServer::start().await;
    catalog(&server, row, endpoints).await;
    let error = observe_native_openrouter_profiles(&server.uri(), KEY, GPT)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("openai/flex: a service-tier endpoint a request must opt into"),
        "{error}"
    );

    // A retained profile can never name a tier endpoint or a tier variant.
    let (_, gpt) = captured_profiles(GPT).await;
    let mut flex = tagged(&gpt, "openai");
    flex.endpoint_tag = "openai/flex".into();
    assert!(flex.validate().is_err());
    let mut nitro = tagged(&gpt, "openai");
    nitro.model = format!("{GPT}:nitro");
    assert!(nitro.validate().is_err());
    for variant in [":nitro", ":floor"] {
        let refused = observe_native_openrouter_profiles(
            "http://127.0.0.1:9",
            KEY,
            &format!("{GPT}{variant}"),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(refused.contains("service tier"), "{refused}");
    }
}

#[tokio::test]
async fn a_response_at_another_service_tier_is_refused() {
    for (tier, accepted) in [
        (json!("default"), true),
        (Value::Null, true),
        (json!("flex"), false),
        (json!("priority"), false),
    ] {
        let server = MockServer::start().await;
        metadata(&server).await;
        // The tier arrives with the terminal usage, which is kept either way.
        let first = json!({"id":"gen-fixture","model":MODEL,"provider":"FiniteProvider",
            "choices":[{"index":0,"delta":{"content":"verified"},"finish_reason":"stop"}]});
        let usage = json!({"id":"gen-fixture","model":MODEL,"provider":"FiniteProvider",
            "service_tier":tier,"choices":[],"usage":{"prompt_tokens":4,"completion_tokens":2,
            "total_tokens":6,"cost":0.000008,"is_byok":false}});
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                format!("data: {first}\n\ndata: {usage}\n\ndata: [DONE]\n\n"),
                "text/event-stream",
            ))
            .mount(&server)
            .await;
        let outcome = provider(&server)
            .await
            .chat_with_accounting(ChatRequest::simple("Answer once"))
            .await;
        match outcome.response {
            Ok(response) => {
                assert!(accepted, "{tier}");
                assert_eq!(response.content, "verified");
            }
            Err(error) => {
                assert!(!accepted, "{tier}: {error}");
                assert!(error.to_string().contains("service tier"), "{error}");
            }
        }
        assert_eq!(outcome.usage.usage.input_tokens, 4, "{tier}");
        assert_eq!(outcome.cost_microunits, Some(8), "{tier}");
    }
}

#[tokio::test]
async fn image_and_audio_priced_endpoints_run_without_image_or_audio_caps() {
    let server = MockServer::start().await;
    let row = json!({"id":MODEL,"architecture":{"input_modalities":["text","image","audio"],
        "output_modalities":["text"]},"supported_parameters":["max_tokens","tools"]});
    let endpoints = json!({"data":{"id":MODEL,"endpoints":[{"model_id":MODEL,
        "provider_name":"FiniteProvider","tag":"finite/exact","context_length":2048,
        "max_completion_tokens":128,"supported_parameters":["max_tokens","tools"],
        "pricing":{"prompt":"0.000001","completion":"0.000002","image":"0.0000001",
            "audio":"0.0000003"},"status":0}]}});
    catalog(&server, row, endpoints).await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(reply(true), "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let profile = observe_native_openrouter_profiles(&server.uri(), KEY, MODEL)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(profile.unrequested_priced_features, ["audio", "image"]);
    let provider = NativeOpenRouterProvider::connect_observed(profile, KEY, 32, 65536, None)
        .await
        .unwrap();
    let outcome = provider
        .chat_with_accounting(ChatRequest::simple("Answer once"))
        .await;
    assert_eq!(outcome.response.unwrap().content, "verified");
    // OpenRouter filters endpoints on their listed image and audio prices
    // even for text, so a zero cap there would refuse every call.
    let (body, _) = posted(&server.received_requests().await.unwrap()).remove(0);
    assert_eq!(
        body["provider"]["max_price"],
        json!({"prompt":"1","completion":"2","request":"0"})
    );
}

#[tokio::test]
async fn a_response_allowance_past_the_endpoint_or_byte_limit_is_refused_not_clamped() {
    use axocoatl_core::ReasoningEffort as E;
    use NativeOpenRouterReasoningRequest as R;
    let (server, claude) = captured_profiles(CLAUDE).await;
    let anthropic = tagged(&claude, "anthropic");
    let bytes = 1024 * 1024;
    // 8,192 at xhigh asks for 163,840 tokens; the endpoint allows 128,000.
    let refused = anthropic
        .response_refusal(8192, Some(R::Effort(E::Xhigh)), bytes)
        .unwrap();
    assert!(
        refused.contains("max_tokens 163840 (8192 of output plus 155648 of reasoning)"),
        "{refused}"
    );
    assert!(refused.contains("128000 output tokens"), "{refused}");
    assert_eq!(
        anthropic.response_allowance(8192, Some(R::Effort(E::Xhigh))),
        163_840
    );
    // 5,300 at max fits the endpoint, but not the response bytes a Session
    // keeps at ten bytes per streamed token.
    let refused = anthropic
        .response_refusal(5300, Some(R::Effort(E::Max)), bytes)
        .unwrap();
    assert!(refused.contains("may stream 106000 tokens"), "{refused}");
    assert_eq!(
        anthropic.response_refusal(4096, Some(R::Effort(E::High)), bytes),
        None
    );
    for (output, effort) in [(8192, E::Xhigh), (5300, E::Max)] {
        assert!(NativeOpenRouterProvider::connect_observed(
            anthropic.clone(),
            KEY,
            output,
            bytes,
            Some(R::Effort(effort)),
        )
        .await
        .is_err());
    }
    drop(server);
}

#[tokio::test]
async fn budgets_see_the_whole_response_and_the_replayed_reasoning() {
    let server = MockServer::start().await;
    let provider = claude_provider(&server, 1024).await;
    let mut request = ChatRequest::simple("again");
    request.tools.push(time_tool());
    // High effort: 1,024 of output plus four times that of reasoning, and
    // never less than the 1,024-token minimum.
    assert_eq!(provider.response_tokens(&request, 1024), 5120);
    assert_eq!(provider.response_tokens(&request, 100), 1124);
    let mut call = axocoatl_llm::ToolCall {
        id: "toolu_1".into(),
        name: "get_time".into(),
        arguments: json!({"city":"Oslo"}),
        provider_metadata: Default::default(),
    };
    call.provider_metadata.insert(
        REASONING_DETAILS_METADATA.into(),
        json!([{"type":"reasoning.text","text":"Oslo.","signature":"sig-1","index":0}]).to_string(),
    );
    call.provider_metadata
        .insert(REASONING_TOKENS_METADATA.into(), "443".into());
    request
        .messages
        .push(axocoatl_core::ChatMessage::assistant_with_tool_calls(
            "",
            vec![call],
        ));
    let mut result = axocoatl_core::ChatMessage::tool("12:00");
    result.tool_call_id = Some("toolu_1".into());
    request.messages.push(result);
    assert_eq!(
        provider.count_tokens(&request),
        axocoatl_llm::approximate_request_tokens(&request) + 443
    );
    // The call after a tool round reserves its grown prompt at the input
    // ceiling ($4/M), the same response, and stays within the prompt limit.
    let bound = provider.execution_bounds(&request).unwrap();
    let grown = provider.follow_up_execution_bounds(&request, 1000).unwrap();
    assert_eq!(grown.token_limit, bound.token_limit + 1000);
    assert_eq!(grown.cost_microunits, bound.cost_microunits + 4000);
    let capped = provider
        .follow_up_execution_bounds(&request, u64::MAX)
        .unwrap();
    assert_eq!(capped.token_limit, 1_000_000 + 5120);
}

/// A refused request states its status and `Retry-After` (seconds or an
/// HTTP date); the provider sends once and leaves any retry to the Session.
#[tokio::test]
async fn refusals_state_their_status_and_retry_after_and_are_sent_once() {
    for (status, retry_after, expected) in [
        (429, Some("7".to_string()), Some(" (Retry-After: 7 s)")),
        (
            503,
            Some(httpdate::fmt_http_date(
                std::time::SystemTime::now() - std::time::Duration::from_secs(5),
            )),
            Some(" (Retry-After: 0 s)"),
        ),
        (401, None, None),
    ] {
        let server = MockServer::start().await;
        metadata(&server).await;
        let mut template = ResponseTemplate::new(status).set_body_string("refused");
        if let Some(value) = &retry_after {
            template = template.insert_header("Retry-After", value.as_str());
        }
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(template)
            .expect(1)
            .mount(&server)
            .await;
        let error = match provider(&server)
            .await
            .chat_stream(ChatRequest::simple("one call"))
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("{status} must be refused"),
        };
        let ProviderError::ApiError {
            status: got,
            message,
            ..
        } = error
        else {
            panic!("{status}: {error:?}")
        };
        assert_eq!(got, status);
        match expected {
            Some(suffix) => assert!(message.ends_with(suffix), "{message}"),
            None => assert!(!message.contains(RETRY_AFTER_SUFFIX), "{message}"),
        }
        assert!(!message.contains(KEY));
    }
}

/// An error OpenRouter reports inside the stream carries the upstream's
/// status when its code is one.
#[tokio::test]
async fn an_error_inside_the_stream_carries_its_status() {
    for (code, status) in [(json!(502), Some(502u16)), (json!("server_error"), None)] {
        let server = MockServer::start().await;
        metadata(&server).await;
        let body = format!(
            ": OPENROUTER PROCESSING\n\ndata: {}\n\n",
            json!({"id":"gen-fixture","model":MODEL,"provider":"FiniteProvider",
                   "error":{"code":code,"message":"upstream failed"},"choices":[]})
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
        let mut stream = provider(&server)
            .await
            .chat_stream(ChatRequest::simple("one call"))
            .await
            .unwrap();
        let first = stream.next().await.unwrap();
        match (first, status) {
            (Err(ProviderError::ApiError { status: got, .. }), Some(status)) => {
                assert_eq!(got, status)
            }
            (Err(ProviderError::Stream(message)), None) => {
                assert!(
                    message.contains(
                        "endpoint finite/exact (FiniteProvider) of meta-llama/test-instruct \
                         reported an error in its answer: upstream failed"
                    ),
                    "{message}"
                )
            }
            (other, _) => panic!("{code}: {other:?}"),
        }
    }
}

/// A server with the catalog row of `model` and `endpoints` as its
/// endpoint list.
async fn serving_endpoints(model: &str, endpoints: &Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{
            "id":model,"architecture":{"input_modalities":["text"],"output_modalities":["text"]},
            "supported_parameters":["max_tokens","tools"]
        }]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/models/{model}/endpoints")))
        .respond_with(ResponseTemplate::new(200).set_body_json(endpoints))
        .mount(&server)
        .await;
    server
}

/// qwen/qwen3-coder's public endpoint fields, captured 2026-10-23, with each
/// endpoint's `status` and `uptime_last_30m` set to `health`.
fn qwen3_coder(health: &[(i64, Option<f64>)]) -> Value {
    let mut endpoints: Value =
        serde_json::from_str(include_str!("fixtures/qwen--qwen3-coder.json")).unwrap();
    for (row, (status, uptime)) in endpoints["data"]["endpoints"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .zip(health)
    {
        row["status"] = json!(status);
        row["uptime_last_30m"] = json!(uptime);
    }
    endpoints
}

/// When OpenRouter reports every endpoint that fits as unavailable or
/// degraded (as it did for qwen/qwen3-coder on 2026-10-23: status -5 and
/// -2, 0 to 80% of requests succeeding), the refusal says so plainly, with
/// each endpoint's status and uptime and what to do, as a 503: OpenRouter's
/// state, not a model identity or a setup problem. No degraded endpoint is
/// used while none is healthy: a run stays on one endpoint with no
/// fallback.
#[tokio::test]
async fn every_endpoint_unavailable_is_said_as_openrouter_status() {
    let model = "qwen/qwen3-coder";
    let server = serving_endpoints(
        model,
        &qwen3_coder(&[(-5, Some(0.0)), (-2, Some(80.0)), (-2, Some(75.0))]),
    )
    .await;
    let error = observe_native_openrouter_profiles(&server.uri(), KEY, model)
        .await
        .unwrap_err();
    let ProviderError::ApiError {
        status: 503,
        message,
        ..
    } = &error
    else {
        panic!("{error:?}");
    };
    assert!(
        message.starts_with(
            "OpenRouter reports every endpoint that could serve qwen/qwen3-coder as unavailable \
             or degraded right now (google-vertex/us-south1: status -5, 0% of requests \
             succeeded over the last 30 minutes; deepinfra/turbo: status -2, 80% of requests \
             succeeded over the last 30 minutes; venice/fp8: status -2, 75% of requests \
             succeeded over the last 30 minutes). This is OpenRouter's provider status, not your \
             setup: try again later, or choose another model"
        ),
        "{message}"
    );
    assert!(!message.contains("identity"), "{message}");
    let endpoints = observe_native_openrouter_endpoints(&server.uri(), KEY, model)
        .await
        .unwrap();
    assert!(endpoints.profiles.is_empty());
    assert_eq!(endpoints.unavailable.len(), 3);
    assert!(endpoints.rejected.is_empty(), "{:?}", endpoints.rejected);

    // One healthy endpoint is used; the degraded ones are not.
    let server = serving_endpoints(
        model,
        &qwen3_coder(&[(-5, Some(0.0)), (0, Some(95.2)), (-5, None)]),
    )
    .await;
    let profiles = observe_native_openrouter_profiles(&server.uri(), KEY, model)
        .await
        .unwrap();
    assert_eq!(
        profiles
            .iter()
            .map(|profile| profile.endpoint_tag.as_str())
            .collect::<Vec<_>>(),
        ["deepinfra/turbo"]
    );
}

/// Another model id under an endpoint is said as that, apart from
/// availability; an endpoint that would not fit is never said to be only
/// unavailable.
#[tokio::test]
async fn identity_and_fit_are_said_apart_from_availability() {
    let model = "qwen/qwen3-coder";
    let mut endpoints = qwen3_coder(&[(0, Some(100.0)), (-2, Some(80.0)), (0, Some(100.0))]);
    endpoints["data"]["endpoints"][0]["model_id"] = json!("qwen/qwen3-coder-other");
    endpoints["data"]["endpoints"][2]["supported_parameters"] = json!(["max_tokens"]);
    let server = serving_endpoints(model, &endpoints).await;
    let error = observe_native_openrouter_profiles(&server.uri(), KEY, model)
        .await
        .unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains(
            "OpenRouter reports every endpoint that could serve qwen/qwen3-coder as unavailable \
             or degraded right now (deepinfra/turbo: status -2, 80% of requests succeeded"
        ),
        "{message}"
    );
    assert!(
        message.contains(
            "Its other endpoints cannot be used: google-vertex/us-south1: OpenRouter lists it \
             under another model id; venice/fp8: does not accept max_tokens and tools."
        ),
        "{message}"
    );
    // Nothing unavailable: the fit reasons alone, as before.
    endpoints["data"]["endpoints"][1]["supported_parameters"] = json!(["max_tokens"]);
    let server = serving_endpoints(model, &endpoints).await;
    let message = observe_native_openrouter_profiles(&server.uri(), KEY, model)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("no OpenRouter endpoint for qwen/qwen3-coder has exact text/tool limits"),
        "{message}"
    );
    assert!(!message.contains("unavailable"), "{message}");
}

/// A Session's retained endpoint that OpenRouter reports degraded when the
/// provider is opened again is said as that, not as a changed contract.
#[tokio::test]
async fn a_retained_endpoint_that_is_degraded_now_is_said_as_that() {
    let model = "qwen/qwen3-coder";
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{
            "id":model,"architecture":{"input_modalities":["text"],"output_modalities":["text"]},
            "supported_parameters":["max_tokens","tools"]
        }]})))
        .mount(&server)
        .await;
    // Healthy when the Session observed it, degraded since.
    Mock::given(method("GET"))
        .and(path(format!("/models/{model}/endpoints")))
        .respond_with(ResponseTemplate::new(200).set_body_json(qwen3_coder(&[
            (-5, Some(0.0)),
            (0, Some(100.0)),
            (-5, Some(0.0)),
        ])))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/models/{model}/endpoints")))
        .respond_with(ResponseTemplate::new(200).set_body_json(qwen3_coder(&[
            (-5, Some(0.0)),
            (-2, Some(60.0)),
            (-5, Some(0.0)),
        ])))
        .with_priority(2)
        .mount(&server)
        .await;
    let profile = observe_native_openrouter_profiles(&server.uri(), KEY, model)
        .await
        .unwrap()
        .remove(0);
    let error =
        match NativeOpenRouterProvider::connect_observed(profile, KEY, 4096, 1 << 20, None).await {
            Ok(_) => panic!("a degraded endpoint must not be opened"),
            Err(error) => error,
        };
    let message = error.to_string();
    assert!(
        message.contains(
            "OpenRouter reports the endpoint this Session uses for qwen/qwen3-coder as \
             unavailable or degraded right now (deepinfra/turbo: status -2, 60% of requests \
             succeeded over the last 30 minutes)"
        ),
        "{message}"
    );
    assert!(!message.contains("review a new profile"), "{message}");
}

// ------------------------------------------------- endpoints and failover

/// What OpenRouter answered on 2026-10-23 when deepseek/deepseek-chat-v3.1's
/// cheapest endpoint was limited: its raw text carries a hint to add a
/// provider key (BYOK), which native Sessions refuse, and an account id.
const UPSTREAM_LIMITED: &str = r#"{"error":{"message":"Provider returned error","code":429,"metadata":{"raw":"deepseek/deepseek-chat-v3.1 is temporarily rate-limited upstream. Please retry shortly, or add your own key to accumulate your rate limits: https://openrouter.ai/settings/integrations","provider_name":"DeepInfra","is_byok":false,"provider_error_code":"rate_limit_exceeded","limit_source":"upstream_provider_shared_pool","remedy_hint":"Retry shortly, add your own provider key (https://openrouter.ai/settings/integrations), or route to another provider with provider routing: https://openrouter.ai/docs/features/provider-routing"}},"user_id":"user_fixture"}"#;
/// OpenRouter's own limit on the account, the same day: no upstream named.
const ACCOUNT_LIMITED_BODY: &str = r#"{"error":{"message":"OpenRouter could not verify available credits for this request in time. Retry shortly.","code":429,"metadata":{"limit_source":"openrouter_admission_control","remedy_hint":"Slow down the request rate for this account, then retry after the reset (see X-RateLimit-Reset).","headers":{"Retry-After":"10"},"provider_name":null}}}"#;

/// One endpoint row of [`MODEL`]: `tag`, served by `provider`, at `scale`
/// times the fixture's prices, with OpenRouter's `status`.
fn endpoint_row(tag: &str, provider: &str, scale: u32, status: i64) -> Value {
    json!({"model_id":MODEL,"provider_name":provider,"tag":tag,"context_length":2048,
        "max_completion_tokens":128,"supported_parameters":["max_tokens","tools"],
        "pricing":{"prompt":format!("0.00000{scale}"),"completion":format!("0.00000{}", 2 * scale)},
        "status":status,"supports_implicit_caching":false})
}

/// A server whose catalog lists `rows` as [`MODEL`]'s endpoints.
async fn endpoints_server(rows: Vec<Value>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/models")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":MODEL,"architecture":{"input_modalities":["text"],"output_modalities":["text"]},"supported_parameters":["max_tokens","tools"]}]}))).mount(&server).await;
    Mock::given(method("GET"))
        .and(path(format!("/models/{MODEL}/endpoints")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data":{"id":MODEL,"endpoints":rows}})),
        )
        .mount(&server)
        .await;
    server
}

/// The reply `provider` streams.
fn reply_from(provider: &str) -> String {
    reply(true).replace("FiniteProvider", provider)
}

/// Inference requests `server` received, by the endpoint each pinned.
async fn pinned(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.method.as_str() == "POST")
        .map(|request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["provider"]["only"], body["provider"]["order"]);
            assert_eq!(body["provider"]["allow_fallbacks"], false);
            body["provider"]["only"][0].as_str().unwrap().to_owned()
        })
        .collect()
}

/// A provider over every endpoint `server` lists now, cheapest first.
async fn provider_over(server: &MockServer) -> NativeOpenRouterProvider {
    let profiles = observe_native_openrouter_profiles(&server.uri(), KEY, MODEL)
        .await
        .unwrap();
    NativeOpenRouterProvider::connect_observed_endpoints(profiles, KEY, 32, 65536, None)
        .await
        .unwrap()
}

/// The failure opening `request` on `provider` meets.
async fn open_failure(provider: &NativeOpenRouterProvider, request: ChatRequest) -> FailedCall {
    match provider.open_chat_stream(request).await {
        Err(failed) => failed,
        Ok(_) => panic!("the call must fail before its answer"),
    }
}

/// A refusal is said in plain words: which endpoint of which model, what
/// OpenRouter reports and what to do. Never OpenRouter's raw JSON, its
/// upstream's text with the hint to add a provider key, the account id, or a
/// body that is not OpenRouter's JSON (an HTML error page). Nothing was
/// generated, so the call is known to have cost nothing; whether the
/// failure is the endpoint's own is said with it, never read from words.
#[tokio::test]
async fn refusals_are_said_in_plain_words_and_known_to_cost_nothing() {
    for (status, body, retry_after, expected, endpoint_failure) in [
        (
            429,
            UPSTREAM_LIMITED,
            Some("7"),
            "OpenRouter reports endpoint finite/exact (FiniteProvider) of \
             meta-llama/test-instruct as rate-limited upstream right now (HTTP 429), a provider \
             outage, not your setup: try again later, or choose another model; nothing was \
             generated (Retry-After: 7 s)",
            true,
        ),
        (
            429,
            ACCOUNT_LIMITED_BODY,
            Some("10"),
            "OpenRouter is limiting this account's requests right now (HTTP 429: OpenRouter \
             could not verify available credits for this request in time. Retry shortly.); \
             this is OpenRouter's limit on the account, not one endpoint's: wait and try again; \
             nothing was generated (Retry-After: 10 s)",
            false,
        ),
        (
            502,
            r#"{"error":{"message":"Provider returned error","code":502,"metadata":{"provider_name":"FiniteProvider","raw":"add your own key"}}}"#,
            None,
            "OpenRouter reports endpoint finite/exact (FiniteProvider) of \
             meta-llama/test-instruct as unavailable right now (HTTP 502), a provider outage, \
             not your setup: try again later, or choose another model; nothing was generated",
            true,
        ),
        (
            503,
            "upstream busy",
            None,
            "OpenRouter reports endpoint finite/exact (FiniteProvider) of \
             meta-llama/test-instruct as unavailable right now (HTTP 503), a provider outage, \
             not your setup: try again later, or choose another model; nothing was generated",
            true,
        ),
        (
            524,
            "<!DOCTYPE html><html><head><title>openrouter.ai | 524: A timeout occurred</title></head><body>add your own key</body></html>",
            None,
            "OpenRouter reports endpoint finite/exact (FiniteProvider) of \
             meta-llama/test-instruct as unavailable right now (HTTP 524), a provider outage, \
             not your setup: try again later, or choose another model; nothing was generated",
            true,
        ),
        (
            402,
            r#"{"error":{"message":"Insufficient credits. Add more using https://openrouter.ai/settings/credits","code":402}}"#,
            None,
            "OpenRouter refused the call to endpoint finite/exact (FiniteProvider) of \
             meta-llama/test-instruct (HTTP 402: Insufficient credits. Add more using \
             https://openrouter.ai/settings/credits); nothing was generated",
            false,
        ),
    ] {
        let server = MockServer::start().await;
        metadata(&server).await;
        let mut template = ResponseTemplate::new(status).set_body_string(body);
        if let Some(value) = retry_after {
            template = template.insert_header("Retry-After", value);
        }
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(template)
            .expect(1)
            .mount(&server)
            .await;
        let provider = provider(&server).await;
        let failed = open_failure(&provider, ChatRequest::simple("one call")).await;
        let ProviderError::ApiError {
            status: got,
            message,
            ..
        } = &failed.error
        else {
            panic!("{status}: {:?}", failed.error)
        };
        assert_eq!(*got, status);
        assert_eq!(message, expected);
        for leaked in [
            "{",
            "<",
            "own key",
            "user_",
            "integrations",
            "provider routing",
            "raw",
        ] {
            assert!(!message.contains(leaked), "{leaked}: {message}");
        }
        assert!(failed.facts.cost_nothing, "{message}");
        assert_eq!(failed.facts.endpoint_failure, endpoint_failure, "{message}");
        assert!(!failed.facts.summary.is_empty(), "{message}");
    }
    // A transport failure after the request may have left is not known to
    // be free; one where no connection was made is.
    let timed_out = failover::transport(
        ProviderError::Network("timed out: the answer did not start".into()),
        false,
        true,
    );
    assert!(!timed_out.facts.cost_nothing);
    assert!(timed_out.facts.endpoint_failure);
    assert_eq!(timed_out.facts.summary, "timed out");
    // A connection to OpenRouter never made is every endpoint's, even one
    // that timed out.
    for interrupted in [false, true] {
        let refused = failover::transport(
            ProviderError::Network("error sending request: connection refused".into()),
            true,
            interrupted,
        );
        assert!(refused.facts.cost_nothing);
        assert!(!refused.facts.endpoint_failure);
    }
}

/// An error OpenRouter reports inside an answer is never known to be free,
/// whatever its message says: the endpoint may have generated (and billed)
/// reasoning before it, which a native answer does not show until later.
/// Its status alone says whether it is the endpoint's own; the endpoint is
/// not counted as having answered the call.
#[tokio::test]
async fn an_error_inside_the_answer_is_never_known_to_be_free() {
    let server = MockServer::start().await;
    metadata(&server).await;
    let reasoning = json!({"id":"gen-fixture","model":MODEL,"provider":"FiniteProvider",
        "choices":[{"index":0,"delta":{"reasoning":"thinking about it"}}]});
    let failure = json!({"id":"gen-fixture","model":MODEL,"provider":"FiniteProvider",
        "error":{"code":502,"message":"Upstream aborted: nothing was generated after the cutoff; \
                 OpenRouter is limiting this account's requests"},"choices":[]});
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!("data: {reasoning}\n\ndata: {failure}\n\n"),
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let served = Arc::new(NativeOpenRouterServed::default());
    let provider = provider(&server)
        .await
        .with_served_endpoints(served.clone());
    let mut stream = provider
        .open_chat_stream(ChatRequest::simple("one call"))
        .await
        .unwrap();
    let error = stream.next().await.unwrap().unwrap_err();
    assert!(matches!(error, ProviderError::ApiError { status: 502, .. }));
    let failed = provider.failed_answer(error);
    assert!(!failed.facts.cost_nothing);
    assert!(failed.facts.endpoint_failure, "a 502 is the endpoint's own");
    assert_eq!(failed.facts.summary, "unavailable (HTTP 502)");
    // The same words as a protocol failure (no numeric status) move nothing.
    let quoted = failover::answer_failure(protocol(
        "endpoint finite/exact reported an error in its answer: upstream timed out",
    ));
    assert!(!quoted.facts.endpoint_failure && !quoted.facts.cost_nothing);
    // The non-streaming path says the same.
    let (outcome, facts) = provider
        .chat_with_accounting_and_facts(ChatRequest::simple("one call"))
        .await;
    assert!(outcome.response.is_err());
    assert!(!outcome.usage.complete);
    assert!(!facts.cost_nothing && facts.endpoint_failure);
    assert!(served.endpoints().is_empty(), "{:?}", served.endpoints());
}

/// A provider over several endpoints starts on the first. The candidates a
/// call can move to are the endpoints after the current one, wrapping
/// around, that can carry the request and were not excluded; each reserves
/// at its own prices. A move takes effect only when the host makes it, and
/// counts against the endpoint it left only when that endpoint failed the
/// call. Calls then stay there, each request pins its one endpoint, and the
/// endpoint that answered each call to the end is recorded.
#[tokio::test]
async fn a_failover_moves_calls_to_the_next_endpoint_that_fits() {
    let server = endpoints_server(vec![
        endpoint_row("finite/exact", "FiniteProvider", 1, 0),
        endpoint_row("other/exact", "OtherProvider", 2, 0),
        endpoint_row("third/exact", "ThirdProvider", 3, 0),
    ])
    .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(wiremock::matchers::body_partial_json(
            json!({"provider":{"only":["finite/exact"]}}),
        ))
        .respond_with(ResponseTemplate::new(429).set_body_string(UPSTREAM_LIMITED))
        .mount(&server)
        .await;
    for (tag, provider) in [
        ("other/exact", "OtherProvider"),
        ("third/exact", "ThirdProvider"),
    ] {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                json!({"provider":{"only":[tag]}}),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(reply_from(provider), "text/event-stream"),
            )
            .mount(&server)
            .await;
    }
    let served = Arc::new(NativeOpenRouterServed::default());
    let provider = provider_over(&server)
        .await
        .with_served_endpoints(served.clone());
    let failover = provider.endpoint_failover().expect("three endpoints");
    assert_eq!(failover.endpoint_count(), 3);
    assert_eq!(failover.current_endpoint(), 0);
    let request = ChatRequest::simple("one call");
    let cheap = provider.execution_bounds(&request).unwrap();
    let failed = open_failure(&provider, request.clone()).await;
    assert!(matches!(
        failed.error,
        ProviderError::ApiError { status: 429, .. }
    ));
    assert!(failed.facts.endpoint_failure && failed.facts.cost_nothing);

    assert_eq!(failover.candidates(&request, &[0]), [1, 2]);
    assert_eq!(failover.candidates(&request, &[1]), [2]);
    let pricier = failover.execution_bounds_on(1, &request).unwrap();
    assert!(pricier.cost_microunits > cheap.cost_microunits);
    assert_eq!(pricier.token_limit, cheap.token_limit);
    assert_eq!(failover.execution_bounds_on(0, &request), Some(cheap));
    assert_eq!(failover.execution_bounds_on(3, &request), None);
    assert_eq!(
        failover.describe_move(0, 1),
        "from endpoint finite/exact (FiniteProvider) to endpoint other/exact (OtherProvider)"
    );
    // Nothing moves until the host moves it.
    assert_eq!(failover.current_endpoint(), 0);
    failover.move_to(1, true);
    assert_eq!(failover.current_endpoint(), 1);
    // The next call reserves at the new endpoint's prices.
    assert_eq!(provider.execution_bounds(&request), Some(pricier));
    // The capabilities the Agent's context is fitted to stay the first's.
    assert_eq!(provider.capabilities().max_context_tokens, 2048);
    let outcome = provider.chat_with_accounting(request.clone()).await;
    assert_eq!(outcome.response.unwrap().content, "verified");
    assert!(outcome.usage.complete);
    assert_eq!(outcome.cost_microunits, Some(8));
    // Later calls stay on the endpoint calls were moved to.
    provider.chat(request.clone()).await.unwrap();
    // The order wraps around from the current endpoint.
    assert_eq!(failover.candidates(&request, &[]), [2, 0]);
    assert_eq!(failover.candidates(&request, &[2, 0]), Vec::<usize>::new());
    // A move the host chose (not a failure) is not counted as one.
    failover.move_to(2, false);
    provider.chat(request.clone()).await.unwrap();
    assert_eq!(
        pinned(&server).await,
        ["finite/exact", "other/exact", "other/exact", "third/exact"]
    );
    assert_eq!(
        served.endpoints(),
        [
            ServedOpenRouterEndpoint {
                endpoint: "finite/exact".into(),
                provider: "FiniteProvider".into(),
                calls: 0,
                moved: 1,
            },
            ServedOpenRouterEndpoint {
                endpoint: "other/exact".into(),
                provider: "OtherProvider".into(),
                calls: 2,
                moved: 0,
            },
            ServedOpenRouterEndpoint {
                endpoint: "third/exact".into(),
                provider: "ThirdProvider".into(),
                calls: 1,
                moved: 0,
            }
        ]
    );
}

/// An answer from another provider than the endpoint the call pinned is
/// refused, on an endpoint retained for failover as on the first, and is
/// not the endpoint's own failure: the call is not moved again.
#[tokio::test]
async fn an_answer_from_another_provider_is_refused_on_a_fallback_too() {
    let server = endpoints_server(vec![
        endpoint_row("finite/exact", "FiniteProvider", 1, 0),
        endpoint_row("other/exact", "OtherProvider", 2, 0),
    ])
    .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(reply_from("FiniteProvider"), "text/event-stream"),
        )
        .mount(&server)
        .await;
    let served = Arc::new(NativeOpenRouterServed::default());
    let provider = provider_over(&server)
        .await
        .with_served_endpoints(served.clone());
    let failover = provider.endpoint_failover().unwrap();
    failover.move_to(1, true);
    let (outcome, facts) = provider
        .chat_with_accounting_and_facts(ChatRequest::simple("one call"))
        .await;
    let message = outcome.response.unwrap_err().to_string();
    assert!(
        message.contains("response changed the exact admitted provider"),
        "{message}"
    );
    assert!(!facts.endpoint_failure && !facts.cost_nothing);
    assert_eq!(
        served.endpoints(),
        [ServedOpenRouterEndpoint {
            endpoint: "finite/exact".into(),
            provider: "FiniteProvider".into(),
            calls: 0,
            moved: 1,
        }]
    );
}

/// An endpoint retained for failover whose contract no longer matches the
/// catalog (here its price rose) is never sent to: the call fails before
/// anything is sent, says which endpoint changed, is known to have cost
/// nothing and to be that endpoint's own failure, and moves on to the next
/// endpoint that still matches.
#[tokio::test]
async fn a_fallback_whose_contract_changed_is_never_sent_to() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/models")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":MODEL,"architecture":{"input_modalities":["text"],"output_modalities":["text"]},"supported_parameters":["max_tokens","tools"]}]}))).mount(&server).await;
    let rows = |other_scale| {
        json!({"data":{"id":MODEL,"endpoints":[
            endpoint_row("finite/exact", "FiniteProvider", 1, 0),
            endpoint_row("other/exact", "OtherProvider", other_scale, 0),
            endpoint_row("third/exact", "ThirdProvider", 3, 0),
        ]}})
    };
    // As Team & budget observed it, then once more as the provider opens.
    Mock::given(method("GET"))
        .and(path(format!("/models/{MODEL}/endpoints")))
        .respond_with(ResponseTemplate::new(200).set_body_json(rows(2)))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/models/{MODEL}/endpoints")))
        .respond_with(ResponseTemplate::new(200).set_body_json(rows(4)))
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(wiremock::matchers::body_partial_json(
            json!({"provider":{"only":["third/exact"]}}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(reply_from("ThirdProvider"), "text/event-stream"),
        )
        .mount(&server)
        .await;
    let provider = provider_over(&server).await;
    let failover = provider.endpoint_failover().unwrap();
    let request = ChatRequest::simple("one call");
    failover.move_to(1, true);
    let changed = open_failure(&provider, request.clone()).await;
    let message = changed.error.to_string();
    assert!(
        message.contains(
            "the retained OpenRouter endpoint other/exact (OtherProvider) of \
             meta-llama/test-instruct changed since Team & budget reviewed it"
        ),
        "{message}"
    );
    assert!(changed.facts.cost_nothing && changed.facts.endpoint_failure);
    assert_eq!(
        changed.facts.summary,
        "its contract changed since Team & budget reviewed it"
    );
    assert!(pinned(&server).await.is_empty(), "nothing was sent");
    assert_eq!(failover.candidates(&request, &[0, 1]), [2]);
    failover.move_to(2, true);
    provider.chat(request).await.unwrap();
    assert_eq!(pinned(&server).await, ["third/exact"]);
}

/// When OpenRouter's catalog no longer lists any endpoint of the model that
/// native Sessions can use, the call is refused before it is sent, saying so
/// once, and no other endpoint is tried for it: every one would say the same.
#[tokio::test]
async fn a_model_with_no_usable_endpoint_now_is_said_once_and_moves_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/models")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":MODEL,"architecture":{"input_modalities":["text"],"output_modalities":["text"]},"supported_parameters":["max_tokens","tools"]}]}))).mount(&server).await;
    let rows = |rows: Vec<Value>| json!({"data":{"id":MODEL,"endpoints":rows}});
    Mock::given(method("GET"))
        .and(path(format!("/models/{MODEL}/endpoints")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(rows(vec![endpoint_row(
                "finite/exact",
                "FiniteProvider",
                1,
                0,
            )])),
        )
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/models/{MODEL}/endpoints")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(rows(vec![endpoint_row(
                "other/exact",
                "OtherProvider",
                2,
                -5,
            )])),
        )
        .with_priority(2)
        .mount(&server)
        .await;
    let provider = provider(&server).await;
    let failed = open_failure(&provider, ChatRequest::simple("one call")).await;
    let message = failed.error.to_string();
    assert!(
        message.contains("OpenRouter reports every endpoint that could serve"),
        "{message}"
    );
    assert_eq!(
        message.matches(failover::NOTHING_SENT).count(),
        1,
        "{message}"
    );
    assert!(failed.facts.cost_nothing && !failed.facts.endpoint_failure);
    assert!(pinned(&server).await.is_empty());
}

/// A provider over several endpoints opens while at least one of them can
/// be used; with none, the refusal names each.
#[tokio::test]
async fn opening_needs_one_usable_endpoint_and_names_each_otherwise() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/models")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":MODEL,"architecture":{"input_modalities":["text"],"output_modalities":["text"]},"supported_parameters":["max_tokens","tools"]}]}))).mount(&server).await;
    let rows = |first, second| {
        json!({"data":{"id":MODEL,"endpoints":[
            endpoint_row("finite/exact", "FiniteProvider", 1, first),
            endpoint_row("other/exact", "OtherProvider", 2, second),
        ]}})
    };
    Mock::given(method("GET"))
        .and(path(format!("/models/{MODEL}/endpoints")))
        .respond_with(ResponseTemplate::new(200).set_body_json(rows(0, 0)))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/models/{MODEL}/endpoints")))
        .respond_with(ResponseTemplate::new(200).set_body_json(rows(-2, 0)))
        .up_to_n_times(1)
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/models/{MODEL}/endpoints")))
        .respond_with(ResponseTemplate::new(200).set_body_json(rows(-2, -5)))
        .with_priority(3)
        .mount(&server)
        .await;
    let profiles = observe_native_openrouter_profiles(&server.uri(), KEY, MODEL)
        .await
        .unwrap();
    assert_eq!(profiles.len(), 2);
    // The first endpoint is degraded now, the other is not.
    let provider = NativeOpenRouterProvider::connect_observed_endpoints(
        profiles.clone(),
        KEY,
        32,
        65536,
        None,
    )
    .await
    .unwrap();
    assert_eq!(provider.endpoint_failover().unwrap().current_endpoint(), 0);
    // Both are now.
    let error = match NativeOpenRouterProvider::connect_observed_endpoints(
        profiles.clone(),
        KEY,
        32,
        65536,
        None,
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("no endpoint can be used"),
    };
    let message = error.to_string();
    assert!(
        message.contains(
            "OpenRouter reports every endpoint of meta-llama/test-instruct that this Agent may \
             use as rate-limited or unavailable right now (finite/exact (FiniteProvider): \
             reported unavailable or degraded right now; other/exact (OtherProvider): reported \
             unavailable or degraded right now). This is a provider outage, not your setup: try \
             again later, or choose another model"
        ),
        "{message}"
    );
    // Endpoints of another model, or one endpoint twice, are refused.
    let mut twice = profiles.clone();
    twice[1] = twice[0].clone();
    let mut other_model = profiles;
    other_model[1].model = "meta-llama/other-instruct".into();
    for endpoints in [twice, other_model] {
        assert!(NativeOpenRouterProvider::connect_observed_endpoints(
            endpoints, KEY, 32, 65536, None
        )
        .await
        .is_err());
    }
}

/// After the endpoints failed one call, the error names each endpoint once
/// with its last failure, in the words its failure gave. Outages alone are
/// said as OpenRouter's outage, with what to do and the last `Retry-After`;
/// a changed contract is not an outage, and a last failure that is not the
/// endpoint's own ends the call as it is.
#[test]
fn an_exhausted_call_names_every_endpoint_once() {
    let endpoint = |tag: &str, provider: &str| NativeOpenRouterObservation {
        endpoint_tag: tag.into(),
        provider_name: provider.into(),
        ..legacy_observation()
    };
    let endpoints = [
        endpoint("near-ai/fp4", "Near AI"),
        endpoint("deepinfra/bf16", "DeepInfra"),
        endpoint("groq", "Groq"),
    ];
    let limited =
        |retry_after| failover::refusal(&endpoints[0], 429, UPSTREAM_LIMITED, retry_after);
    let busy = failover::refusal(&endpoints[2], 503, "", None);
    let timed_out = failover::transport(
        ProviderError::Network("timed out: no answer".into()),
        false,
        true,
    );
    let error = failover::exhausted(
        &endpoints,
        vec![
            (0, limited(None)),
            (1, timed_out),
            (2, busy),
            (0, limited(Some(5))),
        ],
    );
    let ProviderError::ApiError {
        status: 429,
        message,
        ..
    } = &error
    else {
        panic!("{error:?}");
    };
    assert_eq!(
        message,
        "OpenRouter reports every endpoint of meta-llama/test-instruct that this Agent may use \
         as rate-limited or unavailable right now (near-ai/fp4 (Near AI): rate-limited (HTTP \
         429); deepinfra/bf16 (DeepInfra): timed out; groq (Groq): unavailable (HTTP 503)). \
         This is a provider outage, not your setup: try again later, or choose another model; \
         nothing from them reached the Agent (Retry-After: 5 s)"
    );
    // An in-answer failure's quoted words never set its Retry-After.
    let quoted = failover::answer_failure(ProviderError::ApiError {
        provider: PROVIDER.into(),
        status: 503,
        message: "native OpenRouter: endpoint groq (Groq) reported an error in its answer: \
                  busy (Retry-After: 99 s); its accounting remains partial"
            .into(),
    });
    let message =
        failover::exhausted(&endpoints, vec![(0, limited(None)), (2, quoted)]).to_string();
    assert!(!message.contains("Retry-After"), "{message}");
    let changed = failover::unsent(
        invalid(format!("other/exact {}", failover::CONTRACT_CHANGED)),
        true,
        "its contract changed since Team & budget reviewed it",
    );
    let error = failover::exhausted(&endpoints, vec![(0, limited(None)), (1, changed)]);
    let message = error.to_string();
    assert!(
        matches!(error, ProviderError::InvalidRequest { .. })
            && message.contains(
                "no endpoint of meta-llama/test-instruct that this Agent may use could take the \
                 call (near-ai/fp4 (Near AI): rate-limited (HTTP 429); deepinfra/bf16 \
                 (DeepInfra): its contract changed since Team & budget reviewed it)"
            ),
        "{message}"
    );
    let account = failover::refusal(&endpoints[1], 429, ACCOUNT_LIMITED_BODY, Some(3));
    let error = failover::exhausted(&endpoints, vec![(0, limited(None)), (1, account)]);
    assert!(
        error
            .to_string()
            .starts_with("openrouter API error: 429 - OpenRouter is limiting this account's"),
        "{error}"
    );
}

fn legacy_observation() -> NativeOpenRouterObservation {
    NativeOpenRouterObservation {
        schema_version: 2,
        base_url: "https://openrouter.ai/api/v1".into(),
        model: MODEL.into(),
        endpoint_tag: "finite/exact".into(),
        provider_name: "FiniteProvider".into(),
        context_tokens: 2048,
        max_output_tokens: 128,
        prompt_price_per_million: "1".into(),
        completion_price_per_million: "2".into(),
        supported_parameters: vec!["max_tokens".into(), "tools".into()],
        non_reasoning_evidence: None,
        max_prompt_tokens: None,
        request_price: None,
        reasoning: None,
        unrequested_priced_features: vec![],
        observed_at_ms: 1,
        billing: "openrouter_credits".into(),
    }
}

/// A read of OpenRouter's catalog that OpenRouter refuses is said in plain
/// words, with its status, OpenRouter's own message and `Retry-After`; the
/// call is never sent, costs nothing, and is not moved to another endpoint,
/// since the catalog is every endpoint's.
#[tokio::test]
async fn a_refused_catalog_read_is_said_plainly_and_moves_nothing() {
    let server = endpoints_server(vec![
        endpoint_row("finite/exact", "FiniteProvider", 1, 0),
        endpoint_row("other/exact", "OtherProvider", 2, 0),
    ])
    .await;
    let provider = provider_over(&server).await;
    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "3")
                .set_body_string(ACCOUNT_LIMITED_BODY),
        )
        .mount(&server)
        .await;
    let failed = open_failure(&provider, ChatRequest::simple("one call")).await;
    let ProviderError::ApiError {
        status: 429,
        message,
        ..
    } = &failed.error
    else {
        panic!("{:?}", failed.error);
    };
    assert_eq!(
        message,
        "OpenRouter's model catalog could not be read (HTTP 429: OpenRouter could not verify \
         available credits for this request in time. Retry shortly.); nothing was sent to the \
         model (Retry-After: 3 s)"
    );
    assert!(failed.facts.cost_nothing);
    assert!(!failed.facts.endpoint_failure);
    assert!(pinned(&server).await.is_empty());
    // A body that is not OpenRouter's JSON (a gateway's text, an HTML page,
    // JSON cut short) is never quoted.
    assert_eq!(
        failover::catalog_refusal(502, "bad gateway"),
        "OpenRouter's model catalog could not be read (HTTP 502)"
    );
    assert_eq!(
        failover::catalog_refusal(502, r#"{"error":{"message":"Provider ret"#),
        "OpenRouter's model catalog could not be read (HTTP 502)"
    );
}
