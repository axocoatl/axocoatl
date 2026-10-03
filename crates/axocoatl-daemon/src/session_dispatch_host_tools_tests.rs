//! Host invocation tools through real native actor admission: offered only
//! when listed, bound to the exact admitted invocation, settled and audited
//! like any tool, refused with a reason when unavailable, and left out of
//! Ways attempt lanes that a tool refuses. Then the real `web_search` and
//! `web_fetch` host tools through the same path, with their network record
//! events and the `sources` evidence they produce.
use super::*;
use crate::session_dispatch_web::{WebRecordSink, WebTools, NETWORK_NONE_REFUSAL};
use axocoatl_session::network_record::{NetworkEvent, WebTool};
use axocoatl_tools::fetch_guard::{FetchError, FetchedPage, PageFetcher};
use axocoatl_tools::{SearchHit, ToolError, WebFetchTool, WebSearchBackend, WebSearchTool};
use std::sync::atomic::AtomicBool;

/// One scripted round of tool calls, then a final answer. Records the tools
/// each request offered and the tool results the answer round saw.
struct HostToolProvider {
    calls: Vec<(&'static str, serde_json::Value)>,
    answer: String,
    round: AtomicUsize,
    offered: Mutex<Vec<Vec<String>>>,
    results: Mutex<Vec<String>>,
    before_calls: Option<Box<dyn Fn() + Send + Sync>>,
}

impl HostToolProvider {
    fn new(calls: Vec<(&'static str, serde_json::Value)>, answer: impl Into<String>) -> Self {
        Self {
            calls,
            answer: answer.into(),
            round: AtomicUsize::new(0),
            offered: Mutex::new(Vec::new()),
            results: Mutex::new(Vec::new()),
            before_calls: None,
        }
    }

    fn offered_first(&self) -> Vec<String> {
        self.offered.lock().unwrap()[0].clone()
    }

    fn results(&self) -> Vec<String> {
        self.results.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for HostToolProvider {
    fn provider_id(&self) -> &str {
        "controlled"
    }
    fn model_id(&self) -> &str {
        "controlled-model"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            ..Default::default()
        }
    }
    fn execution_bounds(&self, _: &ChatRequest) -> Option<ProviderExecutionBounds> {
        Some(ProviderExecutionBounds {
            token_limit: 100,
            cost_microunits: 0,
            response_bytes: 8192,
        })
    }
    async fn chat(&self, _: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> {
        unreachable!("the actual autonomous actor streams")
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<
        Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    > {
        let round = self.round.fetch_add(1, Ordering::SeqCst);
        assert!(round < 2, "only one tool round is scripted");
        self.offered
            .lock()
            .unwrap()
            .push(request.tools.iter().map(|tool| tool.name.clone()).collect());
        let mut events = if round == 0 && !self.calls.is_empty() {
            if let Some(hook) = &self.before_calls {
                hook();
            }
            self.calls
                .iter()
                .enumerate()
                .map(|(index, (name, arguments))| {
                    Ok(StreamEvent::ToolCallDelta {
                        index: Some(index),
                        id: format!("host-call-{index}"),
                        name: Some((*name).into()),
                        args_delta: arguments.to_string(),
                    })
                })
                .collect::<Vec<_>>()
        } else {
            let results: Vec<String> = request
                .messages
                .iter()
                .filter(|message| message.role == MessageRole::Tool)
                .filter_map(|message| message.text_content().map(str::to_owned))
                .collect();
            *self.results.lock().unwrap() = results;
            vec![Ok(StreamEvent::TextDelta {
                delta: self.answer.clone(),
            })]
        };
        let tool_round = round == 0 && !self.calls.is_empty();
        events.push(Ok(StreamEvent::Usage(TokenUsageStats::new(10, 2))));
        events.push(Ok(StreamEvent::Done {
            finish_reason: if tool_round {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
        }));
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

/// A host tool that reports the exact invocation it was bound to.
struct FakeHost {
    refused: Arc<AtomicBool>,
    bound: Arc<Mutex<Vec<HostInvocationContext>>>,
}

impl FakeHost {
    fn new() -> Self {
        Self {
            refused: Arc::new(AtomicBool::new(false)),
            bound: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

struct FakeBound(HostInvocationContext);

#[async_trait]
impl axocoatl_tools::BuiltinTool for FakeBound {
    fn description(&self) -> &str {
        "bound"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _arguments: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, ToolError> {
        Ok(serde_json::json!({
            "invocation": self.0.invocation_id.as_str(),
            "activation": self.0.activation.activation_id.as_str(),
            "agent": self.0.agent,
            "read_only": self.0.read_only,
        }))
    }
}

impl HostInvocationTool for FakeHost {
    fn name(&self) -> &'static str {
        "web_search"
    }
    fn definition(&self) -> Arc<dyn axocoatl_tools::BuiltinTool> {
        Arc::new(HostToolDefinition::new(
            "web_search",
            "fake host search",
            serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}}}),
            axocoatl_llm::ConcurrencyPolicy::Safe,
        ))
    }
    fn refusal(&self, profile: &ExecutionProfile) -> Option<String> {
        self.refused
            .load(Ordering::SeqCst)
            .then(|| format!("fake host search is unavailable to {}", profile.definition))
    }
    fn bind(&self, context: HostInvocationContext) -> Arc<dyn axocoatl_tools::BuiltinTool> {
        self.bound.lock().unwrap().push(context.clone());
        Arc::new(FakeBound(context))
    }
}

fn resources_with(node: &InputNode, provider: Arc<HostToolProvider>) -> AutonomousActivationResources {
    let mut resources = input_resources(node, InputProvider::new("unused", false, false));
    resources.provider = provider;
    resources
}

async fn run_with(
    controller: &SessionDispatchController,
    node: &InputNode,
    provider: Arc<HostToolProvider>,
) -> SettledActivation {
    controller
        .prepare_autonomous_activation(node.input.activation.clone(), resources_with(node, provider))
        .unwrap()
        .run()
        .await
        .unwrap()
}

fn audited_tool(controller: &SessionDispatchController, invocation: &InvocationId) -> String {
    let state = controller.lock().unwrap();
    state
        .audit
        .invocation(invocation)
        .unwrap()
        .expect("the host tool call is audited")
        .intent
        .tool_name
        .clone()
}

fn succeeded(controller: &SessionDispatchController, invocation: &InvocationId) -> bool {
    let state = controller.lock().unwrap();
    state.canonical.records().unwrap().iter().any(|record| {
        matches!(
            &record.event,
            TurnContractEvent::RecordOutcome { invocation_id, outcome: InvocationOutcome::Succeeded, .. }
                if invocation_id == invocation
        )
    })
}

#[tokio::test]
async fn a_listed_host_tool_is_bound_to_the_exact_admitted_invocation_and_settled() {
    let fixture = input_fixture_with_tools(false, &["web_search"]);
    let host = Arc::new(FakeHost::new());
    fixture
        .controller
        .register_host_invocation_tool(host.clone())
        .unwrap();
    start_input(&fixture.controller, &fixture.parent);
    let provider = Arc::new(HostToolProvider::new(
        vec![("web_search", serde_json::json!({"query": "q"}))],
        "searched",
    ));
    let result = run_with(&fixture.controller, &fixture.parent, provider.clone()).await;
    assert!(result.accepted, "{:?}", result.failure);
    assert_eq!(provider.offered_first(), ["web_search"]);

    let bound = host.bound.lock().unwrap().clone();
    assert_eq!(bound.len(), 1, "bound once, for the one admitted call");
    let context = &bound[0];
    assert_eq!(context.activation, fixture.parent.input.activation);
    assert_eq!(context.session_id, "input-session");
    assert_eq!(context.agent, "parent");
    assert!(!context.read_only);
    // The id the tool saw is the audited, settled invocation.
    assert_eq!(audited_tool(&fixture.controller, &context.invocation_id), "web_search");
    assert!(succeeded(&fixture.controller, &context.invocation_id));
    let results = provider.results();
    assert_eq!(results.len(), 1);
    assert!(
        results[0].contains(context.invocation_id.as_str()),
        "{results:?}"
    );
}

#[tokio::test]
async fn an_unlisted_host_tool_is_not_offered_and_a_read_only_helper_that_lists_it_gets_it() {
    let fixture = input_fixture_with_nodes(
        false,
        [(&["effect"], None), (&["web_search"], Some(Vec::new()))],
    );
    let host = Arc::new(FakeHost::new());
    fixture
        .controller
        .register_host_invocation_tool(host.clone())
        .unwrap();
    start_input(&fixture.controller, &fixture.parent);
    let parent_provider = Arc::new(HostToolProvider::new(Vec::new(), "no search"));
    let parent = run_with(&fixture.controller, &fixture.parent, parent_provider.clone()).await;
    assert!(parent.accepted, "{:?}", parent.failure);
    assert!(
        !parent_provider
            .offered_first()
            .iter()
            .any(|tool| tool == "web_search"),
        "tools is an exact allowlist: {:?}",
        parent_provider.offered_first()
    );
    assert!(host.bound.lock().unwrap().is_empty());

    let mut child = fixture.child.clone();
    child.input.parents = vec![accepted_parent(&parent)];
    start_input(&fixture.controller, &child);
    let child_provider = Arc::new(HostToolProvider::new(
        vec![("web_search", serde_json::json!({"query": "q"}))],
        "helper searched",
    ));
    let result = run_with(&fixture.controller, &child, child_provider.clone()).await;
    assert!(result.accepted, "{:?}", result.failure);
    assert_eq!(child_provider.offered_first(), ["web_search"]);
    let bound = host.bound.lock().unwrap().clone();
    assert_eq!(bound.len(), 1);
    assert!(bound[0].read_only, "the helper's own empty write scope");
    assert_eq!(bound[0].agent, "child");
}

#[tokio::test]
async fn preparation_refuses_a_listed_host_tool_that_is_unavailable_or_missing() {
    let fixture = input_fixture_with_tools(false, &["web_search"]);
    start_input(&fixture.controller, &fixture.parent);
    let provider = Arc::new(HostToolProvider::new(Vec::new(), "unused"));
    let missing = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.parent.input.activation.clone(),
            resources_with(&fixture.parent, provider.clone()),
        )
        .err()
        .expect("no web_search registered")
        .to_string();
    assert!(
        missing.contains("web_search is listed for parent but this daemon does not provide web_search"),
        "{missing}"
    );

    let host = Arc::new(FakeHost::new());
    host.refused.store(true, Ordering::SeqCst);
    fixture.controller.register_host_invocation_tool(host).unwrap();
    let refused = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.parent.input.activation.clone(),
            resources_with(&fixture.parent, provider.clone()),
        )
        .err()
        .expect("refused")
        .to_string();
    assert!(refused.contains("fake host search is unavailable to parent"), "{refused}");
    assert_eq!(provider.round.load(Ordering::SeqCst), 0, "no model call");
}

#[tokio::test]
async fn admission_declines_a_listed_tool_that_became_unavailable_and_records_nothing() {
    let fixture = input_fixture_with_tools(false, &["web_search"]);
    let host = Arc::new(FakeHost::new());
    fixture
        .controller
        .register_host_invocation_tool(host.clone())
        .unwrap();
    start_input(&fixture.controller, &fixture.parent);
    let mut provider = HostToolProvider::new(
        vec![("web_search", serde_json::json!({"query": "q"}))],
        "declined",
    );
    let refused = host.refused.clone();
    provider.before_calls = Some(Box::new(move || refused.store(true, Ordering::SeqCst)));
    let provider = Arc::new(provider);
    let result = run_with(&fixture.controller, &fixture.parent, provider.clone()).await;
    assert!(result.accepted, "{:?}", result.failure);
    assert!(host.bound.lock().unwrap().is_empty(), "never bound");
    let results = provider.results();
    assert!(
        results[0].contains("fake host search is unavailable to parent"),
        "{results:?}"
    );
    let state = fixture.controller.lock().unwrap();
    assert!(
        state.audit.records().unwrap().is_empty(),
        "a declined call records nothing"
    );
}

#[test]
fn host_tool_names_validate_and_others_still_do_not() {
    validate_repository_tools(&["web_search".into(), "web_fetch".into(), "browser".into()])
        .unwrap();
    let error = validate_repository_tools(&["web_crawl".into()])
        .unwrap_err()
        .to_string();
    assert!(error.contains("web_crawl") && error.contains("web_search, web_fetch, browser"), "{error}");
    struct NotHost;
    impl HostInvocationTool for NotHost {
        fn name(&self) -> &'static str {
            "read_file"
        }
        fn definition(&self) -> Arc<dyn axocoatl_tools::BuiltinTool> {
            unreachable!()
        }
        fn refusal(&self, _: &ExecutionProfile) -> Option<String> {
            None
        }
        fn bind(&self, _: HostInvocationContext) -> Arc<dyn axocoatl_tools::BuiltinTool> {
            unreachable!()
        }
    }
    let fixture = input_fixture();
    assert!(fixture
        .controller
        .register_host_invocation_tool(Arc::new(NotHost))
        .is_err());
}

// --- The real web tools through the same path ---------------------------

const RUST_HOME: &str = "https://www.rust-lang.org/";
const RUST_BOOK: &str = "https://doc.rust-lang.org/book/";

struct FixedSearch;

#[async_trait]
impl WebSearchBackend for FixedSearch {
    fn name(&self) -> &str {
        "searxng"
    }
    async fn search(&self, _query: &str, _max: usize) -> std::result::Result<Vec<SearchHit>, String> {
        Ok(vec![
            SearchHit {
                title: "Rust".into(),
                url: RUST_HOME.into(),
                snippet: "A language empowering everyone".into(),
                engines: vec!["wikipedia".into()],
            },
            SearchHit {
                title: "The Book".into(),
                url: RUST_BOOK.into(),
                snippet: "The Rust Programming Language".into(),
                engines: vec!["wikipedia".into()],
            },
        ])
    }
}

#[derive(Default)]
struct FixedFetcher {
    calls: AtomicUsize,
}

#[async_trait]
impl PageFetcher for FixedFetcher {
    async fn fetch(&self, url: &str) -> std::result::Result<FetchedPage, FetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(FetchedPage {
            url: url.into(),
            final_url: url.into(),
            status: 200,
            content_type: "text/html".into(),
            charset: None,
            body: b"<title>Rust</title><p>Rust is fast.</p><p>Rust is memory safe.</p>".to_vec(),
            body_truncated: false,
            redirects: Vec::new(),
        })
    }
}

/// Network records in the fixture Session's own canonical store.
struct ControllerRecords(SessionDispatchController);

impl crate::session_network::RecordNamespaces for ControllerRecords {
    fn writer_namespace(
        &self,
        _session_id: &str,
    ) -> std::result::Result<axocoatl_session::execution_namespace::OwnedExecutionNamespace, String>
    {
        self.0
            .network_record_namespace()
            .map_err(|error| error.to_string())
    }

    fn read_existing(
        &self,
        _session_id: &str,
        after: Option<u64>,
        limit: usize,
        limits: axocoatl_session::network_record::RecordLimits,
    ) -> std::result::Result<
        Option<(
            Vec<axocoatl_session::network_record::NetworkLine>,
            axocoatl_session::network_record::RecordStats,
        )>,
        String,
    > {
        self.0
            .read_network_record(after, limit, limits)
            .map_err(|error| error.to_string())
    }
}

struct FailingSink;

#[async_trait]
impl WebRecordSink for FailingSink {
    async fn append(&self, _session: &str, _event: NetworkEvent) -> std::result::Result<u64, String> {
        Err("network record is full".into())
    }
}

/// A record that takes `room` more events, then is full: it fills between
/// a call's write-ahead event and its result, as parallel calls can.
struct FillingSink {
    room: AtomicUsize,
    events: Mutex<Vec<NetworkEvent>>,
}

#[async_trait]
impl WebRecordSink for FillingSink {
    async fn append(&self, _session: &str, event: NetworkEvent) -> std::result::Result<u64, String> {
        if self
            .room
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |room| room.checked_sub(1))
            .is_err()
        {
            return Err("network record is full".into());
        }
        let mut events = self.events.lock().unwrap();
        events.push(event);
        Ok(events.len() as u64)
    }
}

fn web_tools(
    network: &str,
    fetcher: Arc<FixedFetcher>,
    records: Arc<dyn WebRecordSink>,
) -> Arc<WebTools> {
    Arc::new(WebTools::from_parts(
        network,
        Some(Arc::new(WebSearchTool::new(Arc::new(FixedSearch)))),
        Some(Arc::new(WebFetchTool::new(fetcher))),
        records,
    ))
}

#[tokio::test]
async fn web_calls_are_recorded_with_their_invocation_and_cited_sources_are_marked() {
    let fixture = input_fixture_with_tools(false, &["web_search", "web_fetch"]);
    let records = Arc::new(crate::session_network::SessionNetworkRecords::new(
        Arc::new(ControllerRecords(fixture.controller.clone())),
        50_000,
    ));
    let fetcher = Arc::new(FixedFetcher::default());
    for tool in web_tools("bridge", fetcher.clone(), records.clone()).host_tools() {
        fixture.controller.register_host_invocation_tool(tool).unwrap();
    }
    let home = axocoatl_tools::source_id(RUST_HOME).unwrap();
    let book = axocoatl_tools::source_id(RUST_BOOK).unwrap();
    start_input(&fixture.controller, &fixture.parent);
    let provider = Arc::new(HostToolProvider::new(
        vec![
            ("web_search", serde_json::json!({"query": "rust language"})),
            ("web_fetch", serde_json::json!({"url": RUST_HOME})),
        ],
        format!("Rust is fast [{home} ¶1]."),
    ));
    let result = run_with(&fixture.controller, &fixture.parent, provider.clone()).await;
    assert!(result.accepted, "{:?}", result.failure);
    let offered = provider.offered_first();
    assert!(offered.contains(&"web_search".to_string()) && offered.contains(&"web_fetch".to_string()));
    let results = provider.results();
    assert!(results.iter().any(|text| text.contains(&book)), "{results:?}");
    assert!(results.iter().any(|text| text.contains("¶1 Rust is fast.")), "{results:?}");
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);

