//! Native `web_search` and `web_fetch` for Session Agents.
//!
//! Both tools run on the host, in the daemon, never in the Session container.
//! An Agent gets one only when its own `tools` list names it; a read-only
//! helper or required reviewer is no exception, and a lead cannot grant one
//! to an ad hoc helper, whose list comes from its approved template.
//!
//! Every call appends a `web` event to the Session's network record with the
//! invocation, activation and Agent that made it, the decision, the source
//! ids it returned and the hashes of what it read. A search records the
//! SHA-256 and length of its query, not the query text, which is already in
//! the audited call arguments. If the record cannot be written, the call
//! fails, so the model never receives a page or result the record lacks.
//!
//! The Session control plane joins those events to activations as `sources`
//! evidence, marking each source the activation's final output cites.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axocoatl_config::AxocoatlConfig;
use axocoatl_core::SecureDir;
use axocoatl_isolation::searxng::{SearxngService, SearxngSettings, SEARXNG_IMAGE};
use axocoatl_session::control_authority::ExecutionProfile;
use axocoatl_session::network_record::{
    Decision, NetworkEvent, NetworkLine, WebSource, WebTool, MAX_LINE_BYTES,
    MAX_RECORDED_SOURCE_URL_BYTES, MAX_RECORDED_WEB_URL_BYTES,
};
use axocoatl_tools::{
    BuiltinTool, FixedSearxngEndpoint, SearxngEndpoint, SearxngQuerySettings, ToolError,
    WebFetchReport, WebFetchTool, WebSearchReport, WebSearchTool,
};

use crate::session_control_plane::{
    ControlPlaneActivationRef, ControlPlaneEvidence, EvidenceValue, SessionTurnControlPlane,
};
use crate::session_dispatch::{HostInvocationContext, HostInvocationTool, HostToolDefinition};

/// Refusal for any web tool when Session containers have no network.
pub const NETWORK_NONE_REFUSAL: &str = "web tools reach the internet from your computer; this \
     daemon runs Sessions with network: none";
/// Most entries recorded from a search's unresponsive engines.
const RECORDED_UNRESPONSIVE_MAX: usize = 16;
const RECORDED_UNRESPONSIVE_BYTES: usize = 96;
const RECORDED_REASON_BYTES: usize = 128;
const RECORDED_AGENT_BYTES: usize = 256;
/// Most sources one activation's `sources` evidence lists.
const EVIDENCE_SOURCES_MAX: usize = 200;
/// Most web events read from a record for one projection.
const PROJECTION_WEB_EVENTS_MAX: usize = 20_000;

/// Where `web` events are written.
#[async_trait]
pub(crate) trait WebRecordSink: Send + Sync {
    /// Fail when the Session's record cannot take another event, before the
    /// tool does any work.
    async fn writable(&self, session: &str) -> Result<(), String>;
    async fn append(&self, session: &str, event: NetworkEvent) -> Result<u64, String>;
}

#[async_trait]
impl WebRecordSink for crate::session_network::SessionNetworkRecords {
    async fn writable(&self, session: &str) -> Result<(), String> {
        let stats = self
            .stats(session)
            .await
            .map_err(|error| error.to_string())?;
        if stats.full {
            return Err(format!(
                "the Session's network record is full ({} events)",
                stats.events
            ));
        }
        Ok(())
    }

    async fn append(&self, session: &str, event: NetworkEvent) -> Result<u64, String> {
        crate::session_network::SessionNetworkRecords::append(self, session, event)
            .await
            .map_err(|error| error.to_string())
    }
}

/// A managed SearXNG as the search tool's endpoint.
struct ManagedSearxng(Arc<SearxngService>);

#[async_trait]
impl SearxngEndpoint for ManagedSearxng {
    async fn base_url(&self) -> Result<String, String> {
        self.0.base_url().await.map_err(|error| error.to_string())
    }
}

