//! Web search tool for session agents.
//!
//! The backend is pluggable behind [`WebSearchBackend`]. The default is
//! Tavily — purpose-built for AI agents: it returns clean, extracted content,
//! so the agent gets usable results in one call. With no provider configured,
//! [`NullBackend`] returns a clear "not configured" error.

use std::sync::Arc;

use crate::builtin::BuiltinTool;
use crate::error::ToolError;

/// One search result.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// A pluggable web-search provider.
#[async_trait::async_trait]
pub trait WebSearchBackend: Send + Sync + 'static {
    /// Provider name (for diagnostics).
    fn name(&self) -> &str;
    /// Run a search, returning up to `max_results` hits.
    async fn search(&self, query: &str, max_results: usize) -> Result<Vec<SearchHit>, String>;
}

/// Tavily backend — `https://api.tavily.com/search`.
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
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("Tavily returned HTTP {}", resp.status()));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("bad response: {e}"))?;
        let hits = body["results"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|r| SearchHit {
                        title: r["title"].as_str().unwrap_or("").to_string(),
                        url: r["url"].as_str().unwrap_or("").to_string(),
                        snippet: r["content"].as_str().unwrap_or("").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(hits)
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
            "web search is not configured — add a [web_search] block with a \
             provider and api_key to axocoatl.yaml"
                .to_string(),
        )
    }
}

/// The `web_search` tool — searches the web via the configured backend.
pub struct WebSearchTool {
    backend: Arc<dyn WebSearchBackend>,
}

impl WebSearchTool {
    pub fn new(backend: Arc<dyn WebSearchBackend>) -> Self {
        Self { backend }
    }

    /// Build from config: Tavily when a key is present, else the null backend.
    pub fn from_config(provider: &str, api_key: &str) -> Self {
        let backend: Arc<dyn WebSearchBackend> = match provider {
            "tavily" if !api_key.is_empty() => Arc::new(TavilyBackend::new(api_key.to_string())),
            _ => Arc::new(NullBackend),
        };
        Self { backend }
    }
}

#[async_trait::async_trait]
impl BuiltinTool for WebSearchTool {
    fn description(&self) -> &str {
        "Search the web for current, real-world information. Returns titles, \
         URLs, and content snippets."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to search for" },
                "max_results": { "type": "integer", "description": "How many results (default 5)" }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let query = arguments
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: "web_search".to_string(),
                reason: "expected string field 'query'".to_string(),
            })?;
        let max = arguments
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, 15) as usize;

        let hits =
            self.backend
                .search(query, max)
                .await
                .map_err(|e| ToolError::ExecutionFailed {
                    tool: "web_search".to_string(),
                    reason: e,
                })?;

        Ok(serde_json::json!({
            "results": hits
                .iter()
                .map(|h| serde_json::json!({
                    "title": h.title, "url": h.url, "snippet": h.snippet,
                }))
                .collect::<Vec<_>>(),
        }))
    }
}
