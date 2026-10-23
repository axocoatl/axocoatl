//! A native Ollama answer may take the first-token timeout to start (Ollama
//! sends its headers with the first token, after loading the model and
//! reading the prompt), which `providers.ollama.first_token_timeout_secs`
//! sets; the idle timeout only applies between chunks.

use std::time::Duration;

use axocoatl_llm::transport::StreamTimeouts;
use axocoatl_llm::{ChatRequest, LlmProvider};
use axocoatl_llm_ollama::{NativeOllamaConfig, NativeOllamaProvider, AUDITED_OLLAMA_VERSION};
use serde_json::json;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

/// An Ollama that answers each chat after `delay`.
async fn slow_ollama(delay: Duration) -> MockServer {
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
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"details": {"format": "gguf"}, "capabilities": ["completion"]}),
            ),
        )
        .mount(&server)
        .await;
    let record = json!({
        "model": "test-model",
        "message": {"role": "assistant", "content": "ready"},
        "done": true,
        "done_reason": "stop",
        "prompt_eval_count": 8,
        "eval_count": 1,
    });
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(format!("{record}\n"), "application/x-ndjson")
                .set_delay(delay),
        )
        .mount(&server)
        .await;
    server
}

async fn provider(server: &MockServer, timeouts: StreamTimeouts) -> NativeOllamaProvider {
    NativeOllamaProvider::connect(NativeOllamaConfig {
        base_url: server.uri(),
        model: "test-model".into(),
        context_tokens: 2048,
        max_output_tokens: 32,
        max_response_bytes: 64 * 1024,
    })
    .await
    .unwrap()
    .with_stream_timeouts(timeouts)
}

#[tokio::test]
async fn a_slow_start_within_the_first_token_timeout_is_answered() {
    let server = slow_ollama(Duration::from_millis(400)).await;
    // The answer starts 400 ms after the request, far past the idle
    // timeout: the first-token timeout governs that wait.
    let provider = provider(
        &server,
        StreamTimeouts {
            first_token: Duration::from_secs(5),
            idle: Duration::from_millis(50),
        },
    )
    .await;
    let response = provider.chat(ChatRequest::simple("go")).await.unwrap();
    assert_eq!(response.content, "ready");
}

#[tokio::test]
async fn an_answer_that_does_not_start_in_time_names_the_setting() {
    let server = slow_ollama(Duration::from_secs(3)).await;
    let provider = provider(
        &server,
        StreamTimeouts {
            first_token: Duration::from_millis(200),
            idle: Duration::from_secs(60),
        },
    )
    .await;
    let started = std::time::Instant::now();
    let error = provider
        .chat(ChatRequest::simple("go"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "waited {:?}",
        started.elapsed()
    );
    assert!(error.contains("first-token timeout"), "{error}");
    assert!(
        error.contains("providers.ollama.first_token_timeout_secs"),
        "{error}"
    );
}

#[test]
fn a_local_model_server_gets_ten_minutes_to_start_by_default() {
    assert_eq!(StreamTimeouts::LOCAL.first_token, Duration::from_secs(600));
    assert_eq!(StreamTimeouts::LOCAL.idle, Duration::from_secs(90));
}
