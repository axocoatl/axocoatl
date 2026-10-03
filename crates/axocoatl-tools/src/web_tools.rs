//! Web research tools for Session Agents: `web_search` and `web_fetch`.
//!
//! `web_search` asks a pluggable [`WebSearchBackend`]: [`SearxngBackend`],
//! which queries a SearXNG instance Axocoatl runs (or one the user names), or
//! the legacy Tavily backend kept for configs that already use it. With no
//! provider configured, [`NullBackend`] returns a clear "not configured"
//! error.
//!
//! Every search result and fetched page carries a `source_id`, `S` plus the
//! first 8 hex digits of the SHA-256 of its normalized URL, so an Agent can
//! cite a claim as `[S1a2b3c4d]` or `[S1a2b3c4d ¶3]` and the host can join the
//! citation to the page it recorded.
//!
//! `web_fetch` reads one page through [`FetchGuard`], which refuses private
//! and local addresses and checks every redirect, and returns its text as
//! numbered paragraphs.

use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::builtin::BuiltinTool;
use crate::error::ToolError;
use crate::fetch_guard::{check_url_with, FetchError, FetchGuard, PageFetcher};
use crate::html_text::{self, ExtractedPage};
use crate::limits::{ensure_json_input, limit_error_text, limit_text, SIMPLE_TOOL_INPUT_MAX_BYTES};

const WEB_SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
const WEB_RESPONSE_MAX_BYTES: usize = 2 * 1024 * 1024;
const WEB_ERROR_BODY_MAX_BYTES: usize = 64 * 1024;
const WEB_QUERY_MAX_BYTES: usize = 8 * 1024;
const WEB_MAX_RESULTS: usize = 15;
const WEB_TITLE_MAX_BYTES: usize = 512;
const WEB_URL_MAX_BYTES: usize = 4 * 1024;
const WEB_SNIPPET_MAX_BYTES: usize = 8 * 1024;
const WEB_ENGINES_MAX: usize = 8;
const WEB_ENGINE_NAME_MAX_BYTES: usize = 64;
const UNRESPONSIVE_ENGINES_MAX: usize = 32;
const UNRESPONSIVE_ENGINE_MAX_BYTES: usize = 160;

/// Default, smallest and largest `max_chars` for `web_fetch`.
pub const WEB_FETCH_DEFAULT_MAX_CHARS: usize = 32_768;
pub const WEB_FETCH_MIN_MAX_CHARS: usize = 1_000;
pub const WEB_FETCH_MAX_MAX_CHARS: usize = 65_536;
/// Longest `web_fetch` URL, in characters.
pub const WEB_FETCH_URL_MAX_CHARS: usize = crate::fetch_guard::MAX_URL_CHARS;
/// Largest `start_paragraph` accepted.
const WEB_FETCH_MAX_START_PARAGRAPH: u64 = 1_000_000;

const SEARXNG_DESCRIPTION: &str = "Search the web through the local SearXNG. Each result has a \
     source_id; cite claims as [S1a2b3c4d]. Use web_fetch to read a page before quoting it.";
const LEGACY_SEARCH_DESCRIPTION: &str = "Search the web for current, real-world information. \
     Returns titles, URLs, and bounded content snippets. Each result has a source_id; cite \
     claims as [S1a2b3c4d].";
const WEB_FETCH_DESCRIPTION: &str = "Read one web page as numbered paragraphs (¶n). Cite as \
     [S1a2b3c4d ¶n]. Pages on private or local addresses are refused.";

struct BodyAccumulator {
    bytes: Vec<u8>,
    max_bytes: usize,
}

impl BodyAccumulator {
    fn new(max_bytes: usize, content_length: Option<u64>) -> Result<Self, String> {
        if content_length.is_some_and(|length| length > max_bytes as u64) {
            return Err(format!(
                "response declares more than the {max_bytes}-byte limit"
            ));
        }
        let capacity = content_length
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0)
            .min(max_bytes);
        Ok(Self {
            bytes: Vec::with_capacity(capacity),
            max_bytes,
        })
    }

    fn push(&mut self, chunk: &[u8]) -> Result<(), String> {
        let next = self
            .bytes
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| "response size overflowed".to_string())?;
        if next > self.max_bytes {
            return Err(format!(
                "response exceeded the {}-byte limit while streaming",
                self.max_bytes
            ));
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

async fn read_response_body_limited(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    let mut body = BodyAccumulator::new(max_bytes, response.content_length())?;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("reading response: {error}"))?
    {
        body.push(&chunk)?;
    }
    Ok(body.finish())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// RFC 3339 UTC time, to the second, for a millisecond timestamp.
pub fn rfc3339_utc(ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// The URL a source id is computed from: parsed, scheme and host lowercase,
/// default port and fragment removed, path and query kept. `None` for
/// anything but an `http`/`https` URL with a host.
pub fn normalize_source_url(raw: &str) -> Option<String> {
    let mut url = url::Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    url.set_fragment(None);
    // The parser already lowercases the scheme and a domain host and drops
    // the scheme's default port.
    Some(url.to_string())
}

/// `"S"` plus the first 8 hex digits of the SHA-256 of the normalized URL.
pub fn source_id(raw: &str) -> Option<String> {
    let normalized = normalize_source_url(raw)?;
    Some(format!("S{}", &sha256_hex(normalized.as_bytes())[..8]))
}

/// One search result.
#[derive(Debug, Clone, Default)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    /// Engines that returned this result (SearXNG only).
    pub engines: Vec<String>,
}

/// One page of search results with the backend's own diagnostics.
#[derive(Debug, Clone, Default)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    /// Engines that did not answer, as `"name (reason)"`.
    pub unresponsive_engines: Vec<String>,
}

/// A pluggable web-search provider.
#[async_trait::async_trait]
pub trait WebSearchBackend: Send + Sync + 'static {
    /// Provider name (for diagnostics).
    fn name(&self) -> &str;
    /// Run a search, returning up to `max_results` hits.
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<SearchHit>, String>;
    /// Run a search and report engines that did not answer. The default has
    /// no such report.
    async fn search_page(&self, query: &str, max_results: usize) -> Result<SearchPage, String> {
        Ok(SearchPage {
            hits: self.search(query, max_results).await?,
            unresponsive_engines: Vec::new(),
        })
    }
}

/// Tavily backend — `https://api.tavily.com/search`. Kept only for configs
/// that already name it; new configs use SearXNG.
pub struct TavilyBackend {
    api_key: String,
    client: reqwest::Client,
}

