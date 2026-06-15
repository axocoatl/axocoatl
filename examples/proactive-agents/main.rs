//! Proactive agents — `trigger.type: schedule` vs `trigger.type: on_event`.
//!
//! Most of Axocoatl runs *reactively*: a user (or a workflow) hands an agent
//! input, the agent runs, it stops. **Proactive agents** are the autonomous
//! half. Nobody prompts them. They sit on the `EventLattice` and act on their
//! own when a **trigger** fires:
//!
//! - `trigger.type: schedule` — wake on a fixed interval (`every: 30s`).
//! - `trigger.type: on_event` — wake whenever a named lattice event occurs
//!   (here: `AgentFailed`).
//!
//! This is the *agent-acts-on-its-own* half of "Always-On". The other half is
//! the Always-On **Service** (`axocoatl service install`), which keeps the
//! daemon *process* alive 24/7 so the triggers have something to fire inside.
//! See the README for the service side; this demo is the trigger side.
//!
//! ## What this example proves
//!
//! It runs the **real** pieces, not a sketch of them:
//!
//! 1. Loads the companion `axocoatl.proactive.example.yaml` through the real
//!    `axocoatl_config::parse_config` — the same parser the daemon uses. If the
//!    YAML didn't match the real schema, this would error out.
//! 2. Reads the two real `proactive:` entries it parsed (`hourly-briefing`, a
//!    `schedule`; `failure-watch`, an `on_event` watcher for `AgentFailed`).
//! 3. Spawns the `ops` agent as a real `ractor` actor (same path the daemon
//!    uses) and wires an **event-triggered runner** onto a real `EventLattice`.
//!    The runner mirrors `axocoatl-daemon/src/proactive.rs` exactly: it matches
//!    the event name against the trigger, honours the live `enabled` flag, and
//!    enforces the same `EVENT_COOLDOWN_SECS` self-loop guard.
//! 4. Publishes a real `EventType::AgentFailed` event to the lattice and lets
//!    the watcher activate the `ops` agent with its diagnostic prompt + the
//!    failing agent's error — the event-trigger wiring, end to end.
//!
//! It then demonstrates the two guardrails that make on_event safe in the
//! daemon: the **cooldown** (a second failure inside the window is ignored) and
//! the **enabled flag** (a disabled watcher ignores a matching event).
//!
//! ## proactive vs the `schedules:` section
//!
//! `axocoatl.yaml` has *both* a `schedules:` block and a `proactive:` block.
//! They are not the same thing:
//!
//! | `schedules:`                              | `proactive:`                          |
//! |-------------------------------------------|---------------------------------------|
//! | fires a whole **workflow** (a DAG)        | fires a **single agent**              |
//! | only time-triggered (`every:`)            | time- **or** event-triggered          |
//! | references a `workflows:` entry by id     | names one `agent:` directly           |
//!
//! A schedule is "re-run this multi-agent pipeline on a clock." A proactive
//! agent is "let this one agent watch the world and act when something happens."
//! This example is about the proactive side; for scheduled multi-agent DAGs see
//! the `stigmergic-workflow` example plus the `schedules:` block in the YAML.
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
use axocoatl_config::{parse_config, ProactiveConfigYaml, ProactiveTrigger};
use axocoatl_coordination::{EventId, EventLattice, EventNotification, EventType, LatticeEvent};
use axocoatl_core::{AgentConfig, AgentId, AgentInput, AgentOutput, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};

/// Minimum gap between two fires of an event-triggered proactive agent — copied
/// verbatim from `axocoatl-daemon/src/proactive.rs`. Guards against a self-loop
/// where the agent emits the very event it reacts to.
const EVENT_COOLDOWN_SECS: u64 = 30;

// ---------------------------------------------------------------------------
// The canonical name of a lattice event, for matching against an
// `OnEvent { event }` trigger. This is the exact function the daemon uses
// (`axocoatl-daemon/src/proactive.rs::event_name`) — reproduced here so the
// example's matching is identical to production, not an approximation.
// ---------------------------------------------------------------------------

fn event_name(et: &EventType) -> String {
    match et {
        EventType::TaskAvailable { .. } => "TaskAvailable".to_string(),
        EventType::TaskCompleted { .. } => "TaskCompleted".to_string(),
        EventType::AgentActivated { .. } => "AgentActivated".to_string(),
        EventType::AgentFailed { .. } => "AgentFailed".to_string(),
        EventType::ToolResult { .. } => "ToolResult".to_string(),
        EventType::UserInput => "UserInput".to_string(),
        EventType::WorkflowCompleted => "WorkflowCompleted".to_string(),
        EventType::Custom(s) => s.clone(),
    }
}

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
        // runner built) so the canned reply demonstrably reacts to it.
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
// the agent the `failure-watch` proactive entry names; the runner activates it.
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
// Live state of one proactive agent — the slice of
// `axocoatl-daemon::proactive::ProactiveState` the runner reads each fire, so
// toggling `enabled` or observing `last_fired_unix` takes effect live.
// ---------------------------------------------------------------------------

