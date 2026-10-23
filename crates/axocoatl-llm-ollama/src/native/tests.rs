use super::*;
use axocoatl_llm::ToolDefinition;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

fn config(server: &MockServer) -> NativeOllamaConfig {
    NativeOllamaConfig {
        base_url: server.uri(),
        model: "test-model".into(),
        context_tokens: 2048,
        max_output_tokens: 32,
        max_response_bytes: 64 * 1024,
    }
}
async fn profile(server: &MockServer, version: Value, status: Value, show: Value) {
    Mock::given(method("GET"))
        .and(path("/api/version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(version))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(status))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .respond_with(ResponseTemplate::new(200).set_body_json(show))
        .mount(server)
        .await;
}
fn show() -> Value {
    json!({"details":{"format":"gguf"}, "capabilities":["completion","tools","thinking"]})
}
async fn valid_profile(server: &MockServer) {
    profile(
        server,
        json!({"version":AUDITED_OLLAMA_VERSION}),
        json!({"cloud":{"disabled":true,"source":"env"}}),
        show(),
    )
    .await;
}
fn record(content: &str, done: bool) -> Value {
    let mut value =
        json!({"model":"test-model", "message":{"role":"assistant","content":content},"done":done});
    if done {
        value["done_reason"] = json!("stop");
    }
    value
}
fn counted(mut value: Value, input: Value, output: Value) -> Value {
    value["prompt_eval_count"] = input;
    value["eval_count"] = output;
    value
}
async fn response(server: &MockServer, records: &[Value]) {
    let body = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/x-ndjson"))
        .mount(server)
        .await;
}
fn tools_request() -> ChatRequest {
    let mut request = ChatRequest::simple("look up");
    request.tools.push(ToolDefinition {
        name: "lookup".into(),
        description: "read data".into(),
        parameters: json!({"type":"object"}),
        concurrency: Default::default(),
    });
    request
}

#[tokio::test]
async fn native_wire_preserves_explicit_limits_and_common_options() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    response(
        &server,
        &[counted(record("{\"answer\":42}", true), json!(9), json!(3))],
    )
    .await;
    let provider = NativeOllamaProvider::connect(config(&server))
        .await
        .unwrap();
    let mut request = ChatRequest::with_system("rules", "question");
    request.max_tokens = Some(16);
    request.temperature = Some(0.2);
    request.top_p = Some(0.8);
    request.stop_sequences = vec!["END".into()];
    request.response_format = Some(ResponseFormat::Json);
    request.provider_options = Some(json!({"reasoning_effort":"none"}));
    let bound = provider.execution_bounds(&request).unwrap();
    assert_eq!(bound.token_limit, 2 * (2048 + 16));
    assert_eq!(bound.cost_microunits, 0);
    let outcome = provider.chat_with_accounting(request).await;
    assert_eq!(outcome.response.unwrap().content, "{\"answer\":42}");
    assert_eq!(outcome.usage.usage, TokenUsageStats::new(9, 3));
    assert!(!outcome.usage.complete);
    let requests = server.received_requests().await.unwrap();
    let inference = requests
        .iter()
        .find(|r| r.url.path() == "/api/chat")
        .unwrap();
    let body: Value = serde_json::from_slice(&inference.body).unwrap();
    assert_eq!(body["options"]["num_ctx"], 2048);
    assert_eq!(body["options"]["num_predict"], 16);
    assert_eq!(body["options"]["stop"], json!(["END"]));
    assert_eq!(body["truncate"], false);
    assert_eq!(body["shift"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body["format"], "json");
    assert_eq!(body["think"], false);
    assert_eq!(body["messages"][0]["content"], "rules");
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path() == "/api/version")
            .count(),
        2
    );
}

fn image_request() -> ChatRequest {
    let mut request = ChatRequest::simple("inspect the retained screenshot");
    request.messages[0].content = MessageContent::Parts(vec![
        ContentPart::Text("inspect the retained screenshot".into()),
        ContentPart::Image {
            url: "data:image/png;base64,iVBORw0KGgo=".into(),
            detail: ImageDetail::Auto,
        },
    ]);
    request.max_tokens = Some(16);
    request
}

#[tokio::test]
async fn native_image_wire_uses_verified_vision_and_the_same_full_context_reservation() {
    let server = MockServer::start().await;
    let mut vision = show();
    vision["capabilities"]
        .as_array_mut()
        .unwrap()
        .push(json!("vision"));
    profile(
        &server,
        json!({"version":AUDITED_OLLAMA_VERSION}),
        json!({"cloud":{"disabled":true}}),
        vision,
    )
    .await;
    response(
        &server,
        &[counted(
            record("screenshot observed", true),
            json!(1024),
            json!(3),
        )],
    )
    .await;
    let provider = NativeOllamaProvider::connect(config(&server))
        .await
        .unwrap();
    assert!(provider.capabilities().vision);
    let request = image_request();
    let bounds = provider.execution_bounds(&request).unwrap();
    assert_eq!(bounds.token_limit, 2048 + 16);
    assert_eq!(bounds.cost_microunits, 0);
    let outcome = provider.chat_with_accounting(request).await;
    assert_eq!(outcome.response.unwrap().content, "screenshot observed");
    assert!(outcome.usage.complete);
    assert_eq!(outcome.usage.usage, TokenUsageStats::new(1024, 3));
    let requests = server.received_requests().await.unwrap();
    let inference = requests
        .iter()
        .find(|request| request.url.path() == "/api/chat")
        .unwrap();
    let body: Value = serde_json::from_slice(&inference.body).unwrap();
    assert_eq!(
        body["messages"][0]["content"],
        "inspect the retained screenshot"
    );
    assert_eq!(body["messages"][0]["images"], json!(["iVBORw0KGgo="]));
    assert_eq!(body["options"]["num_ctx"], 2048);
    assert_eq!(body["options"]["num_predict"], 16);
    assert_eq!(body["truncate"], false);
    assert_eq!(body["shift"], false);
}

