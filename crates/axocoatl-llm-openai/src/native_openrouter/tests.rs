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
                    .execution_bounds(2048, 65536)
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
    NativeOpenRouterProvider::connect_observed(profile, KEY, 32, 65536)
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
        NativeOpenRouterProvider::connect_observed(profile, KEY, 32, 65536)
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
    assert_eq!(money::charge_bound(2048, 32, "0.135", "0.4").unwrap(), 290);
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
