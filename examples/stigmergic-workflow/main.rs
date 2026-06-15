//! Stigmergic workflow — `EventLattice` + `depends_on` DAG.
//!
//! Axocoatl's headline claim is **no central orchestrator**. Nothing tells the
//! agents what order to run in. They activate themselves when the shared
//! coordination space — the `EventLattice` — accumulates enough signal.
//!
//! This example wires three agents into a small DAG and lets the lattice decide
//! the running order:
//!
//! ```text
//!     planner ──completes──▶ implementer ──completes──▶ reviewer
//!        └────────────────────────────────────────────────▶┘
//!     (reviewer waits for BOTH planner AND implementer)
//! ```
//!
//! ## The pheromone math
//!
//! This mirrors exactly what the daemon does (`lattice_params` in
//! `axocoatl-daemon`):
//!
//! - An **entry** agent (empty `depends_on`) gets threshold `1.0` and is
//!   activated directly with the user's input.
//! - A **downstream** agent with `N` dependencies gets threshold `0.5 × N`.
//! - Every `TaskCompleted` event deposits a signal of strength `0.5` onto
//!   every registered agent's accumulator.
//!
//! So with this DAG:
//!
//! | agent       | depends_on            | threshold | fires when                       |
//! |-------------|-----------------------|-----------|----------------------------------|
//! | planner     | (none)                | 1.0       | directly, at kickoff             |
//! | implementer | planner               | 0.5       | after 1 upstream completes (0.5) |
//! | reviewer    | planner, implementer  | 1.0       | after 2 upstream complete (1.0)  |
//!
//! No line of code says "run implementer after planner." The order *emerges*
//! from signals crossing thresholds — that is stigmergy.
//!
//! This is a **workflow** (a `depends_on` DAG). For event-driven capability
//! routing where one event fans out to every agent that declares it reacts to
//! it, see the `skills-lattice` example.
//!
//! Run: `cargo run` from `examples/stigmergic-workflow/` (no API keys — mock LLM).

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ractor::Actor;
use tokio_stream::Stream;

use axocoatl_actor::{execute_agent, AgentActor, AgentBehavior, AgentError};
use axocoatl_coordination::{EventId, EventLattice, EventType, LatticeEvent};
use axocoatl_core::{AgentConfig, AgentId, AgentInput, AgentOutput, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};

// ---------------------------------------------------------------------------
// Mock LLM — one canned reply per role, so the example runs with no API keys.
// In a real app this is an Ollama / OpenAI / Anthropic provider.
// ---------------------------------------------------------------------------

struct RoleLlm {
    model: &'static str,
    reply: String,
}

#[async_trait::async_trait]
impl LlmProvider for RoleLlm {
    fn provider_id(&self) -> &str {
        "mock"
    }