#[tokio::test]
async fn native_images_refuse_missing_or_changed_vision_and_external_or_invalid_bodies() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    let text_provider = NativeOllamaProvider::connect(config(&server))
        .await
        .unwrap();
    assert!(text_provider.execution_bounds(&image_request()).is_none());
    assert!(text_provider.chat(image_request()).await.is_err());
    server.reset().await;
    let mut vision = show();
    vision["capabilities"]
        .as_array_mut()
        .unwrap()
        .push(json!("vision"));
    profile(
        &server,
        json!({"version":AUDITED_OLLAMA_VERSION}),
        json!({"cloud":{"disabled":true}}),
        vision,
    )
    .await;
    let provider = NativeOllamaProvider::connect(config(&server))
        .await
        .unwrap();
    for url in [
        "https://never-fetch.invalid/image.png",
        "data:image/png;base64,%%%",
        "data:text/plain;base64,aGk=",
        "data:image/png;base64,",
    ] {
        let mut request = image_request();
        request.messages[0].content = MessageContent::Parts(vec![ContentPart::Image {
            url: url.into(),
            detail: ImageDetail::Auto,
        }]);
        assert!(provider.execution_bounds(&request).is_none(), "{url}");
        assert!(provider.chat(request).await.is_err());
    }
    server.reset().await;
    valid_profile(&server).await;
    assert!(provider.chat(image_request()).await.is_err());
    assert!(!server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|request| request.url.path() == "/api/chat"));
}

#[tokio::test]
async fn profile_refuses_unknown_version_missing_status_cloud_and_unsupported_models() {
    let audited = json!({"version":AUDITED_OLLAMA_VERSION});
    let mut cases = Vec::new();
    for (version, reason) in [
        ("0.20.6", "no longer supported"),
        ("0.40.5", "older than native Sessions support"),
        ("1.0.0", "new major release"),
        ("0.40.7-rc0", "pre-release"),
        ("0.0.0", "development build"),
    ] {
        cases.push((
            json!({"version":version}),
            json!({"cloud":{"disabled":true}}),
            show(),
            reason,
        ));
    }
    cases.extend([
        (audited.clone(), json!({}), show(), "cloud-disabled"),
        (
            audited.clone(),
            json!({"cloud":{"disabled":false}}),
            show(),
            "cloud-disabled",
        ),
        (
            audited.clone(),
            json!({"cloud":{"disabled":"true"}}),
            show(),
            "cloud-disabled",
        ),
        (
            audited.clone(),
            json!({"cloud":{"disabled":true}}),
            json!({"details":{"format":"safetensors"},"capabilities":["completion"]}),
            "a safetensors model",
        ),
        (
            audited.clone(),
            json!({"cloud":{"disabled":true}}),
            json!({"details":{"format":"gguf","runner":"mlx"},"capabilities":["completion"]}),
            "MLX runner",
        ),
        (
            audited.clone(),
            json!({"cloud":{"disabled":true}}),
            json!({"details":{"format":"gguf"},"capabilities":["embedding"]}),
            "does not report text completion",
        ),
        (
            audited.clone(),
            json!({"cloud":{"disabled":true}}),
            json!({"details":{"format":"gguf"},"capabilities":["completion"],"remote_host":"https://ollama.com"}),
            "cloud model",
        ),
    ]);
    for (version, status, show, reason) in cases {
        let server = MockServer::start().await;
        profile(&server, version.clone(), status, show).await;
        let refusal = NativeOllamaProvider::connect(config(&server))
            .await
            .err()
            .unwrap_or_else(|| panic!("{version} must be refused for {reason}"))
            .to_string();
        assert!(refusal.contains(reason), "{reason}: {refusal}");
        assert!(!server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path() == "/api/chat"));
    }
}

/// Both runners that report a GGUF model are accepted: Ollama 0.40.6 runs
/// every GGUF model in llama-server, and a legacy manifest says `ggml`.
#[tokio::test]
async fn gguf_models_of_either_manifest_runner_are_accepted() {
    for runner in [json!("ggml"), json!("llamacpp"), Value::Null] {
        let server = MockServer::start().await;
        let mut shown = show();
        if !runner.is_null() {
            shown["details"]["runner"] = runner.clone();
        }
        profile(
            &server,
            json!({"version":"0.40.7"}),
            json!({"cloud":{"disabled":true}}),
            shown,
        )
        .await;
        assert!(
            NativeOllamaProvider::connect(config(&server)).await.is_ok(),
            "{runner}"
        );
    }
}

#[tokio::test]
async fn profile_revalidation_refuses_changed_server_before_inference() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    let provider = NativeOllamaProvider::connect(config(&server))
        .await
        .unwrap();
    server.reset().await;
    profile(
        &server,
        json!({"version":AUDITED_OLLAMA_VERSION}),
        json!({"cloud":{"disabled":false}}),
        show(),
    )
    .await;
    assert!(provider.chat(ChatRequest::simple("secret")).await.is_err());
    assert!(!server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|r| r.url.path() == "/api/chat"));
}

