//! A native Ollama tool call's number arguments reach the Agent as the exact
//! f64 Ollama wrote. The provider parses Ollama's NDJSON record, re-encodes the
//! arguments and parses them again; without serde_json's `float_roundtrip`
//! about a third of random values came back 1 ULP off.

use axocoatl_llm::{ChatRequest, LlmProvider, ToolDefinition};
use axocoatl_llm_ollama::{NativeOllamaConfig, NativeOllamaProvider, AUDITED_OLLAMA_VERSION};
use serde_json::{json, Value};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

/// Finite f64 values from random bit patterns (xorshift64*, fixed seed),
/// with the values that once came back changed.
fn values(count: usize) -> Vec<f64> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut values = vec![-5.220838365510166e38, 1.0e-320, f64::MAX, f64::MIN_POSITIVE];
    while values.len() < count {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let value = f64::from_bits(state.wrapping_mul(0x2545_F491_4F6C_DD1D));
        if value.is_finite() {
            values.push(value);
        }
    }
    values
}

#[tokio::test]
async fn native_tool_call_number_arguments_arrive_bit_for_bit() {
    let server = MockServer::start().await;
    for (route, body) in [
        ("/api/version", json!({"version": AUDITED_OLLAMA_VERSION})),
        (
            "/api/status",
            json!({"cloud": {"disabled": true, "source": "env"}}),
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
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"details": {"format": "gguf"}, "capabilities": ["completion", "tools"]}),
        ))
        .mount(&server)
        .await;
    let values = values(2048);
    let record = json!({
        "model": "test-model",
        "message": {"role": "assistant", "content": "", "tool_calls": [
            {"id": "call_1", "function": {"index": 0, "name": "measure",
                "arguments": {"values": values}}}
        ]},
        "done": true,
        "done_reason": "stop",
        "prompt_eval_count": 8,
        "eval_count": 4,
    });
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(format!("{record}\n"), "application/x-ndjson"),
        )
        .mount(&server)
        .await;
    let provider = NativeOllamaProvider::connect(NativeOllamaConfig {
        base_url: server.uri(),
        model: "test-model".into(),
        context_tokens: 2048,
        max_output_tokens: 32,
        max_response_bytes: 1024 * 1024,
    })
    .await
    .unwrap();
    let mut request = ChatRequest::simple("measure");
    request.tools.push(ToolDefinition {
        name: "measure".into(),
        description: "take numbers".into(),
        parameters: json!({"type": "object"}),
        concurrency: Default::default(),
    });
    let response = provider.chat(request).await.unwrap();
    let received: Vec<u64> = response.tool_calls[0].arguments["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| Value::as_f64(value).unwrap().to_bits())
        .collect();
    let sent: Vec<u64> = values.iter().map(|value| value.to_bits()).collect();
    assert_eq!(received.len(), sent.len());
    let changed = received
        .iter()
        .zip(&sent)
        .filter(|(received, sent)| received != sent)
        .count();
    assert_eq!(changed, 0, "{changed} of {} values changed", sent.len());
}