/// The daemon's web tools, built once from configuration.
pub(crate) struct WebTools {
    network: String,
    /// The search tool native Sessions may use (SearXNG only).
    search: Option<Arc<WebSearchTool>>,
    /// Why native Sessions have no search, with `{agent}` to fill in.
    search_unavailable: String,
    /// The search tool legacy Sessions get: SearXNG, or a configured Tavily.
    legacy_search: Option<Arc<WebSearchTool>>,
    fetch: Option<Arc<WebFetchTool>>,
    searxng: Option<Arc<SearxngService>>,
    records: Arc<dyn WebRecordSink>,
}

impl std::fmt::Debug for WebTools {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebTools")
            .field("network", &self.network)
            .field("search", &self.search.is_some())
            .field("fetch", &self.fetch.is_some())
            .field("managed_searxng", &self.searxng.is_some())
            .finish_non_exhaustive()
    }
}

impl WebTools {
    pub(crate) fn from_config(
        config: &AxocoatlConfig,
        data_root: &SecureDir,
        runtime_authority: &str,
        records: Arc<dyn WebRecordSink>,
    ) -> Result<Self, String> {
        let mut searxng = None;
        let mut search = None;
        let mut legacy_search = None;
        let search_unavailable;
        match config.web_search.as_ref() {
            Some(web) if web.provider == "searxng" => {
                let settings = web.searxng.clone().unwrap_or_default();
                let query = SearxngQuerySettings {
                    language: settings.language.clone(),
                    safesearch: settings.safesearch,
                    timeout: Duration::from_secs(settings.timeout_secs),
                };
                let tool = if settings.managed {
                    let service = Arc::new(
                        SearxngService::new(
                            runtime_authority.to_string(),
                            settings
                                .image
                                .clone()
                                .unwrap_or_else(|| SEARXNG_IMAGE.to_string()),
                            SearxngSettings {
                                engines: settings.engines.clone(),
                                language: settings.language.clone(),
                                safesearch: settings.safesearch,
                                timeout_secs: settings.timeout_secs,
                            },
                            data_root,
                        )
                        .map_err(|error| format!("preparing the managed SearXNG: {error}"))?,
                    );
                    searxng = Some(service.clone());
                    WebSearchTool::searxng(Arc::new(ManagedSearxng(service)), query, false)?
                } else {
                    let url = settings.url.clone().unwrap_or_default();
                    WebSearchTool::searxng(Arc::new(FixedSearxngEndpoint::new(url)), query, true)?
                };
                let tool = Arc::new(tool);
                search = Some(tool.clone());
                legacy_search = Some(tool);
                search_unavailable = String::new();
            }
            Some(web) if web.provider == "tavily" => {
                legacy_search = Some(Arc::new(WebSearchTool::from_config(
                    &web.provider,
                    web.api_key.expose_secret(),
                )));
                search_unavailable = "web_search is listed for {agent} but web_search.provider \
                                      is tavily, which only legacy Sessions use; set \
                                      web_search.provider: searxng"
                    .to_string();
            }
            Some(web) if !web.provider.is_empty() => {
                legacy_search = Some(Arc::new(WebSearchTool::from_config(
                    &web.provider,
                    web.api_key.expose_secret(),
                )));
                search_unavailable = format!(
                    "web_search is listed for {{agent}} but web_search.provider {:?} is not \
                     supported; set web_search.provider: searxng",
                    web.provider
                );
            }
            _ => {
                search_unavailable = "web_search is listed for {agent} but \
                                      web_search.provider is not configured; set \
                                      web_search.provider: searxng"
                    .to_string();
            }
        }
        let fetch = match config.web_fetch.as_ref() {
            Some(fetch) => Some(Arc::new(WebFetchTool::from_config(
                fetch.max_bytes,
                Duration::from_secs(fetch.timeout_secs),
            )?)),
            None => None,
        };
        Ok(Self {
            network: config.sandbox.network.clone(),
            search,
            search_unavailable,
            legacy_search,
            fetch,
            searxng,
            records,
        })
    }