#[tokio::test]
async fn local_profile_rejects_remote_endpoints_and_unbounded_configuration() {
    let server = MockServer::start().await;
    for field in 0..6 {
        let mut config = config(&server);
        match field {
            0 => config.base_url = "https://remote.example".into(),
            1 => config.context_tokens = 0,
            2 => config.max_output_tokens = 0,
            3 => config.max_output_tokens = 20481,
            4 => config.max_response_bytes = MAX_RESPONSE_BYTES + 1,
            _ => config.base_url = "http://key@localhost:1".into(),
        }
        assert!(NativeOllamaProvider::connect(config).await.is_err());
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn text_format_is_plain_and_requests_cannot_expand_the_profile() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    let provider = NativeOllamaProvider::connect(config(&server))
        .await
        .unwrap();
    let mut plain = ChatRequest::simple("hi");
    plain.response_format = Some(ResponseFormat::Text);
    assert_eq!(provider.execution_bounds(&plain).unwrap().token_limit, 2080);
    assert!(provider
        .wire_request(&plain)
        .unwrap()
        .get("format")
        .is_none());
    for kind in 0..4 {
        let mut request = ChatRequest::simple("hi");
        match kind {
            0 => request.max_tokens = Some(33),
            1 => request.model_override = Some("other".into()),
            2 => request.provider_options = Some(json!({"num_ctx":9000})),
            _ => {
                request.provider_options =
                    Some(json!({"reasoning_effort":"none","unsupported":true}))
            }
        }
        assert!(provider.execution_bounds(&request).is_none());
        assert!(provider.chat(request).await.is_err());
    }
    assert!(!server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|r| r.url.path() == "/api/chat"));
}

#[tokio::test]
async fn zero_missing_and_partial_counters_are_distinct() {
    for (input, output, expected, complete) in [
        (Some(0), Some(0), TokenUsageStats::new(0, 0), true),
        (Some(7), Some(2), TokenUsageStats::new(7, 2), true),
        (Some(7), None, TokenUsageStats::new(7, 0), false),
        (None, Some(2), TokenUsageStats::new(0, 2), false),
        (None, None, TokenUsageStats::new(0, 0), false),
    ] {
        let server = MockServer::start().await;
        valid_profile(&server).await;
        let mut final_record = record("answer", true);
        if let Some(n) = input {
            final_record["prompt_eval_count"] = json!(n);
        }
        if let Some(n) = output {
            final_record["eval_count"] = json!(n);
        }
        response(&server, &[final_record]).await;
        let provider = NativeOllamaProvider::connect(config(&server))
            .await
            .unwrap();
        let outcome = provider
            .chat_with_accounting(ChatRequest::simple("hi"))
            .await;
        assert_eq!(outcome.response.unwrap().usage, expected);
        assert_eq!(outcome.usage.usage, expected);
        assert_eq!(outcome.usage.complete, complete);
    }
}

#[tokio::test]
async fn malformed_error_and_early_eof_retain_observed_usage() {
    for kind in 0..4 {
        let server = MockServer::start().await;
        valid_profile(&server).await;
        let mut row = counted(record("partial", kind != 0), json!(7), json!(2));
        match kind {
            1 => row["error"] = json!("failed"),
            2 => row["message"] = json!(false),
            3 => row["eval_count"] = json!("bad"),
            _ => {}
        }
        response(&server, &[row]).await;
        let provider = NativeOllamaProvider::connect(config(&server))
            .await
            .unwrap();
        let outcome = provider
            .chat_with_accounting(ChatRequest::simple("hi"))
            .await;
        assert!(outcome.response.is_err());
        assert!(!outcome.usage.complete);
        assert_eq!(outcome.usage.usage.input_tokens, 7);
        assert_eq!(
            outcome.usage.usage.output_tokens,
            if kind == 3 { 0 } else { 2 }
        );
    }
}

#[tokio::test]
async fn decreasing_or_overbound_counts_fail_without_discarding_evidence() {
    for overrun in [false, true] {
        let server = MockServer::start().await;
        valid_profile(&server).await;
        let rows = if overrun {
            vec![counted(record("ok", true), json!(9000), json!(2))]
        } else {
            vec![
                counted(record("part", false), json!(7), json!(3)),
                counted(record("end", true), json!(7), json!(2)),
            ]
        };
        response(&server, &rows).await;
        let provider = NativeOllamaProvider::connect(config(&server))
            .await
            .unwrap();
        let outcome = provider
            .chat_with_accounting(ChatRequest::simple("hi"))
            .await;
        assert!(outcome.response.is_err());
        assert!(!outcome.usage.complete);
        assert_eq!(
            outcome.usage.usage.input_tokens,
            if overrun { 9000 } else { 7 }
        );
    }
}

#[tokio::test]
async fn terminal_protocol_rejects_duplicates_trailing_malformed_data_and_wrong_model() {
    for kind in 0..4 {
        let server = MockServer::start().await;
        valid_profile(&server).await;
        let mut last = counted(record("answer", true), json!(7), json!(2));
        let body = match kind {
            0 => format!("{last}\n{last}\n"),
            1 => format!("{last}\nnot-json\n"),
            2 => {
                last["model"] = json!("other");
                format!("{last}\n")
            }
            _ => {
                last["done_reason"] = json!("load");
                format!("{last}\n")
            }
        };
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/x-ndjson"))
            .mount(&server)
            .await;
        let provider = NativeOllamaProvider::connect(config(&server))
            .await
            .unwrap();
        let outcome = provider
            .chat_with_accounting(ChatRequest::simple("hi"))
            .await;
        assert!(outcome.response.is_err());
        assert!(!outcome.usage.complete);
        assert_eq!(outcome.usage.usage.input_tokens, 7);
    }
}

