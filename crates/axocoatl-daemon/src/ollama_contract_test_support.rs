//! Test servers that stand in for Ollama answer the native contract probes
//! the way Ollama 0.40 does ([`axocoatl_llm_ollama::ContractProbe::answer`]):
//! the over-long prompt is refused with HTTP 400 and an `{"error"}` body, the
//! one-token request stops after one token, the long answer stops at its
//! limit, and the request to call the offered tool calls it.

use axocoatl_llm_ollama::ContractProbe;
use serde_json::Value;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

/// The contract probe a `/api/chat` request body is, or `None` for any other
/// request body.
pub(crate) fn contract_probe_kind(body: &[u8]) -> Option<ContractProbe> {
    ContractProbe::of(&serde_json::from_slice::<Value>(body).ok()?)
}

/// A request to `/api/chat` that is one of the contract probes.
pub(crate) fn is_contract_probe(request: &wiremock::Request) -> bool {
    request.url.path() == "/api/chat" && contract_probe_kind(&request.body).is_some()
}

struct ContractProbeRequest;
impl wiremock::Match for ContractProbeRequest {
    fn matches(&self, request: &wiremock::Request) -> bool {
        is_contract_probe(request)
    }
}

/// Answer the contract probes before any other `/api/chat` mock of `server`.
pub(crate) async fn mount_contract_probes(server: &MockServer) {
    Mock::given(method("POST"))
        .and(ContractProbeRequest)
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or_default();
            let probe = ContractProbe::of(&body).expect("a contract probe");
            let (status, kind, answer) = probe.answer(&body);
            ResponseTemplate::new(status).set_body_raw(answer, kind)
        })
        .with_priority(1)
        .mount(server)
        .await;
}