struct ProactiveState {
    config: ProactiveConfigYaml,
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

/// Deliver one lattice notification to one event-triggered proactive agent,
/// applying the daemon's exact gate order: name match → `enabled` →
/// cooldown → fire. Mirrors `spawn_event_runner` in the daemon, but fires the
/// agent actor directly instead of routing through `execute_automation` (which
/// would need a full daemon + automation graph).
async fn deliver(
    notif: &EventNotification,
    state: &Mutex<ProactiveState>,
    ops_ref: &ractor::ActorRef<axocoatl_actor::AgentMessage>,
) -> FireOutcome {
    let mut st = state.lock().await;

    // 1. Does this event match the trigger's target event?
    let target = match &st.config.trigger {
        ProactiveTrigger::OnEvent { event } => event.clone(),
        // A schedule-triggered entry never reacts to events — only its ticker
        // fires it. The daemon routes those to a separate interval runner.
        ProactiveTrigger::Schedule { .. } => return FireOutcome::NotMatched,
    };
    if event_name(&notif.event_type) != target {
        return FireOutcome::NotMatched;
    }

    // 2. Live enabled gate.
    if !st.config.enabled {
        return FireOutcome::SkippedDisabled;
    }

    // 3. Cooldown — never react faster than once per window.
    if let Some(last) = st.last_fired_unix {
        if now_unix().saturating_sub(last) < EVENT_COOLDOWN_SECS {
            return FireOutcome::SkippedCooldown;
        }
    }

    // 4. Fire: build the agent input from the configured instruction plus the
    //    event payload (so the diagnostic actually sees what failed), then run
    //    the agent. The daemon's `fire()` does the analogous projection into
    //    `execute_automation`; here we hand it straight to the actor.
    let input_text = format!(
        "{}\n\nFailing event payload:\n{}",
        st.config.input,
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
    println!("=== Axocoatl: Proactive Agents (schedule + on_event triggers) ===\n");

    // -----------------------------------------------------------------------
    // 1. Load the companion YAML through the REAL config parser. This both
    //    validates the file against the real schema and gives us the real
    //    parsed `proactive:` entries to drive the demo with.
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

    // -----------------------------------------------------------------------
    // 2. Show the proactive table and how each entry's trigger reads. This is
    //    exactly what `GET /api/proactive` surfaces in a running daemon.
    // -----------------------------------------------------------------------
    println!("Proactive agents on this config:");
    for p in &config.proactive {
        let trigger = match &p.trigger {
            ProactiveTrigger::Schedule { every } => format!("schedule  · every {every}"),
            ProactiveTrigger::OnEvent { event } => format!("on_event  · {event}"),
        };
        let state = if p.enabled { "enabled " } else { "DISABLED" };
        println!(
            "  • {:<14} [{state}] agent={:<10} trigger={trigger}",
            p.id, p.agent
        );
    }
    println!();
    println!("proactive vs schedules: a `schedules:` entry re-runs a whole *workflow* on a");
    println!("clock; a `proactive:` entry watches for *one agent* to act, on a clock OR an");
    println!("event. This demo drives the on_event side end to end.\n");
    println!("{}", "─".repeat(70));

    // -----------------------------------------------------------------------
    // 3. Find the event-triggered watcher (`failure-watch`) in the parsed
    //    config and spawn the agent it names (`ops`) as a real ractor actor.
    // -----------------------------------------------------------------------
    let watcher = config
        .proactive
        .iter()
        .find(|p| matches!(p.trigger, ProactiveTrigger::OnEvent { .. }))
        .cloned()
        .expect("companion YAML defines an on_event proactive agent");

    let target_event = match &watcher.trigger {
        ProactiveTrigger::OnEvent { event } => event.clone(),
        ProactiveTrigger::Schedule { .. } => unreachable!("filtered to OnEvent above"),
    };

    // The system prompt comes from the agent the proactive entry names.
    let ops_agent_cfg = config
        .agents
        .iter()
        .find(|a| a.id == watcher.agent)
        .expect("the proactive entry's agent must exist in agents:");
    let ops_system_prompt = ops_agent_cfg
        .system_prompt
        .clone()
        .unwrap_or_else(|| "You are an operations agent.".to_string());

    let ops_id = AgentId::new(&watcher.agent);
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
        Some(watcher.agent.clone()),
        AgentActor,
        (ops_config, Box::new(ops_behavior) as Box<dyn AgentBehavior>),
    )
    .await?;

    // The live state the runner reads each delivery (enabled flag, last-fired).
    let state = Mutex::new(ProactiveState {
        config: watcher.clone(),
        last_fired_unix: None,
        run_count: 0,
    });

    // -----------------------------------------------------------------------
    // 4. Build a real EventLattice. The watcher subscribes to it exactly as the
    //    daemon's event runner does. We don't even need a background task for
    //    this demo: we publish, take the broadcast notification, and deliver it.
    // -----------------------------------------------------------------------
    let lattice = EventLattice::new(64);
    let mut events = lattice.subscribe();

    println!(
        "\n'{}' is watching the lattice for `{target_event}` events (agent: {}).",
        watcher.id, watcher.agent
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
                "    ⚡ '{}' ACTIVATED — `{}` matched its on_event trigger.",
                watcher.id,
                event_name(&notif.event_type)
            );
            println!(
                "    The {} agent ran with its diagnostic prompt:\n",
                watcher.agent
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
        event_name(&notif.event_type),
    );

    // --- Event 3: a second AgentFailed inside the cooldown → suppressed. ------
    println!("\n{}", "─".repeat(70));
    println!(
        "\n[3] Publishing a SECOND AgentFailed immediately (within the {EVENT_COOLDOWN_SECS}s cooldown)"
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
        st.config.enabled = false;
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
        "    {} — toggling `enabled` takes effect live; a disabled proactive agent ignores",
        describe(&outcome)
    );
    println!("    its trigger without a daemon restart.");

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
    println!("disabled). No user prompted the ops agent: the lattice did.");

    // 6. Shut the actor down.
    ops_ref.stop(None);
    let _ = ops_handle.await;

    println!("\n=== Done ===");
    Ok(())
}