#[tokio::test]
async fn native_tool_ids_arguments_results_and_thinking_roundtrip() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    let mut row = record("", true);
    row["message"]["thinking"] = json!("check the source");
    row["message"]["tool_calls"] =
        json!([{"id":"native_123","function":{"index":0,"name":"lookup","arguments":{"key":"x"}}}]);
    response(&server, &[counted(row, json!(8), json!(4))]).await;
    let provider = NativeOllamaProvider::connect(config(&server))
        .await
        .unwrap();
    let mut request = tools_request();
    let result = provider.chat(request.clone()).await.unwrap();
    assert_eq!(result.finish_reason, FinishReason::ToolUse);
    assert_eq!(result.tool_calls[0].id, "native_123");
    assert_eq!(result.tool_calls[0].arguments, json!({"key":"x"}));
    let mut assistant = ChatMessage::assistant("");
    assistant.tool_calls = result.tool_calls;
    request.messages.push(assistant);
    request.messages.push(ChatMessage::tool_result(
        "{\"value\":5}",
        "lookup",
        "native_123",
    ));
    let body = provider.wire_request(&request).unwrap();
    assert_eq!(body["messages"][1]["thinking"], "check the source");
    assert_eq!(
        body["messages"][1]["tool_calls"][0]["function"]["arguments"],
        json!({"key":"x"})
    );
    assert_eq!(body["messages"][2]["tool_call_id"], "native_123");
    assert_eq!(body["messages"][2]["tool_name"], "lookup");
    request.messages.last_mut().unwrap().tool_call_id = Some("wrong".into());
    assert!(provider.validate_request(&request).is_err());
}

#[tokio::test]
async fn malformed_unreplayable_duplicate_or_unidentified_tools_are_not_actionable() {
    for calls in [
        json!([{"function":{"name":"lookup","arguments":{}}}]),
        json!([{"id":"x","function":{"name":"not/declared","arguments":{}}}]),
        json!([{"id":"x","function":{"name":"lookup","arguments":"{}"}}]),
        json!([{"id":"x","function":{"name":"lookup","arguments":{}}},{"id":"x","function":{"name":"lookup","arguments":{}}}]),
    ] {
        let server = MockServer::start().await;
        valid_profile(&server).await;
        let mut row = record("", true);
        row["message"]["tool_calls"] = calls;
        response(&server, &[row]).await;
        let provider = NativeOllamaProvider::connect(config(&server))
            .await
            .unwrap();
        assert!(provider.chat(tools_request()).await.is_err());
    }
}

/// Small models call tools nobody declared next to a finished answer. The
/// call is returned for the caller to answer with a tool error; it is not a
/// protocol failure that discards the response.
#[tokio::test]
async fn a_well_formed_call_to_an_undeclared_tool_is_returned_with_the_answer() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    let mut call = record("", false);
    call["message"]["tool_calls"] =
        json!([{"id":"call_1","function":{"index":0,"name":"report","arguments":{"issue":"x"}}}]);
    response(
        &server,
        &[
            record("The bug is in manifest.js.", false),
            call,
            counted(record("", true), json!(11), json!(4)),
        ],
    )
    .await;
    let provider = NativeOllamaProvider::connect(config(&server))
        .await
        .unwrap();
    let outcome = provider.chat_with_accounting(tools_request()).await;
    let response = outcome.response.unwrap();
    assert_eq!(response.content, "The bug is in manifest.js.");
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].name, "report");
    assert!(outcome.usage.complete);
}

/// A malformed call is still refused, but only after the response's `done`
/// record: the usage the server reported for it comes first, complete, so
/// the call can be settled instead of losing its accounting.
#[tokio::test]
async fn a_refused_tool_call_is_reported_after_the_terminal_usage() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    let mut call = record("", false);
    call["message"]["tool_calls"] =
        json!([{"id":"call_1","function":{"index":0,"name":"lookup","arguments":"[]"}}]);
    response(
        &server,
        &[call, counted(record("", true), json!(11), json!(4))],
    )
    .await;
    let provider = NativeOllamaProvider::connect(config(&server))
        .await
        .unwrap();
    let mut stream = provider.chat_stream(tools_request()).await.unwrap();
    let mut last_usage = None;
    let error = loop {
        match stream
            .next()
            .await
            .expect("the stream ends with its refusal")
        {
            Ok(StreamEvent::UsageObservation(usage)) => last_usage = Some(usage),
            Ok(StreamEvent::ToolCallDelta { .. } | StreamEvent::Done { .. }) => {
                panic!("a refused call is never released")
            }
            Ok(_) => {}
            Err(error) => break error,
        }
    };
    assert!(
        matches!(&error, ProviderError::RefusedResponse { message, .. } if message.contains("non-object")),
        "{error:?}"
    );
    let usage = last_usage.expect("terminal usage precedes the refusal");
    assert!(usage.complete);
    assert_eq!(usage.usage, TokenUsageStats::new(11, 4));
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn byte_limit_and_reasoning_only_terminal_prevent_success() {
    for oversized in [false, true] {
        let server = MockServer::start().await;
        valid_profile(&server).await;
        let mut row = record("", true);
        row["message"]["thinking"] = json!(if oversized {
            "x".repeat(4096)
        } else {
            "thinking".into()
        });
        response(&server, &[row]).await;
        let mut settings = config(&server);
        settings.max_response_bytes = 2048;
        let provider = NativeOllamaProvider::connect(settings).await.unwrap();
        assert!(provider.chat(ChatRequest::simple("hi")).await.is_err());
    }
}

