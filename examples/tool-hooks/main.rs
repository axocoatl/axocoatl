//! Tool hooks — pre/post execution policy and audit logging.
//!
//! Every tool call an Axocoatl agent makes passes through the agent's
//! [`HookRegistry`] before it reaches the executor, and the result passes back
//! through it afterward. A hook is a `ToolHook` that returns one of three
//! actions:
//!
//! - `Allow` — let the call proceed (or pass the result through unchanged)
//! - `Deny` — block the call (Pre only); the agent gets the reason as the tool
//!   result and can recover on its next turn
//! - `Transform` — rewrite the arguments (Pre) or the result (Post)
//!
//! This is the exact extension point the production daemon uses to gate MCP
//! tool calls behind user approval (`McpApprovalHook` in
//! `crates/axocoatl-daemon/src/mcp_approval_hook.rs`). Here we wire two custom
//! hooks plus the built-in `LoggingHook` onto a real `DefaultAgentBehavior`
//! agent and watch a deny→recover cascade play out:
//!
//! ```text
//!   turn 1  LLM asks: write_file("../../etc/passwd", …)
//!             └─ Pre hooks: AuditHook logs it, DenyHook BLOCKS it
//!                  → agent receives {"error": "path escapes workspace"}
//!   turn 2  LLM sees the denial, retries: write_file("notes/summary.md", …)
//!             └─ Pre hooks: allowed → tool runs → Post hook audits the result
//!   turn 3  LLM sees the success and writes its closing message
//! ```
//!
//! Nothing about the deny is special-cased in the example: the denial flows
//! through the real `DefaultAgentBehavior` tool loop
//! (`crates/axocoatl-actor/src/default_behavior.rs`), which records the deny
//! reason as a tool result in the session and makes a follow-up LLM call — so
//! the model genuinely *recovers* from the policy block.
//!
//! Two scoping rules are shown side by side:
//!
//! - `AuditHook` is registered **globally** — it sees every tool, every phase.
//! - `DenyHook` is registered **for `write_file` only** — a per-tool policy.
//!
//! This maps directly to enterprise patterns: an allowlist that denies tools
//! outside a sanctioned set, a Transform hook that redacts PII from arguments
//! before a tool ever sees them, and a global audit trail for compliance. See
//! the README for the mapping.
//!
//! Run: `cargo run` from `examples/tool-hooks/` (no API keys — mock LLM).

use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use ractor::Actor;
use tokio_stream::Stream;

use axocoatl_actor::{execute_agent, AgentActor, AgentBehavior, DefaultAgentBehavior};
use axocoatl_core::{AgentConfig, AgentId, AgentInput, ChatMessage, MessageRole, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};
use axocoatl_token::TokenCounter;
use axocoatl_tools::{
    BuiltinTool, HookAction, HookContext, HookPhase, HookRegistry, LoggingHook, ToolError,
    ToolExecutor, ToolHook,
};

// ---------------------------------------------------------------------------
// A real built-in tool: write_file, scoped to a workspace directory.
//
// The tool itself does NOT decide what is allowed — that is the hook's job. It
// writes wherever it is told. The DenyHook below is what stops a write from
// escaping the workspace, which is exactly how policy and capability stay
// separated in the runtime: tools do work, hooks enforce policy.
// ---------------------------------------------------------------------------

struct WriteFileTool {
    workspace: PathBuf,
}

#[async_trait::async_trait]
impl BuiltinTool for WriteFileTool {
    fn description(&self) -> &str {
        "Write text content to a file at the given path"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to write to" },
                "content": { "type": "string", "description": "Text content to write" }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let rel = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: "write_file".to_string(),
                reason: "missing 'path' string".to_string(),
            })?;
        let content = arguments
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let full = self.workspace.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ToolError::ExecutionFailed {
                tool: "write_file".to_string(),
                reason: e.to_string(),
            })?;
        }
        std::fs::write(&full, content).map_err(|e| ToolError::ExecutionFailed {
            tool: "write_file".to_string(),
            reason: e.to_string(),
        })?;

        Ok(serde_json::json!({
            "ok": true,
            "path": rel,
            "bytes_written": content.len(),
        }))
    }
}

// ---------------------------------------------------------------------------
// DenyHook — a Pre hook that blocks write_file calls escaping the workspace.
//
// Mirrors the production `McpApprovalHook`: a Pre-phase `ToolHook` that returns
// `HookAction::Deny { reason }` to stop a call before it runs. We scope it to
// the `write_file` tool via `register_for_tool`, so it is a per-tool policy and
// never fires on other tools.
// ---------------------------------------------------------------------------

struct DenyHook {
    workspace: PathBuf,
}

