use super::*;
use axocoatl_session::turn_contract::{
    ActivationId, ActivationRef, ExecutionEpochId, InvocationId, LogicalTurnId, SessionId,
    TurnNodeId,
};

struct NullSink;

#[async_trait]
impl WebRecordSink for NullSink {
    async fn append(&self, _session: &str, _event: NetworkEvent) -> Result<u64, String> {
        Ok(1)
    }
}

fn context() -> HostInvocationContext {
    HostInvocationContext {
        session_id: "session".into(),
        invocation_id: InvocationId::new("tool-0123").unwrap(),
        activation: ActivationRef {
            session_id: SessionId::new("session").unwrap(),
            turn_id: LogicalTurnId::new("turn").unwrap(),
            execution_epoch_id: ExecutionEpochId::new("epoch").unwrap(),
            node_id: TurnNodeId::new("lead").unwrap(),
            generation: 1,
            activation_id: ActivationId::new("lead-activation-1").unwrap(),
        },
        agent: "researcher".into(),
        read_only: true,
    }
}

fn profile(tools: &[&str]) -> ExecutionProfile {
    ExecutionProfile {
        definition: "researcher".into(),
        provider: "ollama".into(),
        model: "m".into(),
        isolation: "in-process".into(),
        tools: tools.iter().map(|tool| (*tool).to_string()).collect(),
        write_scope: Some(Vec::new()),
    }
}

fn config(yaml: &str) -> AxocoatlConfig {
    axocoatl_config::parse_config(yaml, std::path::Path::new("test.yaml")).unwrap()
}

fn tools(yaml: &str) -> (tempfile::TempDir, Arc<WebTools>) {
    let root = tempfile::tempdir().unwrap();
    let data = SecureDir::open(root.path()).unwrap();
    let tools =
        WebTools::from_config(&config(yaml), &data, "test-authority", Arc::new(NullSink)).unwrap();
    (root, Arc::new(tools))
}

fn refusals(tools: &Arc<WebTools>) -> (Option<String>, Option<String>) {
    let hosts = tools.host_tools();
    let profile = profile(&["web_search", "web_fetch"]);
    (hosts[0].refusal(&profile), hosts[1].refusal(&profile))
}

#[test]
fn configuration_decides_which_web_tools_native_sessions_get() {
    let (_root, none) = tools("{}");
    let (search, fetch) = refusals(&none);
    assert!(search
        .unwrap()
        .contains("web_search is listed for researcher but web_search.provider is not configured"));
    assert!(fetch
        .unwrap()
        .contains("web_fetch is listed for researcher but web_fetch is not configured"));
    let lists_fetch = ["read_file".to_string(), "web_fetch".to_string()];
    assert!(none.legacy_search_tool().is_none() && none.legacy_fetch_tool(&lists_fetch).is_none());

    let (_root, managed) =
        tools("web_search:\n  provider: searxng\nweb_fetch:\n  max_bytes: 65536\n");
    assert_eq!(refusals(&managed), (None, None));
    assert!(managed.managed_searxng().is_some());
    assert_eq!(
        managed.legacy_search_tool().unwrap().backend_name(),
        "searxng"
    );
    // Legacy web_fetch goes only to an Agent that names it: an empty list,
    // which inherits the whole legacy executor, does not gain it.
    assert!(managed.legacy_fetch_tool(&lists_fetch).is_some());
    assert!(managed.legacy_fetch_tool(&[]).is_none());
    assert!(managed
        .legacy_fetch_tool(&["read_file".to_string()])
        .is_none());

    let (_root, unmanaged) = tools(
        "web_search:\n  provider: searxng\n  searxng:\n    managed: false\n    url: http://127.0.0.1:8888\n",
    );
    assert_eq!(refusals(&unmanaged).0, None);
    assert!(unmanaged.managed_searxng().is_none());

    let (_root, tavily) = tools("web_search:\n  provider: tavily\n  api_key: test-key\n");
    let (search, _) = refusals(&tavily);
    assert!(search
        .unwrap()
        .contains("tavily, which only legacy Sessions use"));
    assert_eq!(
        tavily.legacy_search_tool().unwrap().backend_name(),
        "tavily"
    );

    let (_root, offline) =
        tools("sandbox:\n  network: none\nweb_search:\n  provider: searxng\nweb_fetch: {}\n");
    let (search, fetch) = refusals(&offline);
    assert_eq!(search.as_deref(), Some(NETWORK_NONE_REFUSAL));
    assert_eq!(fetch.as_deref(), Some(NETWORK_NONE_REFUSAL));
    // Legacy Sessions never get web_fetch under network: none, even when an
    // Agent lists it; legacy web_search keeps its 1.0 behavior.
    assert!(offline.legacy_fetch_tool(&lists_fetch).is_none());
    assert!(offline.legacy_search_tool().is_some());
}