    /// Explicit parts, for tests.
    #[cfg(test)]
    pub(crate) fn from_parts(
        network: &str,
        search: Option<Arc<WebSearchTool>>,
        fetch: Option<Arc<WebFetchTool>>,
        records: Arc<dyn WebRecordSink>,
    ) -> Self {
        Self {
            network: network.into(),
            legacy_search: search.clone(),
            search,
            search_unavailable: "web_search is listed for {agent} but web_search.provider is \
                                 not configured; set web_search.provider: searxng"
                .into(),
            fetch,
            searxng: None,
            records,
        }
    }

    /// The host tools for a native Session controller.
    pub(crate) fn host_tools(self: &Arc<Self>) -> Vec<Arc<dyn HostInvocationTool>> {
        vec![
            Arc::new(WebSearchHost(self.clone())),
            Arc::new(WebFetchHost(self.clone())),
        ]
    }

    /// `web_search` for a legacy (1.0-format) Session. Legacy Sessions have
    /// no network record, so nothing is recorded.
    pub(crate) fn legacy_search_tool(&self) -> Option<Arc<WebSearchTool>> {
        self.legacy_search.clone()
    }

    /// `web_fetch` for a legacy Session.
    pub(crate) fn legacy_fetch_tool(&self) -> Option<Arc<WebFetchTool>> {
        self.fetch.clone()
    }

    /// The managed SearXNG, when configured.
    pub(crate) fn managed_searxng(&self) -> Option<Arc<SearxngService>> {
        self.searxng.clone()
    }

    /// Stop the managed SearXNG, if it runs.
    pub(crate) async fn stop(&self) {
        if let Some(searxng) = &self.searxng {
            searxng.stop().await;
        }
    }

    fn network_refusal(&self) -> Option<String> {
        (self.network == "none").then(|| NETWORK_NONE_REFUSAL.to_string())
    }

    fn search_refusal(&self, profile: &ExecutionProfile) -> Option<String> {
        if self.search.is_none() {
            return Some(
                self.search_unavailable
                    .replace("{agent}", &profile.definition),
            );
        }
        self.network_refusal()
    }

    fn fetch_refusal(&self, profile: &ExecutionProfile) -> Option<String> {
        if self.fetch.is_none() {
            return Some(format!(
                "web_fetch is listed for {} but web_fetch is not configured; add a web_fetch: \
                 block to the configuration",
                profile.definition
            ));
        }
        self.network_refusal()
    }
}

fn record_unavailable(tool: &str, reason: String) -> ToolError {
    ToolError::ExecutionFailed {
        tool: tool.to_string(),
        reason: format!(
            "network record unavailable: {reason}; Axocoatl does not return web results it \
             cannot record"
        ),
    }
}

/// Refusal for web tools in Explore several ways attempts.
pub const WAYS_REFUSAL: &str = "is withheld from Explore several ways attempts, like other \
     tools that reach outside the attempt; run this Agent in the Session itself to use it";

/// Host tools that refuse every web call, for controllers whose activations
/// must not reach the web (Explore several ways attempts).
pub(crate) fn withheld_web_tools() -> Vec<Arc<dyn HostInvocationTool>> {
    ["web_search", "web_fetch"]
        .into_iter()
        .map(|name| Arc::new(WithheldWebTool(name)) as Arc<dyn HostInvocationTool>)
        .collect()
}

