use super::*;
use axocoatl_llm::{ChatRequest, LlmProvider};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

const DIGEST: &str = "a80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72";
const OTHER_DIGEST: &str = "b80c4f17acd55265feec403c7aef86be0c25983ab279d83f3bcd3abbcb5b8b72";

fn shown() -> Value {
    json!({"details":{"format":"gguf"}, "capabilities":["completion","tools"],
        "model_info":{"llama.context_length":131072}})
}
fn listed(digest: &str) -> Value {
    json!({"models":[{"name":"test-model:latest", "model":"test-model:latest", "digest":digest}]})
}
fn loaded(context: Value, digest: &str) -> Value {
    let mut value = listed(digest);
    value["models"][0]["details"] = json!({"format":"gguf"});
    value["models"][0]["context_length"] = context;
    value
}
fn acknowledgement() -> Value {
    json!({"model":"test-model:latest", "created_at":"2026-09-15T00:00:00Z",
        "response":"", "done":true, "done_reason":"load"})
}
async fn fixed(server: &MockServer, verb: &str, endpoint: &str, value: Value) {
    Mock::given(method(verb))
        .and(path(endpoint))
        .respond_with(ResponseTemplate::new(200).set_body_json(value))
        .mount(server)
        .await;
}
async fn sequence(server: &MockServer, verb: &str, endpoint: &str, values: Vec<Value>) {
    let next = Arc::new(AtomicUsize::new(0));
    Mock::given(method(verb))
        .and(path(endpoint))
        .respond_with(move |_: &wiremock::Request| {
            let index = next.fetch_add(1, Ordering::SeqCst).min(values.len() - 1);
            ResponseTemplate::new(200).set_body_json(&values[index])
        })
        .mount(server)
        .await;
}
async fn profile(server: &MockServer, version: &str, disabled: bool, show: Value) {
    fixed(server, "GET", "/api/version", json!({"version":version})).await;
    fixed(
        server,
        "GET",
        "/api/status",
        json!({"cloud":{"disabled":disabled}}),
    )
    .await;
    fixed(server, "POST", "/api/show", show).await;
}
async fn standard(server: &MockServer) {
    profile(server, "0.20.6", true, shown()).await;
    fixed(server, "GET", "/api/tags", listed(DIGEST)).await;
    fixed(server, "GET", "/api/ps", loaded(json!(4096), DIGEST)).await;
    fixed(server, "POST", "/api/generate", acknowledgement()).await;
}
async fn count(server: &MockServer, endpoint: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == endpoint)
        .count()
}

#[tokio::test]
async fn observed_runner_context_becomes_explicit_request_without_inventing_a_default() {
    let server = MockServer::start().await;
    standard(&server).await;
    let observation = observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .unwrap();
    assert_eq!(observation.context_tokens, 4096);
    assert_ne!(observation.context_tokens, 131072);
    assert_eq!(observation.model_digest, DIGEST);
    assert_eq!(observation.requested_model, "test-model");
    assert_eq!(observation.resolved_model, "test-model:latest");
    assert!(observation.observed_at_ms > 0);
    let retained = serde_json::to_vec(&observation).unwrap();
    let observation: NativeOllamaContextObservation = serde_json::from_slice(&retained).unwrap();
    assert_eq!(serde_json::to_vec(&observation).unwrap(), retained);
    let requests = server.received_requests().await.unwrap();
    let preload = requests
        .iter()
        .find(|request| request.url.path() == "/api/generate")
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&preload.body).unwrap(),
        json!({"model":"test-model:latest","stream":false})
    );
    assert_eq!(count(&server, "/api/chat").await, 0);
    assert_eq!(count(&server, "/api/pull").await, 0);

    // A later server default can differ; the accepted observation is explicit
    // request configuration. Connecting must not silently reload/re-resolve it.
    server.reset().await;
    profile(&server, "0.20.6", true, shown()).await;
    fixed(&server, "GET", "/api/tags", listed(DIGEST)).await;
    fixed(&server, "GET", "/api/ps", loaded(json!(8192), DIGEST)).await;
    fixed(
        &server,
        "POST",
        "/api/chat",
        json!({"model":"test-model",
        "message":{"role":"assistant","content":"actual response fixture"},
        "done":true,"done_reason":"stop","prompt_eval_count":3,"eval_count":4}),
    )
    .await;
    let provider = NativeOllamaProvider::connect_observed(observation, 32, 64 * 1024)
        .await
        .unwrap();
    assert_eq!(provider.capabilities().max_context_tokens, 4096);
    let result = provider
        .chat(ChatRequest::simple("question"))
        .await
        .unwrap();
    assert_eq!(result.content, "actual response fixture");
    let requests = server.received_requests().await.unwrap();
    let chat = requests
        .iter()
        .find(|request| request.url.path() == "/api/chat")
        .unwrap();
    let body: Value = serde_json::from_slice(&chat.body).unwrap();
    assert_eq!(body.pointer("/options/num_ctx"), Some(&json!(4096)));
    assert_eq!(body.pointer("/options/num_predict"), Some(&json!(32)));
    assert_eq!(count(&server, "/api/ps").await, 0);
    assert_eq!(count(&server, "/api/generate").await, 0);
    assert_eq!(count(&server, "/api/pull").await, 0);
}

