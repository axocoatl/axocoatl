//! A2A protocol — expose an Axocoatl agent as an Agent-to-Agent endpoint.
//!
//! A2A (Agent-to-Agent) is the *cross-framework* interop protocol. It lets an
//! agent built in some other system discover one of your agents and hand it a
//! task over plain HTTP, without sharing a process, a runtime, or a language.
//! Two HTTP endpoints carry the whole conversation:
//!
//! ```text
//!   GET  {endpoint}/.well-known/agent.json   → the Agent Card (discovery)
//!   POST {endpoint}/tasks                     → submit a task, get a result
//! ```
//!
//! This example runs **both halves in one process** so it is self-verifying
//! with no external tools:
//!
//! ```text
//!   ┌─────────────────────────── this binary ───────────────────────────┐
//!   │                                                                    │
//!   │  AgentActor ("echo-bot")        A2A server (axum)      A2A client  │
//!   │       ▲                          GET /.well-known/…  ◀──discover── │
//!   │       │ execute_agent()          POST /tasks         ◀──send_task─ │
//!   │  TaskHandler ──────────────────────┘                              │
//!   │       (maps an inbound A2A task onto a real agent execution)       │
//!   └────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Every piece is the real thing from the runtime, not a re-implementation:
//!
//! - `axocoatl_a2a::build_a2a_router` / `A2AServerState` / `TaskHandler` — the
//!   actual server scaffold the crate ships (`crates/axocoatl-a2a/src/server.rs`).
//! - `axocoatl_a2a::A2AClient` — the actual client (`.../src/client.rs`), driven
//!   over a real loopback TCP connection via `reqwest`.
//! - `axocoatl_actor::AgentActor` — the same ractor actor the daemon spawns, so
//!   an inbound task runs through the genuine agent execution path.
//!
//! The only mock is the LLM provider (one canned reply), so the example runs
//! with no API keys — exactly the convention the other examples use.
//!
//! ## A2A vs MCP vs the HTTP execute endpoint
//!
//! - **A2A** (this example): *agent calls agent across frameworks.* The unit of
//!   work is a delegated **task**; discovery is a published Agent Card. Use it
//!   when something outside Axocoatl needs to treat one of your agents as a
//!   peer.
//! - **MCP**: *an agent calls a tool.* The agent is the client, the MCP server
//!   exposes capabilities (functions, resources) the agent pulls in. Direction
//!   is inverted from A2A.
//! - **`POST /api/agents/{id}/execute`** (the daemon's own HTTP API): *your app
//!   drives your agent.* It is internal, Axocoatl-shaped, and not a cross-vendor
//!   contract. A2A is the public, framework-neutral face of the same execution.
//!
//! Run: `cargo run` from `examples/a2a-server/` (no API keys — mock LLM).

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use ractor::{Actor, ActorRef};
use tokio::sync::RwLock;
use tokio_stream::Stream;

use axocoatl_a2a::{
    build_a2a_router, A2AClient, A2AServerState, A2ATask, AgentCard, AuthSpec, TaskContext,
    TaskHandler, TaskStatus,
};
use axocoatl_actor::{execute_agent, AgentActor, AgentBehavior, AgentError, AgentMessage};
use axocoatl_core::{AgentConfig, AgentId, AgentInput, AgentOutput, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};

// ---------------------------------------------------------------------------
// Mock LLM — one canned reply, so the example runs with no API keys. In a real
// deployment this is an Ollama / OpenAI / Anthropic provider.
// ---------------------------------------------------------------------------

struct EchoLlm;

#[async_trait::async_trait]
impl LlmProvider for EchoLlm {
    fn provider_id(&self) -> &str {
        "mock"
    }

