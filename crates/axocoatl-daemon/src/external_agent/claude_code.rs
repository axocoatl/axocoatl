//! Claude Code CLI: headless `claude -p --output-format stream-json`.
//! Owner: workstream `agents`.
//!
//! Verified against the pinned 2.1.292 (`claude --help` in the recipe image,
//! and runs against a recording fake API, `fixtures/claude-code-2.1.292-*`):
//! `-p` reads the prompt from stdin, `--output-format stream-json` needs
//! `--verbose`, `--dangerously-skip-permissions` bypasses permission prompts
//! (the Session container is the boundary; the program runs as the non-root
//! writer user, where the flag is accepted), and `--no-session-persistence`
//! keeps sessions off disk. With `CLAUDE_CODE_OAUTH_TOKEN` set (to the
//! route's placeholder) and nonessential traffic disabled, its model
//! requests are `POST /v1/messages?beta=true` to `api.anthropic.com`, with
//! `Authorization: Bearer <token>`; the route replaces the placeholder. It
//! also asks `api.anthropic.com` for `GET /api/claude_code/policy_limits` and
//! `GET /api/claude_code/settings`, which the route refuses (and records); a
//! run goes on without them (`actual_loadout_run_with_the_pinned_claude_code`
//! in `bootstrap_external_turn_tests.rs`).

use serde_json::Value;

use super::json::{preview, str_at, u64_at};
use super::{
    bound_text, json_lines, ExternalActivationResult, ExternalAgentError, ExternalItem, Items,
    MAX_ANSWER_BYTES,
};

/// The npm package the recipe installs (its native linux build).
pub const CLAUDE_CODE_PACKAGE: &str = "@anthropic-ai/claude-code";
/// The pinned version.
pub const CLAUDE_CODE_VERSION: &str = "2.1.292";
/// Secret name `axocoatl secret set` stores the `claude setup-token` output as.
pub const CLAUDE_CODE_SECRET: &str = "claude-code-oauth";
/// The definition provider of a Claude Code writer.
pub const PROVIDER: &str = "claude-code";
/// The provider of the model API the program calls, as a loadout names its
/// model (`{provider: anthropic, model: …}`) and the Outcome records it.
pub const MODEL_PROVIDER: &str = "anthropic";
/// The model API host its route serves.
pub const API_HOST: &str = "api.anthropic.com";
/// The variable the route sets to its placeholder in the program's
/// environment.
pub const TOKEN_PLACEHOLDER_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";
/// Fixed environment of every run: no telemetry, error reports, update
/// checks or other nonessential traffic.
pub const ENV: &[(&str, &str)] = &[
    ("DISABLE_TELEMETRY", "1"),
    ("DISABLE_ERROR_REPORTING", "1"),
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ("DISABLE_AUTOUPDATER", "1"),
];

/// The argv that runs one activation (the prompt comes on stdin).
pub fn argv(model: &str) -> Result<Vec<String>, ExternalAgentError> {
    super::validate_model(model)?;
    Ok([
        "claude",
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
        model,
        "--dangerously-skip-permissions",
        "--no-session-persistence",
    ]
    .iter()
    .map(|arg| (*arg).to_string())
    .collect())
}

/// `--max-budget-usd` at `cost_microunits`: Claude Code's own spending stop
/// (its cost is its own computation from list prices), set to what the
/// activation's grant still allows.
pub fn budget_args(cost_microunits: u64) -> Vec<String> {
    vec![
        "--max-budget-usd".into(),
        format!(
            "{}.{:06}",
            cost_microunits / 1_000_000,
            cost_microunits % 1_000_000
        ),
    ]
}

/// Dollars to micro-dollars, rounded up, after dropping floating-point noise
/// below a thousandth of a micro-dollar (`0.000123` is 123, not 124).
fn usd_to_microunits(cost: f64) -> Option<u64> {
    (cost.is_finite() && cost >= 0.0)
        .then(|| ((cost * 1_000_000_000.0).round() / 1000.0).ceil() as u64)
}

/// Text of a `tool_result` content (a string or a list of text blocks).
fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| match str_at(block, &["type"]) {
                Some("text") => str_at(block, &["text"]).unwrap_or_default().to_string(),
                Some(other) => format!("[{other}]"),
                None => preview(block),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => preview(other),
    }
}

