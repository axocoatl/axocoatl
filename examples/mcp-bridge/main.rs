//! MCP bridge — consume an external MCP tool through the real `McpToolRegistry`.
//!
//! MCP (the Model Context Protocol) is the wire format agents use to reach
//! tools that live in *another process*: a filesystem server, a GitHub server,
//! a weather server. Axocoatl speaks MCP on both sides — it can be a **client**
//! (call other people's tools) and a **server** (expose its own agents as tools
//! for someone else). This example runs the **client** path end to end, with a
//! real tool call over a real stdio transport, and documents the **server**
//! path in the README.
//!
//! ## What actually happens when you `cargo run`
//!
//! ```text
//!   ┌────────────────────────────┐         stdio (JSON-RPC)        ┌──────────────────────────┐
//!   │ mcp-bridge (this binary)   │  ── spawns child process ──▶    │ mcp-bridge --mcp-server  │
//!   │                            │                                 │  exposes one tool:       │
//!   │  1. McpToolRegistry        │  ◀── tool list (discovery) ──   │     get_weather(city)    │
//!   │     .connect_server(stdio) │                                 │  (rmcp #[tool] over stdio)│
//!   │  2. mock LLM emits a call  │  ── call_tool(get_weather) ──▶  │                          │
//!   │     for mcp__weather__…    │  ◀── real result: "18°C, fog"   │                          │
//!   │  3. agent prints the result│                                 └──────────────────────────┘
//!   └────────────────────────────┘
//! ```
//!
//! The trivial MCP server is **this same binary re-executed** with `--mcp-server`.
//! That's the cleanest fully self-contained stdio server: no `npx`, no Python, no
//! network — and it exercises exactly the path the registry uses in production
//! (`TokioChildProcess` spawning a command, then the MCP initialize handshake).
//!
//! ## The execution path
//!
//! `McpToolRegistry::connect_server` (in `crates/axocoatl-mcp/src/registry.rs`)
//! does the discovery handshake and **keeps the client alive** in a per-server
//! connection pool. The product's `ToolExecutor`
//! (`crates/axocoatl-tools/src/executor.rs`) routes an `mcp__server__tool` call
//! to that live client via `McpToolRegistry::call_tool`. So this example drives
//! the real path end to end: both discovery and execution go through the same
//! registry + executor the daemon wires up — there is no out-of-band client.
//! Nothing here is mocked except the LLM: the tool list, the tool call, and the
//! returned weather string all cross a real MCP connection.
//!
//! Run: `cargo run -p mcp-bridge` (no API keys, no external servers — mock LLM
//! plus the subprocess this binary spawns of itself).

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use ractor::Actor;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_stream::Stream;

use axocoatl_actor::{execute_agent, AgentActor, AgentBehavior, AgentError};
use axocoatl_core::{
    AgentConfig, AgentId, AgentInput, AgentOutput, TokenUsageStats, ToolCall, ToolCallRecord,
};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent, ToolDefinition,
};
use axocoatl_mcp::{McpToolRegistry, McpTransportType};
use axocoatl_tools::ToolExecutor;

// ===========================================================================
// PART 1 — The trivial MCP server (the "external" tool we consume).
//
// One tool, `get_weather`, defined with rmcp's `#[tool]` macros — the same
// surface a real third-party MCP server would expose. It speaks the protocol
// over stdio when this binary is launched with `--mcp-server`. The numbers are
// canned (this is an example, not a weather provider) but everything about how
// the value reaches the agent — list_tools, call_tool, the JSON-RPC framing —
// is real.
// ===========================================================================

/// Arguments for `get_weather`. The `#[tool]` macro turns this struct's
/// `JsonSchema` into the tool's input schema that the client discovers.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct GetWeatherArgs {
    /// City to look up the weather for.
    city: String,
}

/// The stateless weather server. `tool_router` holds the generated routing
/// table; `#[tool_handler]` wires `list_tools` + `call_tool` to it.
#[derive(Clone)]
struct WeatherServer {
    tool_router: ToolRouter<Self>,
}