#[test]
fn host_tool_definitions_describe_but_never_run() {
    let (_root, managed) = tools("web_search:\n  provider: searxng\nweb_fetch: {}\n");
    let hosts = managed.host_tools();
    assert_eq!(hosts[0].name(), "web_search");
    assert_eq!(hosts[1].name(), "web_fetch");
    let search = hosts[0].definition();
    assert!(search.description().contains("cite claims as [S1a2b3c4d]"));
    let fetch = hosts[1].definition();
    assert!(fetch.description().contains("[S1a2b3c4d ¶n]"));
    assert_eq!(fetch.parameters_schema()["additionalProperties"], false);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let error = runtime
        .block_on(fetch.execute(serde_json::json!({"url": "https://example.com/"})))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("only through its admitted invocation"),
        "{error}"
    );
}

#[test]
fn citations_are_source_ids_inside_brackets() {
    let text = "Fast [S1a2b3c4d ¶3], safe [S5E6F7A8B, S00000000] and \
                more [see S99999999]. Not cited: S12345678, [XS11111111], [S1234567], \
                [S123456789], [unterminated S22222222";
    let cited: Vec<String> = cited_source_ids(text).into_iter().collect();
    assert_eq!(cited, ["S00000000", "S1a2b3c4d", "S5e6f7a8b", "S99999999"]);
    assert!(cited_source_ids("").is_empty());
    assert!(cited_source_ids("[]").is_empty());
    // Non-ASCII around a citation is fine.
    assert!(cited_source_ids("é [Sabcdef01 ¶1] ü").contains("Sabcdef01"));
}

#[test]
fn search_events_carry_hashes_and_sources_not_the_query() {
    let report = WebSearchReport {
        backend: "searxng".into(),
        query_sha256: Some("ab".repeat(32)),
        query_bytes: Some(11),
        results: Some(2),
        unresponsive_engines: (0..40)
            .map(|i| format!("engine-{i} ({})", "x".repeat(200)))
            .collect(),
        sources: vec![
            ("S1a2b3c4d".into(), "https://example.com/a".into()),
            (
                "S5e6f7a8b".into(),
                format!("https://example.com/{}", "p".repeat(5000)),
            ),
        ],
        retrieved_at_ms: 7,
        refused: false,
        reason: None,
    };
    let event = search_event(&context(), &report, Duration::from_millis(42));
    event.validate().unwrap();
    let NetworkEvent::Web {
        tool,
        invocation_id,
        activation_id,
        agent,
        decision,
        query_sha256,
        query_bytes,
        unresponsive_engines,
        source_ids,
        sources,
        url,
        ms,
        ..
    } = &event
    else {
        unreachable!()
    };
    assert_eq!(*tool, WebTool::WebSearch);
    assert_eq!(invocation_id, "tool-0123");
    assert_eq!(activation_id, "lead-activation-1");
    assert_eq!(agent, "researcher");
    assert_eq!(*decision, Decision::Allow);
    assert_eq!(query_sha256.as_deref(), Some("ab".repeat(32).as_str()));
    assert_eq!(*query_bytes, Some(11));
    assert_eq!(source_ids, &["S1a2b3c4d", "S5e6f7a8b"]);
    assert_eq!(sources[1].url.len(), MAX_RECORDED_SOURCE_URL_BYTES);
    assert!(sources[1].url_truncated && !sources[0].url_truncated);
    assert_eq!(unresponsive_engines.len(), RECORDED_UNRESPONSIVE_MAX);
    assert!(url.is_none());
    assert_eq!(*ms, 42);
    // No query text anywhere in the line.
    let line = serde_json::to_string(&event).unwrap();
    assert!(!line.contains("query\""), "{line}");

    let refused = search_event(
        &context(),
        &WebSearchReport {
            refused: true,
            reason: Some("invalid_arguments".into()),
            ..WebSearchReport::default()
        },
        Duration::ZERO,
    );
    assert!(matches!(
        refused,
        NetworkEvent::Web { decision: Decision::Deny, reason: Some(ref reason), .. } if reason == "invalid_arguments"
    ));
}

