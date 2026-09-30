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
//! 3. Fire the configured Skills onto a real `EventFeed`, publishing exactly
//!    the events `POST /api/skills/{id}/fire` publishes. A Skill's declared
//!    event is the only kind of event the daemon puts on its feed.
//! 4. Match `BuildFailed`, gate on the canonical `enabled` field, suppress a
//!    repeat inside a demo cooldown, and activate a real `ractor` agent.
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
use axocoatl_config::{
    parse_config, Automation, AutomationNodeKind, AutomationTrigger, SkillConfigYaml,
};
use axocoatl_core::event_feed::{EventFeed, EventId, EventNotification, EventType, FeedEvent};
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
// provider. The mock echoes back the trigger input it was handed so the output
// visibly shows the Automation's instruction flowing into the prompt.
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
        // Pull the user turn (the trigger input the demo helper resolved) so
        // the canned reply demonstrably reacts to it.
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
             Likely cause: a change landed whose tests were not run locally, \
             or a dependency moved under an unpinned version range.\n\
             Suggested fix:\n\
             1. Re-run the failing job and compare its lockfile with the last \
                green build.\n\
             2. Reproduce the failing test locally before changing code.\n\
             3. Pin the dependency if the lockfile changed without a commit \
                that meant to change it."
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

/// Publish a Skill's declared events exactly as `POST /api/skills/{id}/fire`
/// and an Agent's `skill_<id>` tool do: one `Custom` event per `emits` name,
/// produced by `skill:<id>`, carrying only the Skill id.
fn fire_skill(feed: &EventFeed, skill: &SkillConfigYaml) -> usize {
    for emit in &skill.emits {
        feed.publish(FeedEvent {
            id: EventId::random(),
            event_type: EventType::Custom(emit.clone()),
            payload: serde_json::json!({ "fired_by_skill": skill.id }),
            produced_by: format!("skill:{}", skill.id),
            timestamp: now_unix(),
        });
    }
    skill.emits.len()
}

/// The daemon's rule for an event trigger's input: a payload `input` or
/// `content` string wins, then the Automation's configured input. A Skill's
/// payload carries neither, so its configured input is what the Agent reads.
fn trigger_input(payload: &serde_json::Value, fallback: &str) -> String {
    payload
        .get("input")
        .or_else(|| payload.get("content"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(fallback)
        .to_string()
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

    // 4. Fire: resolve the input the way the daemon does, then run the agent.
    //    The daemon hands this input to `execute_automation`; here it goes
    //    straight to the actor.
    let input_text = trigger_input(&notif.payload, &fallback_input);

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
        "  {} agent(s), {} Skill(s), {} workflow(s), {} schedule(s), {} proactive agent(s).\n",
        config.agents.len(),
        config.skills.len(),
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
    // 4. Build a real EventFeed. The demo fires the configured Skills, reads
    //    each broadcast notification, and hands it to the small illustrative
    //    guard helper.
    // -----------------------------------------------------------------------
    let skill = |id: &str| {
        config
            .skills
            .iter()
            .find(|skill| skill.id == id)
            .expect("the companion YAML configures this Skill")
    };
    let build_failed = skill("build-failed");
    let deploy_finished = skill("deploy-finished");

    let feed = EventFeed::new(64);
    let mut published = 0;
    let mut events = feed.subscribe();

    println!(
        "\n'{}' is watching the event feed for `{target_event}` (agent: {}).",
        watcher.id, watcher_agent
    );

    // --- Event 1: the build-failed Skill fires → the watcher should activate.
    println!(
        "\n[1] Firing the '{}' Skill, which publishes {:?}",
        build_failed.id, build_failed.emits
    );
    published += fire_skill(&feed, build_failed);

    let notif = events.recv().await?;
    match deliver(&notif, &state, &ops_ref).await {
        FireOutcome::Fired { output } => {
            println!(
                "    '{}' ACTIVATED — `{}` from {} matched its OnEvent trigger.",
                watcher.id,
                notif.event_type.name(),
                notif.produced_by
            );
            println!("    The {watcher_agent} agent ran with its configured input:\n");
            for line in output.lines() {
                println!("      {line}");
            }
        }
        other => println!("    (unexpected outcome: {})", describe(&other)),
    }

    // --- Event 2: an unrelated Skill event → must NOT activate. --------------
    println!("\n{}", "─".repeat(70));
    println!(
        "\n[2] Firing the '{}' Skill, which publishes {:?}",
        deploy_finished.id, deploy_finished.emits
    );
    published += fire_skill(&feed, deploy_finished);
    let notif = events.recv().await?;
    let outcome = deliver(&notif, &state, &ops_ref).await;
    println!(
        "    {} — `{}` is not the watcher's target event, so the watcher stayed asleep.",
        describe(&outcome),
        notif.event_type.name(),
    );

    // --- Event 3: build-failed again inside the cooldown → suppressed. -------
    println!("\n{}", "─".repeat(70));
    println!(
        "\n[3] Firing '{}' AGAIN immediately (within the {DEMO_COOLDOWN_SECS}s demo cooldown)",
        build_failed.id
    );
    published += fire_skill(&feed, build_failed);
    let notif = events.recv().await?;
    let outcome = deliver(&notif, &state, &ops_ref).await;
    println!(
        "    {} — the cooldown stops a burst of failures from re-firing the watcher (and",
        describe(&outcome)
    );
    println!("    stops a self-loop if the ops agent ever fired build-failed itself).");

    // --- Event 4: disable the watcher, then publish a matching event. --------
    println!("\n{}", "─".repeat(70));
    println!(
        "\n[4] Setting enabled=false on the watcher, then firing '{}' again",
        build_failed.id
    );
    {
        let mut st = state.lock().await;
        st.automation.enabled = false;
        // Clear last-fired so the cooldown isn't what's blocking it — we want to
        // prove the *enabled* gate, in isolation.
        st.last_fired_unix = None;
    }
    published += fire_skill(&feed, build_failed);
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
        published, runs,
    );
    println!("BuildFailed — every other event was correctly gated out (wrong event, cooldown,");
    println!("disabled). This offline helper illustrates the guards; the daemon's shared");
    println!("Automation runtime owns production dispatch and completion cooldown.");

    // 6. Shut the actor down.
    ops_ref.stop(None);
    let _ = ops_handle.await;

    println!("\n=== Done ===");
    Ok(())
}
