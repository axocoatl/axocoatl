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

/// End-to-end: needs a live Ollama that nothing else uses, with
/// `AXOCOATL_LIVE_OLLAMA_MODEL` pulled and not loaded (`ollama stop` it
/// first). A one-second first-token timeout passes while Ollama loads the
/// model; the call fails naming the setting, and closing the connection
/// makes Ollama abort the load (Ollama 0.40.8 logs "client connection
/// closed before llama-server finished loading, aborting load"): the model
/// is not loaded afterwards. Run with:
///
/// ```text
/// AXOCOATL_LIVE_OLLAMA_URL=http://127.0.0.1:11434 AXOCOATL_LIVE_OLLAMA_MODEL=gpt-oss:20b \
///   cargo test -p axocoatl-llm-ollama --test stream_timeouts -- --ignored --exact \
///   giving_up_on_a_load_makes_a_live_ollama_abort_it
/// ```
#[tokio::test]
#[ignore = "needs a live Ollama: AXOCOATL_LIVE_OLLAMA_URL and AXOCOATL_LIVE_OLLAMA_MODEL"]
async fn giving_up_on_a_load_makes_a_live_ollama_abort_it() {
    let base_url = std::env::var("AXOCOATL_LIVE_OLLAMA_URL").expect("AXOCOATL_LIVE_OLLAMA_URL");
    let model = std::env::var("AXOCOATL_LIVE_OLLAMA_MODEL").expect("AXOCOATL_LIVE_OLLAMA_MODEL");
    let loaded = || async {
        let ps: serde_json::Value = reqwest::get(format!("{base_url}/api/ps"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        ps["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|loaded| loaded["name"] == model.as_str())
    };
    assert!(
        !loaded().await,
        "{model} is loaded already: `ollama stop` it"
    );
    let provider = NativeOllamaProvider::connect(NativeOllamaConfig {
        base_url: base_url.clone(),
        model: model.clone(),
        context_tokens: 8192,
        max_output_tokens: 32,
        max_response_bytes: 64 * 1024,
    })
    .await
    .unwrap()
    .with_stream_timeouts(StreamTimeouts {
        first_token: Duration::from_secs(1),
        idle: Duration::from_secs(60),
    });
    let started = std::time::Instant::now();
    let error = provider
        .chat(ChatRequest::simple("Say ready."))
        .await
        .unwrap_err()
        .to_string();
    println!("gave up after {:?}: {error}", started.elapsed());
    assert!(started.elapsed() < Duration::from_secs(3), "{error}");
    assert!(error.contains("first-token timeout"), "{error}");
    // Loading a model this size takes longer than this wait: had Ollama
    // gone on loading it, it would be loaded by now.
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(!loaded().await, "Ollama went on loading {model}");
}

/// End-to-end: needs a live Ollama with `AXOCOATL_LIVE_OLLAMA_MODEL`
/// loaded at `AXOCOATL_LIVE_OLLAMA_CONTEXT` tokens (as `ollama ps` shows
/// it, so that this call does not load it again). A two-second first-token
/// timeout passes before the answer starts (while Ollama reads the long
/// prompt, or while the request waits for the model's one slot); the call
/// fails naming the setting, and closing the connection makes Ollama
/// abandon the answer: Ollama 0.40.8 logs "aborting completion request due
/// to client closing the connection" and ends the request after about the
/// timeout. Run with:
///
/// ```text
/// AXOCOATL_LIVE_OLLAMA_URL=http://127.0.0.1:11434 AXOCOATL_LIVE_OLLAMA_MODEL=qwen3-coder:30b \
///   AXOCOATL_LIVE_OLLAMA_CONTEXT=32768 cargo test -p axocoatl-llm-ollama --test stream_timeouts \
///   -- --ignored --exact giving_up_on_an_answer_makes_a_live_ollama_stop_it
/// ```
#[tokio::test]
#[ignore = "needs a live Ollama: AXOCOATL_LIVE_OLLAMA_URL, _MODEL and _CONTEXT"]
async fn giving_up_on_an_answer_makes_a_live_ollama_stop_it() {
    let base_url = std::env::var("AXOCOATL_LIVE_OLLAMA_URL").expect("AXOCOATL_LIVE_OLLAMA_URL");
    let model = std::env::var("AXOCOATL_LIVE_OLLAMA_MODEL").expect("AXOCOATL_LIVE_OLLAMA_MODEL");
    let context: usize = std::env::var("AXOCOATL_LIVE_OLLAMA_CONTEXT")
        .expect("AXOCOATL_LIVE_OLLAMA_CONTEXT")
        .parse()
        .unwrap();
    let ps: serde_json::Value = reqwest::get(format!("{base_url}/api/ps"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        ps["models"].as_array().unwrap().iter().any(|loaded| {
            loaded["name"] == model.as_str() && loaded["context_length"] == context
        }),
        "{model} is not loaded at {context} tokens: {ps}"
    );
    let provider = NativeOllamaProvider::connect(NativeOllamaConfig {
        base_url,
        model,
        context_tokens: context,
        max_output_tokens: 32,
        max_response_bytes: 64 * 1024,
    })
    .await
    .unwrap()
    .with_stream_timeouts(StreamTimeouts {
        first_token: Duration::from_secs(2),
        idle: Duration::from_secs(60),
    });
    // About 20,000 tokens of prompt: several seconds to read.
    let prompt = format!(
        "{}\nHow many times does the word harbor appear above?",
        "The harbor lights flicker over the quiet water while the night shift counts crates. "
            .repeat(1300)
    );
    let started = std::time::Instant::now();
    let error = provider
        .chat(ChatRequest::simple(&prompt))
        .await
        .unwrap_err()
        .to_string();
    println!("gave up after {:?}: {error}", started.elapsed());
    assert!(started.elapsed() < Duration::from_secs(4), "{error}");
    assert!(error.contains("first-token timeout"), "{error}");
}