    // One web event per call, attributed to the audited invocation, each
    // after the web_request written before the call was sent.
    let page = records.read_after("input-session", None, 100).await.unwrap();
    let web: Vec<&NetworkEvent> = page
        .events
        .iter()
        .map(|line| &line.event)
        .filter(|event| event.kind() == "web")
        .collect();
    assert_eq!(web.len(), 2, "{web:?}");
    let invocation_of = |event: &NetworkEvent| match event {
        NetworkEvent::WebRequest { invocation_id, .. } | NetworkEvent::Web { invocation_id, .. } => {
            invocation_id.clone()
        }
        _ => unreachable!(),
    };
    for event in &web {
        let invocation = invocation_of(event);
        let request = page
            .events
            .iter()
            .position(|line| {
                line.event.kind() == "web_request" && invocation_of(&line.event) == invocation
            })
            .expect("a web_request for every call");
        let result = page
            .events
            .iter()
            .position(|line| line.event.kind() == "web" && invocation_of(&line.event) == invocation)
            .unwrap();
        assert!(request < result, "the request is recorded before its result");
        match &page.events[request].event {
            NetworkEvent::WebRequest {
                tool: WebTool::WebFetch,
                url,
                ..
            } => assert_eq!(url.as_deref(), Some(RUST_HOME)),
            NetworkEvent::WebRequest {
                tool: WebTool::WebSearch,
                query_sha256,
                url,
                ..
            } => assert!(query_sha256.is_some() && url.is_none()),
            other => panic!("{other:?}"),
        }
    }
    for event in &web {
        let NetworkEvent::Web {
            tool,
            invocation_id,
            activation_id,
            agent,
            source_ids,
            query_sha256,
            content_sha256,
            ..
        } = event
        else {
            unreachable!()
        };
        assert_eq!(activation_id, "parent-activation-1");
        assert_eq!(agent, "parent");
        let invocation = InvocationId::new(invocation_id.clone()).unwrap();
        let expected = match tool {
            WebTool::WebSearch => {
                assert_eq!(source_ids, &[home.clone(), book.clone()]);
                assert!(query_sha256.is_some() && content_sha256.is_none());
                "web_search"
            }
            WebTool::WebFetch => {
                assert_eq!(source_ids, std::slice::from_ref(&home));
                assert!(content_sha256.is_some() && query_sha256.is_none());
                "web_fetch"
            }
        };
        assert_eq!(audited_tool(&fixture.controller, &invocation), expected);
        assert!(succeeded(&fixture.controller, &invocation));
    }