    fn model_id(&self) -> &str {
        "mock-echo-v1"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tool_calling: false,
            structured_output: false,
            vision: false,
            reasoning: false,
            embeddings: false,
            max_context_tokens: 32_000,
            max_output_tokens: 1_024,
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        // Reflect the caller's prompt back so the round-trip is visibly real:
        // whatever the remote A2A client sent appears in the returned result.
        let prompt = request
            .messages
            .iter()
            .rev()
            .find_map(|m| m.text_content())
            .unwrap_or("(no input)");
        Ok(ChatResponse {
            content: format!("echo-bot received: \"{prompt}\""),
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: TokenUsageStats::new(12, 8),
            model: "mock-echo-v1".to_string(),
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

// ---------------------------------------------------------------------------
// Agent behavior — a generic agent that calls its provider with its system
// prompt. Same shape as the other examples' agents.
// ---------------------------------------------------------------------------

struct EchoAgent {
    system_prompt: String,
    provider: Arc<dyn LlmProvider>,
}

#[async_trait::async_trait]
impl AgentBehavior for EchoAgent {
    async fn on_start(&mut self, _config: &AgentConfig) -> Result<(), AgentError> {
        Ok(())
    }

    async fn execute(&mut self, input: AgentInput) -> Result<AgentOutput, AgentError> {
        let request = ChatRequest::with_system(&self.system_prompt, &input.content);
        let response = self
            .provider
            .chat(request)
            .await
            .map_err(|e| AgentError::Provider(e.to_string()))?;
        Ok(AgentOutput {
            content: response.content,
            tool_calls: vec![],
            token_usage: response.usage,
        })
    }

    async fn on_stop(&mut self) -> Result<(), AgentError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// TaskHandler — the bridge the A2A server calls for every inbound task. This is
// the trait `crates/axocoatl-a2a/src/server.rs` defines; the daemon implements
// it by dispatching to the named agent. We do the same: map the task onto a
// real `execute_agent` call against the spawned actor.
//
// This mirrors the production handler in `axocoatl-server`
// (`routes::a2a_receive_task`): pull `input.input` (string), run the agent,
// and wrap success/failure in an `A2ATaskResult` with the matching `TaskStatus`.
// ---------------------------------------------------------------------------

struct AgentTaskHandler {
    actor: ActorRef<AgentMessage>,
}

#[async_trait::async_trait]
impl TaskHandler for AgentTaskHandler {
    async fn handle_task(&self, task: A2ATask) -> Result<axocoatl_a2a::A2ATaskResult, String> {
        // The Agent Card advertises an input schema of {"input": <string>}.
        // Accept that shape, and fall back to the raw JSON if it is absent —
        // exactly what the production `/a2a/tasks` handler does.
        let input = task
            .input
            .get("input")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| task.input.to_string());

        match execute_agent(&self.actor, AgentInput::text(input)).await {
            Ok(output) => Ok(axocoatl_a2a::A2ATaskResult {
                task_id: task.id,
                status: TaskStatus::Completed,
                output: Some(serde_json::json!({ "content": output.content })),
                error: None,
            }),
            Err(e) => Ok(axocoatl_a2a::A2ATaskResult {
                task_id: task.id,
                status: TaskStatus::Failed,
                output: None,
                error: Some(e),
            }),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Axocoatl: A2A Server (expose an agent over the Agent-to-Agent protocol) ===\n");

    // -----------------------------------------------------------------------
    // 1. Spawn the agent we want to expose. This is the exact actor the daemon
    //    spawns — nothing A2A-specific about it.
    // -----------------------------------------------------------------------
    let agent_id = AgentId::new("echo-bot");
    let system_prompt = "You are echo-bot. Acknowledge whatever the caller sends.";
    let config = AgentConfig {
        id: agent_id.clone(),
        name: "Echo Bot".to_string(),
        provider: "mock".to_string(),
        model: "mock-echo-v1".to_string(),
        system_prompt: Some(system_prompt.to_string()),
        ..AgentConfig::default()
    };
    let behavior = EchoAgent {
        system_prompt: system_prompt.to_string(),
        provider: Arc::new(EchoLlm),
    };
    let (actor_ref, actor_handle) = AgentActor::spawn(
        Some("echo-bot".to_string()),
        AgentActor,
        (config, Box::new(behavior) as Box<dyn AgentBehavior>),
    )
    .await?;
    println!("Spawned agent '{agent_id}' as a ractor actor.");

    // -----------------------------------------------------------------------
    // 2. Bind an ephemeral localhost port FIRST, so we know the real URL to
    //    publish in the Agent Card before we start serving. The crate's
    //    `A2AClient::send_task` posts to `{card.endpoint}/tasks`, and
    //    `build_a2a_router` mounts `/tasks` — so the card's `endpoint` is the
    //    server's base URL.
    // -----------------------------------------------------------------------
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr: SocketAddr = listener.local_addr()?;
    let base_url = format!("http://{addr}");
    println!("A2A server bound on {base_url} (ephemeral port).\n");

    // -----------------------------------------------------------------------
    // 3. Build the Agent Card — the document an external agent fetches to learn
    //    what this agent is and how to call it. The capabilities list, schemas,
    //    and auth scheme match the shape the production server publishes.
    // -----------------------------------------------------------------------
    let agent_card = AgentCard {
        id: agent_id.to_string(),
        name: "Echo Bot".to_string(),
        description: "A minimal Axocoatl agent exposed over A2A; echoes the caller's input."
            .to_string(),
        version: "0.1.0".to_string(),
        endpoint: base_url.clone(),
        capabilities: vec!["echo".to_string()],
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "input": { "type": "string" } },
            "required": ["input"]
        }),
        output_schema: serde_json::json!({
            "type": "object",
            "properties": { "content": { "type": "string" } }
        }),
        authentication: AuthSpec {
            scheme: "none".to_string(),
            endpoint: None,
        },
    };

    // -----------------------------------------------------------------------
    // 4. Stand up the A2A server. `A2AServerState` holds the card plus the
    //    `TaskHandler`; `build_a2a_router` wires the two A2A routes onto it.
    // -----------------------------------------------------------------------
    let state = Arc::new(RwLock::new(A2AServerState {
        agent_card: agent_card.clone(),
        task_handler: Arc::new(AgentTaskHandler {
            actor: actor_ref.clone(),
        }),
    }));
    let router = build_a2a_router(state);

    // Serve in the background; capture the handle so we can shut it down cleanly.
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("a2a server crashed");
    });

    // -----------------------------------------------------------------------
    // 5. Now act as a SEPARATE A2A client — the role a foreign agent framework
    //    would play. Everything below talks to the server only over HTTP.
    // -----------------------------------------------------------------------
    let client = A2AClient::new();

    // (a) Discovery: fetch the Agent Card from /.well-known/agent.json.
    println!("{}", "─".repeat(72));
    println!("[client] GET {base_url}/.well-known/agent.json  (discovery)\n");
    let discovered = client.discover(&base_url).await?;
    println!("Discovered Agent Card:");
    println!("  id           : {}", discovered.id);
    println!("  name         : {}", discovered.name);
    println!("  version      : {}", discovered.version);
    println!("  endpoint     : {}", discovered.endpoint);
    println!("  capabilities : {:?}", discovered.capabilities);
    println!("  auth scheme  : {}", discovered.authentication.scheme);

    // (b) Task submission: POST a task to /tasks and wait for the result. We
    //     build the task by hand here to show the wire shape an external caller
    //     constructs; `receiver_id` names the agent to run (its card `id`).
    println!("\n{}", "─".repeat(72));
    let task = A2ATask {
        id: "task-001".to_string(),
        sender_id: "external-client".to_string(),
        receiver_id: discovered.id.clone(),
        input: serde_json::json!({ "input": "Hello from a foreign A2A agent!" }),
        context: TaskContext {
            workflow_id: None,
            correlation_id: "corr-001".to_string(),
            token_budget: None,
        },
        timeout_secs: Some(30),
    };
    println!(
        "[client] POST {base_url}/tasks  (submit task '{}')",
        task.id
    );
    println!("         input: {}\n", serde_json::to_string(&task.input)?);

    let result = client.send_task(&discovered, task).await?;
    println!("Task result:");
    println!("  task_id : {}", result.task_id);
    println!("  status  : {:?}", result.status);
    if let Some(output) = &result.output {
        println!("  output  : {output}");
    }
    if let Some(err) = &result.error {
        println!("  error   : {err}");
    }

    // -----------------------------------------------------------------------
    // 6. Assert the round-trip actually worked, so `cargo run` is self-checking
    //    and fails loudly if the flow ever regresses.
    // -----------------------------------------------------------------------
    assert_eq!(
        discovered.id,
        agent_id.to_string(),
        "discovered wrong agent"
    );
    assert_eq!(
        result.status,
        TaskStatus::Completed,
        "task did not complete"
    );
    let content = result
        .output
        .as_ref()
        .and_then(|o| o.get("content"))
        .and_then(|c| c.as_str())
        .expect("result missing output.content");
    assert!(
        content.contains("Hello from a foreign A2A agent!"),
        "agent output did not reflect the submitted input: {content:?}"
    );

    println!("\n{}", "─".repeat(72));
    println!("\nVerified end-to-end: discovery returned the card, the task ran through");
    println!("the real agent actor, and the result came back over HTTP. ✓");

    // -----------------------------------------------------------------------
    // 7. Shut down the server and the actor.
    // -----------------------------------------------------------------------
    server.abort();
    actor_ref.stop(None);
    let _ = actor_handle.await;

    println!("\n=== Done ===");
    Ok(())
}