#[tokio::test]
async fn cloud_remote_unknown_version_and_non_gguf_are_refused_before_load() {
    let mut remote = shown();
    remote["remote_model"] = json!("cloud-model");
    let mut non_gguf = shown();
    non_gguf["details"]["format"] = json!("safetensors");
    let mut image = shown();
    image["capabilities"] = json!(["completion", "image"]);
    for (version, disabled, show) in [
        ("0.20.6", false, shown()),
        ("0.20.7", true, shown()),
        ("0.20.6", true, remote),
        ("0.20.6", true, non_gguf),
        ("0.20.6", true, image),
    ] {
        let server = MockServer::start().await;
        profile(&server, version, disabled, show).await;
        assert!(observe_native_ollama_context(&server.uri(), "test-model")
            .await
            .is_err());
        assert_eq!(count(&server, "/api/generate").await, 0);
        assert_eq!(count(&server, "/api/chat").await, 0);
        assert_eq!(count(&server, "/api/pull").await, 0);
    }
}

#[tokio::test]
async fn missing_or_ambiguous_installed_identity_never_loads_or_downloads() {
    let mut ambiguous = listed(DIGEST);
    let duplicate = ambiguous["models"][0].clone();
    ambiguous["models"].as_array_mut().unwrap().push(duplicate);
    let mut disagreement = listed(DIGEST);
    disagreement["models"][0]["model"] = json!("foreign:latest");
    for models in [json!({"models":[]}), ambiguous, disagreement] {
        let server = MockServer::start().await;
        profile(&server, "0.20.6", true, shown()).await;
        fixed(&server, "GET", "/api/tags", models).await;
        assert!(observe_native_ollama_context(&server.uri(), "test-model")
            .await
            .is_err());
        assert_eq!(count(&server, "/api/generate").await, 0);
        assert_eq!(count(&server, "/api/pull").await, 0);
    }
}

#[tokio::test]
async fn load_requires_exact_generation_free_acknowledgement() {
    let mut output = acknowledgement();
    output["response"] = json!("unexpected inference");
    let mut usage = acknowledgement();
    usage["eval_count"] = json!(1);
    let mut prompt_usage = acknowledgement();
    prompt_usage["prompt_eval_count"] = json!(1);
    let mut unfinished = acknowledgement();
    unfinished["done"] = json!(false);
    let mut wrong = acknowledgement();
    wrong["model"] = json!("foreign:latest");
    let mut tool = acknowledgement();
    tool["tool_calls"] = json!([{"name":"unexpected"}]);
    for ack in [output, usage, prompt_usage, unfinished, wrong, tool] {
        let server = MockServer::start().await;
        profile(&server, "0.20.6", true, shown()).await;
        fixed(&server, "GET", "/api/tags", listed(DIGEST)).await;
        fixed(&server, "POST", "/api/generate", ack).await;
        assert!(observe_native_ollama_context(&server.uri(), "test-model")
            .await
            .is_err());
        assert_eq!(count(&server, "/api/generate").await, 1);
        assert_eq!(count(&server, "/api/ps").await, 0);
        assert_eq!(count(&server, "/api/chat").await, 0);
    }
}

#[tokio::test]
async fn architecture_metadata_cannot_replace_missing_invalid_or_unloaded_context() {
    for value in [
        Value::Null,
        json!(0),
        json!(2047),
        json!(-1),
        json!(1.5),
        json!("4096"),
        json!(16777217),
    ] {
        let server = MockServer::start().await;
        profile(&server, "0.20.6", true, shown()).await;
        fixed(&server, "GET", "/api/tags", listed(DIGEST)).await;
        fixed(&server, "POST", "/api/generate", acknowledgement()).await;
        fixed(&server, "GET", "/api/ps", loaded(value, DIGEST)).await;
        assert!(observe_native_ollama_context(&server.uri(), "test-model")
            .await
            .is_err());
        assert_eq!(count(&server, "/api/generate").await, 1);
        assert_eq!(count(&server, "/api/chat").await, 0);
    }
    let server = MockServer::start().await;
    profile(&server, "0.20.6", true, shown()).await;
    fixed(&server, "GET", "/api/tags", listed(DIGEST)).await;
    fixed(&server, "POST", "/api/generate", acknowledgement()).await;
    fixed(&server, "GET", "/api/ps", json!({"models":[]})).await;
    assert!(observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .is_err());
}

