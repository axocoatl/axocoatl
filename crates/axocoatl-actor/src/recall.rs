//! Agent-callable memory recall tools (MemGPT/Letta-style).
//!
//! These are per-agent [`BuiltinTool`]s — each captures THIS agent's memory
//! stores. They are owned by the behavior, not the shared `ToolExecutor` (whose
//! tools receive no agent identity in `execute`, so they can't reach a specific
//! agent's per-agent stores). `recall_search` does semantic search over Tier-4
//! memory; `recall_timeframe` reads the Tier-2 daily log by date.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use axocoatl_memory::{DailyLogMemory, SemanticMemory};
use axocoatl_tools::{BuiltinTool, ToolError};

pub const RECALL_SEARCH: &str = "recall_search";
pub const RECALL_TIMEFRAME: &str = "recall_timeframe";

/// Upper bound on `k`, so a hallucinated value can't request an unbounded scan.
const MAX_K: usize = 20;
/// Largest span `recall_timeframe` will scan — `read_range` iterates day-by-day,
/// so an unbounded range from a hallucinated date would loop for ages.
const MAX_RANGE_DAYS: i64 = 31;
/// Cap on entries returned from a timeframe read (a day's log can be huge).
const MAX_TIMEFRAME_ENTRIES: usize = 40;
/// Per-entry content preview length, in characters.
const ENTRY_PREVIEW_CHARS: usize = 500;

/// `recall_search` — semantic search over the agent's long-term (Tier-4) memory.
pub struct RecallSearchTool {
    semantic: Arc<SemanticMemory>,
    default_k: usize,
    min_score: f32,
}

impl RecallSearchTool {
    pub fn new(semantic: Arc<SemanticMemory>, default_k: usize, min_score: f32) -> Self {
        Self {
            semantic,
            default_k,
            min_score,
        }
    }
}

#[async_trait]
impl BuiltinTool for RecallSearchTool {
    fn description(&self) -> &str {
        "Search your memory of past sessions and earlier in this conversation for \
         information related to a query. Use this when the user refers to something you don't \
         see in the current conversation, or before saying you don't know or don't remember."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to search your memory for" },
                "k": {
                    "type": "integer",
                    "description": "Max results (default 5, max 20)",
                    "minimum": 1,
                    "maximum": 20
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, arguments: Value) -> Result<Value, ToolError> {
        let query = arguments
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: RECALL_SEARCH.to_string(),
                reason: "missing or empty 'query'".to_string(),
            })?;

        // Accept `k` as a number or a stringified number — models routinely
        // pass `"k": "20"` instead of `"k": 20`.
        let k = arguments
            .get("k")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            })
            .map(|n| (n as usize).clamp(1, MAX_K))
            .unwrap_or_else(|| self.default_k.clamp(1, MAX_K));

        let hits = self
            .semantic
            .search(query, k)
            .map_err(|e| ToolError::ExecutionFailed {
                tool: RECALL_SEARCH.to_string(),
                reason: e.to_string(),
            })?;

        // Same relevance bar as the passive injection path, so the two agree.
        let results: Vec<Value> = hits
            .into_iter()
            .filter(|h| h.score > self.min_score)
            .map(|h| json!({ "text": h.text, "score": h.score }))
            .collect();

        Ok(json!({ "count": results.len(), "results": results }))
    }
}

/// `recall_timeframe` — read the agent's raw daily log (Tier-2) for a date/range.
pub struct RecallTimeframeTool {
    daily_log: Arc<DailyLogMemory>,
}

impl RecallTimeframeTool {
    pub fn new(daily_log: Arc<DailyLogMemory>) -> Self {
        Self { daily_log }
    }
}

fn parse_date(s: &str) -> Result<chrono::NaiveDate, ToolError> {
    chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").map_err(|_| ToolError::InvalidArgs {
        tool: RECALL_TIMEFRAME.to_string(),
        reason: format!("invalid date '{s}', expected YYYY-MM-DD"),
    })
}