    // The control plane joins them as sources evidence for the activation.
    let view = fixture.controller.control_plane().unwrap();
    let activation = view
        .nodes
        .iter()
        .flat_map(|node| &node.activations)
        .find(|item| {
            matches!(&item.reference, crate::session_control_plane::ControlPlaneActivationRef::Exact { activation }
                if *activation == fixture.parent.input.activation)
        })
        .unwrap();
    let sources = activation
        .evidence
        .iter()
        .find(|evidence| evidence.kind == "sources")
        .expect("sources evidence");
    let crate::session_control_plane::EvidenceValue::Available { value: details } = &sources.details
    else {
        panic!("{:?}", sources.details)
    };
    let rows = details["sources"].as_array().unwrap();
    let row = |id: &str| rows.iter().find(|row| row["source_id"] == id).unwrap().clone();
    assert_eq!(row(&home)["cited"], true);
    assert_eq!(row(&home)["fetched"], true);
    assert_eq!(row(&home)["searched"], true);
    assert!(row(&home)["content_sha256"].is_string());
    assert_eq!(row(&book)["cited"], false);
    assert_eq!(row(&book)["fetched"], false);
    assert_eq!(row(&book)["url"], RUST_BOOK);
    assert!(matches!(
        &sources.summary,
        crate::session_control_plane::EvidenceValue::Available { value } if value == "2 sources (1 read), 1 cited"
    ));
    drop(view);
    records.close("input-session").await;
}

#[tokio::test]
async fn a_record_that_fills_after_the_request_keeps_the_request_and_fails_the_call() {
    let fixture = input_fixture_with_tools(false, &["web_fetch"]);
    let fetcher = Arc::new(FixedFetcher::default());
    let sink = Arc::new(FillingSink {
        room: AtomicUsize::new(1),
        events: Mutex::new(Vec::new()),
    });
    for tool in web_tools("bridge", fetcher.clone(), sink.clone()).host_tools() {
        fixture.controller.register_host_invocation_tool(tool).unwrap();
    }
    start_input(&fixture.controller, &fixture.parent);
    let provider = Arc::new(HostToolProvider::new(
        vec![("web_fetch", serde_json::json!({"url": RUST_HOME}))],
        "could not read",
    ));
    let result = run_with(&fixture.controller, &fixture.parent, provider.clone()).await;
    assert!(result.accepted, "{:?}", result.failure);
    // The page was requested, so its URL is in the record; the result could
    // not be written, so the model gets an error, not the page.
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
    let events = sink.events.lock().unwrap().clone();
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(matches!(
        &events[0],
        NetworkEvent::WebRequest { tool: WebTool::WebFetch, url: Some(url), .. } if url == RUST_HOME
    ));
    let results = provider.results();
    assert!(results[0].contains("network record unavailable"), "{results:?}");
    assert!(!results[0].contains("Rust is fast"), "{results:?}");
}

#[tokio::test]
async fn an_unwritable_record_fails_the_call_before_any_fetch() {
    let fixture = input_fixture_with_tools(false, &["web_fetch"]);
    let fetcher = Arc::new(FixedFetcher::default());
    for tool in web_tools("bridge", fetcher.clone(), Arc::new(FailingSink)).host_tools() {
        fixture.controller.register_host_invocation_tool(tool).unwrap();
    }
    start_input(&fixture.controller, &fixture.parent);
    let provider = Arc::new(HostToolProvider::new(
        vec![("web_fetch", serde_json::json!({"url": RUST_HOME}))],
        "could not read",
    ));
    let result = run_with(&fixture.controller, &fixture.parent, provider.clone()).await;
    assert!(result.accepted, "{:?}", result.failure);
    let results = provider.results();
    assert!(results[0].contains("network record unavailable"), "{results:?}");
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0, "nothing fetched");
}

