use super::*;
use crate::native::{
    contract_probe::STOP_PROBE_PROMPT, OllamaServedVersions, AUDITED_OLLAMA_VERSION,
};
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

/// A digest of its own, so a test's probes never find another test's pass in
/// this process's probe cache.
fn unique_digest(seed: u8) -> String {
    format!("{seed:02x}{}", &DIGEST[2..])
}

/// How the test server answers the contract probes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probes {
    Pass,
    AcceptOverLong,
    IgnoreNumPredict,
}

/// `Some(true)` for the over-long probe, `Some(false)` for the stop probe.
fn probe_kind(body: &[u8]) -> Option<bool> {
    let value: Value = serde_json::from_slice(body).ok()?;
    if value.pointer("/options/num_predict") != Some(&json!(1)) {
        return None;
    }
    let content = value.pointer("/messages/0/content")?.as_str()?;
    if content == STOP_PROBE_PROMPT {
        Some(false)
    } else if content.starts_with("x x ") {
        Some(true)
    } else {
        None
    }
}

struct ProbeRequest;
impl wiremock::Match for ProbeRequest {
    fn matches(&self, request: &wiremock::Request) -> bool {
        request.url.path() == "/api/chat" && probe_kind(&request.body).is_some()
    }
}

fn ndjson(value: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(format!("{value}\n"), "application/x-ndjson")
}

async fn probes(server: &MockServer, behavior: Probes) {
    Mock::given(method("POST"))
        .and(ProbeRequest)
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let model = body["model"].clone();
            match (probe_kind(&request.body) == Some(true), behavior) {
                (true, Probes::AcceptOverLong) => ndjson(json!({"model":model,
                    "message":{"role":"assistant","content":"x"},"done":true,
                    "done_reason":"length","prompt_eval_count":4095,"eval_count":1})),
                (true, _) => ResponseTemplate::new(400).set_body_json(json!({"error":
                    "the prompt is longer than the context length currently available to the model"})),
                (false, Probes::IgnoreNumPredict) => ndjson(json!({"model":model,
                    "message":{"role":"assistant","content":"ready, and more"},"done":true,
                    "done_reason":"stop","prompt_eval_count":14,"eval_count":7})),
                (false, _) => ndjson(json!({"model":model,
                    "message":{"role":"assistant","content":"ready"},"done":true,
                    "done_reason":"length","prompt_eval_count":14,"eval_count":1})),
            }
        })
        .with_priority(1)
        .mount(server)
        .await;
}