struct WithheldWebTool(&'static str);

impl HostInvocationTool for WithheldWebTool {
    fn name(&self) -> &'static str {
        self.0
    }

    fn definition(&self) -> Arc<dyn BuiltinTool> {
        Arc::new(HostToolDefinition::new(
            self.0,
            "unavailable in this attempt",
            serde_json::json!({"type": "object"}),
            axocoatl_llm::ConcurrencyPolicy::Safe,
        ))
    }

    fn refusal(&self, _profile: &ExecutionProfile) -> Option<String> {
        Some(format!("{} {WAYS_REFUSAL}", self.0))
    }

    fn bind(&self, _context: HostInvocationContext) -> Arc<dyn BuiltinTool> {
        Arc::new(RefusedTool {
            name: self.0,
            reason: format!("{} {WAYS_REFUSAL}", self.0),
        })
    }
}

/// A tool that only refuses, bound when a refused tool is admitted anyway.
struct RefusedTool {
    name: &'static str,
    reason: String,
}

#[async_trait]
impl BuiltinTool for RefusedTool {
    fn description(&self) -> &str {
        "unavailable"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _arguments: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        Err(ToolError::ExecutionFailed {
            tool: self.name.into(),
            reason: self.reason.clone(),
        })
    }
}

struct WebSearchHost(Arc<WebTools>);

impl HostInvocationTool for WebSearchHost {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn definition(&self) -> Arc<dyn BuiltinTool> {
        Arc::new(HostToolDefinition::new(
            "web_search",
            axocoatl_tools::web_search_description(),
            axocoatl_tools::web_search_schema(),
            axocoatl_llm::ConcurrencyPolicy::Safe,
        ))
    }

    fn refusal(&self, profile: &ExecutionProfile) -> Option<String> {
        self.0.search_refusal(profile)
    }

    fn bind(&self, context: HostInvocationContext) -> Arc<dyn BuiltinTool> {
        match &self.0.search {
            Some(tool) => Arc::new(BoundWebSearch {
                tool: tool.clone(),
                records: self.0.records.clone(),
                context,
            }),
            None => Arc::new(RefusedTool {
                name: "web_search",
                reason: self.0.search_unavailable.replace("{agent}", &context.agent),
            }),
        }
    }
}

struct WebFetchHost(Arc<WebTools>);

impl HostInvocationTool for WebFetchHost {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn definition(&self) -> Arc<dyn BuiltinTool> {
        Arc::new(HostToolDefinition::new(
            "web_fetch",
            axocoatl_tools::web_fetch_description(),
            axocoatl_tools::web_fetch_schema(),
            axocoatl_llm::ConcurrencyPolicy::Safe,
        ))
    }

    fn refusal(&self, profile: &ExecutionProfile) -> Option<String> {
        self.0.fetch_refusal(profile)
    }

    fn bind(&self, context: HostInvocationContext) -> Arc<dyn BuiltinTool> {
        match &self.0.fetch {
            Some(tool) => Arc::new(BoundWebFetch {
                tool: tool.clone(),
                records: self.0.records.clone(),
                context,
            }),
            None => Arc::new(RefusedTool {
                name: "web_fetch",
                reason: "web_fetch is not configured".into(),
            }),
        }
    }
}

struct BoundWebSearch {
    tool: Arc<WebSearchTool>,
    records: Arc<dyn WebRecordSink>,
    context: HostInvocationContext,
}

#[async_trait]
impl BuiltinTool for BoundWebSearch {
    fn description(&self) -> &str {
        self.tool.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.tool.parameters_schema()
    }

    async fn execute(&self, arguments: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let started = Instant::now();
        self.records
            .writable(&self.context.session_id)
            .await
            .map_err(|reason| record_unavailable("web_search", reason))?;
        let (result, report) = self.tool.search_with_report(arguments).await;
        let event = search_event(&self.context, &report, started.elapsed());
        self.records
            .append(&self.context.session_id, event)
            .await
            .map_err(|reason| record_unavailable("web_search", reason))?;
        result
    }
}

struct BoundWebFetch {
    tool: Arc<WebFetchTool>,
    records: Arc<dyn WebRecordSink>,
    context: HostInvocationContext,
}