#[tokio::test]
async fn web_tools_are_refused_under_network_none_and_when_unconfigured() {
    let fixture = input_fixture_with_tools(false, &["web_search"]);
    for tool in web_tools("none", Arc::new(FixedFetcher::default()), Arc::new(FailingSink)).host_tools() {
        fixture.controller.register_host_invocation_tool(tool).unwrap();
    }
    start_input(&fixture.controller, &fixture.parent);
    let provider = Arc::new(HostToolProvider::new(Vec::new(), "unused"));
    let refused = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.parent.input.activation.clone(),
            resources_with(&fixture.parent, provider.clone()),
        )
        .err()
        .unwrap()
        .to_string();
    assert!(refused.contains(NETWORK_NONE_REFUSAL), "{refused}");

    let unconfigured = Arc::new(WebTools::from_parts("bridge", None, None, Arc::new(FailingSink)));
    for tool in unconfigured.host_tools() {
        fixture.controller.register_host_invocation_tool(tool).unwrap();
    }
    let refused = fixture
        .controller
        .prepare_autonomous_activation(
            fixture.parent.input.activation.clone(),
            resources_with(&fixture.parent, provider),
        )
        .err()
        .unwrap()
        .to_string();
    assert!(
        refused.contains("web_search is listed for parent but web_search.provider is not configured"),
        "{refused}"
    );
}

