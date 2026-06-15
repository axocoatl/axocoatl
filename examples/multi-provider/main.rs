//! Multi-provider routing — a cheap local model and a frontier model in ONE
//! workflow.
//!
//! A core Axocoatl claim is **per-agent provider selection**: each agent picks
//! its own provider, so you can route the easy, high-volume steps to a cheap
//! local model and reserve the expensive frontier model for the one step that
//! actually needs the big context window and tool-calling. Same DAG, mixed
//! providers, very different cost per agent.
//!
//! This example wires three agents into a `depends_on` DAG and gives them two
//! genuinely different providers:
//!
//! ```text
//!     triage ──────▶ drafter ──────▶ synthesizer
//!     (local)         (local)         (frontier)
//! ```
//!
//! | agent       | provider        | model                 | why this tier                          |
//! |-------------|-----------------|-----------------------|----------------------------------------|
//! | triage      | local-small     | llama3.2:3b           | classify + route — trivial, runs cheap |
//! | drafter     | local-small     | llama3.2:3b           | first pass — high volume, runs cheap   |
//! | synthesizer | frontier        | claude-sonnet (mock)  | needs big context + tool-calling       |
//!
//! The two providers report different `capabilities()` (context window,
//! tool_calling, reasoning) and different `TokenUsageStats`, and we attach a
//! per-1K-token price to each so the cost contrast is concrete: the two local
//! steps together cost a fraction of the single frontier step.
//!
//! The order the agents run in is **not scripted** — it emerges from the
//! `EventLattice`, exactly like the `stigmergic-workflow` example. The new thing
//! here is the *provider per agent*, not the coordination.
//!
//! ## Mock mode (this binary) vs live mode (the YAML)
//!
//! This binary runs with **zero API keys** — both providers are mocks with
//! canned replies, so it is CI-safe and deterministic. To run the same shape
//! against a real local Ollama model and a real frontier model, see
//! `axocoatl.multi-provider.yaml` and the README. The mock costs below are
//! illustrative public list prices, not a live quote.
//!
//! Run: `cargo run -p multi-provider` (no API keys — mock providers).

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
// Two mock providers with genuinely different capabilities + price.
//
// `MockLocalProvider` stands in for a small model served locally by Ollama
// (llama3.2:3b). `MockFrontierProvider` stands in for a hosted frontier model
// (Anthropic Claude Sonnet). They differ where it matters for routing:
// context window, tool_calling, reasoning — and price.
//
// In a real app these are `axocoatl_llm_ollama::OllamaProvider` and
// `axocoatl_llm_anthropic::AnthropicProvider`; the agent code below does not
// change at all when you swap them in — see the companion YAML.
// ---------------------------------------------------------------------------

/// Per-1K-token list price for a provider, used to turn a `TokenUsageStats`
/// into a dollar cost so the cheap-vs-frontier contrast is visible. Input and
/// output are priced separately because every real provider prices them apart.
#[derive(Clone, Copy)]
struct Pricing {
    /// USD per 1,000 input (prompt) tokens.
    input_per_1k: f64,
    /// USD per 1,000 output (completion) tokens.
    output_per_1k: f64,
}

impl Pricing {
    /// Cost in USD for a given usage at this price.
    fn cost(&self, usage: &TokenUsageStats) -> f64 {
        (usage.input_tokens as f64 / 1000.0) * self.input_per_1k
            + (usage.output_tokens as f64 / 1000.0) * self.output_per_1k
    }
}

/// Local model served by Ollama on the box — free to run (price is `0.0`), a
/// modest context window, and no tool-calling. Perfect for high-volume,
/// low-stakes steps.
struct MockLocalProvider {
    /// The canned reply this agent's role returns.
    reply: String,
}

#[async_trait::async_trait]
impl LlmProvider for MockLocalProvider {
    fn provider_id(&self) -> &str {
        // Mirrors the `provider:` value an agent would reference in YAML.
        "local-small"
    }