#[test]
fn the_largest_search_and_fetch_events_fit_one_record_line() {
    let search = WebSearchReport {
        backend: "searxng".into(),
        query_sha256: Some("ab".repeat(32)),
        query_bytes: Some(8192),
        results: Some(15),
        unresponsive_engines: (0..64).map(|_| "é".repeat(400)).collect(),
        sources: (0..15)
            .map(|i| {
                (
                    format!("S{i:08x}"),
                    format!("https://example.com/{}", "é".repeat(4000)),
                )
            })
            .collect(),
        retrieved_at_ms: u64::MAX,
        refused: false,
        reason: Some("r".repeat(10_000)),
    };
    let mut big = context();
    big.agent = "a".repeat(10_000);
    let event = search_event(&big, &search, Duration::MAX);
    let bytes = serde_json::to_vec(&event).unwrap().len();
    assert!(bytes + 64 < MAX_LINE_BYTES, "{bytes}");

    let long = format!("https://example.com/{}", "é".repeat(4000));
    let fetch = WebFetchReport {
        url: Some(long.clone()),
        final_url: Some(long.clone()),
        status: Some(200),
        redirects: vec![long.clone(); 9],
        source_id: Some("S1a2b3c4d".into()),
        bytes: Some(8 << 20),
        content_sha256: Some("cd".repeat(32)),
        text_sha256: Some("ef".repeat(32)),
        retrieved_at_ms: u64::MAX,
        refused: false,
        reason: None,
    };
    let event = fetch_event(&big, &fetch, Duration::MAX);
    event.validate().unwrap();
    let bytes = serde_json::to_vec(&event).unwrap().len();
    assert!(bytes + 64 < MAX_LINE_BYTES, "{bytes}");
    let NetworkEvent::Web {
        redirects,
        url,
        url_truncated,
        final_url_truncated,
        redirects_truncated,
        source_ids,
        ..
    } = &event
    else {
        unreachable!()
    };
    assert_eq!(redirects.len(), axocoatl_tools::fetch_guard::MAX_REDIRECTS);
    assert!(url.as_ref().unwrap().len() <= MAX_RECORDED_WEB_URL_BYTES);
    assert!(*url_truncated && *final_url_truncated && *redirects_truncated);
    assert_eq!(source_ids, &["S1a2b3c4d"]);

    // The write-ahead request events are bounded the same way.
    let request = fetch_request_event(&big, &long);
    request.validate().unwrap();
    let bytes = serde_json::to_vec(&request).unwrap().len();
    assert!(bytes + 64 < MAX_LINE_BYTES, "{bytes}");
    assert!(matches!(
        &request,
        NetworkEvent::WebRequest { url: Some(url), url_truncated: true, .. }
            if url.len() <= MAX_RECORDED_WEB_URL_BYTES
    ));
}

