//! Codex CLI: headless `codex exec --json`. Owner: workstream `agents`.
//!
//! Verified against the pinned 0.160.1 (`codex exec --help` in the recipe
//! image, and runs against a recording fake API,
//! `fixtures/codex-0.160.1-*`). With its built-in OpenAI provider the
//! program first opens a WebSocket to `wss://api.openai.com/v1/responses`
//! (a route refuses upgrades) and also connects to `chatgpt.com`,
//! `github.com` and `ab.chatgpt.com`; and it sends no `Authorization` header
//! for an API key given only in `OPENAI_API_KEY`. So each run defines its
//! own HTTP-only provider (`model_providers.axocoatl`: the official base URL,
//! the Responses API, no WebSockets, the key from `OPENAI_API_KEY`) and turns
//! off the apps, plugins, update check and analytics. The only requests are
//! then `POST /v1/responses` with `Authorization: Bearer <key>`, which the
//! route replaces. Codex's own sandbox is disabled inside the container
//! (`--dangerously-bypass-approvals-and-sandbox`): the Session container,
//! its hardened writer user and the egress routes are the boundary.
//!
//! The parser also reads the Codex app-server notifications that the
//! adapter removed in b922287 recorded (`fixtures/codex-0.153.4-*`).

use serde_json::Value;

use super::json::{preview, str_at, u64_at};
use super::{
    bound_text, json_lines, ExternalActivationResult, ExternalAgentError, ExternalItem, Items,
    MAX_ANSWER_BYTES,
};

/// The npm package the recipe installs (its native linux build).
pub const CODEX_PACKAGE: &str = "@openai/codex";
/// The pinned version.
pub const CODEX_VERSION: &str = "0.160.1";
/// Secret name for the OpenAI API key.
pub const CODEX_SECRET: &str = "codex-openai";
/// The definition provider of a Codex writer.
pub const PROVIDER: &str = "codex";
/// The model API host its route serves.
pub const API_HOST: &str = "api.openai.com";
/// The variable the route sets to its placeholder in the program's
/// environment.
pub const TOKEN_PLACEHOLDER_ENV: &str = "OPENAI_API_KEY";
/// Fixed environment of every run.
pub const ENV: &[(&str, &str)] = &[("NO_COLOR", "1")];

/// The argv that runs one activation (the prompt comes on stdin, `-`).
pub fn argv(model: &str) -> Result<Vec<String>, ExternalAgentError> {
    super::validate_model(model)?;
    let mut argv: Vec<String> = [
        "codex",
        "exec",
        "--json",
        "--model",
        model,
        "--dangerously-bypass-approvals-and-sandbox",
        "--skip-git-repo-check",
        "--ephemeral",
        "--color",
        "never",
    ]
    .iter()
    .map(|arg| (*arg).to_string())
    .collect();
    for feature in [
        "apps",
        "plugins",
        "remote_plugin",
        "unbounded_connection_retries",
        "tool_suggest",
        "skill_mcp_dependency_install",
    ] {
        argv.push("--disable".into());
        argv.push(feature.into());
    }
    for setting in [
        "check_for_update_on_startup=false".to_string(),
        "analytics.enabled=false".to_string(),
        "model_provider=\"axocoatl\"".to_string(),
        "model_providers.axocoatl.name=\"axocoatl\"".to_string(),
        format!("model_providers.axocoatl.base_url=\"https://{API_HOST}/v1\""),
        "model_providers.axocoatl.wire_api=\"responses\"".to_string(),
        "model_providers.axocoatl.supports_websockets=false".to_string(),
        format!("model_providers.axocoatl.env_key=\"{TOKEN_PLACEHOLDER_ENV}\""),
    ] {
        argv.push("-c".into());
        argv.push(setting);
    }
    argv.push("-".into());
    Ok(argv)
}

#[derive(Default)]
struct Usage {
    input: u64,
    output: u64,
    reported: bool,
}