    fn model_id(&self) -> &str {
        "llama3.2:3b"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        // A small local model: shorter context, no tool-calling, no reasoning.
        ProviderCapabilities {
            streaming: true,
            tool_calling: false,
            structured_output: false,
            vision: false,
            reasoning: false,
            embeddings: false,
            max_context_tokens: 8_192,
            max_output_tokens: 2_048,
        }
    }

    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        // A small model on a cheap step: modest token counts.
        Ok(ChatResponse {
            content: self.reply.clone(),
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: TokenUsageStats::new(180, 90),
            model: self.model_id().to_string(),
            provider: self.provider_id().to_string(),
        })
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        Err(ProviderError::Stream(
            "mock local provider has no streaming".into(),
        ))
    }
}

/// Hosted frontier model — a large context window, tool-calling, reasoning, and
/// a real per-token price. Reserved for the step that needs it.
struct MockFrontierProvider {
    /// The canned reply this agent's role returns.
    reply: String,
}

#[async_trait::async_trait]
impl LlmProvider for MockFrontierProvider {
    fn provider_id(&self) -> &str {
        "frontier"
    }

    fn model_id(&self) -> &str {
        "claude-sonnet-4-6"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        // A frontier model: much larger context, tool-calling, reasoning.
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            structured_output: true,
            vision: true,
            reasoning: true,
            max_context_tokens: 200_000,
            max_output_tokens: 64_000,
            embeddings: false,
        }
    }

    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        // The frontier step reads BOTH upstream outputs and writes a longer,
        // higher-quality synthesis: more input tokens, more output tokens.
        Ok(ChatResponse {
            content: self.reply.clone(),
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: TokenUsageStats::new(1_400, 620),
            model: self.model_id().to_string(),
            provider: self.provider_id().to_string(),
        })
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        Err(ProviderError::Stream(
            "mock frontier provider has no streaming".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// One generic behavior — calls whatever provider it was handed with its system
// prompt. The role + provider differences live in the spec, not here. This is
// the same single-behavior pattern as `stigmergic-workflow`.
// ---------------------------------------------------------------------------

struct RoutedAgent {
    system_prompt: String,
    provider: Arc<dyn LlmProvider>,
}

#[async_trait::async_trait]
impl AgentBehavior for RoutedAgent {
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
// Which tier a given agent runs on. The whole point of the example is that
// this is a per-agent choice, declared right next to the agent.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Tier {
    /// Cheap local model (Ollama). Free to run.
    Local,
    /// Hosted frontier model. Priced per token.
    Frontier,
}

/// One agent in the DAG: its id, dependencies, system prompt, which provider
/// tier it runs on, and the canned reply its mock provider returns.
struct AgentSpec {
    id: &'static str,
    depends_on: &'static [&'static str],
    tier: Tier,
    system_prompt: &'static str,
    reply: &'static str,
}

fn dag() -> Vec<AgentSpec> {
    vec![
        // triage: classify the request. Trivial work → cheap local model.
        AgentSpec {
            id: "triage",
            depends_on: &[],
            tier: Tier::Local,
            system_prompt: "You are a triage agent. Classify the request and name the \
                            sub-questions a researcher should answer. Be terse.",
            reply: "CLASSIFICATION: technical / comparison request.\n  \
                    Sub-questions:\n  \
                    1. What does Axocoatl coordinate, and how?\n  \
                    2. How does it differ from a central orchestrator?\n  \
                    Route: send to drafter for a first pass.",
        },
        // drafter: produce a rough first draft. High-volume → cheap local model.
        AgentSpec {
            id: "drafter",
            depends_on: &["triage"],
            tier: Tier::Local,
            system_prompt: "You are a drafter. Using the triage notes, write a quick, rough \
                            first-pass answer. Don't polish it — the synthesizer will.",
            reply: "DRAFT (rough):\n  \
                    - Axocoatl coordinates agents via a shared EventLattice, not a controller.\n  \
                    - Agents self-activate when accumulated signal crosses a threshold.\n  \
                    - Unlike a central orchestrator, no component holds the run order.\n  \
                    (notes terse, needs tightening + a clear contrast paragraph)",
        },
        // synthesizer: read triage + draft, produce the final answer. This is
        // the step that benefits from a big context window and the strongest
        // model → frontier tier. It is also the expensive one.
        AgentSpec {
            id: "synthesizer",
            depends_on: &["triage", "drafter"],
            tier: Tier::Frontier,
            system_prompt: "You are a senior synthesizer. Read the triage classification AND \
                            the rough draft, then write the final, polished answer with a \
                            crisp contrast paragraph.",
            reply: "FINAL ANSWER:\n  \
                    Axocoatl coordinates agents through a shared coordination space — the \
                    EventLattice — rather than a central controller. Each agent registers an \
                    activation threshold; when completion events deposit enough signal to \
                    cross it, the agent self-activates. The running order is therefore an \
                    emergent property of the signals, not a script.\n\n  \
                    Contrast: a central orchestrator owns a plan and dispatches steps in a \
                    fixed sequence; if it stalls, the whole run stalls. In Axocoatl no single \
                    component holds the order, so the same DAG keeps making progress as long \
                    as upstream signals keep landing. Verdict: ready to ship.",
        },
    ]
}

/// `threshold = 0.5 × N` for downstream agents; entry agents get `1.0`. This is
/// the exact rule the daemon uses (`lattice_params` in `axocoatl-daemon`), and
/// it is the same rule the `stigmergic-workflow` example documents.
fn threshold_for(depends_on: &[&str]) -> f32 {
    if depends_on.is_empty() {
        1.0
    } else {
        depends_on.len() as f32 * 0.5
    }
}

/// The price + display label for a tier. Mock list prices for illustration:
/// the local model is free to run, the frontier model is priced roughly at
/// Claude Sonnet's public per-token rate ($3 / 1M input, $15 / 1M output).
fn tier_pricing(tier: Tier) -> Pricing {
    match tier {
        Tier::Local => Pricing {
            input_per_1k: 0.0,
            output_per_1k: 0.0,
        },
        Tier::Frontier => Pricing {
            input_per_1k: 0.003,
            output_per_1k: 0.015,
        },
    }
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// What we learned about one agent after it ran — kept so we can print a
/// per-agent provider + cost table at the end.
struct AgentResult {
    provider_id: String,
    model_id: String,
    tier: Tier,
    usage: TokenUsageStats,
    cost_usd: f64,
    output: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Axocoatl: Multi-Provider Routing (local + frontier in one DAG) ===\n");

    let specs = dag();
    let goal =
        "Explain how Axocoatl coordinates agents and how that differs from a central orchestrator.";

    // -----------------------------------------------------------------------
    // 1. Build the lattice and register each agent with its activation params,
    //    exactly like the stigmergic-workflow example. The DAG topology is the
    //    same idea; the new thing is the provider tier attached to each agent.
    // -----------------------------------------------------------------------
    let lattice = EventLattice::new(64);
    let mut deps_of: HashMap<AgentId, Vec<AgentId>> = HashMap::new();
    let mut tier_of: HashMap<AgentId, Tier> = HashMap::new();

    println!("Registering agents on the lattice (each with its own provider):");
    for spec in &specs {
        let id = AgentId::new(spec.id);
        let threshold = threshold_for(spec.depends_on);
        // decay_rate 0.0 — signals don't fade, so a join is exact: 0.5 + 0.5 is
        // exactly 1.0. Same deterministic setup the stigmergic-workflow example
        // uses; the daemon defaults downstream agents to a small 0.01 decay.
        lattice.register_agent(id.clone(), threshold, 0.0);
        deps_of.insert(
            id.clone(),
            spec.depends_on.iter().map(|d| AgentId::new(*d)).collect(),
        );
        tier_of.insert(id.clone(), spec.tier);
        let provider_label = match spec.tier {
            Tier::Local => "local-small  (llama3.2:3b)",
            Tier::Frontier => "frontier     (claude-sonnet-4-6)",
        };
        println!(
            "  • {:<12} provider={:<34} threshold {:.1}",
            spec.id, provider_label, threshold
        );
    }
    println!();

    // -----------------------------------------------------------------------
    // 2. Print the capability contrast up front — this is what a router would
    //    inspect to decide which tier a step belongs on.
    // -----------------------------------------------------------------------
    let local_caps = MockLocalProvider {
        reply: String::new(),
    }
    .capabilities();
    let frontier_caps = MockFrontierProvider {
        reply: String::new(),
    }
    .capabilities();
    println!("Provider capability contrast:");
    println!(
        "  {:<13} context {:>7}  tool_calling={:<5}  reasoning={:<5}  cost=free",
        "local-small", local_caps.max_context_tokens, local_caps.tool_calling, local_caps.reasoning
    );
    println!(
        "  {:<13} context {:>7}  tool_calling={:<5}  reasoning={:<5}  cost=$3/$15 per 1M tok",
        "frontier",
        frontier_caps.max_context_tokens,
        frontier_caps.tool_calling,
        frontier_caps.reasoning
    );
    println!();

    // -----------------------------------------------------------------------
    // 3. Spawn each agent as a ractor actor, handing it the provider for its
    //    tier. This is the line that does the routing: a Local-tier agent gets
    //    a `MockLocalProvider`, a Frontier-tier agent gets a frontier one.
    // -----------------------------------------------------------------------
    let mut refs: HashMap<AgentId, ractor::ActorRef<axocoatl_actor::AgentMessage>> = HashMap::new();
    let mut handles = Vec::new();
    for spec in &specs {
        let id = AgentId::new(spec.id);
        let provider: Arc<dyn LlmProvider> = match spec.tier {
            Tier::Local => Arc::new(MockLocalProvider {
                reply: spec.reply.to_string(),
            }),
            Tier::Frontier => Arc::new(MockFrontierProvider {
                reply: spec.reply.to_string(),
            }),
        };
        // The AgentConfig records the chosen provider/model — the same fields a
        // YAML agent sets via `provider:` / `model:`.
        let config = AgentConfig {
            id: id.clone(),
            name: spec.id.to_string(),
            provider: provider.provider_id().to_string(),
            model: provider.model_id().to_string(),
            system_prompt: Some(spec.system_prompt.to_string()),
            ..AgentConfig::default()
        };
        let behavior = RoutedAgent {
            system_prompt: spec.system_prompt.to_string(),
            provider,
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
    // 4. Drive the cascade off the lattice — the same activation loop as the
    //    stigmergic-workflow example. Entry agents are kicked off directly;
    //    each completion publishes a TaskCompleted event; the lattice returns
    //    whoever just crossed their threshold; a `completed` guard stops re-runs.
    // -----------------------------------------------------------------------
    let mut completed: HashMap<AgentId, AgentResult> = HashMap::new();
    let mut activation_order: Vec<String> = Vec::new();

    let mut queue: VecDeque<AgentId> = specs
        .iter()
        .filter(|s| s.depends_on.is_empty())
        .map(|s| AgentId::new(s.id))
        .collect();

    println!("Goal: {goal}\n{}", "─".repeat(72));

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
                if let Some(res) = completed.get(d) {
                    buf.push_str(&format!("\n[from {d}]\n{}\n", res.output));
                }
            }
            buf
        };

        let tier = *tier_of.get(&agent_id).expect("tier set at registration");
        let tier_label = match tier {
            Tier::Local => "local-small",
            Tier::Frontier => "frontier",
        };

        // A short, honest activation line showing WHY it fired and on which tier.
        if deps.is_empty() {
            println!("\n⚡ {agent_id} activated — entry agent, kicked off directly  [provider: {tier_label}]");
        } else {
            let signal = deps.len() as f32 * 0.5;
            let threshold = deps.len() as f32 * 0.5;
            println!(
                "\n⚡ {agent_id} activated — {} upstream complete, signal {signal:.1} ≥ threshold {threshold:.1}  [provider: {tier_label}]",
                deps.len(),
            );
        }

        let actor = refs.get(&agent_id).expect("agent spawned above");
        let output = execute_agent(actor, AgentInput::text(&input_text))
            .await
            .map_err(|e| format!("{agent_id} failed: {e}"))?;

        let pricing = tier_pricing(tier);
        let cost = pricing.cost(&output.token_usage);
        println!("{}", output.content);
        println!(
            "   └─ {} tok ({} in + {} out)  →  ${:.5}",
            output.token_usage.total(),
            output.token_usage.input_tokens,
            output.token_usage.output_tokens,
            cost
        );

        completed.insert(
            agent_id.clone(),
            AgentResult {
                provider_id: tier_label.to_string(),
                model_id: match tier {
                    Tier::Local => "llama3.2:3b".to_string(),
                    Tier::Frontier => "claude-sonnet-4-6".to_string(),
                },
                tier,
                usage: output.token_usage.clone(),
                cost_usd: cost,
                output: output.content.clone(),
            },
        );
        activation_order.push(agent_id.to_string());

        // Publish the completion. publish() deposits signal on every registered
        // agent and returns whoever just crossed their threshold.
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
            let known = deps_of.contains_key(&next);
            if known && !completed.contains_key(&next) && !queue.contains(&next) {
                queue.push_back(next);
            }
        }
    }

    // -----------------------------------------------------------------------
    // 5. Report — the per-agent provider + cost table is the payoff. Two cheap
    //    local steps and one frontier step ran in the same DAG; the cost lives
    //    almost entirely on the one agent that needed the frontier model.
    // -----------------------------------------------------------------------
    println!("\n{}", "─".repeat(72));
    println!("\nActivation order (emergent, not scripted):");
    for (i, id) in activation_order.iter().enumerate() {
        println!("  {}. {id}", i + 1);
    }

    println!("\nPer-agent provider + cost:");
    println!(
        "  {:<13} {:<13} {:<20} {:>10} {:>12}",
        "agent", "tier", "model", "tokens", "cost (USD)"
    );
    let mut local_total = TokenUsageStats::default();
    let mut frontier_total = TokenUsageStats::default();
    let mut local_cost = 0.0;
    let mut frontier_cost = 0.0;
    // Report in activation order so the table reads top-to-bottom like the run.
    for id in &activation_order {
        let res = completed
            .get(&AgentId::new(id.as_str()))
            .expect("ran above");
        println!(
            "  {:<13} {:<13} {:<20} {:>10} {:>12}",
            id,
            res.provider_id,
            res.model_id,
            res.usage.total(),
            format!("${:.5}", res.cost_usd),
        );
        match res.tier {
            Tier::Local => {
                local_total.merge(&res.usage);
                local_cost += res.cost_usd;
            }
            Tier::Frontier => {
                frontier_total.merge(&res.usage);
                frontier_cost += res.cost_usd;
            }
        }
    }

    println!("\nCost contrast:");
    println!(
        "  local tier:    {:>5} tokens  →  ${:.5}  (2 agents, runs on your box)",
        local_total.total(),
        local_cost
    );
    println!(
        "  frontier tier: {:>5} tokens  →  ${:.5}  (1 agent, hosted)",
        frontier_total.total(),
        frontier_cost
    );
    let total_cost = local_cost + frontier_cost;
    if total_cost > 0.0 {
        let frontier_pct = (frontier_cost / total_cost) * 100.0;
        println!(
            "  the frontier step is {:.0}% of the ${:.5} total — routing it to a cheap",
            frontier_pct, total_cost
        );
        println!(
            "  local model would have flattened the bill, but the synthesis needs the big model."
        );
    }

    // 6. Shut the actors down.
    for actor in refs.values() {
        actor.stop(None);
    }
    for handle in handles {
        let _ = handle.await;
    }

    println!("\n=== Done ===");
    Ok(())
}
