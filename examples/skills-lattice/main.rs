//! Skills — event-driven lattice activation (`emits` / `reacts_to`).
//!
//! A **Skill** in Axocoatl is not a tool an agent calls directly. It is a
//! lattice-aware capability declaration: it says which events it `emits` and
//! which events it `reacts_to`, and the lattice does the routing. When a
//! `reacts_to` event lands on the shared coordination space, *every* agent that
//! holds a matching skill activates — fan-out, no central picker.
//!
//! This example wires the exact skill from the docs (`code-review-checklist`)
//! held by two agents, fires it, and lets the lattice activate both holders at
//! once. Each holder then emits its own event, which a *second* skill reacts to
//! — so the routing chains, again with nobody scheduling it.
//!
//! ```text
//!   CodeReady ─┐
//!              ├─▶ skill: code-review-checklist  (reacts_to CodeReady)
//!              │      holders: reviewer, coder         ← one event, BOTH fire
//!              │        reviewer ──emits──▶ ReviewComplete
//!              │        coder    ──emits──▶ ReviewComplete + BlockingIssueFound
//!              │
//!   ReviewComplete ─▶ skill: deploy-gate  (reacts_to ReviewComplete)
//!                        holder: deployer        ← activated by the chain
//! ```
//!
//! ## How a skill routes (the actual mechanism)
//!
//! This mirrors the runtime exactly:
//!
//! - Firing a skill publishes each of its `emits` strings to the lattice as
//!   `EventType::Custom(name)` — this is what `POST /api/skills/{id}/fire` and
//!   the in-session `SkillTool` both do (`axocoatl-daemon/src/skill_tool.rs`,
//!   `axocoatl-server/src/routes.rs::fire_skill`).
//! - A `Custom` event deposits a signal of strength `0.5` onto *every*
//!   registered agent (`EventLattice::publish`, the `Custom(_) => 0.5` arm).
//! - So registering each holder of a skill at threshold `0.5` means a single
//!   matching event crosses all of them together. `publish()` returns exactly
//!   that set — the "plain fan-out, no central picker" the Skills doc describes.
//!
//! ## Skill prompt ≠ agent system prompt
//!
//! Each holder agent has its **own** `system_prompt` (its standing role). The
//! skill carries a separate `prompt` template that is handed to whichever holder
//! activates *for this skill*. The agent's voice stays constant; the skill
//! supplies the task. This example prints both so the difference is visible.
//!
//! ## Skills vs Workflows
//!
//! This is event-driven **capability routing**: declare `reacts_to`, and the
//! lattice fires every holder when that event appears — no fixed graph. For a
//! fixed `depends_on` DAG where order emerges from join thresholds, see the
//! `stigmergic-workflow` example. Same lattice, two routing styles.
//!
//! Run: `cargo run` from `examples/skills-lattice/` (no API keys — mock LLM).

use std::collections::HashMap;
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
// Mock LLM — one canned, structured-JSON reply per agent, so the example runs
// with no API keys. In a real app this is an Ollama / OpenAI / Anthropic
// provider and the agent returns whatever the model produced for the skill
// prompt it was handed.
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
            structured_output: true,
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
            usage: TokenUsageStats::new(48, 72),
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
// One generic behavior. When a skill activates this agent, the agent is called
// with the SKILL's prompt as the system message — distinct from the agent's own
// standing `system_prompt`. The role differences live in the config + the
// provider's canned reply.
// ---------------------------------------------------------------------------

struct HolderAgent {
    /// The agent's standing role. Constant across every skill it holds.
    system_prompt: String,
    provider: Arc<dyn LlmProvider>,
}

#[async_trait::async_trait]
impl AgentBehavior for HolderAgent {
    async fn on_start(&mut self, _config: &AgentConfig) -> Result<(), AgentError> {
        Ok(())
    }