impl DenyHook {
    /// True when `rel`, resolved against the workspace, stays inside it.
    /// Rejects absolute paths and any `..` that climbs above the root. We
    /// resolve lexically (no filesystem touch) because the target need not
    /// exist yet — the same way a deployed policy gate must reason about a path
    /// before the write happens. `depth` is how deep below the workspace root
    /// we are; it must never go negative.
    fn stays_inside(&self, rel: &str) -> bool {
        let mut depth: i32 = 0;
        for comp in Path::new(rel).components() {
            match comp {
                // An absolute path (leading "/" or a Windows prefix) escapes by
                // definition — it ignores the workspace root entirely.
                Component::RootDir | Component::Prefix(_) => return false,
                Component::ParentDir => {
                    depth -= 1;
                    if depth < 0 {
                        return false;
                    }
                }
                Component::Normal(_) => depth += 1,
                Component::CurDir => {}
            }
        }
        true
    }
}

#[async_trait::async_trait]
impl ToolHook for DenyHook {
    fn name(&self) -> &str {
        "workspace_jail"
    }

    fn phases(&self) -> Vec<HookPhase> {
        vec![HookPhase::Pre]
    }

    async fn execute(&self, ctx: &HookContext) -> HookAction {
        let path = ctx
            .value
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if self.stays_inside(path) {
            HookAction::Allow
        } else {
            HookAction::Deny {
                reason: format!(
                    "write_file denied: path '{path}' escapes the agent workspace \
                     '{}'. Stay inside the workspace (relative path, no '..').",
                    self.workspace.display()
                ),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// AuditHook — a global Pre+Post hook that appends one JSON line per event to a
// shared in-memory log AND prints it. This is the compliance trail: every tool
// call and every result, no matter which tool, in the order they happened.
// ---------------------------------------------------------------------------

struct AuditHook {
    log: Arc<Mutex<Vec<serde_json::Value>>>,
}

#[async_trait::async_trait]
impl ToolHook for AuditHook {
    fn name(&self) -> &str {
        "audit"
    }

    fn phases(&self) -> Vec<HookPhase> {
        vec![HookPhase::Pre, HookPhase::Post]
    }

    async fn execute(&self, ctx: &HookContext) -> HookAction {
        let phase = match ctx.phase {
            HookPhase::Pre => "pre",
            HookPhase::Post => "post",
        };
        let entry = serde_json::json!({
            "phase": phase,
            "agent": ctx.agent_id,
            "tool": ctx.tool_name,
            // Pre: the arguments. Post: the result. Same field, per HookContext.
            "value": ctx.value,
        });
        println!("  [audit] {entry}");
        if let Ok(mut log) = self.log.lock() {
            log.push(entry);
        }
        // The auditor only observes — it never blocks or rewrites.
        HookAction::Allow
    }
}

// ---------------------------------------------------------------------------
// Mock LLM — drives the deny→recover cascade with no API keys.
//
// The DefaultAgentBehavior tool loop always calls `chat_stream`, so the mock
// emits provider stream events. It decides what to do by inspecting the
// conversation it is handed:
//
//   - No tool result yet      → ask for the FORBIDDEN write (turn 1)
//   - Saw a "denied" result   → retry with an ALLOWED path (turn 2)
//   - Saw a successful result → write the closing message (turn 3)
//
// A real provider does exactly this: it reads the tool results already in the
// transcript and plans its next move. Here the moves are scripted, but the
// control flow — deny lands as a tool result, model reacts to it — is the
// genuine runtime path, not faked in the example.
// ---------------------------------------------------------------------------

struct PolicyProbeLlm;

/// Classification of the most recent `Tool` result in the transcript.
enum LastToolResult {
    None,
    Denied,
    Succeeded,
}

fn last_tool_result(messages: &[ChatMessage]) -> LastToolResult {
    for m in messages.iter().rev() {
        if m.role == MessageRole::Tool {
            let text = m.text_content().unwrap_or_default();
            return if text.contains("denied") {
                LastToolResult::Denied
            } else {
                LastToolResult::Succeeded
            };
        }
    }
    LastToolResult::None
}

/// Emit a single tool call as a stream the DefaultAgentBehavior can accumulate.
/// The whole arguments blob goes in one delta — providers may chunk it, but the
/// accumulator in `stream_chat` handles either shape.
fn tool_call_stream(
    id: &str,
    name: &str,
    args: serde_json::Value,
) -> Vec<Result<StreamEvent, ProviderError>> {
    vec![
        Ok(StreamEvent::ToolCallDelta {
            index: Some(0),
            id: id.to_string(),
            name: Some(name.to_string()),
            args_delta: args.to_string(),
        }),
        Ok(StreamEvent::Usage(TokenUsageStats::new(60, 20))),
        Ok(StreamEvent::Done {
            finish_reason: FinishReason::ToolUse,
        }),
    ]
}

/// Emit a plain text completion stream.
fn text_stream(text: &str) -> Vec<Result<StreamEvent, ProviderError>> {
    vec![
        Ok(StreamEvent::TextDelta {
            delta: text.to_string(),
        }),
        Ok(StreamEvent::Usage(TokenUsageStats::new(50, 30))),
        Ok(StreamEvent::Done {
            finish_reason: FinishReason::Stop,
        }),
    ]
}

#[async_trait::async_trait]
impl LlmProvider for PolicyProbeLlm {
    fn provider_id(&self) -> &str {
        "mock"
    }

    fn model_id(&self) -> &str {
        "policy-probe-v1"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            structured_output: false,
            vision: false,
            reasoning: false,
            embeddings: false,
            max_context_tokens: 32_000,
            max_output_tokens: 1_024,
        }
    }

    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        // DefaultAgentBehavior always streams; chat() is unused here.
        Err(ProviderError::Stream(
            "mock provider is streaming-only".into(),
        ))
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        let events = match last_tool_result(&request.messages) {
            // Turn 1: try to write OUTSIDE the workspace — DenyHook blocks it.
            LastToolResult::None => tool_call_stream(
                "call-1",
                "write_file",
                serde_json::json!({
                    "path": "../../etc/passwd",
                    "content": "pwned"
                }),
            ),
            // Turn 2: the deny landed as a tool result. Recover with a legal path.
            LastToolResult::Denied => tool_call_stream(
                "call-2",
                "write_file",
                serde_json::json!({
                    "path": "notes/summary.md",
                    "content": "# Summary\n\nWrote inside the workspace after the policy block."
                }),
            ),
            // Turn 3: the write succeeded. Close out.
            LastToolResult::Succeeded => text_stream(
                "Done. My first path was blocked by policy, so I wrote to \
                 notes/summary.md inside the workspace instead.",
            ),
        };
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Axocoatl: Tool Hooks (pre/post policy + audit logging) ===\n");

    // -----------------------------------------------------------------------
    // 1. Set up a real workspace dir + the write_file tool that targets it.
    //    A unique temp dir keeps the example hermetic and repeatable.
    // -----------------------------------------------------------------------
    let workspace =
        std::env::temp_dir().join(format!("axocoatl-tool-hooks-{}", std::process::id()));
    std::fs::create_dir_all(&workspace)?;
    println!(
        "Workspace (the only place writes are allowed):\n  {}\n",
        workspace.display()
    );

    let mut executor = ToolExecutor::new();
    executor.register_builtin(
        "write_file",
        Arc::new(WriteFileTool {
            workspace: workspace.clone(),
        }),
    );
    let executor = Arc::new(executor);

    // -----------------------------------------------------------------------
    // 2. Build the hook policy for this agent.
    //
    //    - LoggingHook  (built-in)  : global, Pre+Post — emits tracing spans.
    //    - AuditHook    (custom)    : global, Pre+Post — JSON line per event.
    //    - DenyHook     (custom)    : write_file ONLY, Pre — the workspace jail.
    //
    //    `register_global` vs `register_for_tool` is the global / per-tool
    //    policy distinction the issue calls out: audit everything, but only
    //    police the one dangerous tool.
    // -----------------------------------------------------------------------
    let audit_log: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));

    let mut registry = HookRegistry::new();
    registry.register_global(Arc::new(LoggingHook));
    registry.register_global(Arc::new(AuditHook {
        log: audit_log.clone(),
    }));
    registry.register_for_tool(
        "write_file",
        Arc::new(DenyHook {
            workspace: workspace.clone(),
        }),
    );
    let registry = Arc::new(registry);

    println!("Hook policy registered:");
    println!("  - logging        global     Pre+Post   (built-in tracing)");
    println!("  - audit          global     Pre+Post   (JSON audit trail)");
    println!("  - workspace_jail write_file Pre        (per-tool deny policy)");
    println!("  {} hooks total\n", registry.hook_count());

    // -----------------------------------------------------------------------
    // 3. Build a real DefaultAgentBehavior agent with the executor + hooks
    //    attached, then spawn it as a ractor actor — the same wiring the daemon
    //    uses (`with_tool_executor` + `with_hook_registry`).
    // -----------------------------------------------------------------------
    let provider: Arc<dyn LlmProvider> = Arc::new(PolicyProbeLlm);
    let counter: Arc<dyn TokenCounter> = Arc::new(CharCounter);
    let behavior = DefaultAgentBehavior::new(provider, counter)
        .with_tool_executor(executor.clone())
        .with_hook_registry(registry.clone());

    let config = AgentConfig {
        id: AgentId::new("file-writer"),
        name: "File Writer".to_string(),
        provider: "mock".to_string(),
        model: "policy-probe-v1".to_string(),
        system_prompt: Some(
            "You write files for the user. You only have access to the agent workspace."
                .to_string(),
        ),
        tools: vec!["write_file".to_string()],
        ..AgentConfig::default()
    };

    let (actor_ref, handle) = AgentActor::spawn(
        Some("file-writer".to_string()),
        AgentActor,
        (config, Box::new(behavior) as Box<dyn AgentBehavior>),
    )
    .await?;

    // -----------------------------------------------------------------------
    // 4. Run one task. The mock LLM first attempts a forbidden write; the
    //    DenyHook blocks it; the agent recovers and writes a legal file. All of
    //    that happens inside ONE execute_agent call — the tool loop iterates.
    // -----------------------------------------------------------------------
    let task = "Save a short project summary to a notes file.";
    println!("Task: {task}\n{}", "─".repeat(64));
    println!("\nTool-call activity (audit hook prints each Pre/Post event):\n");

    let output = execute_agent(&actor_ref, AgentInput::text(task))
        .await
        .map_err(|e| format!("agent failed: {e}"))?;

    // -----------------------------------------------------------------------
    // 5. Report. Walk the agent's own tool-call record to show the deny + the
    //    recovery, then prove the file actually landed inside the workspace.
    // -----------------------------------------------------------------------
    println!("\n{}", "─".repeat(64));
    println!("\nAgent's final message:\n  {}\n", output.content);

    println!("Tool calls the agent attempted (from its output record):");
    let mut denied = 0usize;
    for (i, tc) in output.tool_calls.iter().enumerate() {
        let path = tc
            .arguments
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let outcome = match &tc.result {
            Some(r) if r.get("error").is_some() => {
                denied += 1;
                format!(
                    "DENIED — {}",
                    r.get("error").and_then(|v| v.as_str()).unwrap_or("")
                )
            }
            Some(r) if r.get("ok").is_some() => format!(
                "WROTE {} bytes",
                r.get("bytes_written").and_then(|v| v.as_u64()).unwrap_or(0)
            ),
            Some(r) => r.to_string(),
            None => "no result".to_string(),
        };
        println!("  {}. write_file path={path}\n       -> {outcome}", i + 1);
    }
    println!(
        "\n{} of {} attempts were blocked by the deny policy; the agent recovered \
         and succeeded on the next turn.",
        denied,
        output.tool_calls.len()
    );

    // The audit trail is the observe-only record: one entry per hook firing.
    // A denied call short-circuits in the Pre phase (the executor never runs),
    // so it shows up as a single Pre entry with no matching Post — the absence
    // of a Post pair is itself the signal that the call was stopped. Compute the
    // counts in a scope so the guard is dropped before the shutdown awaits.
    let (total_events, pre, post) = {
        let log = audit_log.lock().unwrap();
        let pre = log.iter().filter(|e| e["phase"] == "pre").count();
        let post = log.iter().filter(|e| e["phase"] == "post").count();
        (log.len(), pre, post)
    };
    println!(
        "Audit trail: {total_events} events ({pre} pre, {post} post). The {} \
         pre-without-post gap is the blocked call.",
        pre - post
    );

    // Prove the filesystem state: the legal file exists; the escape never ran.
    let legal = workspace.join("notes/summary.md");
    println!(
        "\nFilesystem check: {} exists inside workspace: {}",
        legal.display(),
        legal.exists()
    );
    // The traversal target was never written — the hook stopped it before the
    // tool ran, so the deny line in the trail above is the proof.

    // -----------------------------------------------------------------------
    // 6. Shut the actor down and clean up the temp workspace.
    // -----------------------------------------------------------------------
    actor_ref.stop(None);
    let _ = handle.await;
    let _ = std::fs::remove_dir_all(&workspace);

    println!("\n=== Done ===");
    Ok(())
}

// ---------------------------------------------------------------------------
// Minimal token counter — examples don't need a real tokenizer. ~4 chars/token.
// ---------------------------------------------------------------------------

struct CharCounter;

impl TokenCounter for CharCounter {
    fn count_text(&self, text: &str) -> usize {
        text.len() / 4 + 1
    }

    fn count_messages(&self, messages: &[ChatMessage]) -> usize {
        messages
            .iter()
            .map(|m| m.text_content().map_or(1, |t| self.count_text(t)))
            .sum()
    }

    fn count_tool_definition(&self, tool_json: &serde_json::Value) -> usize {
        self.count_text(&tool_json.to_string())
    }
}