impl TavilyBackend {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl WebSearchBackend for TavilyBackend {
    fn name(&self) -> &str {
        "tavily"
    }

    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<SearchHit>, String> {
        let resp = self
            .client
            .post("https://api.tavily.com/search")
            .json(&serde_json::json!({
                "api_key": self.api_key,
                "query": query,
                "max_results": max_results,
                "search_depth": "basic",
            }))
            .timeout(WEB_SEARCH_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status();
        let body_limit = if status.is_success() {
            WEB_RESPONSE_MAX_BYTES
        } else {
            WEB_ERROR_BODY_MAX_BYTES
        };
        let body = read_response_body_limited(resp, body_limit).await?;
        if !status.is_success() {
            let detail = String::from_utf8_lossy(&body);
            return Err(format!("Tavily returned HTTP {status}: {}", detail.trim()));
        }
        let body: serde_json::Value =
            serde_json::from_slice(&body).map_err(|e| format!("bad response: {e}"))?;
        let hits = body["results"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .take(max_results.min(WEB_MAX_RESULTS))
                    .map(|r| SearchHit {
                        title: r["title"].as_str().unwrap_or("").to_string(),
                        url: r["url"].as_str().unwrap_or("").to_string(),
                        snippet: r["content"].as_str().unwrap_or("").to_string(),
                        engines: Vec::new(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(hits)
    }
}

/// Where a SearXNG instance answers. A managed instance starts on first use.
#[async_trait::async_trait]
pub trait SearxngEndpoint: Send + Sync + 'static {
    /// Base URL such as `http://127.0.0.1:49152`, starting the instance if
    /// needed.
    async fn base_url(&self) -> Result<String, String>;
}

/// An instance the user runs at a fixed URL (`managed: false`).
pub struct FixedSearxngEndpoint {
    url: String,
}

impl FixedSearxngEndpoint {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }
}

#[async_trait::async_trait]
impl SearxngEndpoint for FixedSearxngEndpoint {
    async fn base_url(&self) -> Result<String, String> {
        Ok(self.url.clone())
    }
}

/// Query settings for [`SearxngBackend`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearxngQuerySettings {
    /// SearXNG language code, `"all"` for any.
    pub language: String,
    /// 0 (off), 1 (moderate) or 2 (strict).
    pub safesearch: u8,
    pub timeout: Duration,
}

impl Default for SearxngQuerySettings {
    fn default() -> Self {
        Self {
            language: "all".into(),
            safesearch: 0,
            timeout: Duration::from_secs(15),
        }
    }
}

/// SearXNG's JSON search API: `GET {base}/search?format=json`.
pub struct SearxngBackend {
    endpoint: Arc<dyn SearxngEndpoint>,
    client: reqwest::Client,
    settings: SearxngQuerySettings,
}

impl SearxngBackend {
    /// `use_environment_proxy` is false for a managed instance on loopback,
    /// so a proxy from the environment never sees local search traffic.
    pub fn new(
        endpoint: Arc<dyn SearxngEndpoint>,
        settings: SearxngQuerySettings,
        use_environment_proxy: bool,
    ) -> Result<Self, String> {
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(settings.timeout);
        if !use_environment_proxy {
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .map_err(|error| format!("building the SearXNG client failed: {error}"))?;
        Ok(Self {
            endpoint,
            client,
            settings,
        })
    }

    /// The search URL for `query`.
    pub fn search_url(
        base: &str,
        query: &str,
        settings: &SearxngQuerySettings,
    ) -> Result<url::Url, String> {
        let mut url = url::Url::parse(&format!("{}/search", base.trim_end_matches('/')))
            .map_err(|error| format!("SearXNG URL {base:?} is invalid: {error}"))?;
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("format", "json")
            .append_pair("pageno", "1")
            .append_pair("language", &settings.language)
            .append_pair("safesearch", &settings.safesearch.to_string());
        Ok(url)
    }

    /// Map a SearXNG JSON response: keep only `http`/`https` results, drop
    /// duplicates by normalized URL, and carry the unresponsive engines.
    pub fn parse_response(body: &[u8]) -> Result<SearchPage, String> {
        let body: serde_json::Value = serde_json::from_slice(body)
            .map_err(|error| format!("SearXNG returned invalid JSON: {error}"))?;
        let results = body
            .get("results")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| "SearXNG response has no results array".to_string())?;
        let mut seen = std::collections::HashSet::new();
        let mut hits = Vec::new();
        for result in results {
            let Some(url) = result.get("url").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(normalized) = normalize_source_url(url) else {
                continue;
            };
            if !seen.insert(normalized) {
                continue;
            }
            let mut engines: Vec<String> = result
                .get("engines")
                .and_then(serde_json::Value::as_array)
                .map(|engines| {
                    engines
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if engines.is_empty() {
                if let Some(engine) = result.get("engine").and_then(serde_json::Value::as_str) {
                    engines.push(engine.to_string());
                }
            }
            hits.push(SearchHit {
                title: result
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                url: url.to_string(),
                snippet: result
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                engines,
            });
        }
        let unresponsive_engines = body
            .get("unresponsive_engines")
            .and_then(serde_json::Value::as_array)
            .map(|engines| {
                engines
                    .iter()
                    .filter_map(|engine| match engine {
                        serde_json::Value::Array(pair) => {
                            let name = pair.first()?.as_str()?;
                            Some(match pair.get(1).and_then(serde_json::Value::as_str) {
                                Some(reason) if !reason.is_empty() => {
                                    format!("{name} ({reason})")
                                }
                                _ => name.to_string(),
                            })
                        }
                        serde_json::Value::String(name) => Some(name.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(SearchPage {
            hits,
            unresponsive_engines,
        })
    }
}

#[async_trait::async_trait]
impl WebSearchBackend for SearxngBackend {
    fn name(&self) -> &str {
        "searxng"
    }

    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<SearchHit>, String> {
        Ok(self.search_page(query, max_results).await?.hits)
    }

    async fn search_page(&self, query: &str, _max_results: usize) -> Result<SearchPage, String> {
        let base = self.endpoint.base_url().await?;
        let url = Self::search_url(&base, query, &self.settings)?;
        let response = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|error| format!("SearXNG request failed: {error}"))?;
        let status = response.status();
        let limit = if status.is_success() {
            WEB_RESPONSE_MAX_BYTES
        } else {
            WEB_ERROR_BODY_MAX_BYTES
        };
        let body = read_response_body_limited(response, limit).await?;
        if !status.is_success() {
            let detail = String::from_utf8_lossy(&body);
            return Err(format!(
                "SearXNG returned HTTP {status}: {}",
                limit_text(detail.trim().to_string(), 512).text
            ));
        }
        Self::parse_response(&body)
    }
}

/// Fallback when no provider is configured.
pub struct NullBackend;

#[async_trait::async_trait]
impl WebSearchBackend for NullBackend {
    fn name(&self) -> &str {
        "none"
    }
    async fn search(&self, _query: &str, _max: usize) -> Result<Vec<SearchHit>, String> {
        Err(
            "web search is not configured — add a web_search block with provider: searxng \
             to axocoatl.yaml"
                .to_string(),
        )
    }
}

/// What one `web_search` call asked and got, for the Session's network
/// record. The query text itself is not here: it is already in the audited
/// call arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebSearchReport {
    pub backend: String,
    pub query_sha256: Option<String>,
    pub query_bytes: Option<u32>,
    /// Results returned to the model.
    pub results: Option<u32>,
    pub unresponsive_engines: Vec<String>,
    /// `(source_id, url)` for each returned result, in order.
    pub sources: Vec<(String, String)>,
    pub retrieved_at_ms: u64,
    /// The call was refused before any search (invalid arguments).
    pub refused: bool,
    /// Why it was refused or failed.
    pub reason: Option<String>,
}

/// A `web_search` call whose arguments are valid, before it is sent.
#[derive(Debug, Clone)]
pub struct PreparedSearch {
    query: String,
    max: usize,
    report: WebSearchReport,
}

impl PreparedSearch {
    /// SHA-256 hex of the query, for the record; never the query text.
    pub fn query_sha256(&self) -> Option<&str> {
        self.report.query_sha256.as_deref()
    }

    pub fn query_bytes(&self) -> Option<u32> {
        self.report.query_bytes
    }
}

/// The `web_search` tool — searches the web via the configured backend.
pub struct WebSearchTool {
    backend: Arc<dyn WebSearchBackend>,
    description: &'static str,
}

impl WebSearchTool {
    pub fn new(backend: Arc<dyn WebSearchBackend>) -> Self {
        let description = if backend.name() == "searxng" {
            SEARXNG_DESCRIPTION
        } else {
            LEGACY_SEARCH_DESCRIPTION
        };
        Self {
            backend,
            description,
        }
    }

    /// Build for a legacy provider config: Tavily when a key is present,
    /// else the null backend. SearXNG needs an endpoint; use
    /// [`WebSearchTool::searxng`].
    pub fn from_config(provider: &str, api_key: &str) -> Self {
        let backend: Arc<dyn WebSearchBackend> = match provider {
            "tavily" if !api_key.is_empty() => Arc::new(TavilyBackend::new(api_key.to_string())),
            _ => Arc::new(NullBackend),
        };
        Self::new(backend)
    }

    /// Search through a SearXNG instance.
    pub fn searxng(
        endpoint: Arc<dyn SearxngEndpoint>,
        settings: SearxngQuerySettings,
        use_environment_proxy: bool,
    ) -> Result<Self, String> {
        Ok(Self::new(Arc::new(SearxngBackend::new(
            endpoint,
            settings,
            use_environment_proxy,
        )?)))
    }

    /// Provider name: `searxng`, `tavily` or `none`.
    pub fn backend_name(&self) -> &str {
        self.backend.name()
    }

    fn arguments(arguments: &serde_json::Value) -> Result<(&str, usize), ToolError> {
        ensure_json_input(arguments, "web_search", SIMPLE_TOOL_INPUT_MAX_BYTES)?;
        let query = arguments
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: "web_search".to_string(),
                reason: "expected string field 'query'".to_string(),
            })?;
        if query.len() > WEB_QUERY_MAX_BYTES {
            return Err(ToolError::InvalidArgs {
                tool: "web_search".to_string(),
                reason: format!(
                    "field 'query' is {} bytes; the limit is {WEB_QUERY_MAX_BYTES} bytes",
                    query.len()
                ),
            });
        }
        let max = match arguments.get("max_results") {
            None | Some(serde_json::Value::Null) => 5,
            Some(value) => {
                let value = value.as_u64().ok_or_else(|| ToolError::InvalidArgs {
                    tool: "web_search".to_string(),
                    reason: "field 'max_results' must be an integer".to_string(),
                })?;
                if !(1..=WEB_MAX_RESULTS as u64).contains(&value) {
                    return Err(ToolError::InvalidArgs {
                        tool: "web_search".to_string(),
                        reason: format!(
                            "field 'max_results' must be between 1 and {WEB_MAX_RESULTS}"
                        ),
                    });
                }
                value as usize
            }
        };
        Ok((query, max))
    }

    /// Run one call and say what it did, for the network record. The record
    /// entry is the caller's to write; the result is the same as `execute`.
    pub async fn search_with_report(
        &self,
        arguments: serde_json::Value,
    ) -> (Result<serde_json::Value, ToolError>, WebSearchReport) {
        match self.prepare_search(&arguments) {
            Ok(prepared) => self.run_search(prepared).await,
            Err((error, report)) => (Err(error), *report),
        }
    }

    /// Check one call's arguments. Nothing leaves this computer; a caller
    /// can record the request before [`Self::run_search`] sends it. An
    /// invalid call comes back with its error and its report.
    pub fn prepare_search(
        &self,
        arguments: &serde_json::Value,
    ) -> Result<PreparedSearch, (ToolError, Box<WebSearchReport>)> {
        let mut report = WebSearchReport {
            backend: self.backend.name().to_string(),
            retrieved_at_ms: now_ms(),
            ..WebSearchReport::default()
        };
        let (query, max) = match Self::arguments(arguments) {
            Ok(parsed) => parsed,
            Err(error) => {
                report.refused = true;
                report.reason = Some("invalid_arguments".into());
                return Err((error, Box::new(report)));
            }
        };
        report.query_sha256 = Some(sha256_hex(query.as_bytes()));
        report.query_bytes = Some(query.len() as u32);
        Ok(PreparedSearch {
            query: query.to_string(),
            max,
            report,
        })
    }

    /// Send a prepared search and say what it did.
    pub async fn run_search(
        &self,
        prepared: PreparedSearch,
    ) -> (Result<serde_json::Value, ToolError>, WebSearchReport) {
        let PreparedSearch {
            query,
            max,
            mut report,
        } = prepared;
        let query = query.as_str();

        let page = match self.backend.search_page(query, max).await {
            Ok(page) => page,
            Err(error) => {
                report.reason = Some("search_failed".into());
                return (
                    Err(ToolError::ExecutionFailed {
                        tool: "web_search".to_string(),
                        reason: limit_error_text(error),
                    }),
                    report,
                );
            }
        };
        report.retrieved_at_ms = now_ms();

        let total_count = page.hits.len();
        let mut any_field_truncated = false;
        let mut results = Vec::new();
        for hit in page.hits.into_iter().take(max) {
            let id = source_id(&hit.url);
            if let Some(id) = &id {
                report.sources.push((id.clone(), hit.url.clone()));
            }
            let title = limit_text(hit.title, WEB_TITLE_MAX_BYTES);
            let url = limit_text(hit.url, WEB_URL_MAX_BYTES);
            let snippet = limit_text(hit.snippet, WEB_SNIPPET_MAX_BYTES);
            let engines: Vec<String> = hit
                .engines
                .into_iter()
                .take(WEB_ENGINES_MAX)
                .map(|engine| limit_text(engine, WEB_ENGINE_NAME_MAX_BYTES).text)
                .collect();
            let field_truncated = title.truncated || url.truncated || snippet.truncated;
            any_field_truncated |= field_truncated;
            results.push(serde_json::json!({
                "source_id": id,
                "title": title.text,
                "url": url.text,
                "snippet": snippet.text,
                "engines": engines,
                "field_truncated": field_truncated,
                "title_truncated": title.truncated,
                "url_truncated": url.truncated,
                "snippet_truncated": snippet.truncated,
                "title_original_bytes": title.original_bytes,
                "url_original_bytes": url.original_bytes,
                "snippet_original_bytes": snippet.original_bytes,
            }));
        }
        let unresponsive: Vec<String> = page
            .unresponsive_engines
            .into_iter()
            .take(UNRESPONSIVE_ENGINES_MAX)
            .map(|engine| limit_text(engine, UNRESPONSIVE_ENGINE_MAX_BYTES).text)
            .collect();
        report.unresponsive_engines = unresponsive.clone();
        let count = results.len();
        report.results = Some(count as u32);
        (
            Ok(serde_json::json!({
                "query": query,
                "backend": self.backend.name(),
                "retrieved_at": rfc3339_utc(report.retrieved_at_ms),
                "results": results,
                "count": count,
                "total_count": total_count,
                "truncated": total_count > count || any_field_truncated,
                "result_limit": max,
                "unresponsive_engines": unresponsive,
            })),
            report,
        )
    }
}

#[async_trait::async_trait]
impl BuiltinTool for WebSearchTool {
    fn description(&self) -> &str {
        self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        web_search_schema()
    }

    async fn execute(&self, arguments: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        self.search_with_report(arguments).await.0
    }
}

/// The `web_search` argument schema.
pub fn web_search_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "What to search for (maximum 8 KiB)" },
            "max_results": { "type": "integer", "description": "How many results (default 5, maximum 15)", "minimum": 1, "maximum": 15 }
        },
        "required": ["query"]
    })
}

/// What one `web_fetch` call asked and got, for the Session's network record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebFetchReport {
    /// The requested URL, when the arguments had one.
    pub url: Option<String>,
    pub final_url: Option<String>,
    pub status: Option<u16>,
    pub redirects: Vec<String>,
    pub source_id: Option<String>,
    /// Body bytes read.
    pub bytes: Option<u64>,
    pub content_sha256: Option<String>,
    pub text_sha256: Option<String>,
    pub retrieved_at_ms: u64,
    /// Axocoatl refused the destination or the arguments; nothing was read.
    pub refused: bool,
    /// Why it was refused or failed, as a stable code.
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
struct FetchArguments {
    url: String,
    max_chars: usize,
    start_paragraph: usize,
}

/// A `web_fetch` call whose arguments are valid, before anything is
/// requested.
#[derive(Debug, Clone)]
pub struct PreparedFetch {
    arguments: FetchArguments,
    report: WebFetchReport,
}

impl PreparedFetch {
    /// The URL that will be requested, as the call gave it.
    pub fn url(&self) -> &str {
        &self.arguments.url
    }
}

/// The `web_fetch` tool — reads one public page as numbered paragraphs.
pub struct WebFetchTool {
    fetcher: Arc<dyn PageFetcher>,
}

impl WebFetchTool {
    pub fn new(fetcher: Arc<dyn PageFetcher>) -> Self {
        Self { fetcher }
    }