/// The probe requests and the other `/api/chat` requests the server received.
async fn chats(server: &MockServer) -> (Vec<Value>, usize) {
    let mut probes = Vec::new();
    let mut other = 0;
    for request in server.received_requests().await.unwrap() {
        if request.url.path() != "/api/chat" {
            continue;
        }
        if probe_kind(&request.body).is_some() {
            probes.push(serde_json::from_slice(&request.body).unwrap());
        } else {
            other += 1;
        }
    }
    (probes, other)
}

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
    value["models"][0]["runner"] = json!("llamacpp");
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
    standard_with(server, AUDITED_OLLAMA_VERSION, DIGEST, Probes::Pass).await;
}
async fn standard_with(server: &MockServer, version: &str, digest: &str, behavior: Probes) {
    profile(server, version, true, shown()).await;
    fixed(server, "GET", "/api/tags", listed(digest)).await;
    fixed(server, "GET", "/api/ps", loaded(json!(4096), digest)).await;
    fixed(server, "POST", "/api/generate", acknowledgement()).await;
    probes(server, behavior).await;
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
    // The load starts the runner without context shifting, as inference asks,
    // so the first inference does not reload the model.
    assert_eq!(
        serde_json::from_slice::<Value>(&preload.body).unwrap(),
        json!({"model":"test-model:latest","stream":false,"shift":false})
    );
    assert_eq!(observation.schema_version, OBSERVATION_SCHEMA_VERSION);
    assert_eq!(observation.server_version, AUDITED_OLLAMA_VERSION);
    // The two contract probes ran with the observed context and the
    // inference settings; nothing else was generated.
    let (probe_bodies, other) = chats(&server).await;
    assert_eq!(other, 0);
    assert_eq!(probe_bodies.len(), 2);
    for body in &probe_bodies {
        assert_eq!(body["truncate"], false);
        assert_eq!(body["shift"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body.pointer("/options/num_ctx"), Some(&json!(4096)));
        assert_eq!(body.pointer("/options/num_predict"), Some(&json!(1)));
        assert_eq!(body["model"], "test-model");
    }
    let over_long = probe_bodies[0]["messages"][0]["content"].as_str().unwrap();
    assert_eq!(over_long.len(), 4 * 4096);
    assert_eq!(probe_bodies[1]["messages"][0]["content"], STOP_PROBE_PROMPT);
    assert_eq!(count(&server, "/api/pull").await, 0);

    // A later server default can differ; the accepted observation is explicit
    // request configuration. Connecting must not silently reload/re-resolve it.
    server.reset().await;
    profile(&server, AUDITED_OLLAMA_VERSION, true, shown()).await;
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
    let mut mlx = shown();
    mlx["details"]["runner"] = json!("mlx");
    for (version, disabled, show) in [
        (AUDITED_OLLAMA_VERSION, false, shown()),
        ("0.20.6", true, shown()),
        ("0.20.7", true, shown()),
        ("1.0.0", true, shown()),
        ("0.40.7-rc0", true, shown()),
        (AUDITED_OLLAMA_VERSION, true, remote),
        (AUDITED_OLLAMA_VERSION, true, non_gguf),
        (AUDITED_OLLAMA_VERSION, true, mlx),
        (AUDITED_OLLAMA_VERSION, true, image),
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
        profile(&server, AUDITED_OLLAMA_VERSION, true, shown()).await;
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
        profile(&server, AUDITED_OLLAMA_VERSION, true, shown()).await;
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
        profile(&server, AUDITED_OLLAMA_VERSION, true, shown()).await;
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
    profile(&server, AUDITED_OLLAMA_VERSION, true, shown()).await;
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
        profile(&server, AUDITED_OLLAMA_VERSION, true, shown()).await;
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
    fixed(
        &server,
        "GET",
        "/api/version",
        json!({"version":AUDITED_OLLAMA_VERSION}),
    )
    .await;
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
    profile(&server, AUDITED_OLLAMA_VERSION, true, shown()).await;
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

#[tokio::test]
async fn the_observation_records_the_server_version_it_saw() {
    let server = MockServer::start().await;
    standard_with(&server, "0.40.7", &unique_digest(1), Probes::Pass).await;
    let observation = observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .unwrap();
    assert_eq!(observation.server_version, "0.40.7");
}

#[tokio::test]
async fn a_loaded_mlx_or_unknown_runner_is_refused_before_any_probe() {
    for (runner, reason) in [
        (json!("mlx"), "MLX runner"),
        (json!("ggml"), "not llama.cpp's llama-server"),
        (Value::Null, "not llama.cpp's llama-server"),
    ] {
        let server = MockServer::start().await;
        let digest = unique_digest(2);
        standard_with(&server, AUDITED_OLLAMA_VERSION, &digest, Probes::Pass).await;
        let mut ps = loaded(json!(4096), &digest);
        ps["models"][0]["runner"] = runner.clone();
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ps))
            .with_priority(1)
            .mount(&server)
            .await;
        let refusal = observe_native_ollama_context(&server.uri(), "test-model")
            .await
            .unwrap_err()
            .to_string();
        assert!(refusal.contains(reason), "{runner}: {refusal}");
        assert_eq!(chats(&server).await, (Vec::new(), 0));
    }
}

/// A server that generates from an over-long prompt, or past num_predict,
/// never becomes an observation: the refusal names the guarantee.
#[tokio::test]
async fn a_failed_contract_probe_refuses_the_model_and_names_the_guarantee() {
    for (seed, behavior, guarantee) in [
        (
            3,
            Probes::AcceptOverLong,
            "over-long prompt refusal (truncate:false)",
        ),
        (4, Probes::IgnoreNumPredict, "output limit (num_predict)"),
    ] {
        let server = MockServer::start().await;
        standard_with(
            &server,
            AUDITED_OLLAMA_VERSION,
            &unique_digest(seed),
            behavior,
        )
        .await;
        let refusal = observe_native_ollama_context(&server.uri(), "test-model")
            .await
            .unwrap_err()
            .to_string();
        assert!(refusal.contains(guarantee), "{refusal}");
        assert!(refusal.contains("native Sessions refuse"), "{refusal}");
        if behavior == Probes::IgnoreNumPredict {
            assert!(refusal.contains("generated 7"), "{refusal}");
        }
        // No other inference ran.
        assert_eq!(chats(&server).await.1, 0);
    }
}

/// An over-long probe that is refused for another reason (the server is
/// busy) proves nothing: the model is not accepted on it.
#[tokio::test]
async fn a_busy_server_does_not_pass_the_over_long_probe() {
    let server = MockServer::start().await;
    let digest = unique_digest(5);
    profile(&server, AUDITED_OLLAMA_VERSION, true, shown()).await;
    fixed(&server, "GET", "/api/tags", listed(&digest)).await;
    fixed(&server, "GET", "/api/ps", loaded(json!(4096), &digest)).await;
    fixed(&server, "POST", "/api/generate", acknowledgement()).await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(503)
                .set_body_json(json!({"error":"server busy, please try again"})),
        )
        .mount(&server)
        .await;
    let refusal = observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        refusal.contains("not a refusal of the prompt's length"),
        "{refusal}"
    );
}