#[test]
fn a_cut_url_is_flagged_and_a_short_one_is_not() {
    let exfiltration = format!("https://collector.example/?d={}", "A".repeat(8 * 1024));
    let report = WebFetchReport {
        url: Some(exfiltration.clone()),
        final_url: Some("https://collector.example/ok".into()),
        status: Some(200),
        redirects: vec!["https://collector.example/ok".into()],
        source_id: Some("S1a2b3c4d".into()),
        bytes: Some(2),
        content_sha256: Some("cd".repeat(32)),
        text_sha256: Some("ef".repeat(32)),
        retrieved_at_ms: 1,
        refused: false,
        reason: None,
    };
    let event = fetch_event(&context(), &report, Duration::ZERO);
    let line = serde_json::to_string(&event).unwrap();
    assert!(line.contains("\"url_truncated\":true"), "{line}");
    assert!(!line.contains("final_url_truncated"), "{line}");
    assert!(!line.contains("redirects_truncated"), "{line}");
    let NetworkEvent::Web { url, .. } = &event else {
        unreachable!()
    };
    assert_eq!(url.as_ref().unwrap().len(), MAX_RECORDED_WEB_URL_BYTES);
    // A short URL carries no flag at all.
    let short = fetch_event(
        &context(),
        &WebFetchReport {
            url: Some("https://example.com/".into()),
            ..report
        },
        Duration::ZERO,
    );
    assert!(!serde_json::to_string(&short).unwrap().contains("truncated"));
    let request = fetch_request_event(&context(), "https://example.com/");
    assert!(!serde_json::to_string(&request)
        .unwrap()
        .contains("truncated"));
}

#[test]
fn ways_attempts_withhold_the_web_tools() {
    let withheld = withheld_web_tools();
    let names: Vec<&str> = withheld.iter().map(|tool| tool.name()).collect();
    assert_eq!(names, ["web_search", "web_fetch"]);
    let listed = profile(&["web_search", "web_fetch"]);
    for tool in &withheld {
        // Withheld: not a reason to stop an activation that lists it...
        assert!(tool.withheld());
        // ...but a call to it anyway is declined with this reason.
        let reason = tool.refusal(&listed).unwrap();
        assert!(
            reason.starts_with(tool.name()) && reason.contains("Explore several ways"),
            "{reason}"
        );
    }
    let registered = withheld
        .iter()
        .map(|tool| (tool.name(), tool.clone()))
        .collect();
    assert_eq!(
        crate::session_dispatch::host_tool_refusal(&registered, &listed),
        None
    );
    // The daemon's own web tools are never withheld.
    let (_root, managed) = tools("web_search:\n  provider: searxng\nweb_fetch: {}\n");
    assert!(managed.host_tools().iter().all(|tool| !tool.withheld()));
}

#[derive(Default)]
struct MemorySink {
    events: std::sync::Mutex<Vec<NetworkEvent>>,
}

#[async_trait]
impl WebRecordSink for MemorySink {
    async fn append(&self, _session: &str, event: NetworkEvent) -> Result<u64, String> {
        let mut events = self.events.lock().unwrap();
        events.push(event);
        Ok(events.len() as u64)
    }
}

/// Removes the test's SearXNG container by its unique name, also when an
/// assertion fails.
struct RemoveContainer(String);

impl Drop for RemoveContainer {
    fn drop(&mut self) {
        let _ = std::process::Command::new("podman")
            .args([
                "rm",
                "--force",
                "--volumes",
                "--time",
                "0",
                "--ignore",
                &self.0,
            ])
            .output();
    }
}