    /// The real tool: a [`FetchGuard`] reading at most `max_bytes` of a body,
    /// with `timeout` for the whole fetch.
    pub fn from_config(max_bytes: u64, timeout: Duration) -> Result<Self, String> {
        Ok(Self::new(Arc::new(FetchGuard::new(max_bytes, timeout)?)))
    }

    fn invalid(reason: impl Into<String>) -> ToolError {
        ToolError::InvalidArgs {
            tool: "web_fetch".to_string(),
            reason: reason.into(),
        }
    }

    fn arguments(arguments: &serde_json::Value) -> Result<FetchArguments, ToolError> {
        ensure_json_input(arguments, "web_fetch", SIMPLE_TOOL_INPUT_MAX_BYTES)?;
        let object = arguments
            .as_object()
            .ok_or_else(|| Self::invalid("arguments must be an object"))?;
        if let Some(unknown) = object
            .keys()
            .find(|key| !matches!(key.as_str(), "url" | "max_chars" | "start_paragraph"))
        {
            return Err(Self::invalid(format!(
                "unknown field '{unknown}'; web_fetch takes url, max_chars and start_paragraph"
            )));
        }
        let url = object
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Self::invalid("expected string field 'url'"))?;
        if url.chars().count() > WEB_FETCH_URL_MAX_CHARS {
            return Err(Self::invalid(format!(
                "field 'url' is longer than {WEB_FETCH_URL_MAX_CHARS} characters"
            )));
        }
        // Scheme, user information and syntax; the destination's class is
        // the guard's to decide.
        check_url_with(url, |_| axocoatl_core::netaddr::AddrClass::Public)
            .map_err(|error| Self::invalid(error.detail))?;
        let integer = |name: &str, default: u64, min: u64, max: u64| -> Result<u64, ToolError> {
            match object.get(name) {
                None | Some(serde_json::Value::Null) => Ok(default),
                Some(value) => {
                    let value = value.as_u64().ok_or_else(|| {
                        Self::invalid(format!("field '{name}' must be an integer"))
                    })?;
                    if !(min..=max).contains(&value) {
                        return Err(Self::invalid(format!(
                            "field '{name}' must be between {min} and {max}"
                        )));
                    }
                    Ok(value)
                }
            }
        };
        let max_chars = integer(
            "max_chars",
            WEB_FETCH_DEFAULT_MAX_CHARS as u64,
            WEB_FETCH_MIN_MAX_CHARS as u64,
            WEB_FETCH_MAX_MAX_CHARS as u64,
        )? as usize;
        let start_paragraph =
            integer("start_paragraph", 1, 1, WEB_FETCH_MAX_START_PARAGRAPH)? as usize;
        Ok(FetchArguments {
            url: url.to_string(),
            max_chars,
            start_paragraph,
        })
    }