fn preview_content(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if s.chars().count() > ENTRY_PREVIEW_CHARS {
        let head: String = s.chars().take(ENTRY_PREVIEW_CHARS).collect();
        format!("{head}…")
    } else {
        s
    }
}

#[async_trait]
impl BuiltinTool for RecallTimeframeTool {
    fn description(&self) -> &str {
        "Read your raw daily activity log for a specific date or date range (YYYY-MM-DD). Use \
         this to recall what happened on a particular day. For relative references like \
         'yesterday', compute the calendar date first."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "date": { "type": "string", "description": "A single day, YYYY-MM-DD" },
                "from": { "type": "string", "description": "Range start, YYYY-MM-DD (use with 'to')" },
                "to":   { "type": "string", "description": "Range end, YYYY-MM-DD (use with 'from')" }
            }
        })
    }

    async fn execute(&self, arguments: Value) -> Result<Value, ToolError> {
        let date = arguments.get("date").and_then(|v| v.as_str());
        let from_arg = arguments.get("from").and_then(|v| v.as_str());
        let to_arg = arguments.get("to").and_then(|v| v.as_str());

        let (mut from, mut to) = if let Some(d) = date {
            let d = parse_date(d)?;
            (d, d)
        } else if let (Some(f), Some(t)) = (from_arg, to_arg) {
            (parse_date(f)?, parse_date(t)?)
        } else {
            return Err(ToolError::InvalidArgs {
                tool: RECALL_TIMEFRAME.to_string(),
                reason: "provide either 'date' or both 'from' and 'to' (YYYY-MM-DD)".to_string(),
            });
        };

        if from > to {
            std::mem::swap(&mut from, &mut to);
        }
        if (to - from).num_days() > MAX_RANGE_DAYS {
            return Err(ToolError::InvalidArgs {
                tool: RECALL_TIMEFRAME.to_string(),
                reason: format!("date range too large (max {MAX_RANGE_DAYS} days)"),
            });
        }

        let entries =
            self.daily_log
                .read_range(from, to)
                .await
                .map_err(|e| ToolError::ExecutionFailed {
                    tool: RECALL_TIMEFRAME.to_string(),
                    reason: e.to_string(),
                })?;

        let total = entries.len();
        let truncated = total > MAX_TIMEFRAME_ENTRIES;
        // Keep the most recent entries (in chronological order) when over the cap.
        let start = total.saturating_sub(MAX_TIMEFRAME_ENTRIES);
        let shown: Vec<Value> = entries
            .into_iter()
            .skip(start)
            .map(|e| {
                json!({
                    "timestamp": e.timestamp,
                    "type": format!("{:?}", e.entry_type),
                    "content": preview_content(&e.content),
                })
            })
            .collect();

        Ok(json!({
            "date_range": format!("{from}..{to}"),
            "count": total,
            "truncated": truncated,
            "entries": shown,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_memory::{LogEntry, LogEntryType};

    fn semantic(dir: &std::path::Path) -> Arc<SemanticMemory> {
        Arc::new(SemanticMemory::new_hashed("test-agent", dir).unwrap())
    }

    #[tokio::test]
    async fn recall_search_returns_stored_hit() {
        let dir = tempfile::tempdir().unwrap();
        let mem = semantic(dir.path());
        mem.store(
            "the auth refactor moved tokens to httpOnly cookies",
            json!({}),
        )
        .unwrap();
        let tool = RecallSearchTool::new(mem, 5, 0.0);
        let out = tool
            .execute(json!({ "query": "auth refactor tokens cookies" }))
            .await
            .unwrap();
        assert!(out["count"].as_u64().unwrap() >= 1, "got {out}");
        assert!(out["results"][0]["text"].as_str().unwrap().contains("auth"));
    }

    #[tokio::test]
    async fn recall_search_min_score_filters() {
        let dir = tempfile::tempdir().unwrap();
        let mem = semantic(dir.path());
        mem.store("alpha beta gamma delta", json!({})).unwrap();
        // An impossibly high bar filters everything out → valid empty result.
        let tool = RecallSearchTool::new(mem, 5, 0.999);
        let out = tool
            .execute(json!({ "query": "something unrelated entirely" }))
            .await
            .unwrap();
        assert_eq!(out["count"].as_u64().unwrap(), 0);
        assert!(out["results"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn recall_search_rejects_empty_query() {
        let dir = tempfile::tempdir().unwrap();
        let tool = RecallSearchTool::new(semantic(dir.path()), 5, 0.15);
        assert!(tool.execute(json!({ "query": "  " })).await.is_err());
        assert!(tool.execute(json!({})).await.is_err());
    }

    #[tokio::test]
    async fn recall_search_accepts_stringified_k() {
        // Real models routinely pass `"k": "2"` (a string) — it must be honored,
        // not silently dropped to the default. Three matching records, k="2"
        // → exactly 2 results (default 5 would return all 3).
        let dir = tempfile::tempdir().unwrap();
        let mem = semantic(dir.path());
        for t in ["shared word one", "shared word two", "shared word three"] {
            mem.store(t, json!({})).unwrap();
        }
        let tool = RecallSearchTool::new(mem, 5, 0.0);
        let out = tool
            .execute(json!({ "query": "shared word", "k": "2" }))
            .await
            .unwrap();
        assert_eq!(
            out["count"].as_u64().unwrap(),
            2,
            "stringified k honored: {out}"
        );
    }

    async fn daily_log_with_entries(dir: &std::path::Path, n: usize) -> Arc<DailyLogMemory> {
        let log = Arc::new(DailyLogMemory::new("test-agent", dir));
        for i in 0..n {
            log.append(LogEntry {
                timestamp: i as u64,
                entry_type: LogEntryType::Note,
                content: json!(format!("entry number {i}")),
            })
            .await
            .unwrap();
        }
        log
    }

    #[tokio::test]
    async fn recall_timeframe_reads_today() {
        let dir = tempfile::tempdir().unwrap();
        let log = daily_log_with_entries(dir.path(), 3).await;
        let today = chrono::Local::now().date_naive().to_string();
        let tool = RecallTimeframeTool::new(log);
        let out = tool.execute(json!({ "date": today })).await.unwrap();
        assert_eq!(out["count"].as_u64().unwrap(), 3);
        assert_eq!(out["truncated"], json!(false));
        assert_eq!(out["entries"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn recall_timeframe_truncates_large_days() {
        let dir = tempfile::tempdir().unwrap();
        let log = daily_log_with_entries(dir.path(), MAX_TIMEFRAME_ENTRIES + 5).await;
        let today = chrono::Local::now().date_naive().to_string();
        let tool = RecallTimeframeTool::new(log);
        let out = tool.execute(json!({ "date": today })).await.unwrap();
        assert_eq!(
            out["count"].as_u64().unwrap(),
            (MAX_TIMEFRAME_ENTRIES + 5) as u64
        );
        assert_eq!(out["truncated"], json!(true));
        assert_eq!(
            out["entries"].as_array().unwrap().len(),
            MAX_TIMEFRAME_ENTRIES
        );
    }

    #[tokio::test]
    async fn recall_timeframe_validates_args() {
        let dir = tempfile::tempdir().unwrap();
        let tool = RecallTimeframeTool::new(Arc::new(DailyLogMemory::new("a", dir.path())));
        // No args.
        assert!(tool.execute(json!({})).await.is_err());
        // Bad date.
        assert!(tool.execute(json!({ "date": "not-a-date" })).await.is_err());
        // Range too large.
        assert!(tool
            .execute(json!({ "from": "2020-01-01", "to": "2025-01-01" }))
            .await
            .is_err());
    }
}