/// A retained profile on an upgraded, accepted Ollama is probed again on the
/// new version before it serves, once; the inference names the new version.
#[tokio::test]
async fn an_upgraded_server_is_probed_again_before_a_retained_profile_serves() {
    let server = MockServer::start().await;
    let digest = unique_digest(6);
    standard_with(&server, AUDITED_OLLAMA_VERSION, &digest, Probes::Pass).await;
    let observation = observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .unwrap();
    server.reset().await;
    standard_with(&server, "0.40.7", &digest, Probes::Pass).await;
    fixed(
        &server,
        "POST",
        "/api/chat",
        json!({"model":"test-model",
        "message":{"role":"assistant","content":"served"},
        "done":true,"done_reason":"stop","prompt_eval_count":3,"eval_count":4}),
    )
    .await;
    let served = Arc::new(OllamaServedVersions::default());
    let provider = NativeOllamaProvider::connect_observed(observation.clone(), 32, 64 * 1024)
        .await
        .unwrap()
        .with_served_versions(served.clone());
    assert_eq!(chats(&server).await.0.len(), 2, "probed on 0.40.7");
    assert_eq!(
        provider
            .chat(ChatRequest::simple("question"))
            .await
            .unwrap()
            .content,
        "served"
    );
    // The pass is cached for this server, version, digest and context.
    let (probe_bodies, inferences) = chats(&server).await;
    assert_eq!((probe_bodies.len(), inferences), (2, 1));
    assert_eq!(served.versions(), ["0.40.7"]);
    assert_eq!(count(&server, "/api/generate").await, 0);

    // On a version that fails the probes, the retained profile is refused.
    server.reset().await;
    standard_with(&server, "0.40.8", &digest, Probes::IgnoreNumPredict).await;
    let refusal = NativeOllamaProvider::connect_observed(observation, 32, 64 * 1024)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(refusal.contains("output limit (num_predict)"), "{refusal}");
}