    /// Run one call and say what it did, for the network record.
    pub async fn fetch_with_report(
        &self,
        arguments: serde_json::Value,
    ) -> (Result<serde_json::Value, ToolError>, WebFetchReport) {
        match self.prepare_fetch(&arguments) {
            Ok(prepared) => self.run_fetch(prepared).await,
            Err((error, report)) => (Err(error), *report),
        }
    }

    /// Check one call's arguments. Nothing is requested; a caller can record
    /// the request before [`Self::run_fetch`] sends it. An invalid call comes
    /// back with its error and its report.
    pub fn prepare_fetch(
        &self,
        arguments: &serde_json::Value,
    ) -> Result<PreparedFetch, (ToolError, Box<WebFetchReport>)> {
        let mut report = WebFetchReport {
            url: arguments
                .get("url")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            retrieved_at_ms: now_ms(),
            ..WebFetchReport::default()
        };
        let parsed = match Self::arguments(arguments) {
            Ok(parsed) => parsed,
            Err(error) => {
                report.refused = true;
                report.reason = Some("invalid_arguments".into());
                return Err((error, Box::new(report)));
            }
        };
        report.source_id = source_id(&parsed.url);
        Ok(PreparedFetch {
            arguments: parsed,
            report,
        })
    }

    /// Request a prepared fetch and say what it did.
    pub async fn run_fetch(
        &self,
        prepared: PreparedFetch,
    ) -> (Result<serde_json::Value, ToolError>, WebFetchReport) {
        let PreparedFetch {
            arguments: parsed,
            mut report,
        } = prepared;
        let page = match self.fetcher.fetch(&parsed.url).await {
            Ok(page) => page,
            Err(error) => {
                report.refused = error.kind.is_refusal();
                report.reason = Some(error.kind.code().to_string());
                report.redirects = error.redirects.clone();
                return (Err(fetch_failure(&parsed.url, &error)), report);
            }
        };
        report.retrieved_at_ms = now_ms();
        report.url = Some(page.url.clone());
        report.final_url = Some(page.final_url.clone());
        report.status = Some(page.status);
        report.redirects = page.redirects.clone();
        report.bytes = Some(page.body.len() as u64);
        report.content_sha256 = Some(sha256_hex(&page.body));

        let text = decode_body(&page.body, page.charset.as_deref());
        let extracted = extract(&page.content_type, &text);
        let joined = extracted.paragraphs.join("\n\n");
        report.text_sha256 = Some(sha256_hex(joined.as_bytes()));

        let total = extracted.paragraphs.len();
        let (content, last, cut) = render_window(
            &extracted.paragraphs,
            parsed.start_paragraph,
            parsed.max_chars,
        );
        let content_truncated =
            cut || last < total || page.body_truncated || extracted.input_truncated;
        let mut output = serde_json::json!({
            "source_id": report.source_id,
            "url": page.url,
            "final_url": page.final_url,
            "status": page.status,
            "content_type": page.content_type,
            "title": extracted.title.map(|title| limit_text(title, WEB_TITLE_MAX_BYTES).text),
            "retrieved_at": rfc3339_utc(report.retrieved_at_ms),
            "content_sha256": report.content_sha256,
            "text_sha256": report.text_sha256,
            "paragraphs": {
                "first": parsed.start_paragraph,
                "last": last,
                "total": total,
            },
            "content": content,
            "content_truncated": content_truncated,
            "redirects": page.redirects,
        });
        if parsed.start_paragraph > total {
            output["note"] = serde_json::json!(format!(
                "start_paragraph {} is past the last paragraph ({total})",
                parsed.start_paragraph
            ));
        }
        (Ok(output), report)
    }
}

