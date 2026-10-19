//! Test servers that stand in for Ollama answer the native contract probes
//! the way Ollama 0.40.6 does: the over-long prompt is refused with HTTP 400
//! and an `{"error"}` body, and the one-token request stops after one token.

use serde_json::{json, Value};
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

/// `Some(true)` for the over-long probe, `Some(false)` for the stop probe,
/// `None` for any other request body.
pub(crate) fn contract_probe_kind(body: &[u8]) -> Option<bool> {
    let value: Value = serde_json::from_slice(body).ok()?;
    if value.pointer("/options/num_predict") != Some(&json!(1))
        || value.get("truncate") != Some(&json!(false))
    {
        return None;
    }
    let content = value.pointer("/messages/0/content")?.as_str()?;
    if content == axocoatl_llm_ollama::CONTRACT_STOP_PROBE_PROMPT {
        Some(false)
    } else if content.starts_with("x x ") {
        Some(true)
    } else {
        None
    }
}

/// A request to `/api/chat` that is one of the contract probes.
pub(crate) fn is_contract_probe(request: &wiremock::Request) -> bool {
    request.url.path() == "/api/chat" && contract_probe_kind(&request.body).is_some()
}

struct ContractProbe;
impl wiremock::Match for ContractProbe {
    fn matches(&self, request: &wiremock::Request) -> bool {
        is_contract_probe(request)
    }
}

/// Answer the contract probes before any other `/api/chat` mock of `server`.
pub(crate) async fn mount_contract_probes(server: &MockServer) {
    Mock::given(method("POST"))
        .and(ContractProbe)
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or_default();
            if contract_probe_kind(&request.body) == Some(true) {
                return ResponseTemplate::new(400).set_body_json(json!({
                    "error": "request exceeds the available context size, try increasing it"
                }));
            }
            let record = json!({"model": body["model"], "created_at": "2026-10-19T00:00:00Z",
                "message": {"role": "assistant", "content": "ready"}, "done": true,
                "done_reason": "length", "prompt_eval_count": 14, "eval_count": 1});
            ResponseTemplate::new(200).set_body_raw(format!("{record}\n"), "application/x-ndjson")
        })
        .with_priority(1)
        .mount(server)
        .await;
}