/// A profile Axocoatl 1.4 retained (schema 1, Ollama 0.20.6) says what
/// happened and what to do, whatever the server runs now.
#[tokio::test]
async fn a_profile_retained_on_ollama_0_20_6_gets_a_specific_message() {
    let server = MockServer::start().await;
    standard_with(
        &server,
        AUDITED_OLLAMA_VERSION,
        &unique_digest(7),
        Probes::Pass,
    )
    .await;
    let mut retained = observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .unwrap();
    retained.schema_version = 1;
    retained.server_version = "0.20.6".into();
    // Exactly the shape 1.4.0 wrote still reads.
    let bytes = serde_json::to_vec(&retained).unwrap();
    let retained: NativeOllamaContextObservation = serde_json::from_slice(&bytes).unwrap();
    server.reset().await;
    standard(&server).await;
    let refusal = NativeOllamaProvider::connect_observed(retained.clone(), 32, 64 * 1024)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(
        refusal.contains(&format!(
            "Ollama was upgraded from 0.20.6 to {AUDITED_OLLAMA_VERSION}; review Team & budget again"
        )),
        "{refusal}"
    );
    server.reset().await;
    standard_with(&server, "0.20.6", DIGEST, Probes::Pass).await;
    let refusal = NativeOllamaProvider::connect_observed(retained.clone(), 32, 64 * 1024)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(
        refusal.contains("0.20.6 is no longer supported"),
        "{refusal}"
    );
    // A server that cannot be reached still gets the reason.
    let unreachable = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut moved = retained;
    moved.base_url = format!("http://127.0.0.1:{unreachable}");
    let refusal = NativeOllamaProvider::connect_observed(moved, 32, 64 * 1024)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(refusal.contains("recorded with Ollama 0.20.6"), "{refusal}");
    assert_eq!(chats(&server).await, (Vec::new(), 0));
}

/// An Ollama upgraded while a provider is connected is probed on its new
/// version before the next inference is sent.
#[tokio::test]
async fn an_upgrade_between_inferences_is_probed_before_the_next_one() {
    let server = MockServer::start().await;
    let digest = unique_digest(8);
    standard_with(&server, AUDITED_OLLAMA_VERSION, &digest, Probes::Pass).await;
    let observation = observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .unwrap();
    let served = Arc::new(OllamaServedVersions::default());
    let provider = NativeOllamaProvider::connect_observed(observation, 32, 64 * 1024)
        .await
        .unwrap()
        .with_served_versions(served.clone());
    server.reset().await;
    standard_with(&server, "0.40.9", &digest, Probes::IgnoreNumPredict).await;
    fixed(
        &server,
        "POST",
        "/api/chat",
        json!({"model":"test-model",
        "message":{"role":"assistant","content":"never sent"},
        "done":true,"done_reason":"stop","prompt_eval_count":3,"eval_count":4}),
    )
    .await;
    let refusal = provider
        .chat(ChatRequest::simple("question"))
        .await
        .unwrap_err()
        .to_string();
    assert!(refusal.contains("Ollama 0.40.9"), "{refusal}");
    let (probe_bodies, inferences) = chats(&server).await;
    assert_eq!((probe_bodies.len(), inferences), (2, 0));
    assert!(served.versions().is_empty());
}

/// The inspection a dry run may ask for reads metadata only: a loaded
/// model's context, `None` for a model that is not loaded, and the same
/// refusals as an observation (a model that is not listed, a cloud server),
/// but never a load, a prompt or a contract probe.
#[tokio::test]
async fn inspection_reads_a_loaded_context_and_never_loads_or_probes() {
    for (disabled, tags, running, expected) in [
        (
            true,
            listed(DIGEST),
            loaded(json!(8192), DIGEST),
            Ok(Some(8192)),
        ),
        (true, listed(DIGEST), json!({"models": []}), Ok(None)),
        (true, json!({"models": []}), json!({"models": []}), Err(())),
        (false, listed(DIGEST), loaded(json!(8192), DIGEST), Err(())),
    ] {
        let server = MockServer::start().await;
        profile(&server, AUDITED_OLLAMA_VERSION, disabled, shown()).await;
        fixed(&server, "GET", "/api/tags", tags).await;
        fixed(&server, "GET", "/api/ps", running).await;
        fixed(&server, "POST", "/api/generate", acknowledgement()).await;
        probes(&server, Probes::Pass).await;
        let inspected = inspect_native_ollama_model(&server.uri(), "test-model")
            .await
            .map_err(|_| ());
        assert_eq!(inspected, expected);
        for endpoint in ["/api/generate", "/api/chat", "/api/pull"] {
            assert_eq!(count(&server, endpoint).await, 0, "{endpoint}");
        }
    }
}