    async fn execute(&mut self, input: AgentInput) -> Result<AgentOutput, AgentError> {
        // A skill activation passes the skill's prompt as a per-call system
        // override. The agent's own system prompt is its fallback role. This is
        // the prompt/role split made concrete: the skill says WHAT to do, the
        // agent config says WHO is doing it.
        let system = input
            .system_override
            .clone()
            .unwrap_or_else(|| self.system_prompt.clone());
        let request = ChatRequest::with_system(&system, &input.content);
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
// Skill + agent definitions. These mirror the `SkillConfigYaml` /
// `AgentConfigYaml` shapes (axocoatl-config) — id, reacts_to, emits, holder
// agents, and a skill prompt distinct from each agent's system prompt.
// ---------------------------------------------------------------------------

/// A skill declaration — the lattice-routed unit of capability.
/// Same fields as `axocoatl_config::SkillConfigYaml`.
struct Skill {
    id: &'static str,
    name: &'static str,
    /// Events that, when published by anyone, fire this skill's holders.
    reacts_to: &'static [&'static str],
    /// Events each activated holder publishes when it finishes.
    emits: &'static [&'static str],
    /// Agents that hold this skill — any/all activate on fan-out.
    holders: &'static [&'static str],
    /// The task the holder is handed when the skill fires. NOT the agent's
    /// system prompt — see `HolderAgent::execute`.
    prompt: &'static str,
}

/// A holder agent — its standing role and what its mock model returns when a
/// skill activates it.
struct AgentSpec {
    id: &'static str,
    /// The agent's standing system prompt (its identity, used when no skill
    /// prompt overrides it).
    system_prompt: &'static str,
    reply: &'static str,
}

fn skills() -> Vec<Skill> {
    vec![
        Skill {
            id: "code-review-checklist",
            name: "Code Review Checklist",
            reacts_to: &["CodeReady"],
            emits: &["ReviewComplete"],
            holders: &["reviewer", "coder"],
            prompt: "Run the 12-point review checklist on the diff: correctness, \
                     edge cases, error handling, performance, security, naming, \
                     tests. Return JSON with `issues` and `severity`.",
        },
        Skill {
            id: "deploy-gate",
            name: "Deploy Gate",
            reacts_to: &["ReviewComplete"],
            emits: &["DeployApproved"],
            holders: &["deployer"],
            prompt: "A review just completed. Decide whether the change may ship. \
                     Return JSON with `decision` and `reason`.",
        },
    ]
}