#[tokio::test]
async fn actual_ndjson_stream_emits_reasoning_text_and_terminal_usage() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    let mut first = record("", false);
    first["message"]["thinking"] = json!("consider");
    response(
        &server,
        &[
            first,
            record("hello ", false),
            counted(record("world", true), json!(5), json!(3)),
        ],
    )
    .await;
    let provider = NativeOllamaProvider::connect(config(&server))
        .await
        .unwrap();
    let mut stream = provider
        .chat_stream(ChatRequest::simple("hi"))
        .await
        .unwrap();
    assert!(
        matches!(stream.next().await.unwrap().unwrap(),StreamEvent::ReasoningDelta{delta} if delta=="consider")
    );
    assert!(
        matches!(stream.next().await.unwrap().unwrap(),StreamEvent::TextDelta{delta} if delta=="hello ")
    );
    let mut done = false;
    let mut complete = false;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            StreamEvent::UsageObservation(usage) => complete = usage.complete,
            StreamEvent::Done { .. } => done = true,
            _ => {}
        }
    }
    assert!(done && complete);
}

#[tokio::test]
async fn terminal_usage_survives_payload_capacity_failure() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    response(
        &server,
        &[
            record(&"p".repeat(1000), false),
            counted(record(&"x".repeat(600), true), json!(17), json!(9)),
        ],
    )
    .await;
    let mut settings = config(&server);
    settings.max_response_bytes = 4096;
    let provider = NativeOllamaProvider::connect(settings).await.unwrap();
    let result = provider
        .chat_with_accounting(ChatRequest::simple("hi"))
        .await;
    assert!(result.response.is_err());
    assert!(!result.usage.complete);
    assert_eq!(result.usage.usage, TokenUsageStats::new(17, 9));
}

#[test]
fn repeated_thinking_metadata_is_preflighted_before_amplification() {
    let mut state = NativeResponse::new("test-model".into(), tools_request(), 64 * 1024, 2080);
    state.terminal = Some(FinishReason::ToolUse);
    state.thinking = "x".repeat(32 * 1024);
    state.calls = (0..128)
        .map(|n| ToolCall {
            id: format!("call_{n}"),
            name: "lookup".into(),
            arguments: json!({}),
            provider_metadata: provider_tool_metadata(PROVIDER),
        })
        .collect();
    assert!(state.finish().is_err());
    assert_eq!(state.emitted, 0);
}

#[tokio::test]
async fn mixed_dimension_counter_regression_retains_both_highwater_counts() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    response(
        &server,
        &[
            counted(record("a", false), json!(10), json!(2)),
            counted(record("b", true), json!(7), json!(4)),
        ],
    )
    .await;
    let provider = NativeOllamaProvider::connect(config(&server))
        .await
        .unwrap();
    let outcome = provider
        .chat_with_accounting(ChatRequest::simple("hi"))
        .await;
    assert!(outcome.response.is_err());
    assert!(!outcome.usage.complete);
    assert_eq!(outcome.usage.usage, TokenUsageStats::new(10, 4));
}

#[tokio::test]
async fn an_error_inside_an_accepted_response_is_an_incomplete_stream() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    // qwen3-coder can emit tool-call text Ollama cannot parse; Ollama then
    // ends the accepted response with an error record instead of `done`.
    let failure = json!({
        "error": "XML syntax error on line 42: element <parameter> closed by </function>"
    });
    response(&server, &[record("partial answer", false), failure]).await;
    let provider = NativeOllamaProvider::connect(config(&server))
        .await
        .unwrap();
    let mut stream = provider
        .chat_stream(ChatRequest::simple("hi"))
        .await
        .unwrap();
    let mut last = None;
    while let Some(event) = stream.next().await {
        last = Some(event);
    }
    match last {
        Some(Err(ProviderError::IncompleteStream { message, .. })) => {
            assert!(message.contains("XML syntax error"), "{message}")
        }
        other => panic!("expected an incomplete stream, got {other:?}"),
    }
}

#[tokio::test]
async fn a_rejected_request_is_an_api_error_and_never_retried() {
    for (status, body) in [
        (
            400,
            json!({"error": "input length exceeds context"}).to_string(),
        ),
        (500, String::new()),
    ] {
        let server = MockServer::start().await;
        valid_profile(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(status).set_body_raw(body, "application/x-ndjson"))
            .mount(&server)
            .await;
        let provider = NativeOllamaProvider::connect(config(&server))
            .await
            .unwrap();
        let outcome = provider
            .chat_with_accounting(ChatRequest::simple("hi"))
            .await;
        assert!(
            matches!(outcome.response, Err(ProviderError::ApiError { status: s, .. }) if s == status),
            "{status}: {:?}",
            outcome.response
        );
    }
}

/// A refused request states its status and the server's `Retry-After`, so a
/// Session's retry policy can wait for it; the provider itself sends once.
#[tokio::test]
async fn a_busy_server_states_its_status_and_retry_after() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(503)
                .insert_header("Retry-After", "4")
                .set_body_raw(
                    json!({"error": "server busy"}).to_string(),
                    "application/json",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;
    let provider = NativeOllamaProvider::connect(config(&server))
        .await
        .unwrap();
    let outcome = provider
        .chat_with_accounting(ChatRequest::simple("hi"))
        .await;
    match outcome.response {
        Err(ProviderError::ApiError {
            status: 503,
            message,
            ..
        }) => assert!(message.ends_with(" (Retry-After: 4 s)"), "{message}"),
        other => panic!("expected a 503, got {other:?}"),
    }
}