/// Parse the program's stream-json output.
pub fn parse_output(stdout: &[u8]) -> Result<ExternalActivationResult, ExternalAgentError> {
    let mut items = Items::default();
    let values = json_lines(stdout, &mut items);
    let mut tool_names = std::collections::HashMap::<String, String>::new();
    let mut final_answer = None;
    let mut usage_complete = false;
    let mut saw_result = false;
    for value in &values {
        match str_at(value, &["type"]) {
            Some("assistant") => {
                if let Some(error) = str_at(value, &["error"]) {
                    let text = value["message"]["content"]
                        .as_array()
                        .and_then(|blocks| blocks.first())
                        .and_then(|block| str_at(block, &["text"]))
                        .unwrap_or_default();
                    items.push(ExternalItem::Error {
                        message: format!("{error}: {text}"),
                    });
                    continue;
                }
                for block in value["message"]["content"].as_array().into_iter().flatten() {
                    match str_at(block, &["type"]) {
                        Some("text") => {
                            let text = str_at(block, &["text"]).unwrap_or_default();
                            if !text.trim().is_empty() {
                                items.push(ExternalItem::AssistantText { text: text.into() });
                            }
                        }
                        Some("tool_use") => {
                            let name = str_at(block, &["name"]).unwrap_or("tool").to_string();
                            if let Some(id) = str_at(block, &["id"]) {
                                tool_names.insert(id.to_string(), name.clone());
                            }
                            items.push(ExternalItem::ToolCall {
                                name,
                                arguments: preview(&block["input"]),
                            });
                        }
                        // Thinking and other blocks are not recorded.
                        _ => {}
                    }
                }
            }
            Some("user") => {
                for block in value["message"]["content"].as_array().into_iter().flatten() {
                    if str_at(block, &["type"]) != Some("tool_result") {
                        continue;
                    }
                    let name = str_at(block, &["tool_use_id"])
                        .and_then(|id| tool_names.get(id))
                        .cloned()
                        .unwrap_or_else(|| "tool".into());
                    items.push(ExternalItem::ToolResult {
                        name,
                        output: tool_result_text(&block["content"]),
                        is_error: block["is_error"].as_bool().unwrap_or(false),
                    });
                }
            }
            Some("system") if str_at(value, &["subtype"]) == Some("api_retry") => {
                items.push(ExternalItem::Error {
                    message: format!(
                        "model request retried (attempt {} of {}): {}{}",
                        u64_at(value, &["attempt"]).unwrap_or(0),
                        u64_at(value, &["max_retries"]).unwrap_or(0),
                        str_at(value, &["error"]).unwrap_or("error"),
                        u64_at(value, &["error_status"])
                            .map_or_else(String::new, |status| format!(" (HTTP {status})")),
                    ),
                })
            }
            Some("rate_limit_event") => {
                let status = str_at(value, &["rate_limit_info", "status"]).unwrap_or("unknown");
                if status != "allowed" {
                    items.push(ExternalItem::Error {
                        message: format!("rate limit: {status}"),
                    });
                }
            }
            Some("result") => {
                saw_result = true;
                let usage = &value["usage"];
                let reported = usage.is_object();
                let written = u64_at(usage, &["cache_creation_input_tokens"]).unwrap_or(0);
                let cached = u64_at(usage, &["cache_read_input_tokens"]).unwrap_or(0);
                let input = u64_at(usage, &["input_tokens"])
                    .unwrap_or(0)
                    .saturating_add(written)
                    .saturating_add(cached);
                let output = u64_at(usage, &["output_tokens"]).unwrap_or(0);
                let cost = value["total_cost_usd"].as_f64().and_then(usd_to_microunits);
                if reported {
                    usage_complete = true;
                    items.push(ExternalItem::Usage {
                        input_tokens: input,
                        output_tokens: output,
                        cost_microunits: cost,
                        cached_input_tokens: cached,
                        cache_write_tokens: written,
                    });
                }
                let is_error = value["is_error"].as_bool().unwrap_or(true)
                    || str_at(value, &["subtype"]).is_some_and(|kind| kind.starts_with("error"));
                let text = str_at(value, &["result"]).unwrap_or_default();
                if is_error {
                    let mut message = format!(
                        "the run ended with an error ({}{})",
                        str_at(value, &["terminal_reason"])
                            .or_else(|| str_at(value, &["subtype"]))
                            .unwrap_or("error"),
                        u64_at(value, &["api_error_status"])
                            .map_or_else(String::new, |status| format!(", HTTP {status}")),
                    );
                    if !text.is_empty() {
                        message.push_str(": ");
                        message.push_str(text);
                    }
                    if let Some(errors) = value["errors"].as_array() {
                        for error in errors.iter().take(8) {
                            message.push_str("; ");
                            message.push_str(&preview(error));
                        }
                    }
                    items.push(ExternalItem::Error { message });
                } else if !text.trim().is_empty() {
                    final_answer = Some(bound_text(text, MAX_ANSWER_BYTES));
                }
            }
            _ => {}
        }
    }
    if !saw_result {
        items.push(ExternalItem::Error {
            message: "the program printed no result: it ended before finishing its turn".into(),
        });
    }
    Ok(ExternalActivationResult {
        final_answer,
        items: items.finish(),
        exit_code: None,
        usage_complete,
    })
}
