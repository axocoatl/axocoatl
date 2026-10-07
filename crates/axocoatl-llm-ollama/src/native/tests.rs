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
        json!({"version":"0.20.6"}),
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
        json!({"version":VERSION}),
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
        json!({"version":VERSION}),
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
    let cases = vec![
        (
            json!({"version":"0.20.7"}),
            json!({"cloud":{"disabled":true}}),
            show(),
        ),
        (json!({"version":"0.20.6"}), json!({}), show()),
        (
            json!({"version":"0.20.6"}),
            json!({"cloud":{"disabled":false}}),
            show(),
        ),
        (
            json!({"version":"0.20.6"}),
            json!({"cloud":{"disabled":"true"}}),
            show(),
        ),
        (
            json!({"version":"0.20.6"}),
            json!({"cloud":{"disabled":true}}),
            json!({"details":{"format":"safetensors"},"capabilities":["completion"]}),
        ),
        (
            json!({"version":"0.20.6"}),
            json!({"cloud":{"disabled":true}}),
            json!({"details":{"format":"gguf"},"capabilities":["embedding"]}),
        ),
        (
            json!({"version":"0.20.6"}),
            json!({"cloud":{"disabled":true}}),
            json!({"details":{"format":"gguf"},"capabilities":["completion"],"remote_host":"https://ollama.com"}),
        ),
    ];
    for (version, status, show) in cases {
        let server = MockServer::start().await;
        profile(&server, version, status, show).await;
        assert!(NativeOllamaProvider::connect(config(&server))
            .await
            .is_err());
        assert!(!server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path() == "/api/chat"));
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
        json!({"version":"0.20.6"}),
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
