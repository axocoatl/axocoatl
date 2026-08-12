//! Legacy proactive YAML projected into canonical Automations.
//!
//! Axocoatl's production trigger runtime reads one persisted `AutomationStore`.
//! The legacy `workflows:`, `schedules:`, and `proactive:` YAML sections are
//! migration input: when the canonical store file is missing, the daemon projects them
//! into canonical [`Automation`] records. Settings/API edits are live after
//! that; config reload is not a second trigger registry.
//!
//! This offline example demonstrates that projection, then illustrates the
//! matching and guard principles for an `OnEvent` Automation with a mock LLM:
//!
//! 1. Parse the real legacy schema.
//! 2. Project it through `Automation::from_legacy`, the seed conversion used by
//!    `AutomationStore`.
//! 3. Match `AgentFailed`, gate on the canonical `enabled` field, and suppress
//!    a repeat inside a demo cooldown.
//! 4. Activate a real `ractor` agent and show the event payload in its input.
//!
//! The small `deliver` helper is deliberately not presented as the production
//! dispatcher. Production uses one store-watching schedule/event/Skill runtime,
//! single-flight ownership, and cooldown at dispatch and completion.
//!
//! Run: `cargo run -p proactive-agents` (no API keys — mock LLM).

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ractor::Actor;
use tokio::sync::Mutex;
use tokio_stream::Stream;

use axocoatl_actor::{execute_agent, AgentActor, AgentBehavior, AgentError};
use axocoatl_config::{parse_config, Automation, AutomationNodeKind, AutomationTrigger};
use axocoatl_coordination::{EventId, EventLattice, EventNotification, EventType, LatticeEvent};
use axocoatl_core::{AgentConfig, AgentId, AgentInput, AgentOutput, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};

/// Demo-local window used to make the cooldown guard visible in one run.
const DEMO_COOLDOWN_SECS: u64 = 30;

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Mock LLM — one canned diagnostic, so the example runs with no API keys. In a
// real deployment the `ops` agent points at an Ollama / OpenAI / Anthropic
// provider. The mock echoes back the failure context it was handed so the
// output visibly shows the event payload flowing into the prompt.
// ---------------------------------------------------------------------------

struct OpsDiagnosticLlm;

#[async_trait::async_trait]
impl LlmProvider for OpsDiagnosticLlm {
    fn provider_id(&self) -> &str {
        "mock"
    }

