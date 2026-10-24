//! The scheduled Ollama watch (`.github/workflows/ollama-watch.yml`): the
//! contract probes, on a live Ollama of any version (release candidates
//! included) and one installed model, without the version policy that
//! refuses an unaudited pre-release in Sessions. A new Ollama release that
//! breaks a guarantee native execution relies on fails here before anyone
//! runs it. It needs `AXOCOATL_WATCH_OLLAMA_URL` (a loopback server) and
//! `AXOCOATL_WATCH_OLLAMA_MODEL` (a GGUF model already pulled), and runs only
//! with `--ignored`.

use super::*;

#[tokio::test]
#[ignore = "needs a live Ollama: AXOCOATL_WATCH_OLLAMA_URL and AXOCOATL_WATCH_OLLAMA_MODEL"]
async fn the_contract_probes_hold_on_a_live_ollama() {
    let base_url = std::env::var("AXOCOATL_WATCH_OLLAMA_URL")
        .expect("AXOCOATL_WATCH_OLLAMA_URL names the Ollama server to probe");
    let model = std::env::var("AXOCOATL_WATCH_OLLAMA_MODEL")
        .expect("AXOCOATL_WATCH_OLLAMA_MODEL names an installed GGUF model");
    let base_url = base_url.trim_end_matches('/').to_owned();
    let client = local_client(&base_url).expect("a loopback Ollama server");
    let shown = metadata_request(
        &client,
        &base_url,
        "api/show",
        Some(json!({"model": model})),
    )
    .await
    .expect("POST /api/show");
    let tools = check_shown_model(&model, &shown)
        .expect("a model native execution accepts")
        .tools;
    let version = metadata_request(&client, &base_url, "api/version", None)
        .await
        .expect("GET /api/version")["version"]
        .as_str()
        .expect("a reported version")
        .to_owned();
    let first = identity(
        &metadata_request(&client, &base_url, "api/tags", None)
            .await
            .expect("GET /api/tags"),
        &model,
    )
    .expect("the model is installed under that name");
    // The same load the observation sends: no prompt, shift:false.
    let loaded = metadata_request(
        &client,
        &base_url,
        "api/generate",
        Some(json!({"model": first.name, "stream": false, "shift": false})),
    )
    .await
    .expect("the model loads");
    load_acknowledgement(&loaded, &first.name).expect("a load acknowledgement");
    let context = loaded_context(
        &metadata_request(&client, &base_url, "api/ps", None)
            .await
            .expect("GET /api/ps"),
        &first,
    )
    .expect("the loaded context");
    contract_probe::ensure_contract(
        &client,
        ProbeKey {
            base_url: base_url.clone(),
            server_version: version.clone(),
            model_digest: first.digest.clone(),
            context_tokens: context,
            tools,
        },
        &model,
        axocoatl_llm::transport::LOCAL_FIRST_TOKEN_TIMEOUT,
    )
    .await
    .unwrap_or_else(|error| panic!("Ollama {version} with {model}: {error}"));
    println!(
        "Ollama {version} with {model}: the contract probes passed at a {context}-token \
         context (tool call probed: {tools}); the version policy says: {}",
        ollama_version_refusal(&version).unwrap_or_else(|| "accepted".to_string())
    );
}

/// A reasoning-disabled JSON request, as Plan and Judge send one
/// (`think:false`, `format:"json"`), on a live Ollama and a model that thinks,
/// read the way a Session reads it: the answer is the JSON object, any
/// reasoning the model writes anyway comes apart from it (gpt-oss's Harmony
/// format ignores `think:false`), and the usage, reasoning included, stays
/// within the output limit (a lower bound for a format request, which
/// reserves two passes). The same variables as the watch, and
/// `--ignored`:
///
/// ```text
/// AXOCOATL_WATCH_OLLAMA_URL=http://127.0.0.1:11434 AXOCOATL_WATCH_OLLAMA_MODEL=gpt-oss:20b \
///   cargo test -p axocoatl-llm-ollama --lib reasoning_disabled_json -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "needs a live Ollama: AXOCOATL_WATCH_OLLAMA_URL and AXOCOATL_WATCH_OLLAMA_MODEL"]
async fn a_reasoning_disabled_json_request_reads_on_a_live_ollama() {
    use axocoatl_core::ResponseFormat;
    use axocoatl_llm::{ChatRequest, LlmProvider, StreamEvent};
    use tokio_stream::StreamExt;
    const OUTPUT: usize = 256;
    let base_url = std::env::var("AXOCOATL_WATCH_OLLAMA_URL")
        .expect("AXOCOATL_WATCH_OLLAMA_URL names the Ollama server");
    let model = std::env::var("AXOCOATL_WATCH_OLLAMA_MODEL")
        .expect("AXOCOATL_WATCH_OLLAMA_MODEL names an installed GGUF model");
    let observation = observe_native_ollama_context(base_url.trim_end_matches('/'), &model)
        .await
        .unwrap_or_else(|error| panic!("{model}: {error}"));
    let version = observation.server_version.clone();
    let provider = NativeOllamaProvider::connect_observed(observation, OUTPUT, 64 * 1024)
        .await
        .unwrap();
    let mut request =
        ChatRequest::simple("Return a JSON object with one key \"ready\" set to true.");
    request.max_tokens = Some(OUTPUT);
    request.response_format = Some(ResponseFormat::Json);
    request.provider_options = Some(json!({"reasoning_effort": "none"}));
    let mut stream = provider.chat_stream(request).await.unwrap();
    let (mut text, mut reasoning, mut usage) = (String::new(), String::new(), None);
    while let Some(event) = stream.next().await {
        match event.unwrap_or_else(|error| panic!("{model}: {error}")) {
            StreamEvent::TextDelta { delta } => text.push_str(&delta),
            StreamEvent::ReasoningDelta { delta } => reasoning.push_str(&delta),
            StreamEvent::UsageObservation(observed) => usage = Some(observed),
            _ => {}
        }
    }
    let observed = usage.expect("the usage of the answer");
    let usage = &observed.usage;
    let answer: Value = serde_json::from_str(text.trim())
        .unwrap_or_else(|error| panic!("{model} answered {text:?}, not JSON: {error}"));
    assert_eq!(answer, json!({"ready": true}), "{model}: {text}");
    assert!(usage.output_tokens <= OUTPUT, "{model}: {usage:?}");
    println!(
        "Ollama {version} with {model}, think:false and format json: the answer is {answer}; \
         {} characters of reasoning came apart from it; {} prompt and {} generated tokens \
         (output limit {OUTPUT}; recorded as complete: {})",
        reasoning.chars().count(),
        usage.input_tokens,
        usage.output_tokens,
        observed.complete
    );
}
