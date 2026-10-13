//! Routes that check request bodies for provider-side fetches, end to end:
//! the routes Axocoatl adds for external writers and configured routes to
//! a model API, served by the broker against a local upstream that answers
//! at the model API's own host name. The bodies are the pinned programs'
//! own (see `provider_fetch_tests.rs` for how they were captured).

use super::*;
use crate::egress_broker::provider_fetch::MAX_CHECKED_BODY_BYTES;
use crate::egress_broker::rules::ProviderFetch;
use crate::egress_broker::RouteOrigin;
use crate::external_agent::{routes_for_credential, runtime_provider, ExternalCredential};
use axocoatl_config::loadout::AgentRuntime;

const CLAUDE_MESSAGES: &[u8] = include_bytes!("fixtures/claude-code-2.1.292-messages.json");
const CLAUDE_WEB_SEARCH: &[u8] = include_bytes!("fixtures/claude-code-2.1.292-web-search.json");
const CODEX_RESPONSES: &[u8] = include_bytes!("fixtures/codex-0.160.1-responses.json");
const CODEX_WEB_SEARCH_LIVE: &[u8] =
    include_bytes!("fixtures/codex-0.160.1-responses-web-search-live.json");

/// The route Axocoatl adds for `runtime`'s writer sending `credential`, as
/// a loadout Session compiles it, with its credential read from the test's
/// variable.
fn writer_route(runtime: AgentRuntime, credential: ExternalCredential) -> Arc<Route> {
    let yaml = routes_for_credential(runtime, credential)
        .unwrap()
        .remove(0);
    let mut credentials = BTreeMap::new();
    credentials.insert(
        yaml.credential.clone().unwrap(),
        CredentialSourceYaml {
            env: Some(SECRET_ENV.into()),
            file: None,
        },
    );
    RouteTable::compile(&[yaml], &credentials, &[])
        .unwrap()
        .with_origins(&[RouteOrigin::ExternalWriter {
            runtime: runtime_provider(runtime).unwrap().into(),
        }])
        .routes()[0]
        .clone()
}

fn post(path: &str, host: &str, body: &'static [u8]) -> Request<TestBody> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::HOST, host)
        .header(header::CONTENT_TYPE, "application/json")
        .body(full(Bytes::from_static(body)))
        .unwrap()
}