/// One completed item of either protocol.
fn completed_item(item: &Value, items: &mut Items, answer: &mut Option<String>) {
    let kind = str_at(item, &["type"]).unwrap_or_default();
    match kind {
        // `codex exec --json`
        "agent_message" | "agentMessage" => {
            let text = str_at(item, &["text"]).unwrap_or_default();
            if !text.trim().is_empty() {
                items.push(ExternalItem::AssistantText { text: text.into() });
                *answer = Some(bound_text(text, MAX_ANSWER_BYTES));
            }
        }
        "command_execution" | "commandExecution" => {
            let command = item
                .get("command")
                .map(preview)
                .unwrap_or_else(|| "command".into());
            items.push(ExternalItem::ToolCall {
                name: "command".into(),
                arguments: command,
            });
            let exit = item
                .get("exit_code")
                .or_else(|| item.get("exitCode"))
                .and_then(Value::as_i64);
            let output = str_at(item, &["aggregated_output"])
                .or_else(|| str_at(item, &["aggregatedOutput"]))
                .unwrap_or_default();
            items.push(ExternalItem::ToolResult {
                name: "command".into(),
                output: match exit {
                    Some(code) => format!("exit {code}\n{output}"),
                    None => output.to_string(),
                },
                is_error: exit.is_some_and(|code| code != 0)
                    || str_at(item, &["status"]).is_some_and(|status| status == "failed"),
            });
        }
        "file_change" | "fileChange" => {
            items.push(ExternalItem::ToolCall {
                name: "file_change".into(),
                arguments: preview(&item["changes"]),
            });
            items.push(ExternalItem::ToolResult {
                name: "file_change".into(),
                output: str_at(item, &["status"]).unwrap_or("completed").into(),
                is_error: str_at(item, &["status"]).is_some_and(|status| status == "failed"),
            });
        }
        "mcp_tool_call" | "mcpToolCall" => {
            let name = format!(
                "{}.{}",
                str_at(item, &["server"]).unwrap_or("mcp"),
                str_at(item, &["tool"]).unwrap_or("tool")
            );
            items.push(ExternalItem::ToolCall {
                name: name.clone(),
                arguments: preview(&item["arguments"]),
            });
            items.push(ExternalItem::ToolResult {
                name,
                output: preview(item.get("result").unwrap_or(&item["error"])),
                is_error: str_at(item, &["status"]).is_some_and(|status| status == "failed"),
            });
        }
        "web_search" | "webSearch" => items.push(ExternalItem::ToolCall {
            name: "web_search".into(),
            arguments: str_at(item, &["query"]).unwrap_or_default().into(),
        }),
        "error" => items.push(ExternalItem::Error {
            message: str_at(item, &["message"]).unwrap_or("error").into(),
        }),
        // Reasoning, plans, user messages and other items are not recorded.
        _ => {}
    }
}

/// Parse the program's JSON Lines output: `codex exec --json` events, or
/// Codex app-server notifications (`{"method": ..., "params": ...}`).
pub fn parse_output(stdout: &[u8]) -> Result<ExternalActivationResult, ExternalAgentError> {
    let mut items = Items::default();
    let values = json_lines(stdout, &mut items);
    let mut answer = None;
    let mut usage = Usage::default();
    let mut failed = false;
    let mut turns_completed = 0u32;
    for value in &values {
        if let Some(method) = str_at(value, &["method"]) {
            let params = &value["params"];
            match method {
                "item/completed" => completed_item(&params["item"], &mut items, &mut answer),
                "thread/tokenUsage/updated" => {
                    // Cumulative for the thread: the last one counts.
                    let total = &params["tokenUsage"]["total"];
                    usage = Usage {
                        input: u64_at(total, &["inputTokens"]).unwrap_or(0),
                        output: u64_at(total, &["outputTokens"]).unwrap_or(0),
                        reported: total.is_object(),
                    };
                }
                "turn/completed" => {
                    let status = str_at(params, &["turn", "status"]).unwrap_or("unknown");
                    if status == "completed" {
                        turns_completed += 1;
                    } else {
                        failed = true;
                        items.push(ExternalItem::Error {
                            message: format!(
                                "turn {status}{}",
                                str_at(params, &["turn", "error", "message"])
                                    .map_or_else(String::new, |message| format!(": {message}"))
                            ),
                        });
                    }
                }
                "error" => items.push(ExternalItem::Error {
                    message: preview(&params["error"]),
                }),
                _ => {}
            }
            continue;
        }
        match str_at(value, &["type"]) {
            Some("item.completed") => completed_item(&value["item"], &mut items, &mut answer),
            Some("turn.completed") => {
                turns_completed += 1;
                let reported = &value["usage"];
                if reported.is_object() {
                    // Cached input and reasoning output are parts of these.
                    usage.input = usage
                        .input
                        .saturating_add(u64_at(reported, &["input_tokens"]).unwrap_or(0));
                    usage.output = usage
                        .output
                        .saturating_add(u64_at(reported, &["output_tokens"]).unwrap_or(0));
                    usage.reported = true;
                }
            }
            Some("turn.failed") => {
                failed = true;
                items.push(ExternalItem::Error {
                    message: format!(
                        "turn failed: {}",
                        str_at(value, &["error", "message"]).unwrap_or("error")
                    ),
                });
            }
            Some("error") => items.push(ExternalItem::Error {
                message: str_at(value, &["message"]).unwrap_or("error").into(),
            }),
            _ => {}
        }
    }
    if usage.reported {
        items.push(ExternalItem::Usage {
            input_tokens: usage.input,
            output_tokens: usage.output,
            // Codex reports tokens, never a charge.
            cost_microunits: None,
        });
    }
    if turns_completed == 0 && !failed {
        items.push(ExternalItem::Error {
            message: "the program completed no turn: it ended before finishing".into(),
        });
    }
    Ok(ExternalActivationResult {
        final_answer: (!failed && turns_completed > 0).then_some(answer).flatten(),
        items: items.finish(),
        exit_code: None,
        usage_complete: usage.reported && !failed,
    })
}