impl WeatherServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router(router = tool_router)]
impl WeatherServer {
    /// The one tool this server exposes. The return string is what the client
    /// receives as text content in the `CallToolResult`.
    #[tool(description = "Get the current weather for a city.")]
    async fn get_weather(&self, args: Parameters<GetWeatherArgs>) -> String {
        let Parameters(GetWeatherArgs { city }) = args;
        // A tiny canned table so the result visibly depends on the argument —
        // proving the argument really crossed the wire to the server.
        let report = match city.to_ascii_lowercase().as_str() {
            "london" => "13°C, overcast with light drizzle",
            "tokyo" => "22°C, clear",
            "san francisco" | "sf" => "16°C, morning fog burning off by noon",
            _ => "18°C, partly cloudy",
        };
        format!("Weather in {city}: {report}.")
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for WeatherServer {}

/// Run as the MCP server: speak JSON-RPC over stdin/stdout until the client
/// disconnects. This is the branch the spawned child process takes. Logs go to
/// stderr because stdout is the protocol channel (same rule the real
/// `axocoatl mcp serve` follows).
async fn run_mcp_server() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("[weather-server] ready on stdio — exposing get_weather");
    let service = WeatherServer::new()
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?;
    service.waiting().await?;
    Ok(())
}

// ===========================================================================
// PART 2 — The mock LLM.
//
// The only mocked component. It does not hit a network. It plays exactly the
// role a real provider plays in a tool-use turn: on the first turn it emits a
// tool call for whatever weather tool it was handed; on the second turn (after
// it has seen the tool result) it writes the final natural-language answer. The
// qualified tool name is discovered from the registry and threaded in, so the
// mock isn't hard-coding the MCP naming convention.
// ===========================================================================

struct MockWeatherLlm {
    /// The qualified tool name the registry discovered, e.g.
    /// `mcp__weather__get_weather`. The model "decides" to call this — it isn't
    /// hard-coded, it's whatever discovery turned up.
    tool_name: String,
    /// City the model puts in the tool-call arguments. A real model would parse
    /// this out of the user's question; we inject it to keep the mock trivial.
    city: String,
}

#[async_trait::async_trait]
impl LlmProvider for MockWeatherLlm {
    fn provider_id(&self) -> &str {
        "mock"
    }

    fn model_id(&self) -> &str {
        "mock-weather-v1"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tool_calling: true,
            structured_output: false,
            vision: false,
            reasoning: false,
            embeddings: false,
            max_context_tokens: 32_000,
            max_output_tokens: 1_024,
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        // Has a tool result already come back? A real provider keys off the
        // `Tool` role messages in the history; we do the same. If the
        // conversation already contains a tool result, this is the second turn:
        // answer in prose. Otherwise, ask to call the weather tool.
        let has_tool_result = request
            .messages
            .iter()
            .any(|m| matches!(m.role, axocoatl_core::MessageRole::Tool));

        if has_tool_result {
            // Second turn: summarize the tool output the runtime fed back to us.
            let observed = request
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m.role, axocoatl_core::MessageRole::Tool))
                .and_then(|m| m.text_content())
                .unwrap_or("(no tool output)");
            return Ok(ChatResponse {
                content: format!("Here's what I found — {observed}"),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: TokenUsageStats::new(30, 20),
                model: self.model_id().to_string(),
                provider: "mock".to_string(),
            });
        }

        // First turn: emit a structured tool call for the discovered MCP tool.
        Ok(ChatResponse {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_weather_1".to_string(),
                name: self.tool_name.clone(),
                arguments: serde_json::json!({ "city": self.city }),
            }],
            finish_reason: FinishReason::ToolUse,
            usage: TokenUsageStats::new(40, 15),
            model: self.model_id().to_string(),
            provider: "mock".to_string(),
        })
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        Err(ProviderError::Stream(
            "mock provider has no streaming".into(),
        ))
    }
}

// ===========================================================================
// PART 3 — The agent behavior.
//
// A minimal weather agent. It does the real tool-use loop by hand so the data
// flow is visible: ask the model, and if the model wants a tool, dispatch the
// call through the `ToolExecutor` (which routes to the registry's live MCP
// connection), feed the result back as a `Tool` message, then ask the model
// again for the final answer. This is the same shape `DefaultBehavior` runs in
// the daemon (see the tool-execution loop in
// `crates/axocoatl-actor/src/default_behavior.rs`), narrowed to one tool so the
// example stays readable.
// ===========================================================================

struct WeatherAgent {
    provider: Arc<dyn LlmProvider>,
    system_prompt: String,
    /// Runs a tool call: qualified name + args -> result JSON. This is the
    /// product's real `ToolExecutor`, which routes MCP calls over the registry's
    /// persistent connection — the same path the daemon uses.
    tool_executor: Arc<ToolExecutor>,
    /// Tools advertised to the model — sourced from the registry.
    tools: Vec<ToolDefinition>,
}

#[async_trait::async_trait]
impl AgentBehavior for WeatherAgent {
    async fn on_start(&mut self, _config: &AgentConfig) -> Result<(), AgentError> {
        Ok(())
    }