/// A body sent in `pieces` without a length (chunked).
fn chunked(pieces: Vec<Bytes>) -> TestBody {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, BoxError>>(4);
    tokio::spawn(async move {
        for piece in pieces {
            if tx.send(Ok(Frame::data(piece))).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(rx)).boxed()
}

fn refused_with(event: &NetworkEvent, code: &str) -> bool {
    matches!(event, NetworkEvent::Request {
        decision: Decision::Deny, reason: Some(reason), rule: None, credential: None, ..
    } if reason == code)
}

#[tokio::test]
async fn the_claude_code_route_refuses_its_web_search_request_records_why_and_goes_on() {
    let route = writer_route(AgentRuntime::ClaudeCode, ExternalCredential::Default);
    assert_eq!(route.host, "api.anthropic.com");
    assert_eq!(route.provider_fetch, ProviderFetch::Refused);
    let h = harness(route).await;
    let sink = Arc::new(MemorySink::default());
    let mut client = h.client(sink.clone()).await;

    // Claude Code 2.1.292's WebSearch tool asks Anthropic to search the web.
    let response = client
        .sender
        .send_request(post(
            "/v1/messages?beta=true",
            &h.host_header(),
            CLAUDE_WEB_SEARCH,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(
        response.headers()["x-axocoatl-egress"],
        "denied; reason=provider_fetch_refused"
    );
    let json = refusal_json(response).await;
    assert_eq!(json["error"], "provider_fetch_refused");
    assert_eq!(
        json["construct"],
        "tool type web_search_20250305, which the provider runs itself"
    );
    assert_eq!(json["json_path"], "$.tools[0]");
    assert_eq!(json["path"], "/v1/messages");
    let reason = json["reason"].as_str().unwrap();
    assert!(
        reason.contains("web_search_20250305") && reason.contains("at $.tools[0]"),
        "{reason}"
    );
    assert!(
        json["hint"]
            .as_str()
            .unwrap()
            .contains("the route Axocoatl adds for the loadout's claude-code writer"),
        "{json}"
    );
    // It never reached the model API, and the record says why, without a
    // credential.
    assert!(h.upstream.seen().is_empty());
    let events = request_events(&sink);
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(
        refused_with(&events[0], "provider_fetch_refused"),
        "{events:?}"
    );
    assert!(
        matches!(&events[0], NetworkEvent::Request { method, path, .. }
        if method == "POST" && path == "/v1/messages")
    );

    // The connection goes on: Claude Code's own model call reaches the API
    // byte for byte, with the credential.
    let response = client
        .sender
        .send_request(post(
            "/v1/messages?beta=true",
            &h.host_header(),
            CLAUDE_MESSAGES,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let seen = h.upstream.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].body, Bytes::from_static(CLAUDE_MESSAGES));
    assert_eq!(
        seen[0].header("content-length"),
        [CLAUDE_MESSAGES.len().to_string().as_str()]
    );
    assert_eq!(
        seen[0].header("authorization"),
        [format!("Bearer {}", secret()).as_str()]
    );
    let events = request_events(&sink);
    assert!(matches!(&events[1], NetworkEvent::Request {
        decision: Decision::Allow, credential: Some(name), .. } if name == "claude-code-oauth"));
    assert!(matches!(
        wait_for_response_event(&sink).await,
        NetworkEvent::Response { status: 200, up, .. } if up == CLAUDE_MESSAGES.len() as u64
    ));
}

#[tokio::test]
async fn the_codex_routes_send_codexs_requests_byte_for_byte_and_refuse_live_web_search() {
    for (credential, host, path) in [
        (
            ExternalCredential::Default,
            "api.openai.com",
            "/v1/responses",
        ),
        (
            ExternalCredential::CodexChatgpt,
            "chatgpt.com",
            "/backend-api/codex/responses",
        ),
    ] {
        let route = writer_route(AgentRuntime::Codex, credential);
        assert_eq!(route.host, host);
        assert_eq!(route.provider_fetch, ProviderFetch::Refused);
        let h = harness(route).await;
        let sink = Arc::new(MemorySink::default());
        let mut client = h.client(sink.clone()).await;
        // Sent in pieces without a length, as a streaming client may: the
        // upstream gets the same bytes, now with their length.
        let body = Bytes::from_static(CODEX_RESPONSES);
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::HOST, h.host_header())
            .header(header::CONTENT_TYPE, "application/json")
            .body(chunked(vec![
                body.slice(..1000),
                body.slice(1000..1001),
                body.slice(1001..),
            ]))
            .unwrap();
        let response = client.sender.send_request(request).await.unwrap();
        assert_eq!(response.status(), 200, "{host}");
        assert_eq!(body_text(response).await, "ok");
        let seen = h.upstream.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].body, body, "{host}");
        assert_eq!(
            seen[0].header("content-length"),
            [body.len().to_string().as_str()]
        );

        // Codex 0.160.1 without web_search="disabled" offers OpenAI's
        // hosted web search with live access.
        let response = client
            .sender
            .send_request(post(path, &h.host_header(), CODEX_WEB_SEARCH_LIVE))
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "{host}");
        let json = refusal_json(response).await;
        assert_eq!(json["error"], "provider_fetch_refused");
        assert_eq!(json["json_path"], "$.tools[9]");
        assert_eq!(
            json["construct"],
            "tool type web_search, which the provider runs itself"
        );
        assert!(json["hint"].as_str().unwrap().contains("codex writer"));
        assert_eq!(h.upstream.seen().len(), 1, "{host}");
        let events = request_events(&sink);
        assert!(
            refused_with(&events[1], "provider_fetch_refused"),
            "{events:?}"
        );
    }
}

#[tokio::test]
async fn bodies_larger_than_the_check_reads_are_refused_unsent() {
    // A configured route to a model API with the default max_request_bytes
    // (1 GiB): the check reads at most 64 MiB.
    let route = route(
        "host: api.anthropic.com\ncredential: test\ninject: {header: x-api-key}\n\
         rules:\n  - {methods: [POST], path: /v1/messages}\n",
    );
    assert_eq!(route.provider_fetch, ProviderFetch::Refused);
    assert_eq!(route.max_request_bytes, 1 << 30);
    let h = harness(route).await;
    let sink = Arc::new(MemorySink::default());
    let mut client = h.client(sink.clone()).await;
    // Declared larger: refused before the body is read.
    let (_keep, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, BoxError>>(1);
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header(header::HOST, h.host_header())
        .header(header::CONTENT_LENGTH, MAX_CHECKED_BODY_BYTES + 1)
        .body(StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(rx)).boxed())
        .unwrap();
    let response = client.sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), 413);
    let json = refusal_json(response).await;
    assert_eq!(json["error"], "request_too_large");
    assert!(
        json["reason"]
            .as_str()
            .unwrap()
            .contains("reads each request body whole and checks it before sending it, and reads at most 67108864 bytes"),
        "{json}"
    );
    assert!(json["hint"]
        .as_str()
        .unwrap()
        .contains("provider_fetch: allow"));
    assert!(h.upstream.seen().is_empty());
    assert!(refused_with(&request_events(&sink)[0], "request_too_large"));

    // Sent without a length, past a smaller max_request_bytes: cut off
    // while it is read, and none of it is sent (a route that does not check
    // bodies streams what came before the limit).
    let small = harness(super::route(
        "host: api.anthropic.com\ncredential: test\ninject: {header: x-api-key}\n\
         max_request_bytes: 4096\nrules:\n  - {methods: [POST], path: /v1/messages}\n",
    ))
    .await;
    let sink = Arc::new(MemorySink::default());
    let mut client = small.client(sink.clone()).await;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header(header::HOST, small.host_header())
        .body(chunked(vec![Bytes::from(vec![b' '; 1024]); 8]))
        .unwrap();
    let response = client.sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), 413);
    let json = refusal_json(response).await;
    assert_eq!(json["reason"], "the body is larger than 4096 bytes");
    assert!(small.upstream.seen().is_empty());
    let events = sink.events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(refused_with(&events[0], "request_too_large"));
}