#[async_trait]
impl BuiltinTool for BoundWebFetch {
    fn description(&self) -> &str {
        self.tool.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.tool.parameters_schema()
    }

    async fn execute(&self, arguments: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let started = Instant::now();
        self.records
            .writable(&self.context.session_id)
            .await
            .map_err(|reason| record_unavailable("web_fetch", reason))?;
        let (result, report) = self.tool.fetch_with_report(arguments).await;
        let event = fetch_event(&self.context, &report, started.elapsed());
        self.records
            .append(&self.context.session_id, event)
            .await
            .map_err(|reason| record_unavailable("web_fetch", reason))?;
        result
    }
}

/// Cut `text` to at most `max` bytes at a character boundary.
fn bounded(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

fn decision(refused: bool) -> Decision {
    if refused {
        Decision::Deny
    } else {
        Decision::Allow
    }
}

fn millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// The `web` event for one `web_search` call.
pub(crate) fn search_event(
    context: &HostInvocationContext,
    report: &WebSearchReport,
    elapsed: Duration,
) -> NetworkEvent {
    let mut sources: Vec<WebSource> = report
        .sources
        .iter()
        .map(|(id, url)| {
            let (url, url_truncated) = bounded(url, MAX_RECORDED_SOURCE_URL_BYTES);
            WebSource {
                id: id.clone(),
                url,
                url_truncated,
            }
        })
        .collect();
    let mut event = NetworkEvent::Web {
        tool: WebTool::WebSearch,
        invocation_id: context.invocation_id.as_str().to_string(),
        activation_id: context.activation.activation_id.as_str().to_string(),
        agent: bounded(&context.agent, RECORDED_AGENT_BYTES).0,
        decision: decision(report.refused),
        reason: report
            .reason
            .as_deref()
            .map(|reason| bounded(reason, RECORDED_REASON_BYTES).0),
        url: None,
        final_url: None,
        status: None,
        redirects: Vec::new(),
        query_sha256: report.query_sha256.clone(),
        query_bytes: report.query_bytes,
        results: report.results,
        unresponsive_engines: report
            .unresponsive_engines
            .iter()
            .take(RECORDED_UNRESPONSIVE_MAX)
            .map(|engine| bounded(engine, RECORDED_UNRESPONSIVE_BYTES).0)
            .collect(),
        bytes: None,
        content_sha256: None,
        text_sha256: None,
        source_ids: report.sources.iter().map(|(id, _)| id.clone()).collect(),
        sources: sources.clone(),
        retrieved_at_ms: report.retrieved_at_ms,
        ms: millis(elapsed),
    };
    // A record line is at most 16 KiB. The bounds above keep a search well
    // under it; if a future bound changes, shorten the URLs rather than
    // lose the event.
    while serde_json::to_vec(&event).map_or(0, |line| line.len()) + 128 > MAX_LINE_BYTES
        && sources.iter().any(|source| source.url.len() > 64)
    {
        for source in &mut sources {
            let (url, cut) = bounded(&source.url, source.url.len() / 2);
            source.url = url;
            source.url_truncated |= cut;
        }
        if let NetworkEvent::Web {
            sources: recorded, ..
        } = &mut event
        {
            *recorded = sources.clone();
        }
    }
    event
}

/// The `web` event for one `web_fetch` call.
pub(crate) fn fetch_event(
    context: &HostInvocationContext,
    report: &WebFetchReport,
    elapsed: Duration,
) -> NetworkEvent {
    let url = |value: &Option<String>| {
        value
            .as_deref()
            .map(|url| bounded(url, MAX_RECORDED_WEB_URL_BYTES).0)
    };
    NetworkEvent::Web {
        tool: WebTool::WebFetch,
        invocation_id: context.invocation_id.as_str().to_string(),
        activation_id: context.activation.activation_id.as_str().to_string(),
        agent: bounded(&context.agent, RECORDED_AGENT_BYTES).0,
        decision: decision(report.refused),
        reason: report
            .reason
            .as_deref()
            .map(|reason| bounded(reason, RECORDED_REASON_BYTES).0),
        url: url(&report.url),
        final_url: url(&report.final_url),
        status: report.status,
        redirects: report
            .redirects
            .iter()
            .take(axocoatl_tools::fetch_guard::MAX_REDIRECTS)
            .map(|hop| bounded(hop, MAX_RECORDED_WEB_URL_BYTES).0)
            .collect(),
        query_sha256: None,
        query_bytes: None,
        results: None,
        unresponsive_engines: Vec::new(),
        bytes: report.bytes,
        content_sha256: report.content_sha256.clone(),
        text_sha256: report.text_sha256.clone(),
        source_ids: report.source_id.iter().cloned().collect(),
        sources: Vec::new(),
        retrieved_at_ms: report.retrieved_at_ms,
        ms: millis(elapsed),
    }
}

/// Source ids cited in `text`: `S` plus 8 hex digits inside square
/// brackets, as in `[S1a2b3c4d]`, `[S1a2b3c4d ¶3]` or `[S1a2b3c4d, S5e6f7a8b]`.
pub(crate) fn cited_source_ids(text: &str) -> BTreeSet<String> {
    const MAX_BRACKET: usize = 512;
    let bytes = text.as_bytes();
    let mut cited = BTreeSet::new();
    let mut index = 0;
    while let Some(open) = bytes[index..].iter().position(|byte| *byte == b'[') {
        let start = index + open + 1;
        let window = &bytes[start..bytes.len().min(start + MAX_BRACKET)];
        let Some(close) = window.iter().position(|byte| *byte == b']') else {
            index = start;
            continue;
        };
        let inside = &window[..close];
        let mut position = 0;
        while position + 9 <= inside.len() {
            let candidate = &inside[position..position + 9];
            let before_ok = position == 0 || !inside[position - 1].is_ascii_alphanumeric();
            let after_ok = inside
                .get(position + 9)
                .is_none_or(|byte| !byte.is_ascii_alphanumeric());
            if before_ok
                && after_ok
                && candidate[0] == b'S'
                && candidate[1..].iter().all(u8::is_ascii_hexdigit)
            {
                let id = format!(
                    "S{}",
                    std::str::from_utf8(&candidate[1..])
                        .unwrap_or_default()
                        .to_ascii_lowercase()
                );
                cited.insert(id);
                position += 9;
            } else {
                position += 1;
            }
        }
        index = start + close + 1;
    }
    cited
}

#[derive(Default)]
struct SourceRow {
    url: Option<String>,
    final_url: Option<String>,
    searched: bool,
    fetched: bool,
    status: Option<u16>,
    retrieved_at_ms: u64,
    content_sha256: Option<String>,
    text_sha256: Option<String>,
    invocations: BTreeSet<String>,
}

/// Is this a `web` event that returned sources?
pub(crate) fn is_web_event(event: &NetworkEvent) -> bool {
    matches!(
        event,
        NetworkEvent::Web {
            decision: Decision::Allow,
            ..
        }
    )
}

/// Add `sources` evidence to each activation with `web` events in `lines`:
/// every source id it received, its URL, when it was retrieved, the hashes of
/// a fetched page, and whether the activation's final output cites it.
pub(crate) fn add_sources_evidence(view: &mut SessionTurnControlPlane, lines: &[NetworkLine]) {
    let mut by_activation: HashMap<&str, BTreeMap<String, SourceRow>> = HashMap::new();
    let mut latest: HashMap<&str, u64> = HashMap::new();
    for line in lines {
        let NetworkEvent::Web {
            tool,
            activation_id,
            decision: Decision::Allow,
            url,
            final_url,
            status,
            content_sha256,
            text_sha256,
            source_ids,
            sources,
            retrieved_at_ms,
            invocation_id,
            ..
        } = &line.event
        else {
            continue;
        };
        let rows = by_activation.entry(activation_id.as_str()).or_default();
        let stamp = latest.entry(activation_id.as_str()).or_default();
        *stamp = (*stamp).max(*retrieved_at_ms);
        match tool {
            WebTool::WebSearch => {
                for source in sources {
                    let row = rows.entry(source.id.clone()).or_default();
                    row.searched = true;
                    row.url.get_or_insert_with(|| source.url.clone());
                    row.retrieved_at_ms = row.retrieved_at_ms.max(*retrieved_at_ms);
                    row.invocations.insert(invocation_id.clone());
                }
            }
            WebTool::WebFetch => {
                // Only a fetch that read content carries hashes.
                if content_sha256.is_none() {
                    continue;
                }
                for id in source_ids {
                    let row = rows.entry(id.clone()).or_default();
                    row.fetched = true;
                    if let Some(url) = url {
                        row.url = Some(url.clone());
                    }
                    row.final_url = final_url.clone();
                    row.status = *status;
                    row.retrieved_at_ms = row.retrieved_at_ms.max(*retrieved_at_ms);
                    row.content_sha256 = content_sha256.clone();
                    row.text_sha256 = text_sha256.clone();
                    row.invocations.insert(invocation_id.clone());
                }
            }
        }
    }
    if by_activation.is_empty() {
        return;
    }
    for node in &mut view.nodes {
        for item in &mut node.activations {
            let ControlPlaneActivationRef::Exact { activation } = &item.reference else {
                continue;
            };
            let Some(rows) = by_activation.get(activation.activation_id.as_str()) else {
                continue;
            };
            let cited = match &item.output {
                EvidenceValue::Available { value } | EvidenceValue::Truncated { value, .. } => {
                    cited_source_ids(value)
                }
                _ => BTreeSet::new(),
            };
            let total = rows.len();
            let cited_count = rows.keys().filter(|id| cited.contains(*id)).count();
            let fetched_count = rows.values().filter(|row| row.fetched).count();
            let sources: Vec<serde_json::Value> = rows
                .iter()
                .take(EVIDENCE_SOURCES_MAX)
                .map(|(id, row)| {
                    serde_json::json!({
                        "source_id": id,
                        "url": row.url,
                        "final_url": row.final_url,
                        "searched": row.searched,
                        "fetched": row.fetched,
                        "status": row.status,
                        "retrieved_at_ms": row.retrieved_at_ms,
                        "content_sha256": row.content_sha256,
                        "text_sha256": row.text_sha256,
                        "invocations": row.invocations,
                        "cited": cited.contains(id),
                    })
                })
                .collect();
            let uncited_citations: Vec<&String> =
                cited.iter().filter(|id| !rows.contains_key(*id)).collect();
            item.evidence.push(ControlPlaneEvidence {
                kind: "sources".into(),
                reference: EvidenceValue::NotRecorded,
                summary: EvidenceValue::Available {
                    value: format!(
                        "{total} source{} ({fetched_count} read), {cited_count} cited",
                        if total == 1 { "" } else { "s" }
                    ),
                },
                recorded_at: latest
                    .get(activation.activation_id.as_str())
                    .copied()
                    .map_or(EvidenceValue::NotRecorded, |value| {
                        EvidenceValue::Available { value }
                    }),
                details: EvidenceValue::Available {
                    value: serde_json::json!({
                        "sources": sources,
                        "truncated": total > EVIDENCE_SOURCES_MAX,
                        // Ids the output cites that this activation never
                        // received from a web tool.
                        "unknown_citations": uncited_citations,
                    }),
                },
            });
        }
    }
}

/// The most `web` events read for one projection.
pub(crate) fn projection_web_events_max() -> usize {
    PROJECTION_WEB_EVENTS_MAX
}

#[cfg(test)]
#[path = "session_dispatch_web_tests.rs"]
mod tests;