/// The real tools from configuration, a managed SearXNG and real public
/// pages: search, then read the first result.
#[tokio::test]
#[ignore = "requires Podman and the internet: AXOCOATL_LIVE_WEB=1 CONTAINER_CONNECTION=axocoatl-ci-pr74 cargo test -p axocoatl-daemon --lib live_web -- --ignored"]
async fn live_web_search_and_fetch_through_the_managed_searxng() {
    if std::env::var("AXOCOATL_LIVE_WEB").as_deref() != Ok("1") {
        eprintln!("skipped: set AXOCOATL_LIVE_WEB=1");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let data = SecureDir::open(root.path().canonicalize().unwrap()).unwrap();
    let authority = format!(
        "web-live-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let _remove = RemoveContainer(axocoatl_isolation::searxng::container_name(&authority));
    let sink = Arc::new(MemorySink::default());
    let tools = Arc::new(
        WebTools::from_config(
            &config("web_search:\n  provider: searxng\nweb_fetch: {}\n"),
            &data,
            &authority,
            sink.clone(),
        )
        .unwrap(),
    );
    let hosts = tools.host_tools();
    let started = Instant::now();
    let search = hosts[0]
        .bind(context())
        .execute(serde_json::json!({"query": "rust programming language", "max_results": 5}))
        .await
        .unwrap();
    let search_ms = started.elapsed().as_millis();
    let results = search["results"].as_array().unwrap();
    assert!(!results.is_empty(), "{search}");
    let first = results
        .iter()
        .find(|result| {
            result["url"]
                .as_str()
                .is_some_and(|url| url.starts_with("https://"))
        })
        .expect("an https result");
    let url = first["url"].as_str().unwrap();
    let started = Instant::now();
    let page = hosts[1]
        .bind(context())
        .execute(serde_json::json!({"url": url, "max_chars": 4000}))
        .await
        .unwrap();
    let fetch_ms = started.elapsed().as_millis();
    assert_eq!(page["source_id"], first["source_id"]);
    assert!(
        page["content"].as_str().unwrap().starts_with("¶1 "),
        "{page}"
    );
    let events = sink.events.lock().unwrap().clone();
    let kinds: Vec<&str> = events.iter().map(NetworkEvent::kind).collect();
    assert_eq!(kinds, ["web_request", "web", "web_request", "web"]);
    assert!(matches!(
        &events[1],
        NetworkEvent::Web {
            tool: WebTool::WebSearch,
            decision: Decision::Allow,
            query_sha256: Some(_),
            ..
        }
    ));
    assert!(matches!(
        &events[3],
        NetworkEvent::Web {
            tool: WebTool::WebFetch,
            decision: Decision::Allow,
            content_sha256: Some(_),
            status: Some(_),
            ..
        }
    ));
    eprintln!(
        "live web: search {search_ms} ms ({} results, unresponsive {}), fetch of {url} {fetch_ms} ms, {} paragraphs, status {}",
        results.len(),
        search["unresponsive_engines"],
        page["paragraphs"]["total"],
        page["status"]
    );
    tools.stop().await;
    let exists = std::process::Command::new("podman")
        .args([
            "container",
            "exists",
            &axocoatl_isolation::searxng::container_name(&authority),
        ])
        .status()
        .unwrap();
    assert!(!exists.success(), "stop removed the container");
}

#[test]
fn a_projection_reads_only_allowed_web_events_of_its_own_activations() {
    let allowed = search_event(
        &context(),
        &WebSearchReport {
            query_sha256: Some("ab".repeat(32)),
            query_bytes: Some(1),
            ..WebSearchReport::default()
        },
        Duration::ZERO,
    );
    let refused = search_event(
        &context(),
        &WebSearchReport {
            refused: true,
            reason: Some("invalid_arguments".into()),
            ..WebSearchReport::default()
        },
        Duration::ZERO,
    );
    let request = fetch_request_event(&context(), "https://example.com/");
    let mine: HashSet<String> = ["lead-activation-1".to_string()].into();
    let other: HashSet<String> = ["helper-activation-1".to_string()].into();
    assert!(is_web_event_for(&allowed, &mine));
    assert!(!is_web_event_for(&allowed, &other));
    assert!(!is_web_event_for(&refused, &mine));
    assert!(!is_web_event_for(&request, &mine));
}