fn fetch_failure(url: &str, error: &FetchError) -> ToolError {
    let shown = limit_text(url.to_string(), 512).text;
    let verb = if error.kind.is_refusal() {
        "refused"
    } else {
        "could not read"
    };
    ToolError::ExecutionFailed {
        tool: "web_fetch".to_string(),
        reason: limit_error_text(format!("web_fetch {verb} {shown}: {error}")),
    }
}

/// Decode a body by its declared charset, else as UTF-8 (a byte-order mark
/// wins either way). Invalid sequences become U+FFFD.
fn decode_body(body: &[u8], charset: Option<&str>) -> String {
    let encoding = charset
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(body);
    text.into_owned()
}

fn extract(content_type: &str, text: &str) -> ExtractedPage {
    match content_type {
        "text/html" | "application/xhtml+xml" => html_text::html_to_paragraphs(text),
        "application/json" | "application/xml" | "text/xml" => html_text::single_block(text),
        _ => html_text::plain_to_paragraphs(text),
    }
}

/// `¶n text` blocks from `start` (1-based) while they fit in `max_chars`.
/// Returns the content, the last paragraph number included (`start - 1` when
/// none), and whether a paragraph was cut to fit.
fn render_window(paragraphs: &[String], start: usize, max_chars: usize) -> (String, usize, bool) {
    let mut content = String::new();
    let mut used = 0usize;
    let mut last = start.saturating_sub(1);
    for (offset, paragraph) in paragraphs.iter().enumerate().skip(start.saturating_sub(1)) {
        let number = offset + 1;
        let piece = format!("¶{number} {paragraph}");
        let separator = if content.is_empty() { 0 } else { 2 };
        let piece_chars = piece.chars().count();
        if used + separator + piece_chars <= max_chars {
            if separator > 0 {
                content.push_str("\n\n");
            }
            content.push_str(&piece);
            used += separator + piece_chars;
            last = number;
            continue;
        }
        if content.is_empty() {
            // The first paragraph alone is too long: keep its prefix.
            content = piece.chars().take(max_chars).collect();
            return (content, number, true);
        }
        break;
    }
    (content, last, false)
}

#[async_trait::async_trait]
impl BuiltinTool for WebFetchTool {
    fn description(&self) -> &str {
        WEB_FETCH_DESCRIPTION
    }

    fn parameters_schema(&self) -> serde_json::Value {
        web_fetch_schema()
    }

    async fn execute(&self, arguments: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        self.fetch_with_report(arguments).await.0
    }
}

/// The `web_fetch` argument schema.
pub fn web_fetch_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["url"],
        "additionalProperties": false,
        "properties": {
            "url": {"type": "string", "maxLength": WEB_FETCH_URL_MAX_CHARS,
                    "description": "http or https URL; no user:password@"},
            "max_chars": {"type": "integer", "minimum": WEB_FETCH_MIN_MAX_CHARS,
                          "maximum": WEB_FETCH_MAX_MAX_CHARS, "default": WEB_FETCH_DEFAULT_MAX_CHARS},
            "start_paragraph": {"type": "integer", "minimum": 1, "default": 1}
        }
    })
}

/// The `web_fetch` description shown to the model.
pub fn web_fetch_description() -> &'static str {
    WEB_FETCH_DESCRIPTION
}