#[tokio::test]
async fn changed_digest_runner_or_context_never_becomes_an_observation() {
    for (tags, processes) in [
        (
            vec![listed(DIGEST), listed(OTHER_DIGEST)],
            vec![loaded(json!(4096), DIGEST)],
        ),
        (
            vec![listed(DIGEST)],
            vec![loaded(json!(4096), OTHER_DIGEST)],
        ),
        (
            vec![listed(DIGEST)],
            vec![loaded(json!(4096), DIGEST), loaded(json!(8192), DIGEST)],
        ),
        (
            vec![listed(DIGEST)],
            vec![loaded(json!(4096), DIGEST), json!({"models":[]})],
        ),
    ] {
        let server = MockServer::start().await;
        profile(&server, "0.20.6", true, shown()).await;
        sequence(&server, "GET", "/api/tags", tags).await;
        sequence(&server, "GET", "/api/ps", processes).await;
        fixed(&server, "POST", "/api/generate", acknowledgement()).await;
        assert!(observe_native_ollama_context(&server.uri(), "test-model")
            .await
            .is_err());
        assert_eq!(count(&server, "/api/generate").await, 1);
        assert_eq!(count(&server, "/api/chat").await, 0);
    }
}

#[tokio::test]
async fn changed_profile_after_load_is_refused_without_inference() {
    let server = MockServer::start().await;
    fixed(&server, "GET", "/api/version", json!({"version":"0.20.6"})).await;
    sequence(
        &server,
        "GET",
        "/api/status",
        vec![
            json!({"cloud":{"disabled":true}}),
            json!({"cloud":{"disabled":false}}),
        ],
    )
    .await;
    fixed(&server, "POST", "/api/show", shown()).await;
    fixed(&server, "GET", "/api/tags", listed(DIGEST)).await;
    fixed(&server, "GET", "/api/ps", loaded(json!(4096), DIGEST)).await;
    fixed(&server, "POST", "/api/generate", acknowledgement()).await;
    assert!(observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .is_err());
    assert_eq!(count(&server, "/api/generate").await, 1);
    assert_eq!(count(&server, "/api/chat").await, 0);
}

#[tokio::test]
async fn retained_observation_rechecks_identity_before_connect_and_each_inference() {
    let server = MockServer::start().await;
    standard(&server).await;
    let observation = observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .unwrap();
    server.reset().await;
    profile(&server, "0.20.6", true, shown()).await;
    sequence(
        &server,
        "GET",
        "/api/tags",
        vec![listed(DIGEST), listed(OTHER_DIGEST)],
    )
    .await;
    let provider = NativeOllamaProvider::connect_observed(observation.clone(), 32, 64 * 1024)
        .await
        .unwrap();
    assert!(provider
        .chat(ChatRequest::simple("must not reach inference"))
        .await
        .is_err());
    assert!(
        NativeOllamaProvider::connect_observed(observation, 32, 64 * 1024)
            .await
            .is_err()
    );
    assert_eq!(count(&server, "/api/chat").await, 0);
    assert_eq!(count(&server, "/api/generate").await, 0);
}

#[tokio::test]
async fn metadata_redirect_and_byte_bound_failure_do_not_trigger_fallback() {
    for oversized in [false, true] {
        let server = MockServer::start().await;
        let response = if oversized {
            ResponseTemplate::new(200)
                .set_body_bytes(vec![b' '; super::super::MAX_RESPONSE_BYTES + 1])
        } else {
            ResponseTemplate::new(307)
                .insert_header("Location", format!("{}/unexpected", server.uri()))
        };
        Mock::given(method("GET"))
            .and(path("/api/version"))
            .respond_with(response)
            .mount(&server)
            .await;
        assert!(observe_native_ollama_context(&server.uri(), "test-model")
            .await
            .is_err());
        assert_eq!(count(&server, "/unexpected").await, 0);
        assert_eq!(count(&server, "/api/generate").await, 0);
    }
}

#[test]
fn canonical_alias_matching_is_explicit_and_does_not_invent_digest_pinning() {
    for name in [
        "test-model",
        "test-model:latest",
        "library/test-model",
        "registry.ollama.ai/library/test-model:latest",
        "TEST-MODEL:LATEST",
    ] {
        assert_eq!(
            model_key(name).unwrap(),
            "registry.ollama.ai/library/test-model:latest"
        );
    }
    for name in [
        "",
        "test model",
        "test-model@sha256:1234",
        "a/b/c/d",
        "test-model:",
        "test-model:latest:local",
        "../test-model",
    ] {
        assert!(model_key(name).is_err(), "{name}");
    }
    assert_ne!(
        model_key("team/test-model").unwrap(),
        model_key("test-model").unwrap()
    );
    assert_ne!(
        model_key("other.host/library/test-model").unwrap(),
        model_key("test-model").unwrap()
    );
}