    fn model_id(&self) -> &str {
        self.model
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

    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        Ok(ChatResponse {
            content: self.reply.clone(),
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: TokenUsageStats::new(40, 60),
            model: self.model.to_string(),
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
// One generic behavior — calls its provider with its system prompt. The role
// differences live in the config + the provider's canned reply.
// ---------------------------------------------------------------------------

struct LatticeAgent {
    system_prompt: String,
    provider: Arc<dyn LlmProvider>,
}

#[async_trait::async_trait]
impl AgentBehavior for LatticeAgent {
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
// DAG definition — the only place the topology is declared. Note there is NO
// ordering here, only dependencies. The lattice derives the order.
// ---------------------------------------------------------------------------

struct AgentSpec {
    id: &'static str,
    depends_on: &'static [&'static str],
    system_prompt: &'static str,
    reply: &'static str,
}

fn dag() -> Vec<AgentSpec> {
    vec![
        AgentSpec {
            id: "planner",
            depends_on: &[],
            system_prompt: "You are a planner. Break the feature request into concrete steps.",
            reply: "PLAN:\n  1. Add a Review data model (rating, author, body)\n  \
                    2. Add GET/POST /api/reviews\n  3. Render a star-rating block on the product page",
        },
        AgentSpec {
            id: "implementer",
            depends_on: &["planner"],
            system_prompt: "You are an implementer. Write the code that satisfies the plan above.",
            reply: "CODE:\n  + models/review.rs (Review struct + validation)\n  \
                    + routes/reviews.rs (list + create handlers)\n  \
                    ~ pages/product.tsx (renders <StarRating/> from /api/reviews)",
        },
        AgentSpec {
            id: "reviewer",
            depends_on: &["planner", "implementer"],
            system_prompt: "You are a reviewer. Check the implementation against the plan.",
            reply: "REVIEW:\n  - All three plan steps are present. \n  \
                    - Suggest server-side validation on rating (1..=5).\n  Verdict: APPROVED.",
        },
    ]
}

// `threshold = 0.5 × N` for downstream agents; entry agents get 1.0. This is
// the exact rule the daemon uses (axocoatl-daemon `lattice_params`).
fn threshold_for(depends_on: &[&str]) -> f32 {
    if depends_on.is_empty() {
        1.0
    } else {
        depends_on.len() as f32 * 0.5
    }
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Axocoatl: Stigmergic Workflow (EventLattice + depends_on DAG) ===\n");

    let specs = dag();
    let goal = "Add customer reviews to the product page.";

    // -----------------------------------------------------------------------
    // 1. Build the lattice and register each agent with its activation params.
    //    register_agent(id, threshold, decay_rate). The threshold encodes the
    //    DAG: a downstream agent only fires once enough upstream signals land.
    // -----------------------------------------------------------------------
    let lattice = EventLattice::new(64);
    let mut deps_of: HashMap<AgentId, Vec<AgentId>> = HashMap::new();
    let mut prompt_of: HashMap<AgentId, &'static str> = HashMap::new();

    println!("Registering agents on the lattice:");
    for spec in &specs {
        let id = AgentId::new(spec.id);
        let threshold = threshold_for(spec.depends_on);
        // decay_rate 0.0 — signals don't fade, so a join is exact: 0.5 + 0.5 is
        // exactly 1.0. The daemon defaults downstream agents to a small 0.01
        // decay so stale signals expire on long-running graphs; here we want the
        // threshold math to be deterministic, which is also what the lattice's
        // own unit tests use.
        let decay = 0.0;
        lattice.register_agent(id.clone(), threshold, decay);
        deps_of.insert(
            id.clone(),
            spec.depends_on.iter().map(|d| AgentId::new(*d)).collect(),
        );
        prompt_of.insert(id, spec.system_prompt);
        println!(
            "  • {:<12} depends_on={:?}  → threshold {:.1}",
            spec.id, spec.depends_on, threshold
        );
    }
    println!();

    // -----------------------------------------------------------------------
    // 2. Spawn each agent as a ractor actor (same path the daemon uses).
    // -----------------------------------------------------------------------
    let mut refs: HashMap<AgentId, ractor::ActorRef<axocoatl_actor::AgentMessage>> = HashMap::new();
    let mut handles = Vec::new();
    for spec in &specs {
        let id = AgentId::new(spec.id);
        let config = AgentConfig {
            id: id.clone(),
            name: spec.id.to_string(),
            provider: "mock".to_string(),
            model: spec.id.to_string(),
            system_prompt: Some(spec.system_prompt.to_string()),
            ..AgentConfig::default()
        };
        let behavior = LatticeAgent {
            system_prompt: spec.system_prompt.to_string(),
            provider: Arc::new(RoleLlm {
                model: spec.id,
                reply: spec.reply.to_string(),
            }),
        };
        let (actor_ref, handle) = AgentActor::spawn(
            Some(spec.id.to_string()),
            AgentActor,
            (config, Box::new(behavior) as Box<dyn AgentBehavior>),
        )
        .await?;
        refs.insert(id, actor_ref);
        handles.push(handle);
    }

    // -----------------------------------------------------------------------
    // 3. Drive the cascade. This is the same shape as the daemon's activation
    //    loop: entry agents are activated directly; every completion publishes
    //    a TaskCompleted event; the lattice returns whoever just crossed their
    //    threshold; we run those next. A `completed` guard stops re-runs.
    // -----------------------------------------------------------------------
    let mut completed: HashMap<AgentId, String> = HashMap::new();
    let mut activation_order: Vec<String> = Vec::new();

    // Entry agents (no deps) are sent straight in — UserInput is informational,
    // it does not drive activation (again, exactly what the daemon does).
    let mut queue: VecDeque<AgentId> = specs
        .iter()
        .filter(|s| s.depends_on.is_empty())
        .map(|s| AgentId::new(s.id))
        .collect();

    println!("Goal: {goal}\n{}", "─".repeat(64));

    while let Some(agent_id) = queue.pop_front() {
        if completed.contains_key(&agent_id) {
            continue; // already ran — the completed guard, like the daemon's
        }

        // Build this agent's input from its upstream outputs (or the goal, if
        // it is an entry agent).
        let deps = deps_of.get(&agent_id).cloned().unwrap_or_default();
        let input_text = if deps.is_empty() {
            goal.to_string()
        } else {
            let mut buf = format!("Goal: {goal}\n\nUpstream results:\n");
            for d in &deps {
                if let Some(out) = completed.get(d) {
                    buf.push_str(&format!("\n[from {d}]\n{out}\n"));
                }
            }
            buf
        };

        // A short, honest activation line showing WHY it fired now.
        if deps.is_empty() {
            println!("\n⚡ {agent_id} activated — entry agent, kicked off directly");
        } else {
            // Each completed upstream deposited 0.5; the threshold is 0.5 × N.
            let signal = deps.len() as f32 * 0.5;
            let threshold = deps.len() as f32 * 0.5;
            println!(
                "\n⚡ {agent_id} activated — {} upstream complete, accumulated signal {signal:.1} ≥ threshold {threshold:.1}",
                deps.len(),
            );
        }

        let actor = refs.get(&agent_id).expect("agent spawned above");
        let output = execute_agent(actor, AgentInput::text(&input_text))
            .await
            .map_err(|e| format!("{agent_id} failed: {e}"))?;

        println!("{}", output.content);
        completed.insert(agent_id.clone(), output.content.clone());
        activation_order.push(agent_id.to_string());

        // Publish the completion to the lattice. publish() deposits signal on
        // every registered agent and returns whoever just crossed threshold.
        let activated = lattice.publish(LatticeEvent {
            id: EventId::random(),
            event_type: EventType::TaskCompleted {
                task_id: agent_id.to_string(),
            },
            payload: serde_json::json!({ "agent_id": agent_id.to_string() }),
            produced_by: agent_id.to_string(),
            timestamp: now_ts(),
        });

        for next in activated {
            // Only enqueue agents that are part of this DAG, not yet done, and
            // not already queued.
            let known = deps_of.contains_key(&next);
            if known && !completed.contains_key(&next) && !queue.contains(&next) {
                queue.push_back(next);
            }
        }
    }

    // -----------------------------------------------------------------------
    // 4. Report. The order was never written down — the lattice produced it.
    // -----------------------------------------------------------------------
    println!("\n{}", "─".repeat(64));
    println!("\nActivation order (emergent, not scripted):");
    for (i, id) in activation_order.iter().enumerate() {
        println!("  {}. {id}", i + 1);
    }
    println!(
        "\n{} agents ran, {} events on the lattice. No orchestrator decided the order.",
        completed.len(),
        lattice.event_count()
    );

    // 5. Shut the actors down.
    for (id, actor) in &refs {
        let _ = id;
        actor.stop(None);
    }
    for handle in handles {
        let _ = handle.await;
    }

    println!("\n=== Done ===");
    Ok(())
}