/// Ollama 0.41 and later 0.x minors are accepted only through the contract
/// probes: a model on a newer minor becomes an observation, recording that
/// version, when both probes pass, and is refused, naming the guarantee,
/// when one fails. Nothing else is generated either way.
#[tokio::test]
async fn a_newer_minor_is_accepted_only_when_the_contract_probes_pass() {
    let server = MockServer::start().await;
    standard_with(&server, "0.41.0", &unique_digest(9), Probes::Pass).await;
    let observation = observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .unwrap();
    assert_eq!(observation.server_version, "0.41.0");
    let (probe_bodies, other) = chats(&server).await;
    assert_eq!((probe_bodies.len(), other), (2, 0));

    for (seed, behavior, guarantee) in [
        (
            10,
            Probes::AcceptOverLong,
            "over-long prompt refusal (truncate:false)",
        ),
        (11, Probes::IgnoreNumPredict, "output limit (num_predict)"),
    ] {
        let server = MockServer::start().await;
        standard_with(&server, "0.42.1", &unique_digest(seed), behavior).await;
        let refusal = observe_native_ollama_context(&server.uri(), "test-model")
            .await
            .unwrap_err()
            .to_string();
        assert!(refusal.contains("Ollama 0.42.1"), "{refusal}");
        assert!(refusal.contains(guarantee), "{refusal}");
        assert_eq!(chats(&server).await.1, 0);
    }
}

/// A profile observed on the audited release, served after Ollama moved to
/// a newer minor, is probed on that minor before it serves, like any other
/// upgrade; one that fails the probes there is refused.
#[tokio::test]
async fn an_upgrade_to_a_newer_minor_is_probed_before_a_retained_profile_serves() {
    let server = MockServer::start().await;
    let digest = unique_digest(12);
    standard_with(&server, AUDITED_OLLAMA_VERSION, &digest, Probes::Pass).await;
    let observation = observe_native_ollama_context(&server.uri(), "test-model")
        .await
        .unwrap();
    server.reset().await;
    standard_with(&server, "0.41.0", &digest, Probes::Pass).await;
    let served = Arc::new(OllamaServedVersions::default());
    let _provider = NativeOllamaProvider::connect_observed(observation.clone(), 32, 64 * 1024)
        .await
        .unwrap()
        .with_served_versions(served);
    assert_eq!(chats(&server).await.0.len(), 2, "probed on 0.41.0");

    server.reset().await;
    standard_with(&server, "0.43.0", &digest, Probes::AcceptOverLong).await;
    let refusal = NativeOllamaProvider::connect_observed(observation, 32, 64 * 1024)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(refusal.contains("Ollama 0.43.0"), "{refusal}");
    assert!(refusal.contains("over-long prompt refusal"), "{refusal}");
}

/// A model load may take the load timeout (the first-token timeout, never
/// less than two minutes); one that does not finish in time fails naming the
/// setting that gives it longer, without waiting for Ollama.
#[tokio::test]
async fn a_model_load_gets_the_load_timeout_and_names_its_setting_when_late() {
    let server = MockServer::start().await;
    let digest = unique_digest(13);
    profile(&server, AUDITED_OLLAMA_VERSION, true, shown()).await;
    fixed(&server, "GET", "/api/tags", listed(&digest)).await;
    fixed(&server, "GET", "/api/ps", loaded(json!(4096), &digest)).await;
    Mock::given(method("POST"))
        .and(path("/api/generate"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(acknowledgement())
                .set_delay(std::time::Duration::from_secs(2)),
        )
        .mount(&server)
        .await;
    probes(&server, Probes::Pass).await;

    let started = std::time::Instant::now();
    let late = observe(
        &server.uri(),
        "test-model",
        std::time::Duration::from_millis(300),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        started.elapsed() < std::time::Duration::from_millis(1500),
        "waited {:?}",
        started.elapsed()
    );
    assert!(
        late.contains("Ollama did not finish loading test-model:latest"),
        "{late}"
    );
    assert!(
        late.contains("providers.ollama.first_token_timeout_secs"),
        "{late}"
    );
    // A shorter setting still gives a load two minutes: this one answers
    // after two seconds.
    let observation =
        observe_native_ollama_context_within(&server.uri(), "test-model", Duration::from_secs(1))
            .await
            .unwrap();
    assert_eq!(observation.context_tokens, 4096);
}