#[test]
fn retry_after_reads_seconds_and_http_dates() {
    let header = |value: &str| {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_str(value).unwrap(),
        );
        headers
    };
    assert_eq!(retry_after_secs(&header("12")), Some(12));
    assert_eq!(
        retry_after_secs(&header("Sun, 06 Nov 1994 08:49:37 GMT")),
        Some(0)
    );
    let later =
        httpdate::fmt_http_date(std::time::SystemTime::now() + std::time::Duration::from_secs(120));
    let wait = retry_after_secs(&header(&later)).unwrap();
    assert!((118..=121).contains(&wait), "{wait}");
    assert_eq!(retry_after_secs(&header("soon")), None);
    assert_eq!(retry_after_secs(&reqwest::header::HeaderMap::new()), None);
    assert!(matches!(
        with_retry_after(
            ProviderError::Network("x".into()),
            Some(3)
        ),
        ProviderError::Network(message) if message == "x"
    ));
}

/// A metadata request Ollama refuses keeps the status and Ollama's own
/// message: a model it does not have is `ModelNotFound` with the `ollama
/// pull` that fixes it, a model it could not load is a 500 with Ollama's
/// reason, and a busy server is a 503 with its `Retry-After`, which a
/// Session's retry policy honors. Nothing becomes one generic sentence.
#[tokio::test]
async fn refused_metadata_requests_keep_the_status_and_ollamas_message() {
    // Not pulled: `POST /api/show` answers 404.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/version"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"version":AUDITED_OLLAMA_VERSION})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"cloud":{"disabled":true}})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/show"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"error":"model 'test-model' not found"})),
        )
        .mount(&server)
        .await;
    let error = NativeOllamaProvider::connect(config(&server))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&error, ProviderError::ModelNotFound { model, .. } if model == "test-model"),
        "{error}"
    );
    assert_eq!(
        describe_native_ollama_error(&error),
        "Model not found: test-model on ollama. Ollama does not have test-model: run `ollama \
         pull test-model`"
    );
    let shown = show_native_ollama_model(&server.uri(), "test-model")
        .await
        .unwrap_err();
    assert!(
        matches!(shown, ProviderError::ModelNotFound { .. }),
        "{shown}"
    );

    // Pulled, but too large to load: the load answers 500 with Ollama's
    // reason.
    let server = MockServer::start().await;
    valid_profile(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[{
            "name":"test-model:latest","model":"test-model:latest","digest":"a".repeat(64)}]})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .respond_with(
            ResponseTemplate::new(500)
                .set_body_json(json!({"error":"unable to load full model on GPU"})),
        )
        .mount(&server)
        .await;
    let error = observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ProviderError::ApiError { status: 500, message, .. }
            if message == "unable to load full model on GPU"),
        "{error}"
    );
    assert!(native_ollama_error_hint(&error)
        .unwrap()
        .contains("more memory"));

    // Busy: a 503 keeps its status, Ollama's text and its Retry-After.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/version"))
        .respond_with(
            ResponseTemplate::new(503)
                .insert_header("Retry-After", "2")
                .set_body_json(json!({"error":"server busy, please try again.  maximum pending requests exceeded"})),
        )
        .mount(&server)
        .await;
    let error = observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .unwrap_err();
    match error {
        ProviderError::ApiError {
            status: 503,
            message,
            ..
        } => assert_eq!(
            message,
            "server busy, please try again. maximum pending requests exceeded (Retry-After: 2 s)"
        ),
        other => panic!("expected a 503, got {other}"),
    }
    // The server check still reads a refused version request as not
    // reported, not as an unreachable server.
    let check = check_native_ollama_server(&server.uri()).await.unwrap();
    assert_eq!(check.version, None);
    assert_eq!(check.cloud, OllamaCloudMode::Unreported);
}

/// What qwen3-coder:30b on Ollama 0.40.8 wrote for a workbench turn: a call
/// without its opening `<tool_call>` token, which Ollama returned as content
/// ending in `stop`, with no structured call.
const TEXT_CALL: &str = "I'll help you fix the paging bug in `src/paginate.js`. Let me first locate this file and examine its contents to understand the issue.\n\n<function=glob>\n<parameter=pattern>\nsrc/paginate.js\n</parameter>\n</function>\n</tool_call>";

/// The prose before that call, which stays the turn's text.
fn prose() -> &'static str {
    &TEXT_CALL[..TEXT_CALL.find("<function=").unwrap()]
}

/// The same turn streamed from a real Ollama 0.40.8, one record per token.
const TEXT_CALL_STREAM: &str =
    include_str!("fixtures/ollama-0.40.8-qwen3-coder-30b-text-call.jsonl");

fn workbench_config(server: &MockServer) -> NativeOllamaConfig {
    NativeOllamaConfig {
        context_tokens: 32_768,
        max_output_tokens: 256,
        ..config(server)
    }
}

