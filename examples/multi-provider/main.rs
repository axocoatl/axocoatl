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
//!     └──────────────────────────────────▶┘
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
//! The binary runs the agents in plain dependency order: an agent runs once
//! every agent in its `depends_on` has completed, and ties break by the order
//! the agents are declared, so the run is deterministic. The live daemon runs
//! the YAML's agents through a Lattice session in the same dependency order.
//! The capability demonstrated in both modes is the *provider per agent*.
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

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use ractor::Actor;
use tokio_stream::Stream;

use axocoatl_actor::{execute_agent, AgentActor, AgentBehavior, AgentError};
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
        let billable_output = usage
            .output_tokens
            .saturating_add(usage.reasoning_tokens.unwrap_or(0));
        (usage.input_tokens as f64 / 1000.0) * self.input_per_1k
            + (billable_output as f64 / 1000.0) * self.output_per_1k
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
// prompt. The role + provider differences live in the spec, not here.
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
            reply: "CLASSIFICATION: technical / cost-tradeoff request.\n  \
                    Sub-questions:\n  \
                    1. Which steps are simple enough for a small local model?\n  \
                    2. Which step needs the frontier model, and why?\n  \
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
                    - Triage and drafting are short, forgiving steps → local llama3.2:3b, free.\n  \
                    - Synthesis reads everything upstream → big context, frontier model.\n  \
                    - Each agent names its own provider; one DAG, two tiers.\n  \
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
                    This workflow routes its cheap steps to a small local model. Triage and \
                    the rough first draft are short, forgiving and high-volume, so they run \
                    free on your own hardware and the data stays on the box. The frontier \
                    model is reserved for synthesis: the one step that reads everything \
                    upstream produced and whose quality is what ships.\n\n  \
                    Contrast: sending every step to the frontier model would pay frontier \
                    prices for work a 3B model handles; sending every step to the local model \
                    would give up quality on the one step that ships. Verdict: pick the \
                    provider per agent, and pay frontier prices only on the synthesis.",
        },
    ]
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

/// The next agent to run: the first spec, in declaration order, that has not
/// run yet and whose `depends_on` have all completed. Declaration order breaks
/// ties, so the run order is deterministic.
fn next_ready<'a>(
    specs: &'a [AgentSpec],
    completed: &HashMap<&str, AgentResult>,
) -> Option<&'a AgentSpec> {
    specs.iter().find(|spec| {
        !completed.contains_key(spec.id)
            && spec
                .depends_on
                .iter()
                .all(|dep| completed.contains_key(dep))
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Axocoatl: Multi-Provider Routing (local + frontier in one DAG) ===\n");

    let specs = dag();
    let goal = "Explain how this workflow splits its steps between a local model and a frontier model, and what that does to the cost.";

    // -----------------------------------------------------------------------
    // 1. List the agents. Each one declares its dependencies and the provider
    //    tier it runs on side by side — the DAG says *when* a step runs, the
    //    tier says *where*.
    // -----------------------------------------------------------------------
    println!("Agents (each with its own provider):");
    for spec in &specs {
        let provider_label = match spec.tier {
            Tier::Local => "local-small  (llama3.2:3b)",
            Tier::Frontier => "frontier     (claude-sonnet-4-6)",
        };
        println!(
            "  • {:<12} provider={:<34} depends_on=[{}]",
            spec.id,
            provider_label,
            spec.depends_on.join(", ")
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
    let mut refs: HashMap<&str, ractor::ActorRef<axocoatl_actor::AgentMessage>> = HashMap::new();
    let mut handles = Vec::new();
    for spec in &specs {
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
            id: AgentId::new(spec.id),
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
        refs.insert(spec.id, actor_ref);
        handles.push(handle);
    }

    // -----------------------------------------------------------------------
    // 4. Run the DAG in dependency order. Each step takes the first agent (in
    //    spec order) whose `depends_on` have all completed, feeds it the goal
    //    or its upstream outputs, and records its provider, usage and cost.
    // -----------------------------------------------------------------------
    let mut completed: HashMap<&str, AgentResult> = HashMap::new();
    let mut run_order: Vec<&str> = Vec::new();

    println!("Goal: {goal}\n{}", "─".repeat(72));

    while let Some(spec) = next_ready(&specs, &completed) {
        let agent_id = spec.id;

        // Build this agent's input from its upstream outputs (or the goal, if
        // it is an entry agent).
        let input_text = if spec.depends_on.is_empty() {
            goal.to_string()
        } else {
            let mut buf = format!("Goal: {goal}\n\nUpstream results:\n");
            for dep in spec.depends_on {
                buf.push_str(&format!("\n[from {dep}]\n{}\n", completed[dep].output));
            }
            buf
        };

        let tier = spec.tier;
        let tier_label = match tier {
            Tier::Local => "local-small",
            Tier::Frontier => "frontier",
        };

        // A short, honest run line showing WHY it is ready and on which tier.
        if spec.depends_on.is_empty() {
            println!(
                "\n▶ {agent_id} runs — entry agent, starts from the goal  [provider: {tier_label}]"
            );
        } else {
            println!(
                "\n▶ {agent_id} runs — all dependencies complete ({})  [provider: {tier_label}]",
                spec.depends_on.join(", "),
            );
        }

        let actor = refs.get(agent_id).expect("agent spawned above");
        let output = execute_agent(actor, AgentInput::text(&input_text))
            .await
            .map_err(|e| format!("{agent_id} failed: {e}"))?;

        let pricing = tier_pricing(tier);
        let cost = pricing.cost(&output.token_usage);
        println!("{}", output.content);
        println!(
            "   └─ {} tok ({} in + {} out + {} reasoning)  →  ${:.5}",
            output.token_usage.total(),
            output.token_usage.input_tokens,
            output.token_usage.output_tokens,
            output.token_usage.reasoning_tokens.unwrap_or(0),
            cost
        );

        completed.insert(
            agent_id,
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
        run_order.push(agent_id);
    }

    // Anything left over depends on an agent that never completed (a typo or a
    // cycle in `depends_on`) — say so instead of printing a partial report.
    if completed.len() != specs.len() {
        let stuck: Vec<&str> = specs
            .iter()
            .map(|s| s.id)
            .filter(|id| !completed.contains_key(id))
            .collect();
        return Err(format!("never ran (unmet depends_on): {}", stuck.join(", ")).into());
    }

    // -----------------------------------------------------------------------
    // 5. Report — the per-agent provider + cost table is the payoff. Two cheap
    //    local steps and one frontier step ran in the same DAG; the cost lives
    //    almost entirely on the one agent that needed the frontier model.
    // -----------------------------------------------------------------------
    println!("\n{}", "─".repeat(72));
    println!("\nRun order (dependency order):");
    for (i, id) in run_order.iter().enumerate() {
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
    // Report in run order so the table reads top-to-bottom like the run.
    for id in &run_order {
        let res = &completed[id];
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
            "  the frontier step is {frontier_pct:.0}% of the ${total_cost:.5} total — routing it to a cheap"
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