    async fn execute(&mut self, input: AgentInput) -> Result<AgentOutput, AgentError> {
        // Turn 1 — give the model the user's question plus the discovered tools.
        let mut request = ChatRequest::with_system(&self.system_prompt, &input.content);
        request.tools = self.tools.clone();

        let first = self
            .provider
            .chat(request.clone())
            .await
            .map_err(|e| AgentError::Provider(e.to_string()))?;

        let mut total = first.usage.clone();
        let mut tool_records: Vec<ToolCallRecord> = Vec::new();

        if first.tool_calls.is_empty() {
            // Model answered without a tool — nothing to bridge.
            return Ok(AgentOutput {
                content: first.content,
                tool_calls: tool_records,
                token_usage: total,
            });
        }

        // The model asked for one or more tools. Dispatch each REAL call.
        // Record the assistant's tool-call turn, then each tool result, so the
        // follow-up request reads [assistant(tool_calls), tool(result), …] —
        // the ordering every provider requires (the daemon does the same).
        let mut messages = request.messages.clone();
        messages.push(axocoatl_core::ChatMessage::assistant_with_tool_calls(
            &first.content,
            first.tool_calls.clone(),
        ));

        for call in &first.tool_calls {
            println!(
                "  → agent calls MCP tool `{}` with {}",
                call.name, call.arguments
            );

            let result = self
                .tool_executor
                .execute(&call.name, call.arguments.clone())
                .await
                .map_err(|e| AgentError::Provider(format!("MCP call failed: {e}")))?;

            println!("  ← MCP server returned: {result}");

            tool_records.push(ToolCallRecord {
                tool_name: call.name.clone(),
                arguments: call.arguments.clone(),
                result: Some(result.clone()),
            });

            // Feed the tool result back into the conversation as a `Tool`
            // message correlated by the call id, exactly like a real run. We
            // pass the tool's text payload (what a real provider sees), not the
            // JSON wrapper — the wrapper is kept in the structured record above.
            // Signature is tool_result(content, name, tool_call_id).
            let result_text = result
                .get("text")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| result.to_string());
            messages.push(axocoatl_core::ChatMessage::tool_result(
                result_text,
                &call.name,
                &call.id,
            ));
        }

        // Turn 2 — model now sees the tool output and writes the final answer.
        let mut followup = request;
        followup.messages = messages;
        let second = self
            .provider
            .chat(followup)
            .await
            .map_err(|e| AgentError::Provider(e.to_string()))?;
        total.merge(&second.usage);

        Ok(AgentOutput {
            content: second.content,
            tool_calls: tool_records,
            token_usage: total,
        })
    }

    async fn on_stop(&mut self) -> Result<(), AgentError> {
        Ok(())
    }
}