fn workbench_request() -> ChatRequest {
    let mut request = ChatRequest::simple("Fix the paging bug in src/paginate.js");
    for (name, properties) in [
        ("glob", json!({"pattern":{"type":"string"}})),
        (
            "read_file",
            json!({"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}}),
        ),
        ("bash", json!({"command":{"type":"string"}})),
    ] {
        request.tools.push(ToolDefinition {
            name: name.into(),
            description: String::new(),
            parameters: json!({"type":"object","properties":properties}),
            concurrency: Default::default(),
        });
    }
    request
}

fn text_of(events: &[StreamEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::TextDelta { delta } => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_tool_call_written_as_text_without_its_opening_tag_is_recovered() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    response(
        &server,
        &[
            record(TEXT_CALL, false),
            counted(record("", true), json!(2054), json!(55)),
        ],
    )
    .await;
    let provider = NativeOllamaProvider::connect(workbench_config(&server))
        .await
        .unwrap();
    let outcome = provider.chat_with_accounting(workbench_request()).await;
    let response = outcome.response.unwrap();
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
    assert_eq!(response.tool_calls.len(), 1);
    let call = &response.tool_calls[0];
    assert_eq!(call.name, "glob");
    assert_eq!(call.arguments, json!({"pattern":"src/paginate.js"}));
    assert!(!call.id.is_empty());
    assert_eq!(
        call.provider_metadata.get(TOOL_METADATA_PROVIDER_ID),
        Some(&PROVIDER.to_string())
    );
    assert_eq!(response.content, prose());
    assert!(!response.content.contains("<function="));
    assert!(outcome.usage.complete);
    assert_eq!(outcome.usage.usage, TokenUsageStats::new(2054, 55));
}

#[tokio::test]
async fn a_text_call_split_across_records_is_never_half_shown() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    let split = TEXT_CALL.find("function=glob>").unwrap();
    response(
        &server,
        &[
            record(&TEXT_CALL[..split], false),
            record(&TEXT_CALL[split..], false),
            counted(record("", true), json!(2054), json!(55)),
        ],
    )
    .await;
    let provider = NativeOllamaProvider::connect(workbench_config(&server))
        .await
        .unwrap();
    let mut stream = provider.chat_stream(workbench_request()).await.unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.unwrap());
    }
    for event in &events {
        if let StreamEvent::TextDelta { delta } = event {
            assert!(!delta.contains('<'), "{delta:?}");
        }
    }
    assert_eq!(text_of(&events), prose());
    let call = events
        .iter()
        .position(|event| matches!(event, StreamEvent::ToolCallDelta { .. }))
        .unwrap();
    assert!(events[call..]
        .iter()
        .all(|event| !matches!(event, StreamEvent::TextDelta { .. })));
    assert!(
        matches!(&events[call..], [
            StreamEvent::ToolCallDelta { name: Some(name), args_delta, .. },
            StreamEvent::ToolCallMetadata { .. },
            StreamEvent::UsageObservation(usage),
            StreamEvent::Done { finish_reason: FinishReason::ToolUse },
        ] if name == "glob" && args_delta == r#"{"pattern":"src/paginate.js"}"# && usage.complete),
        "{events:?}"
    );
}

/// Whatever record boundaries the call arrives in, the text shown is only
/// the prose before it, and the call is recovered once.
#[test]
fn a_text_call_split_anywhere_is_recovered_without_showing_its_markup() {
    let mut splits: Vec<Vec<&str>> = (1..TEXT_CALL.len())
        .map(|at| vec![&TEXT_CALL[..at], &TEXT_CALL[at..]])
        .collect();
    splits.push((0..TEXT_CALL.len()).map(|at| &TEXT_CALL[at..=at]).collect());
    for pieces in splits {
        let mut state =
            NativeResponse::new("test-model".into(), workbench_request(), 64 * 1024, 40_000);
        let mut events = Vec::new();
        for piece in &pieces {
            events.extend(state.record(&record(piece, false), 200).unwrap());
        }
        events.extend(state.record(&record("", true), 200).unwrap());
        events.extend(state.finish().unwrap());
        for event in &events {
            if let StreamEvent::TextDelta { delta } = event {
                assert!(!delta.contains('<'), "{pieces:?}: {delta:?}");
            }
        }
        assert_eq!(text_of(&events), prose(), "{pieces:?}");
        assert_eq!(state.calls.len(), 1, "{pieces:?}");
        assert!(matches!(
            events.last(),
            Some(StreamEvent::Done {
                finish_reason: FinishReason::ToolUse
            })
        ));
    }
}

#[tokio::test]
async fn a_real_ollama_stream_of_a_text_call_runs_the_call() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(TEXT_CALL_STREAM, "application/x-ndjson"),
        )
        .mount(&server)
        .await;
    let settings = NativeOllamaConfig {
        model: "qwen3-coder:30b".into(),
        ..workbench_config(&server)
    };
    let provider = NativeOllamaProvider::connect(settings).await.unwrap();
    let mut stream = provider.chat_stream(workbench_request()).await.unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.unwrap());
    }
    for event in &events {
        if let StreamEvent::TextDelta { delta } = event {
            assert!(!delta.contains('<'), "{delta:?}");
        }
    }
    assert_eq!(
        text_of(&events),
        "I'll help you fix the paging bug in `src/paginate.js`. Let's start by examining the \
         file to understand the current implementation and identify the bug.\n\n"
    );
    let calls: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolCallDelta {
                name: Some(name),
                args_delta,
                ..
            } => Some((name.as_str(), args_delta.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(calls, [("read_file", r#"{"path":"src/paginate.js"}"#)]);
    assert!(matches!(
        &events[events.len() - 2..],
        [
            StreamEvent::UsageObservation(usage),
            StreamEvent::Done { finish_reason: FinishReason::ToolUse },
        ] if usage.complete && usage.usage == TokenUsageStats::new(2054, 56)
    ));
}

/// Nothing but a complete markup call to an offered tool, in a plain-text
/// request, is ever read as a call: these answers stay what the model wrote.
#[tokio::test]
async fn text_that_is_not_a_recoverable_call_stays_the_answer() {
    let quoted_call = r#"{"note":"<function=glob>\n<parameter=pattern>\nsrc/paginate.js\n</parameter>\n</function>"}"#;
    let mut not_offered = workbench_request();
    not_offered.tools.retain(|tool| tool.name != "glob");
    let mut json = workbench_request();
    json.response_format = Some(ResponseFormat::Json);
    for (content, request) in [
        (TEXT_CALL, not_offered),
        (
            "Qwen3-Coder writes <function=glob> when it calls a tool as text.",
            workbench_request(),
        ),
        (TEXT_CALL, ChatRequest::simple("Fix the paging bug")),
        (quoted_call, json),
        (
            r#"Run {"name":"bash","arguments":{"command":"rm -rf build"}} to clean."#,
            workbench_request(),
        ),
    ] {
        let server = MockServer::start().await;
        valid_profile(&server).await;
        response(
            &server,
            &[
                record(content, false),
                counted(record("", true), json!(2054), json!(55)),
            ],
        )
        .await;
        let provider = NativeOllamaProvider::connect(workbench_config(&server))
            .await
            .unwrap();
        let response = provider.chat(request).await.unwrap();
        assert_eq!(response.finish_reason, FinishReason::Stop, "{content}");
        assert!(response.tool_calls.is_empty(), "{content}");
        assert_eq!(response.content, content);
    }
}

#[tokio::test]
async fn a_structured_call_wins_over_markup_in_the_text() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    let mut done = counted(record("", true), json!(2054), json!(55));
    done["message"]["tool_calls"] = json!([{"id":"call_native","function":{"index":0,
        "name":"read_file","arguments":{"path":"src/paginate.js"}}}]);
    response(&server, &[record(TEXT_CALL, false), done]).await;
    let provider = NativeOllamaProvider::connect(workbench_config(&server))
        .await
        .unwrap();
    let response = provider.chat(workbench_request()).await.unwrap();
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].id, "call_native");
    assert_eq!(response.tool_calls[0].name, "read_file");
    assert_eq!(response.content, TEXT_CALL);
}

/// A Session's history keeps one id per call over all its turns, so each
/// recovered call takes an id no earlier call has.
#[tokio::test]
async fn recovered_calls_take_ids_unique_across_the_history() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    response(
        &server,
        &[
            record(TEXT_CALL, false),
            counted(record("", true), json!(2054), json!(55)),
        ],
    )
    .await;
    let provider = NativeOllamaProvider::connect(workbench_config(&server))
        .await
        .unwrap();
    let mut request = workbench_request();
    let mut ids = Vec::new();
    for _ in 0..2 {
        let response = provider.chat(request.clone()).await.unwrap();
        let call = response.tool_calls[0].clone();
        let mut assistant = ChatMessage::assistant(response.content);
        assistant.tool_calls = response.tool_calls;
        request.messages.push(assistant);
        request.messages.push(ChatMessage::tool_result(
            "[\"src/paginate.js\"]",
            "glob",
            call.id.as_str(),
        ));
        ids.push(call.id);
    }
    assert_eq!(ids, ["call_text_0", "call_text_1"]);
    provider.validate_request(&request).unwrap();
    let body = provider.wire_request(&request).unwrap();
    assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "call_text_0");
    assert_eq!(body["messages"][3]["tool_calls"][0]["id"], "call_text_1");
    assert_eq!(body["messages"][4]["tool_name"], "glob");
}