#[tokio::test]
async fn provider_fetch_allow_sends_bodies_unread_and_the_policy_says_so() {
    let route = route(
        "host: api.anthropic.com\ncredential: test\ninject: {header: x-api-key}\n\
         provider_fetch: allow\nrules:\n  - {methods: [POST], path: /v1/messages}\n",
    );
    assert_eq!(route.provider_fetch, ProviderFetch::Allowed);
    assert_eq!(route.canonical()["provider_fetch"], "allow");
    assert!(
        route
            .policy_text()
            .ends_with("for agent, provider fetches allowed)"),
        "{}",
        route.policy_text()
    );
    let h = harness(route).await;
    let sink = Arc::new(MemorySink::default());
    let mut client = h.client(sink.clone()).await;
    let response = client
        .sender
        .send_request(post("/v1/messages", &h.host_header(), CLAUDE_WEB_SEARCH))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let seen = h.upstream.seen();
    assert_eq!(seen[0].body, Bytes::from_static(CLAUDE_WEB_SEARCH));
    assert!(matches!(&request_events(&sink)[0], NetworkEvent::Request {
        decision: Decision::Allow, rule: Some(rule), .. } if rule == "route#0.rules[0]"));

    // A route to another host reads no bodies unless it asks to.
    assert_eq!(bearer_route().provider_fetch, ProviderFetch::NotChecked);
}

#[tokio::test]
async fn bodies_the_check_cannot_read_are_refused_and_empty_ones_pass() {
    let route = route(
        "host: api.openai.com\ncredential: test\ninject: {header: Authorization, format: \"Bearer {}\"}\n\
         rules:\n  - {methods: [POST], path: /v1/responses}\n  - {methods: [POST], path: /v1/files}\n  \
         - {methods: [GET], path: /v1/models}\n",
    );
    let h = harness(route).await;
    let sink = Arc::new(MemorySink::default());
    let mut client = h.client(sink.clone()).await;
    for (path, body, fragment) in [
        (
            "/v1/responses",
            &b"not json"[..],
            "cannot read as one JSON value",
        ),
        (
            "/v1/responses",
            &br#"{"tools":[{"type":"web_search"}],"tools":[]}"#[..],
            "a key that appears twice in one object",
        ),
        (
            "/v1/files",
            &b"--x\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n--x--\r\n"[..],
            "endpoint whose request format the check does not know",
        ),
    ] {
        let response = client
            .sender
            .send_request(post(path, &h.host_header(), body))
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "{path}");
        let json = refusal_json(response).await;
        assert_eq!(json["error"], "provider_fetch_refused");
        assert_eq!(json["json_path"], "$");
        assert!(
            json["construct"].as_str().unwrap().contains(fragment),
            "{json}"
        );
        assert!(json["hint"]
            .as_str()
            .unwrap()
            .contains("set provider_fetch: allow on sandbox.egress.routes[0] (api.openai.com)"));
    }
    // A request without a body passes, and so does an empty one.
    let response = client
        .sender
        .send_request(get("/v1/models", &h.host_header()))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let response = client
        .sender
        .send_request(post("/v1/responses", &h.host_header(), b""))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let seen = h.upstream.seen();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(seen.iter().all(|seen| seen.body.is_empty()));

    // A compressed body is refused unread, and the connection closes.
    let request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header(header::HOST, h.host_header())
        .header(header::CONTENT_ENCODING, "gzip")
        .body(full(vec![0x1f, 0x8b, 8, 0]))
        .unwrap();
    let response = client.sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(response.headers()[header::CONNECTION], "close");
    let json = refusal_json(response).await;
    assert!(json["construct"]
        .as_str()
        .unwrap()
        .starts_with("a compressed request body"));
    assert_eq!(h.upstream.seen().len(), 2);
    let refused = request_events(&sink)
        .iter()
        .filter(|event| refused_with(event, "provider_fetch_refused"))
        .count();
    assert_eq!(refused, 4);
}