/// The `web_search` description for a SearXNG backend.
pub fn web_search_description() -> &'static str {
    SEARXNG_DESCRIPTION
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch_guard::{FetchErrorKind, FetchedPage};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct OversizedBackend;

    #[async_trait::async_trait]
    impl WebSearchBackend for OversizedBackend {
        fn name(&self) -> &str {
            "oversized"
        }

        async fn search(&self, _query: &str, max_results: usize) -> Result<Vec<SearchHit>, String> {
            Ok((0..max_results + 3)
                .map(|index| SearchHit {
                    title: format!("{index}-{}", "t".repeat(WEB_TITLE_MAX_BYTES + 20)),
                    url: format!(
                        "https://example.test/{index}/{}",
                        "u".repeat(WEB_URL_MAX_BYTES)
                    ),
                    snippet: "🦀".repeat(WEB_SNIPPET_MAX_BYTES),
                    engines: Vec::new(),
                })
                .collect())
        }
    }

    struct CountingBackend {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl WebSearchBackend for CountingBackend {
        fn name(&self) -> &str {
            "counting"
        }

        async fn search(
            &self,
            _query: &str,
            _max_results: usize,
        ) -> Result<Vec<SearchHit>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
    }

    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/searxng-rust.json");
    const FIXTURE_EMPTY: &[u8] = include_bytes!("../tests/fixtures/searxng-empty.json");

    struct FixtureBackend(&'static [u8]);

    #[async_trait::async_trait]
    impl WebSearchBackend for FixtureBackend {
        fn name(&self) -> &str {
            "searxng"
        }
        async fn search(&self, query: &str, max: usize) -> Result<Vec<SearchHit>, String> {
            Ok(self.search_page(query, max).await?.hits)
        }
        async fn search_page(&self, _query: &str, _max: usize) -> Result<SearchPage, String> {
            SearxngBackend::parse_response(self.0)
        }
    }

    #[test]
    fn response_accumulator_rejects_declared_and_streamed_overflow() {
        assert!(BodyAccumulator::new(
            WEB_RESPONSE_MAX_BYTES,
            Some((WEB_RESPONSE_MAX_BYTES + 1) as u64)
        )
        .is_err());

        let mut body = BodyAccumulator::new(8, None).unwrap();
        body.push(b"1234").unwrap();
        body.push(b"5678").unwrap();
        assert!(body.push(b"9").is_err());
        assert_eq!(body.finish(), b"12345678");
    }

    #[test]
    fn source_ids_are_stable_and_normalized() {
        let id = source_id("https://example.com/a/b?x=1").unwrap();
        assert_eq!(id.len(), 9);
        assert!(id.starts_with('S'));
        assert!(id[1..].bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(
            normalize_source_url("HTTPS://Example.COM:443/a/b?x=1#frag").as_deref(),
            Some("https://example.com/a/b?x=1")
        );
        // The id is a contract with recorded citations: pin the formula.
        assert_eq!(
            id,
            format!("S{}", &sha256_hex(b"https://example.com/a/b?x=1")[..8])
        );
        for same in [
            "https://EXAMPLE.com/a/b?x=1",
            "https://example.com:443/a/b?x=1",
            "https://example.com/a/b?x=1#section",
            "  https://example.com/a/b?x=1  ",
        ] {
            assert_eq!(source_id(same).as_deref(), Some(id.as_str()), "{same}");
        }
        assert_eq!(
            source_id("http://example.com").unwrap(),
            source_id("http://example.com:80/").unwrap()
        );
        // Path case, query, scheme and a non-default port still distinguish.
        for different in [
            "https://example.com/A/b?x=1",
            "https://example.com/a/b?x=2",
            "http://example.com/a/b?x=1",
            "https://example.com:8443/a/b?x=1",
        ] {
            assert_ne!(
                source_id(different).as_deref(),
                Some(id.as_str()),
                "{different}"
            );
        }
        for invalid in [
            "ftp://example.com/",
            "mailto:a@example.com",
            "not a url",
            "",
        ] {
            assert_eq!(source_id(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn searxng_fixture_maps_results_engines_and_unresponsive_engines() {
        let page = SearxngBackend::parse_response(FIXTURE).unwrap();
        let urls: Vec<&str> = page.hits.iter().map(|hit| hit.url.as_str()).collect();
        // The ftp, magnet and javascript results are dropped, and the
        // fragment and default-port duplicates of the first result are merged.
        assert_eq!(
            urls,
            [
                "https://www.rust-lang.org/",
                "https://doc.rust-lang.org/book/",
                "https://en.wikipedia.org/wiki/Rust_(programming_language)",
                "http://example.org/plain-http"
            ]
        );
        assert_eq!(page.hits[0].title, "Rust Programming Language");
        assert_eq!(page.hits[0].engines, ["duckduckgo", "brave"]);
        assert_eq!(page.hits[2].engines, ["wikipedia"]);
        assert!(page.hits[1]
            .snippet
            .contains("The Rust Programming Language"));
        assert_eq!(
            page.unresponsive_engines,
            ["google (timeout)", "qwant (Suspended: too many requests)"]
        );
        let empty = SearxngBackend::parse_response(FIXTURE_EMPTY).unwrap();
        assert!(empty.hits.is_empty() && empty.unresponsive_engines.is_empty());
        assert!(SearxngBackend::parse_response(b"<html>").is_err());
        assert!(SearxngBackend::parse_response(b"{\"query\": \"x\"}").is_err());
    }

    #[test]
    fn searxng_search_url_carries_settings() {
        let url = SearxngBackend::search_url(
            "http://127.0.0.1:41000/",
            "rust & go?",
            &SearxngQuerySettings {
                language: "en".into(),
                safesearch: 2,
                timeout: Duration::from_secs(5),
            },
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:41000/search?q=rust+%26+go%3F&format=json&pageno=1&language=en&safesearch=2"
        );
    }

    #[tokio::test]
    async fn searxng_output_has_source_ids_and_report() {
        let tool = WebSearchTool::new(Arc::new(FixtureBackend(FIXTURE)));
        assert_eq!(tool.description(), SEARXNG_DESCRIPTION);
        let (result, report) = tool
            .search_with_report(
                serde_json::json!({"query": "rust programming language", "max_results": 3}),
            )
            .await;
        let result = result.unwrap();
        assert_eq!(result["backend"], "searxng");
        assert_eq!(result["count"], 3);
        assert_eq!(result["total_count"], 4);
        assert_eq!(result["truncated"], true);
        let retrieved = result["retrieved_at"].as_str().unwrap();
        assert!(
            retrieved.ends_with('Z') && retrieved.len() == 20,
            "{retrieved}"
        );
        let first = &result["results"][0];
        assert_eq!(
            first["source_id"].as_str(),
            source_id("https://www.rust-lang.org/").as_deref()
        );
        assert_eq!(first["engines"], serde_json::json!(["duckduckgo", "brave"]));
        assert_eq!(first["title_truncated"], false);
        assert_eq!(
            result["unresponsive_engines"],
            serde_json::json!(["google (timeout)", "qwant (Suspended: too many requests)"])
        );
        assert_eq!(report.backend, "searxng");
        assert_eq!(report.results, Some(3));
        assert_eq!(report.query_bytes, Some(25));
        assert_eq!(
            report.query_sha256.as_deref(),
            Some(sha256_hex(b"rust programming language").as_str())
        );
        assert_eq!(report.sources.len(), 3);
        assert_eq!(report.sources[0].1, "https://www.rust-lang.org/");
        assert!(!report.refused && report.reason.is_none());
    }

    #[tokio::test]
    async fn web_results_bound_count_and_each_provider_field() {
        let tool = WebSearchTool::new(Arc::new(OversizedBackend));
        let result = tool
            .execute(serde_json::json!({
                "query": "bounded search",
                "max_results": WEB_MAX_RESULTS,
            }))
            .await
            .unwrap();

        assert_eq!(result["count"], WEB_MAX_RESULTS as u64);
        assert_eq!(result["total_count"], (WEB_MAX_RESULTS + 3) as u64);
        assert_eq!(result["truncated"], true);
        let hits = result["results"].as_array().unwrap();
        assert_eq!(hits.len(), WEB_MAX_RESULTS);
        for hit in hits {
            assert!(hit["title"].as_str().unwrap().len() <= WEB_TITLE_MAX_BYTES);
            assert!(hit["url"].as_str().unwrap().len() <= WEB_URL_MAX_BYTES);
            assert!(hit["snippet"].as_str().unwrap().len() <= WEB_SNIPPET_MAX_BYTES);
            assert_eq!(hit["field_truncated"], true);
            assert_eq!(hit["title_truncated"], true);
            assert_eq!(hit["url_truncated"], true);
            assert_eq!(hit["snippet_truncated"], true);
            // The id is computed from the full URL, not the truncated one.
            assert!(hit["source_id"].as_str().unwrap().starts_with('S'));
        }
    }

    #[tokio::test]
    async fn invalid_web_arguments_fail_before_provider_work() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tool = WebSearchTool::new(Arc::new(CountingBackend {
            calls: calls.clone(),
        }));

        let (result, report) = tool
            .search_with_report(serde_json::json!({
                "query": "q".repeat(WEB_QUERY_MAX_BYTES + 1)
            }))
            .await;
        assert!(result.is_err());
        assert!(report.refused);
        assert_eq!(report.reason.as_deref(), Some("invalid_arguments"));
        assert!(tool
            .execute(serde_json::json!({
                "query": "q",
                "max_results": WEB_MAX_RESULTS + 1,
            }))
            .await
            .is_err());
        assert!(tool
            .execute(serde_json::json!({"query": "q", "max_results": 0}))
            .await
            .is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn provider_errors_are_bounded_before_model_transport() {
        struct ErrorBackend;
        #[async_trait::async_trait]
        impl WebSearchBackend for ErrorBackend {
            fn name(&self) -> &str {
                "error"
            }
            async fn search(
                &self,
                _query: &str,
                _max_results: usize,
            ) -> Result<Vec<SearchHit>, String> {
                Err("🦀".repeat(crate::limits::TOOL_ERROR_MAX_BYTES))
            }
        }

        let (error, report) = WebSearchTool::new(Arc::new(ErrorBackend))
            .search_with_report(serde_json::json!({"query": "q"}))
            .await;
        let rendered = error.unwrap_err().to_string();
        assert!(rendered.contains("error detail truncated"));
        assert!(rendered.len() < crate::limits::TOOL_ERROR_MAX_BYTES + 256);
        assert_eq!(report.reason.as_deref(), Some("search_failed"));
        assert!(!report.refused);
    }

    #[test]
    fn unconfigured_backend_points_at_searxng() {
        let tool = WebSearchTool::from_config("", "");
        assert_eq!(tool.backend_name(), "none");
        assert_eq!(tool.description(), LEGACY_SEARCH_DESCRIPTION);
    }

    /// A fetcher that returns one canned page or error and records each URL.
    struct CannedFetcher {
        result: Result<FetchedPage, FetchError>,
        calls: Mutex<Vec<String>>,
    }

    impl CannedFetcher {
        fn page(content_type: &str, body: &[u8]) -> Self {
            Self {
                result: Ok(FetchedPage {
                    url: "https://example.com/a".into(),
                    final_url: "https://www.example.com/a".into(),
                    status: 200,
                    content_type: content_type.into(),
                    charset: None,
                    body: body.to_vec(),
                    body_truncated: false,
                    redirects: vec!["https://www.example.com/a".into()],
                }),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl PageFetcher for CannedFetcher {
        async fn fetch(&self, url: &str) -> Result<FetchedPage, FetchError> {
            self.calls.lock().unwrap().push(url.to_string());
            self.result.clone()
        }
    }

    #[tokio::test]
    async fn fetch_arguments_are_validated_before_any_request() {
        let fetcher = Arc::new(CannedFetcher::page("text/html", b"<p>x</p>"));
        let tool = WebFetchTool::new(fetcher.clone());
        for arguments in [
            serde_json::json!({}),
            serde_json::json!({"url": 7}),
            serde_json::json!({"url": "ftp://example.com/"}),
            serde_json::json!({"url": "file:///etc/passwd"}),
            serde_json::json!({"url": "javascript:alert(1)"}),
            serde_json::json!({"url": "https://user:secret@example.com/"}),
            serde_json::json!({"url": format!("https://example.com/{}", "a".repeat(WEB_FETCH_URL_MAX_CHARS))}),
            serde_json::json!({"url": "https://example.com/", "max_chars": 999}),
            serde_json::json!({"url": "https://example.com/", "max_chars": 65_537}),
            serde_json::json!({"url": "https://example.com/", "max_chars": "big"}),
            serde_json::json!({"url": "https://example.com/", "start_paragraph": 0}),
            serde_json::json!({"url": "https://example.com/", "headers": {"a": "b"}}),
            serde_json::json!("https://example.com/"),
        ] {
            let (result, report) = tool.fetch_with_report(arguments.clone()).await;
            assert!(
                matches!(result, Err(ToolError::InvalidArgs { .. })),
                "{arguments}: {result:?}"
            );
            assert!(report.refused, "{arguments}");
            assert_eq!(report.reason.as_deref(), Some("invalid_arguments"));
        }
        assert!(fetcher.calls.lock().unwrap().is_empty());
        assert_eq!(web_fetch_schema()["additionalProperties"], false);
    }

    #[tokio::test]
    async fn prepared_calls_request_nothing_until_run() {
        let fetcher = Arc::new(CannedFetcher::page("text/html", b"<p>x</p>"));
        let tool = WebFetchTool::new(fetcher.clone());
        let prepared = tool
            .prepare_fetch(&serde_json::json!({"url": "https://example.com/a#frag"}))
            .unwrap();
        assert_eq!(prepared.url(), "https://example.com/a#frag");
        assert!(
            fetcher.calls.lock().unwrap().is_empty(),
            "nothing requested yet"
        );
        let (result, report) = tool.run_fetch(prepared).await;
        assert!(result.is_ok());
        assert_eq!(fetcher.calls.lock().unwrap().len(), 1);
        assert!(report.content_sha256.is_some());
        let (error, report) = tool
            .prepare_fetch(&serde_json::json!({"url": "ftp://example.com/"}))
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArgs { .. }));
        assert!(report.refused);

        let search = WebSearchTool::new(Arc::new(NullBackend));
        let prepared = search
            .prepare_search(&serde_json::json!({"query": "rust"}))
            .unwrap();
        assert_eq!(prepared.query_bytes(), Some(4));
        assert_eq!(prepared.query_sha256(), Some(sha256_hex(b"rust").as_str()));
        assert!(search
            .prepare_search(&serde_json::json!({"query": 5}))
            .is_err());
    }

    #[test]
    fn null_is_not_given_for_optional_web_arguments() {
        let search = WebSearchTool::new(Arc::new(NullBackend));
        let prepared = search
            .prepare_search(&serde_json::json!({"query": "rust", "max_results": null}))
            .unwrap();
        assert_eq!(prepared.max, 5);
        let tool = WebFetchTool::new(Arc::new(CannedFetcher::page("text/html", b"<p>x</p>")));
        let prepared = tool
            .prepare_fetch(&serde_json::json!({
                "url": "https://example.com/", "max_chars": null, "start_paragraph": null,
            }))
            .unwrap();
        assert_eq!(prepared.arguments.max_chars, WEB_FETCH_DEFAULT_MAX_CHARS);
        assert_eq!(prepared.arguments.start_paragraph, 1);
        // The required fields are still required.
        assert!(search
            .prepare_search(&serde_json::json!({"query": null}))
            .is_err());
        assert!(tool
            .prepare_fetch(&serde_json::json!({"url": null}))
            .is_err());
    }

    #[tokio::test]
    async fn fetch_output_numbers_paragraphs_and_hashes_content() {
        let body = b"<html><head><title>Example</title></head><body><p>One</p><p>Two</p><p>Three</p></body></html>";
        let fetcher = Arc::new(CannedFetcher::page("text/html", body));
        let tool = WebFetchTool::new(fetcher.clone());
        let (result, report) = tool
            .fetch_with_report(serde_json::json!({"url": "https://example.com/a"}))
            .await;
        let result = result.unwrap();
        assert_eq!(
            fetcher.calls.lock().unwrap().as_slice(),
            ["https://example.com/a"]
        );
        assert_eq!(
            result["source_id"].as_str(),
            source_id("https://example.com/a").as_deref()
        );
        assert_eq!(result["title"], "Example");
        assert_eq!(result["content"], "¶1 One\n\n¶2 Two\n\n¶3 Three");
        assert_eq!(
            result["paragraphs"],
            serde_json::json!({"first": 1, "last": 3, "total": 3})
        );
        assert_eq!(result["content_truncated"], false);
        assert_eq!(result["final_url"], "https://www.example.com/a");
        assert_eq!(
            result["redirects"],
            serde_json::json!(["https://www.example.com/a"])
        );
        assert_eq!(
            result["content_sha256"].as_str(),
            Some(sha256_hex(body).as_str())
        );
        assert_eq!(
            result["text_sha256"].as_str(),
            Some(sha256_hex("One\n\nTwo\n\nThree".as_bytes()).as_str())
        );
        assert_eq!(report.status, Some(200));
        assert_eq!(report.bytes, Some(body.len() as u64));
        assert_eq!(report.source_id, source_id("https://example.com/a"));
        assert_eq!(
            report.final_url.as_deref(),
            Some("https://www.example.com/a")
        );
        assert!(!report.refused);

        let (window, _) = tool
            .fetch_with_report(
                serde_json::json!({"url": "https://example.com/a", "start_paragraph": 2}),
            )
            .await;
        let window = window.unwrap();
        assert_eq!(window["content"], "¶2 Two\n\n¶3 Three");
        assert_eq!(window["paragraphs"]["first"], 2);

        let (past, _) = tool
            .fetch_with_report(
                serde_json::json!({"url": "https://example.com/a", "start_paragraph": 9}),
            )
            .await;
        let past = past.unwrap();
        assert_eq!(past["content"], "");
        assert!(past["note"].as_str().unwrap().contains("past the last"));
    }

    #[test]
    fn window_stops_at_max_chars_and_cuts_only_a_first_oversized_paragraph() {
        let paragraphs: Vec<String> = (0..10)
            .map(|index| format!("{index}{}", "x".repeat(499)))
            .collect();
        let (content, last, cut) = render_window(&paragraphs, 1, 1_100);
        assert_eq!(last, 2);
        assert!(!cut);
        assert!(content.chars().count() <= 1_100);
        assert!(content.starts_with("¶1 0x") && content.contains("\n\n¶2 1x"));
        let long = vec!["é".repeat(5_000)];
        let (content, last, cut) = render_window(&long, 1, 1_000);
        assert_eq!((last, cut), (1, true));
        assert_eq!(content.chars().count(), 1_000);
    }

    #[tokio::test]
    async fn fetch_truncation_and_formats() {
        let paragraphs: String = (1..=200)
            .map(|index| format!("<p>paragraph {index} {}</p>", "w ".repeat(100)))
            .collect();
        let tool = WebFetchTool::new(Arc::new(CannedFetcher::page(
            "text/html",
            paragraphs.as_bytes(),
        )));
        let (result, _) = tool
            .fetch_with_report(
                serde_json::json!({"url": "https://example.com/a", "max_chars": 1000}),
            )
            .await;
        let result = result.unwrap();
        assert_eq!(result["content_truncated"], true);
        assert_eq!(result["paragraphs"]["total"], 200);
        assert!(result["content"].as_str().unwrap().chars().count() <= 1000);

        let markdown = WebFetchTool::new(Arc::new(CannedFetcher::page(
            "text/markdown",
            b"# Title\n\nBody line\nnext\n\n- a",
        )));
        let (result, _) = markdown
            .fetch_with_report(serde_json::json!({"url": "https://example.com/a"}))
            .await;
        assert_eq!(
            result.unwrap()["content"],
            "¶1 # Title\n\n¶2 Body line\nnext\n\n¶3 - a"
        );

        let json = WebFetchTool::new(Arc::new(CannedFetcher::page(
            "application/json",
            b"{\"a\": 1}\n\n{\"b\": 2}",
        )));
        let (result, _) = json
            .fetch_with_report(serde_json::json!({"url": "https://example.com/a"}))
            .await;
        assert_eq!(result.unwrap()["paragraphs"]["total"], 1);

        let mut capped = CannedFetcher::page("text/plain", b"short");
        if let Ok(page) = &mut capped.result {
            page.body_truncated = true;
        }
        let (result, _) = WebFetchTool::new(Arc::new(capped))
            .fetch_with_report(serde_json::json!({"url": "https://example.com/a"}))
            .await;
        assert_eq!(result.unwrap()["content_truncated"], true);
    }

    #[test]
    fn declared_charset_decodes_the_body() {
        assert_eq!(decode_body(b"caf\xe9", Some("iso-8859-1")), "café");
        assert_eq!(decode_body("café".as_bytes(), None), "café");
        assert_eq!(decode_body(b"caf\xe9", None), "caf\u{FFFD}");
        assert_eq!(decode_body(b"x", Some("no-such-charset")), "x");
    }

    #[tokio::test]
    async fn fetch_refusals_and_failures_are_reported() {
        let refused = CannedFetcher {
            result: Err(FetchError {
                kind: FetchErrorKind::PrivateDestination,
                detail: "internal.test resolves to 10.0.0.1 (private)".into(),
                redirects: vec!["http://internal.test/".into()],
            }),
            calls: Mutex::new(Vec::new()),
        };
        let (result, report) = WebFetchTool::new(Arc::new(refused))
            .fetch_with_report(serde_json::json!({"url": "https://example.com/a"}))
            .await;
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("refused") && error.contains("private_destination"),
            "{error}"
        );
        assert!(report.refused);
        assert_eq!(report.reason.as_deref(), Some("private_destination"));
        assert_eq!(report.redirects, ["http://internal.test/"]);
        assert!(report.content_sha256.is_none());

        let failed = CannedFetcher {
            result: Err(FetchError {
                kind: FetchErrorKind::UnsupportedContentType,
                detail: "image/png".into(),
                redirects: Vec::new(),
            }),
            calls: Mutex::new(Vec::new()),
        };
        let (result, report) = WebFetchTool::new(Arc::new(failed))
            .fetch_with_report(serde_json::json!({"url": "https://example.com/a"}))
            .await;
        assert!(result.unwrap_err().to_string().contains("image/png"));
        assert!(!report.refused);
        assert_eq!(report.reason.as_deref(), Some("unsupported_content_type"));
    }
}