#[tokio::test]
async fn recovered_values_take_the_types_their_schema_declares() {
    let server = MockServer::start().await;
    valid_profile(&server).await;
    response(
        &server,
        &[
            record(
                "<function=read_file>\n<parameter=path>\nsrc/paginate.js\n</parameter>\n\
                 <parameter=limit>\n100\n</parameter>\n</function>\n</tool_call>",
                false,
            ),
            counted(record("", true), json!(2054), json!(40)),
        ],
    )
    .await;
    let provider = NativeOllamaProvider::connect(workbench_config(&server))
        .await
        .unwrap();
    let response = provider.chat(workbench_request()).await.unwrap();
    assert_eq!(
        response.tool_calls[0].arguments,
        json!({"path":"src/paginate.js","limit":100})
    );
}

#[test]
fn text_values_are_typed_only_as_their_schema_declares() {
    let schema = json!({"type":"object","properties":{
        "limit":{"type":"integer"}, "ratio":{"type":"number"}, "all":{"type":"boolean"},
        "count":{"type":["integer","null"]}, "path":{"type":"string"}, "loose":{}}});
    let mut arguments = json!({"limit":"100", "ratio":"1.5", "all":"True", "count":" 7 ",
        "path":"100", "loose":"true", "other":"false"});
    coerce_text_arguments(&mut arguments, &schema);
    assert_eq!(
        arguments,
        json!({"limit":100, "ratio":1.5, "all":true, "count":7, "path":"100", "loose":"true",
            "other":"false"})
    );
    let mut unparsed = json!({"limit":"lots", "ratio":"NaN", "all":"yes"});
    coerce_text_arguments(&mut unparsed, &schema);
    assert_eq!(
        unparsed,
        json!({"limit":"lots", "ratio":"NaN", "all":"yes"})
    );
}
