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
        },
        &model,
        axocoatl_llm::transport::LOCAL_FIRST_TOKEN_TIMEOUT,
    )
    .await
    .unwrap_or_else(|error| panic!("Ollama {version} with {model}: {error}"));
    println!(
        "Ollama {version} with {model}: both contract probes passed at a {context}-token \
         context; the version policy says: {}",
        ollama_version_refusal(&version).unwrap_or_else(|| "accepted".to_string())
    );
}