// ===========================================================================
// Main — wire discovery + execution (both through the registry/executor) + a
// mock agent.
// ===========================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // If launched as the child MCP server, become the server and nothing else.
    if std::env::args().any(|a| a == "--mcp-server") {
        return run_mcp_server().await;
    }

    println!("=== Axocoatl: MCP Bridge (consume an external MCP tool) ===\n");

    // The transport that points at OUR trivial server: re-exec this binary with
    // `--mcp-server`. In a real config this would be `npx some-mcp-server` or a
    // path to a server binary; the shape is identical.
    let self_exe = std::env::current_exe()?;
    let server_transport = McpTransportType::Stdio {
        command: self_exe.to_string_lossy().into_owned(),
        args: vec!["--mcp-server".to_string()],
        env: HashMap::new(),
    };

    // -----------------------------------------------------------------------
    // 1. DISCOVERY — through the real McpToolRegistry.
    //    connect_server spawns the child, runs the MCP initialize handshake,
    //    lists the server's tools, and indexes them under qualified names.
    // -----------------------------------------------------------------------
    println!("[1] Connecting to the 'weather' MCP server via McpToolRegistry…");
    let mut registry = McpToolRegistry::new();
    registry.connect_server("weather", server_transport).await?;

    let llm_tools = registry.as_llm_tools();
    println!(
        "    discovered {} tool(s) across {} server(s):",
        registry.tool_count(),
        registry.servers().len()
    );
    for (qualified, server, description) in registry.tool_entries() {
        let bare = registry.original_name(&qualified).unwrap_or("?");
        println!("      • {qualified}  (server={server}, bare={bare})  — {description}");
    }
    println!();

    // The qualified name the LLM will see and call.
    let qualified_tool = registry
        .tool_names()
        .into_iter()
        .next()
        .ok_or("registry discovered no tools")?;

    // -----------------------------------------------------------------------
    // 2. Build the product's ToolExecutor over the SAME registry. Registering
    //    each discovered tool lets the executor route an `mcp__…` call to the
    //    Mcp backend; `with_mcp_registry` hands it the registry so that backend
    //    dispatches over the persistent connection. This is exactly how the
    //    daemon wires MCP tools into agents.
    // -----------------------------------------------------------------------
    let mut executor = ToolExecutor::new();
    for def in &llm_tools {
        if let Some(server) = registry.server_for_tool(&def.name).map(str::to_string) {
            executor.register_mcp(def.name.clone(), server, def.clone());
        }
    }
    let registry = Arc::new(tokio::sync::RwLock::new(registry));
    let tool_executor = Arc::new(executor.with_mcp_registry(registry.clone()));

    // -----------------------------------------------------------------------
    // 3. Spawn a weather agent (a ractor actor, same path the daemon uses).
    //    Its mock LLM is told the discovered tool name + the city to ask about.
    // -----------------------------------------------------------------------
    let city = "London";
    let provider: Arc<dyn LlmProvider> = Arc::new(MockWeatherLlm {
        tool_name: qualified_tool.clone(),
        city: city.to_string(),
    });

    let config = AgentConfig {
        id: AgentId::new("weather-agent"),
        name: "Weather Agent".to_string(),
        provider: "mock".to_string(),
        model: "mock-weather-v1".to_string(),
        system_prompt: Some("You answer weather questions using the available tools.".to_string()),
        ..AgentConfig::default()
    };

    let behavior = WeatherAgent {
        provider,
        system_prompt: "You answer weather questions using the available tools.".to_string(),
        tool_executor,
        tools: llm_tools,
    };

    let (agent, handle) = AgentActor::spawn(
        Some("weather-agent".to_string()),
        AgentActor,
        (config, Box::new(behavior) as Box<dyn AgentBehavior>),
    )
    .await?;

    // -----------------------------------------------------------------------
    // 4. Run it. The agent will call the MCP tool for real and print what came
    //    back, then the model turns that into a final answer.
    // -----------------------------------------------------------------------
    let question = format!("What's the weather in {city}?");
    println!("[2] User asks: {question}\n");
    println!("{}", "─".repeat(64));

    let output = execute_agent(&agent, AgentInput::text(&question))
        .await
        .map_err(|e| format!("agent failed: {e}"))?;

    println!("{}", "─".repeat(64));
    println!("\n[3] Final agent answer:\n    {}\n", output.content);

    // Show the recorded tool call carried the REAL result (not a fabricated one).
    for rec in &output.tool_calls {
        println!(
            "    tool call recorded: {} {} → {}",
            rec.tool_name,
            rec.arguments,
            rec.result
                .as_ref()
                .map(|r| r.to_string())
                .unwrap_or_else(|| "(none)".to_string())
        );
    }
    println!(
        "\n    tokens: {} input + {} output = {} total",
        output.token_usage.input_tokens,
        output.token_usage.output_tokens,
        output.token_usage.total()
    );

    // -----------------------------------------------------------------------
    // 5. Shut the agent down.
    // -----------------------------------------------------------------------
    agent.stop(None);
    let _ = handle.await;

    println!("\n=== Done — the result above crossed a real MCP stdio connection. ===");
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The companion config must load and validate through the REAL config
    /// loader — not just be well-formed YAML. This catches schema drift (a
    /// renamed field, an invalid transport) the moment it happens.
    #[test]
    fn example_mcp_config_loads_and_validates() {
        let yaml = include_str!("axocoatl.example.mcp.yaml");
        let config =
            axocoatl_config::parse_config(yaml, std::path::Path::new("axocoatl.example.mcp.yaml"))
                .expect("axocoatl.example.mcp.yaml must parse + validate");

        // Both transports the README documents are present and well-formed.
        assert_eq!(config.mcp_servers.len(), 2);
        let stdio = config
            .mcp_servers
            .iter()
            .find(|s| s.transport == "stdio")
            .expect("stdio server present");
        assert_eq!(stdio.command.as_deref(), Some("npx"));
        let http = config
            .mcp_servers
            .iter()
            .find(|s| s.transport == "streamable_http")
            .expect("streamable_http server present");
        assert!(http.url.as_deref().unwrap_or("").starts_with("https://"));

        // The agent the server path exposes as `agent_weather`.
        assert!(config.agents.iter().any(|a| a.id == "weather"));
    }
}
