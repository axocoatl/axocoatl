//! Web search tool for session agents.
//!
//! The backend is pluggable behind [`WebSearchBackend`]. The default is
//! Tavily — purpose-built for AI agents: it returns clean, extracted content,
//! so the agent gets usable results in one call. With no provider configured,
//! [`NullBackend`] returns a clear "not configured" error.

use std::sync::Arc;
use std::time::Duration;

use crate::builtin::BuiltinTool;
use crate::error::ToolError;
use crate::limits::{ensure_json_input, limit_error_text, limit_text, SIMPLE_TOOL_INPUT_MAX_BYTES};

const WEB_SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
const WEB_RESPONSE_MAX_BYTES: usize = 2 * 1024 * 1024;
const WEB_ERROR_BODY_MAX_BYTES: usize = 64 * 1024;
const WEB_QUERY_MAX_BYTES: usize = 8 * 1024;
const WEB_MAX_RESULTS: usize = 15;
const WEB_TITLE_MAX_BYTES: usize = 512;
const WEB_URL_MAX_BYTES: usize = 4 * 1024;
const WEB_SNIPPET_MAX_BYTES: usize = 8 * 1024;

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
         URLs, and bounded content snippets."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to search for (maximum 8 KiB)" },
                "max_results": { "type": "integer", "description": "How many results (default 5, maximum 15)", "minimum": 1, "maximum": 15 }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        ensure_json_input(&arguments, "web_search", SIMPLE_TOOL_INPUT_MAX_BYTES)?;
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
            None => 5,
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

        let hits =
            self.backend
                .search(query, max)
                .await
                .map_err(|e| ToolError::ExecutionFailed {
                    tool: "web_search".to_string(),
                    reason: limit_error_text(e),
                })?;

        let total_count = hits.len();
        let mut any_field_truncated = false;
        let results = hits
            .into_iter()
            .take(max)
            .map(|hit| {
                let title = limit_text(hit.title, WEB_TITLE_MAX_BYTES);
                let url = limit_text(hit.url, WEB_URL_MAX_BYTES);
                let snippet = limit_text(hit.snippet, WEB_SNIPPET_MAX_BYTES);
                let field_truncated = title.truncated || url.truncated || snippet.truncated;
                any_field_truncated |= field_truncated;
                serde_json::json!({
                    "title": title.text,
                    "url": url.text,
                    "snippet": snippet.text,
                    "field_truncated": field_truncated,
                    "title_truncated": title.truncated,
                    "url_truncated": url.truncated,
                    "snippet_truncated": snippet.truncated,
                    "title_original_bytes": title.original_bytes,
                    "url_original_bytes": url.original_bytes,
                    "snippet_original_bytes": snippet.original_bytes,
                })
            })
            .collect::<Vec<_>>();
        let count = results.len();
        Ok(serde_json::json!({
            "results": results,
            "count": count,
            "total_count": total_count,
            "truncated": total_count > count || any_field_truncated,
            "result_limit": max,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        }
    }

    #[tokio::test]
    async fn invalid_web_arguments_fail_before_provider_work() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tool = WebSearchTool::new(Arc::new(CountingBackend {
            calls: calls.clone(),
        }));

        assert!(tool
            .execute(serde_json::json!({
                "query": "q".repeat(WEB_QUERY_MAX_BYTES + 1)
            }))
            .await
            .is_err());
        assert!(tool
            .execute(serde_json::json!({
                "query": "q",
                "max_results": WEB_MAX_RESULTS + 1,
            }))
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

        let error = WebSearchTool::new(Arc::new(ErrorBackend))
            .execute(serde_json::json!({"query": "q"}))
            .await
            .unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("error detail truncated"));
        assert!(rendered.len() < crate::limits::TOOL_ERROR_MAX_BYTES + 256);
    }
}