    fn model_id(&self) -> &str {
        "mock-ops-v1"
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
        // Pull the user turn (the diagnostic instruction + failure context the
        // demo helper built) so the canned reply demonstrably reacts to it.
        let context = request
            .messages
            .iter()
            .rev()
            .find_map(|m| m.text_content())
            .unwrap_or("(no context)")
            .to_string();

        let content = format!(
            "DIAGNOSIS\n\
             ─────────\n\
             Triggering context:\n  {context}\n\n\
             Likely cause: the failing agent hit an unhandled provider error \
             mid-execution (timeout or rate limit), so its turn never produced \
             output.\n\
             Suggested fix:\n\
             1. Re-run the failed agent with an OverflowPolicy::Warn budget so a \
                spend cap can't abort it silently.\n\
             2. Add a retry-with-backoff around the provider call.\n\
             3. If it recurs, fail the workflow loudly instead of leaving a \
                half-finished DAG."
        );

        Ok(ChatResponse {
            content,
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: TokenUsageStats::new(70, 90),
            model: "mock-ops-v1".to_string(),
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
// The ops agent's behavior — calls its provider with its system prompt. This is
// the Agent node in the projected `pro:failure-watch` Automation.
// ---------------------------------------------------------------------------

struct OpsBehavior {
    system_prompt: String,
    provider: Arc<dyn LlmProvider>,
}

#[async_trait::async_trait]
impl AgentBehavior for OpsBehavior {
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
// Demo observation state around one canonical Automation. The production
// dispatcher reads the persisted AutomationStore again before every run; this
// local Mutex only makes the enabled/cooldown gates easy to demonstrate.
// ---------------------------------------------------------------------------

struct DemoTriggerState {
    automation: Automation,
    last_fired_unix: Option<u64>,
    run_count: u64,
}

/// Outcome of one delivered event, for the demo's narration.
enum FireOutcome {
    Fired { output: String },
    SkippedDisabled,
    SkippedCooldown,
    NotMatched,
}

/// One-word description of a non-firing outcome, for the demo narration.
fn describe(o: &FireOutcome) -> &'static str {
    match o {
        FireOutcome::Fired { .. } => "FIRED",
        FireOutcome::SkippedDisabled => "SKIPPED (disabled)",
        FireOutcome::SkippedCooldown => "SKIPPED (cooldown)",
        FireOutcome::NotMatched => "IGNORED (no trigger match)",
    }
}

/// Illustrate event match → enabled → cooldown → agent activation. This is an
/// offline teaching helper, not a replacement for the production Automation
/// dispatcher (which also owns single-flight and completion cooldown state).
async fn deliver(
    notif: &EventNotification,
    state: &Mutex<DemoTriggerState>,
    ops_ref: &ractor::ActorRef<axocoatl_actor::AgentMessage>,
) -> FireOutcome {
    let mut st = state.lock().await;

    // 1. Does this event match the trigger's target event?
    let (target, fallback_input) = match &st.automation.trigger {
        AutomationTrigger::OnEvent { event, input } => {
            (event.clone(), input.clone().unwrap_or_default())
        }
        _ => return FireOutcome::NotMatched,
    };
    if notif.event_type.name() != target {
        return FireOutcome::NotMatched;
    }

    // 2. Canonical enabled gate.
    if !st.automation.enabled {
        return FireOutcome::SkippedDisabled;
    }

    // 3. Demo cooldown — never react faster than once per window.
    if let Some(last) = st.last_fired_unix {
        if now_unix().saturating_sub(last) < DEMO_COOLDOWN_SECS {
            return FireOutcome::SkippedCooldown;
        }
    }

    // 4. Fire: build the agent input from the configured instruction plus the
    //    event payload (so the diagnostic actually sees what failed), then run
    //    the agent. The daemon's `fire()` does the analogous projection into
    //    `execute_automation`; here we hand it straight to the actor.
    let input_text = format!(
        "{}\n\nFailing event payload:\n{}",
        fallback_input,
        serde_json::to_string_pretty(&notif.payload).unwrap_or_default()
    );

    let output = execute_agent(ops_ref, AgentInput::text(&input_text))
        .await
        .map(|o| o.content)
        .unwrap_or_else(|e| format!("(agent execution failed: {e})"));

    st.last_fired_unix = Some(now_unix());
    st.run_count += 1;

    FireOutcome::Fired { output }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Axocoatl: legacy triggers → canonical Automations ===\n");

    // -----------------------------------------------------------------------
    // 1. Load the companion YAML through the REAL config parser. This both
    //    validates the migration input against the real schema.
    // -----------------------------------------------------------------------
    let yaml_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("axocoatl.proactive.example.yaml");
    let raw = std::fs::read_to_string(&yaml_path)?;
    let config = parse_config(&raw, &yaml_path)?;

    println!(
        "Loaded {} (parsed by axocoatl_config::parse_config — the same parser the daemon uses).",
        yaml_path.display()
    );
    println!(
        "  {} agent(s), {} workflow(s), {} schedule(s), {} proactive agent(s).\n",
        config.agents.len(),
        config.workflows.len(),
        config.schedules.len(),
        config.proactive.len(),
    );

    let dependencies = |agent_id: &str| {
        config
            .agents
            .iter()
            .find(|agent| agent.id == agent_id)
            .map(|agent| agent.depends_on.clone())
            .unwrap_or_default()
    };
    let mut automations = Automation::from_legacy(
        &config.workflows,
        &config.schedules,
        &config.proactive,
        &dependencies,
    );
    automations.sort_by(|a, b| a.id.cmp(&b.id));

    // -----------------------------------------------------------------------
    // 2. Show the canonical records the first-boot seed would persist.
    // -----------------------------------------------------------------------
    println!("First-boot AutomationStore projection:");
    for automation in &automations {
        let trigger = match &automation.trigger {
            AutomationTrigger::Manual => "manual".to_string(),
            AutomationTrigger::Schedule { every, .. } => format!("schedule · every {every}"),
            AutomationTrigger::OnEvent { event, .. } => format!("on_event · {event}"),
            AutomationTrigger::OnSkill { skill_id } => format!("on_skill · {skill_id}"),
        };
        let state = if automation.enabled {
            "enabled "
        } else {
            "DISABLED"
        };
        println!(
            "  - {:<22} [{state}] nodes={:<2} trigger={trigger}",
            automation.id,
            automation.nodes.len(),
        );
    }
    println!();
    println!("These YAML sections are seed input, not parallel runtime registries.");
    println!("Settings and /api/automations own live edits after this projection.\n");
    println!("{}", "─".repeat(70));

    // -----------------------------------------------------------------------
    // 3. Find the projected event Automation and spawn its agent as a real
    //    ractor actor. Production would execute the full Automation graph.
    // -----------------------------------------------------------------------
    let watcher = automations
        .iter()
        .find(|automation| matches!(&automation.trigger, AutomationTrigger::OnEvent { .. }))
        .cloned()
        .expect("companion YAML projects an OnEvent Automation");

    let target_event = match &watcher.trigger {
        AutomationTrigger::OnEvent { event, .. } => event.clone(),
        _ => unreachable!("filtered to OnEvent above"),
    };
    let watcher_agent = watcher
        .nodes
        .iter()
        .find_map(|node| match &node.kind {
            AutomationNodeKind::Agent { agent_id, .. } => Some(agent_id.clone()),
            _ => None,
        })
        .expect("projected proactive Automation has an Agent node");

    // The system prompt comes from the projected Automation's Agent node.
    let ops_agent_cfg = config
        .agents
        .iter()
        .find(|agent| agent.id == watcher_agent)
        .expect("the projected Agent must exist in agents:");
    let ops_system_prompt = ops_agent_cfg
        .system_prompt
        .clone()
        .unwrap_or_else(|| "You are an operations agent.".to_string());

    let ops_id = AgentId::new(&watcher_agent);
    let ops_config = AgentConfig {
        id: ops_id,
        name: ops_agent_cfg.name.clone(),
        provider: "mock".to_string(),
        model: "mock-ops-v1".to_string(),
        system_prompt: Some(ops_system_prompt.clone()),
        ..AgentConfig::default()
    };
    let ops_behavior = OpsBehavior {
        system_prompt: ops_system_prompt,
        provider: Arc::new(OpsDiagnosticLlm),
    };
    let (ops_ref, ops_handle) = AgentActor::spawn(
        Some(watcher_agent.clone()),
        AgentActor,
        (ops_config, Box::new(ops_behavior) as Box<dyn AgentBehavior>),
    )
    .await?;

    let state = Mutex::new(DemoTriggerState {
        automation: watcher.clone(),
        last_fired_unix: None,
        run_count: 0,
    });

    // -----------------------------------------------------------------------
    // 4. Build a real EventLattice. The demo publishes, reads the broadcast
    //    notification, and hands it to the small illustrative guard helper.
    // -----------------------------------------------------------------------
    let lattice = EventLattice::new(64);
    let mut events = lattice.subscribe();

    println!(
        "\n'{}' is watching the lattice for `{target_event}` events (agent: {}).",
        watcher.id, watcher_agent
    );

    // --- Event 1: a genuine AgentFailed → the watcher should activate. -------
    println!("\n[1] Publishing a lattice event: AgentFailed (coder timed out)");
    lattice.publish(LatticeEvent {
        id: EventId::random(),
        event_type: EventType::AgentFailed {
            agent_id: "coder".to_string(),
            error: "provider timeout after 30s".to_string(),
        },
        payload: serde_json::json!({
            "agent_id": "coder",
            "error": "provider timeout after 30s",
            "workflow": "feature-dev",
        }),
        produced_by: "feature-dev".to_string(),
        timestamp: now_unix(),
    });

    let notif = events.recv().await?;
    match deliver(&notif, &state, &ops_ref).await {
        FireOutcome::Fired { output } => {
            println!(
                "    '{}' ACTIVATED — `{}` matched its OnEvent trigger.",
                watcher.id,
                notif.event_type.name()
            );
            println!(
                "    The {} agent ran with its diagnostic prompt:\n",
                watcher_agent
            );
            for line in output.lines() {
                println!("      {line}");
            }
        }
        other => println!("    (unexpected outcome: {})", describe(&other)),
    }

    // --- Event 2: an unrelated event → must NOT activate. --------------------
    println!("\n{}", "─".repeat(70));
    println!("\n[2] Publishing an unrelated event: TaskCompleted");
    lattice.publish(LatticeEvent {
        id: EventId::random(),
        event_type: EventType::TaskCompleted {
            task_id: "doc-writer".to_string(),
        },
        payload: serde_json::json!({ "task_id": "doc-writer" }),
        produced_by: "doc-writer".to_string(),
        timestamp: now_unix(),
    });
    let notif = events.recv().await?;
    let outcome = deliver(&notif, &state, &ops_ref).await;
    println!(
        "    {} — `{}` is not the watcher's target event, so the watcher stayed asleep.",
        describe(&outcome),
        notif.event_type.name(),
    );

    // --- Event 3: a second AgentFailed inside the cooldown → suppressed. ------
    println!("\n{}", "─".repeat(70));
    println!(
        "\n[3] Publishing a SECOND AgentFailed immediately (within the {DEMO_COOLDOWN_SECS}s demo cooldown)"
    );
    lattice.publish(LatticeEvent {
        id: EventId::random(),
        event_type: EventType::AgentFailed {
            agent_id: "tester".to_string(),
            error: "assertion failed".to_string(),
        },
        payload: serde_json::json!({ "agent_id": "tester", "error": "assertion failed" }),
        produced_by: "release-checklist".to_string(),
        timestamp: now_unix(),
    });
    let notif = events.recv().await?;
    let outcome = deliver(&notif, &state, &ops_ref).await;
    println!(
        "    {} — the cooldown stops a failure storm from re-firing the watcher (and stops",
        describe(&outcome)
    );
    println!("    a self-loop if the ops agent's own diagnosis ever emitted AgentFailed).");

    // --- Event 4: disable the watcher, then publish a matching event. --------
    println!("\n{}", "─".repeat(70));
    println!("\n[4] Setting enabled=false on the watcher, then publishing AgentFailed again");
    {
        let mut st = state.lock().await;
        st.automation.enabled = false;
        // Clear last-fired so the cooldown isn't what's blocking it — we want to
        // prove the *enabled* gate, in isolation.
        st.last_fired_unix = None;
    }
    lattice.publish(LatticeEvent {
        id: EventId::random(),
        event_type: EventType::AgentFailed {
            agent_id: "reviewer".to_string(),
            error: "panic in review".to_string(),
        },
        payload: serde_json::json!({ "agent_id": "reviewer", "error": "panic in review" }),
        produced_by: "feature-dev".to_string(),
        timestamp: now_unix(),
    });
    let notif = events.recv().await?;
    let outcome = deliver(&notif, &state, &ops_ref).await;
    println!(
        "    {} — the canonical `enabled` gate prevents this Automation from running",
        describe(&outcome)
    );
    println!("    (in production, Settings/API updates this persisted record live).");

    // -----------------------------------------------------------------------
    // 5. Report.
    // -----------------------------------------------------------------------
    let runs = state.lock().await.run_count;
    println!("\n{}", "─".repeat(70));
    println!(
        "\n{} events published; the watcher fired {} time(s). The only fire was the first",
        lattice.event_count(),
        runs,
    );
    println!("AgentFailed — every other event was correctly gated out (wrong type, cooldown,");
    println!("disabled). This offline helper illustrates the guards; the daemon's shared");
    println!("Automation runtime owns production dispatch and completion cooldown.");

    // 6. Shut the actor down.
    ops_ref.stop(None);
    let _ = ops_handle.await;

    println!("\n=== Done ===");
    Ok(())
}