// --- Tools that refuse a daemon or a Ways attempt lane -------------------

/// Calls `tool` once with `arguments`, then answers with text.
struct CallingProvider {
    tool: &'static str,
    arguments: serde_json::Value,
    expect_offered: bool,
    call: bool,
    expect_result: &'static str,
    calls: AtomicUsize,
    results: Mutex<Vec<String>>,
}

impl CallingProvider {
    fn new(
        tool: &'static str,
        expect_offered: bool,
        expect_result: &'static str,
    ) -> Arc<Self> {
        Arc::new(Self {
            tool,
            arguments: serde_json::json!({"url": "http://localhost:8765/"}),
            expect_offered,
            call: expect_offered,
            expect_result,
            calls: AtomicUsize::new(0),
            results: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl LlmProvider for CallingProvider {
    fn provider_id(&self) -> &str {
        "controlled"
    }
    fn model_id(&self) -> &str {
        "controlled-model"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            ..Default::default()
        }
    }
    fn execution_bounds(&self, _: &ChatRequest) -> Option<ProviderExecutionBounds> {
        Some(ProviderExecutionBounds {
            token_limit: 100,
            cost_microunits: 0,
            response_bytes: 8192,
        })
    }
    async fn chat(&self, _: ChatRequest) -> std::result::Result<ChatResponse, ProviderError> {
        unreachable!("the actual autonomous actor streams")
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> std::result::Result<
        Pin<Box<dyn Stream<Item = std::result::Result<StreamEvent, ProviderError>> + Send>>,
        ProviderError,
    > {
        let round = self.calls.fetch_add(1, Ordering::SeqCst);
        let offered = request.tools.iter().any(|tool| tool.name == self.tool);
        assert_eq!(offered, self.expect_offered, "offered tools: {:?}", request.tools.iter().map(|tool| &tool.name).collect::<Vec<_>>());
        let call = self.call && round == 0;
        let mut events = if call {
            vec![Ok(StreamEvent::ToolCallDelta {
                index: Some(0),
                id: "host-tool-call".into(),
                name: Some(self.tool.into()),
                args_delta: self.arguments.to_string(),
            })]
        } else {
            if self.call {
                let result = request
                    .messages
                    .iter()
                    .find(|message| message.tool_call_id.as_deref() == Some("host-tool-call"))
                    .and_then(|message| message.text_content())
                    .expect("the acknowledged host tool result")
                    .to_owned();
                assert!(result.contains(self.expect_result), "{result}");
                self.results.lock().unwrap().push(result);
            }
            vec![Ok(StreamEvent::TextDelta {
                delta: "done".into(),
            })]
        };
        events.push(Ok(StreamEvent::Usage(TokenUsageStats::new(10, 2))));
        events.push(Ok(StreamEvent::Done {
            finish_reason: if call {
                FinishReason::ToolUse
            } else {
                FinishReason::Stop
            },
        }));
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

#[derive(Default)]
struct FakeHostTool {
    refusal: Option<String>,
    attempt_refusal: Option<String>,
    bound: Mutex<Vec<HostInvocationContext>>,
    executed: AtomicUsize,
}

struct BoundFake {
    owner: Arc<FakeHostTool>,
    context: HostInvocationContext,
}

#[async_trait]
impl axocoatl_tools::BuiltinTool for BoundFake {
    fn description(&self) -> &str {
        "fake browser"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, axocoatl_tools::ToolError> {
        self.owner.executed.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::json!({
            "invocation": self.context.invocation_id.as_str(),
            "agent": self.context.agent,
            "url": arguments["url"],
        }))
    }
}

struct FakeRegistration(Arc<FakeHostTool>);

impl HostInvocationTool for FakeRegistration {
    fn name(&self) -> &'static str {
        "browser"
    }
    fn definition(&self) -> Arc<dyn axocoatl_tools::BuiltinTool> {
        Arc::new(axocoatl_tools::BrowserTool::definition())
    }
    fn refusal(&self, _: &ExecutionProfile) -> Option<String> {
        self.0.refusal.clone()
    }
    fn attempt_refusal(&self) -> Option<String> {
        self.0.attempt_refusal.clone()
    }
    fn bind(&self, context: HostInvocationContext) -> Arc<dyn axocoatl_tools::BuiltinTool> {
        self.0.bound.lock().unwrap().push(context.clone());
        Arc::new(BoundFake {
            owner: self.0.clone(),
            context,
        })
    }
}

fn intents(controller: &SessionDispatchController) -> Vec<InvocationId> {
    let state = controller.lock().unwrap();
    state
        .canonical
        .records()
        .unwrap()
        .iter()
        .filter_map(|record| match &record.event {
            TurnContractEvent::RecordIntent { invocation_id, .. } => Some(invocation_id.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_listed_host_tool_is_offered_and_bound_to_the_exact_call() {
    let fixture = input_fixture_with_tools(false, &["browser"]);
    let fake = Arc::new(FakeHostTool::default());
    fixture
        .controller
        .register_host_invocation_tool(Arc::new(FakeRegistration(fake.clone())))
        .unwrap();
    start_input(&fixture.controller, &fixture.parent);
    let mut resources = input_resources(&fixture.parent, InputProvider::new("unused", false, false));
    let provider = CallingProvider::new("browser", true, "\"agent\":\"parent\"");
    resources.provider = provider.clone();
    let result = fixture
        .controller
        .prepare_autonomous_activation(fixture.parent.input.activation.clone(), resources)
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(result.accepted, "{:?}", result.failure);
    assert_eq!(fake.executed.load(Ordering::SeqCst), 1);
    let bound = fake.bound.lock().unwrap().clone();
    assert_eq!(bound.len(), 1);
    let intents = intents(&fixture.controller);
    assert_eq!(intents.len(), 1);
    assert_eq!(bound[0].invocation_id, intents[0]);
    assert_eq!(bound[0].activation, fixture.parent.input.activation);
    assert_eq!(bound[0].session_id, "input-session");
    assert!(!bound[0].read_only);
    assert!(bound[0].checkout.is_none());
    assert!(!bound[0].attempt);
    assert!(provider.results.lock().unwrap()[0].contains(intents[0].as_str()));
    let state = fixture.controller.lock().unwrap();
    assert!(state.canonical.records().unwrap().iter().any(|record| matches!(
        &record.event,
        TurnContractEvent::RecordOutcome { outcome: InvocationOutcome::Succeeded, invocation_id, .. }
            if *invocation_id == intents[0]
    )));
}

#[tokio::test]
async fn an_unlisted_host_tool_is_not_offered() {
    let fixture = input_fixture_with_tools(false, &["effect"]);
    let fake = Arc::new(FakeHostTool::default());
    fixture
        .controller
        .register_host_invocation_tool(Arc::new(FakeRegistration(fake.clone())))
        .unwrap();
    start_input(&fixture.controller, &fixture.parent);
    let mut resources = input_resources(&fixture.parent, InputProvider::new("unused", false, false));
    resources.provider = CallingProvider::new("browser", false, "");
    let result = fixture
        .controller
        .prepare_autonomous_activation(fixture.parent.input.activation.clone(), resources)
        .unwrap()
        .run()
        .await
        .unwrap();
    assert!(result.accepted, "{:?}", result.failure);
    assert!(fake.bound.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_listed_host_tool_the_daemon_cannot_run_fails_preparation_with_its_reason() {
    let fixture = input_fixture_with_tools(false, &["browser"]);
    let fake = Arc::new(FakeHostTool {
        refusal: Some("the browser block is not configured".into()),
        ..Default::default()
    });
    fixture
        .controller
        .register_host_invocation_tool(Arc::new(FakeRegistration(fake.clone())))
        .unwrap();
    start_input(&fixture.controller, &fixture.parent);
    let mut resources = input_resources(&fixture.parent, InputProvider::new("unused", false, false));
    let provider = CallingProvider::new("browser", false, "");
    resources.provider = provider.clone();
    let refused = fixture
        .controller
        .prepare_autonomous_activation(fixture.parent.input.activation.clone(), resources)
        .err()
        .expect("refused")
        .to_string();
    assert!(refused.contains("the browser block is not configured"), "{refused}");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0, "no model call");
    assert!(fake.bound.lock().unwrap().is_empty());
    assert_eq!(fake.executed.load(Ordering::SeqCst), 0);
    assert!(intents(&fixture.controller).is_empty(), "nothing is recorded");
}

#[test]
fn a_tool_that_refuses_attempt_lanes_is_offered_only_outside_them() {
    let fixture = input_fixture_with_tools(false, &["browser"]);
    let fake = Arc::new(FakeHostTool {
        attempt_refusal: Some("not in a Ways attempt".into()),
        ..Default::default()
    });
    fixture
        .controller
        .register_host_invocation_tool(Arc::new(FakeRegistration(fake.clone())))
        .unwrap();
    let profile = ExecutionProfile {
        definition: "parent".into(),
        provider: "controlled".into(),
        model: "controlled-model".into(),
        isolation: "in-process".into(),
        tools: vec!["browser".into()],
        write_scope: None,
    };
    let state = fixture.controller.lock().unwrap();
    let offered = |attempt| {
        state
            .host_tool_definitions(&profile, attempt)
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
    };
    assert_eq!(offered(false), vec!["browser"]);
    assert!(offered(true).is_empty());
    // Left out of an attempt, it is no reason to refuse the activation.
    assert_eq!(state.host_tool_refusal(&profile, false), None);
    assert_eq!(state.host_tool_refusal(&profile, true), None);
    // An activation without a repository, or with the Session's own, is
    // not an attempt.
    assert!(!crate::session_dispatch::host_tools::bound_to_attempt(None));
}

#[test]
fn only_host_invocation_names_can_be_registered() {
    struct Named;
    impl HostInvocationTool for Named {
        fn name(&self) -> &'static str {
            "bash"
        }
        fn definition(&self) -> Arc<dyn axocoatl_tools::BuiltinTool> {
            Arc::new(axocoatl_tools::BrowserTool::definition())
        }
        fn refusal(&self, _: &ExecutionProfile) -> Option<String> {
            None
        }
        fn bind(&self, _: HostInvocationContext) -> Arc<dyn axocoatl_tools::BuiltinTool> {
            Arc::new(axocoatl_tools::BrowserTool::definition())
        }
    }
    let fixture = input_fixture();
    assert!(fixture
        .controller
        .register_host_invocation_tool(Arc::new(Named))
        .is_err());
    crate::session_dispatch::validate_repository_tools(&[
        "read_file".into(),
        "browser".into(),
        "browser_check".into(),
    ])
    .unwrap();
    assert!(crate::session_dispatch::validate_repository_tools(&["selenium".into()]).is_err());
}