fn agents() -> Vec<AgentSpec> {
    vec![
        AgentSpec {
            id: "reviewer",
            system_prompt: "You are a senior reviewer. You care about correctness and tests.",
            reply: "{\"agent\":\"reviewer\",\"issues\":[\
                    {\"area\":\"tests\",\"note\":\"no test for the empty-cart path\"}],\
                    \"severity\":\"minor\",\"verdict\":\"approve-with-nits\"}",
        },
        AgentSpec {
            id: "coder",
            system_prompt: "You are the implementing engineer. You review for runtime safety.",
            reply: "{\"agent\":\"coder\",\"issues\":[\
                    {\"area\":\"error-handling\",\"note\":\"unwrap() on a network call can panic\"}],\
                    \"severity\":\"blocking\",\"verdict\":\"changes-requested\"}",
        },
        AgentSpec {
            id: "deployer",
            system_prompt: "You are the release gate. You only ship green reviews.",
            reply: "{\"agent\":\"deployer\",\"decision\":\"hold\",\
                    \"reason\":\"a blocking issue was raised during review\"}",
        },
    ]
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Build a `Custom` lattice event for a named skill/agent event.
fn custom_event(name: &str, produced_by: &str) -> LatticeEvent {
    LatticeEvent {
        id: EventId::random(),
        event_type: EventType::Custom(name.to_string()),
        payload: serde_json::json!({ "event": name, "produced_by": produced_by }),
        produced_by: produced_by.to_string(),
        timestamp: now_ts(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Axocoatl: Skills — event-driven lattice activation ===\n");

    let skills = skills();
    let agent_specs = agents();

    // -----------------------------------------------------------------------
    // 1. Spawn every holder agent as a ractor actor (same path the daemon uses).
    // -----------------------------------------------------------------------
    let mut refs: HashMap<AgentId, ractor::ActorRef<axocoatl_actor::AgentMessage>> = HashMap::new();
    let mut system_prompt_of: HashMap<AgentId, &'static str> = HashMap::new();
    let mut handles = Vec::new();

    for spec in &agent_specs {
        let id = AgentId::new(spec.id);
        let config = AgentConfig {
            id: id.clone(),
            name: spec.id.to_string(),
            provider: "mock".to_string(),
            model: spec.id.to_string(),
            system_prompt: Some(spec.system_prompt.to_string()),
            ..AgentConfig::default()
        };
        let behavior = HolderAgent {
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
        refs.insert(id.clone(), actor_ref);
        system_prompt_of.insert(id, spec.system_prompt);
        handles.push(handle);
    }

    // -----------------------------------------------------------------------
    // 2. Build the lattice and the routing index: event name → the skills that
    //    react to it. The lattice's `Custom(_)` signal is event-name-blind by
    //    design (every registered agent gets +0.5); the runtime layers the
    //    event→holder routing on top via `reacts_to`. We mirror that: when an
    //    event fires, we register the holder bindings of *exactly* the skills
    //    that react to it (threshold 0.5), then publish — so the lattice's own
    //    threshold math activates precisely the right fan-out, and nothing else.
    //
    //    Bindings are keyed on a synthetic id `"<skill>::<holder>"` so two
    //    skills held by the same agent stay independent accumulators.
    // -----------------------------------------------------------------------
    let lattice = EventLattice::new(64);
    // event name -> skill indices that react to it (the routing table)
    let mut reactors_of: HashMap<String, Vec<usize>> = HashMap::new();
    // synthetic lattice id -> (skill index, holder agent id)
    let mut binding_of: HashMap<AgentId, (usize, AgentId)> = HashMap::new();

    println!("Skills declared (reacts_to / emits / holders):");
    for (si, skill) in skills.iter().enumerate() {
        for ev in skill.reacts_to {
            reactors_of.entry((*ev).to_string()).or_default().push(si);
        }
        println!(
            "  • {:<22} reacts_to={:?}  emits={:?}  holders={:?}",
            skill.id, skill.reacts_to, skill.emits, skill.holders
        );
    }
    let total_bindings: usize = skills.iter().map(|s| s.holders.len()).sum();
    println!(
        "\n{} skill-holder bindings across {} skills.\n",
        total_bindings,
        skills.len()
    );

    // -----------------------------------------------------------------------
    // 3. Drive the cascade. Publish a seed event; whatever it activates runs;
    //    each activated holder emits its skill's events back onto the lattice;
    //    those may activate further holders. A guard stops a (skill, holder)
    //    binding from running twice. Nobody schedules this — the lattice routes.
    // -----------------------------------------------------------------------
    let seed = "CodeReady";
    println!("Publishing seed event: {seed}  (e.g. a PR's CI just went green)");
    println!("{}", "─".repeat(64));

    // The work queue holds raw event names to publish. The lattice turns each
    // one into a set of activated (skill, holder) bindings.
    let mut pending_events: std::collections::VecDeque<(String, String)> =
        std::collections::VecDeque::new();
    pending_events.push_back((seed.to_string(), "external:ci".to_string()));

    let mut ran: std::collections::HashSet<AgentId> = std::collections::HashSet::new();
    let mut activation_log: Vec<(String, String, String)> = Vec::new(); // (skill, holder, emitted)

    while let Some((event_name, source)) = pending_events.pop_front() {
        // Which skills react to this event? Only their holders should be in the
        // running for activation. Register exactly those holder bindings on the
        // lattice at threshold 0.5 right now, then publish — so the lattice's
        // own threshold math (Custom => 0.5) crosses precisely this fan-out.
        let reacting: Vec<usize> = reactors_of.get(&event_name).cloned().unwrap_or_default();
        if reacting.is_empty() {
            println!(
                "\n· event {event_name} (from {source}) — no skill reacts to it, lattice quiet"
            );
            continue;
        }
        for &si in &reacting {
            for holder in skills[si].holders {
                let binding_id = AgentId::new(format!("{}::{holder}", skills[si].id));
                // decay 0.0 — deterministic for the example (the daemon uses a
                // small decay so stale signals expire on long-running graphs).
                lattice.register_agent(binding_id.clone(), 0.5, 0.0);
                binding_of
                    .entry(binding_id)
                    .or_insert_with(|| (si, AgentId::new(*holder)));
            }
        }

        // Publish the event. publish() deposits 0.5 on every registered binding
        // and returns the bindings that just crossed their 0.5 threshold — i.e.
        // every holder of every skill that reacts to this event.
        let activated = lattice.publish(custom_event(&event_name, &source));

        // Resolve activated bindings to (skill, holder) pairs we know about,
        // skip ones that already ran. Sort for stable, readable output.
        let mut fired: Vec<(usize, AgentId)> = Vec::new();
        for bid in activated {
            if ran.contains(&bid) {
                continue;
            }
            if let Some((si, holder)) = binding_of.get(&bid).cloned() {
                ran.insert(bid);
                fired.push((si, holder));
            }
        }
        fired.sort_by_key(|(si, holder)| (*si, holder.to_string()));

        if fired.is_empty() {
            // Reacting skills exist but every holder already ran — the routing
            // converged. Note it and move on.
            println!("\n· event {event_name} (from {source}) — reacting holders already ran");
            continue;
        }

        println!(
            "\n⚡ event {event_name} (from {source}) → {} skill(s) react, fanning out to {} holder(s):",
            reacting.len(),
            fired.len()
        );

        for (si, holder) in fired {
            let skill = &skills[si];
            // Hand the holder the SKILL's prompt as a per-call system override.
            // Its own standing system prompt is shown alongside to make the
            // distinction explicit.
            let standing = system_prompt_of
                .get(&holder)
                .copied()
                .unwrap_or("(no system prompt)");
            println!(
                "\n  ▸ skill '{}' ({}) activated holder '{holder}'",
                skill.id, skill.name
            );
            println!("      agent system prompt : {standing}");
            println!("      skill prompt (task) : {}", skill.prompt);

            let actor = refs.get(&holder).expect("holder spawned above");
            let input = AgentInput::text(format!(
                "Event {event_name} fired the '{}' skill. {}",
                skill.id, skill.prompt
            ))
            .with_system_override(Some(skill.prompt.to_string()));

            let output = execute_agent(actor, input)
                .await
                .map_err(|e| format!("{holder} failed: {e}"))?;
            println!("      output (structured) : {}", output.content);

            // The holder finished → the skill emits its events back onto the
            // lattice. This is where routing chains: deploy-gate reacts to
            // ReviewComplete, so emitting it here will fan out next iteration.
            let emitted = skill.emits.join(", ");
            for emit in skill.emits {
                println!("      ↳ emits {emit}");
                pending_events.push_back(((*emit).to_string(), format!("{}:{holder}", skill.id)));
            }
            activation_log.push((skill.id.to_string(), holder.to_string(), emitted));
        }
    }

    // -----------------------------------------------------------------------
    // 4. Report. The routing was never scripted — it came from reacts_to /
    //    emits crossing thresholds on the lattice.
    // -----------------------------------------------------------------------
    println!("\n{}", "─".repeat(64));
    println!("\nActivation log (skill → holder → emitted):");
    for (i, (skill, holder, emitted)) in activation_log.iter().enumerate() {
        println!("  {}. {skill:<22} → {holder:<9} → emits [{emitted}]", i + 1);
    }
    println!(
        "\n{} skill activations, {} events on the lattice. \
         No orchestrator decided who ran — reacts_to/emits did.",
        activation_log.len(),
        lattice.event_count()
    );

    // 5. Shut the actors down.
    for actor in refs.values() {
        actor.stop(None);
    }
    for handle in handles {
        let _ = handle.await;
    }

    println!("\n=== Done ===");
    Ok(())
}
