//! Crash recovery — resume a multi-agent workflow from a persisted checkpoint.
//!
//! This is the README hero demo in code: **kill the process mid-run and the
//! workflow restarts from its last checkpoint, not from zero.**
//!
//! The workflow is a two-step pipeline:
//!
//! ```text
//!     researcher ──output──▶ summarizer
//!     (step 1)               (step 2, needs step 1's output as context)
//! ```
//!
//! We run it across **two separate process lifetimes** in one program:
//!
//! 1. **First run.** The researcher executes, then we persist a checkpoint to
//!    disk: step 1 done (with its output captured), step 2 pending. Then we
//!    *crash* — drop every actor and the in-memory workflow struct, so nothing
//!    about the run survives in memory.
//! 2. **Restart.** We build a brand-new, empty workflow and read the checkpoint
//!    back from disk. The persisted state says the researcher already finished,
//!    so step 1 is **skipped entirely** — its agent is never even spawned — and
//!    the summarizer resumes, fed the researcher's persisted output as upstream
//!    context.
//!
//! ## What drives the skip
//!
//! The skip is **not** a hardcoded flag. It is driven by the bytes on disk. The
//! restart phase has zero in-memory knowledge of the first run; it reconstructs
//! the work list purely from [`CheckpointStore::load_latest`]. To prove the
//! researcher is genuinely not re-run, its mock LLM counts its own calls — after
//! the restart that counter is still `1`.
//!
//! ## The persistence mechanism is the real one
//!
//! State is persisted exactly the way the production coordinator persists a
//! resumable run (`crates/axocoatl-actor/src/coordinator.rs`): the workflow
//! state is serialized to JSON and stored in [`AgentCheckpoint::behavior_state`]
//! via [`CheckpointStore`], which writes a bincode snapshot atomically (temp
//! file + rename) and prunes to the last 3 versions. `load_latest` reads the
//! highest version back. We use the same `behavior_state` JSON convention so the
//! example exercises the genuine store, not a toy of its own.
//!
//! Run: `cargo run` from `examples/crash-recovery/` (no API keys — mock LLM).

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ractor::Actor;
use serde::{Deserialize, Serialize};
use tokio_stream::Stream;

use axocoatl_actor::{execute_agent, AgentActor, AgentBehavior, AgentError};
use axocoatl_core::{AgentConfig, AgentId, AgentInput, AgentOutput, TokenUsageStats};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};
use axocoatl_memory::{AgentCheckpoint, CheckpointPolicy, CheckpointStore};

// ---------------------------------------------------------------------------
// Persisted workflow state — serialized into `AgentCheckpoint.behavior_state`
// as JSON, exactly like the coordinator's `OrchestrationState`. This is the
// only source of truth a restart reads from; nothing else survives the crash.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkflowState {
    goal: String,
    steps: Vec<WorkflowStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkflowStep {
    /// Logical agent id for this step (also the actor name).
    agent_id: String,
    /// Human description of what this step does.
    description: String,
    /// Ids of upstream steps whose outputs feed this step's input.
    depends_on: Vec<String>,
    /// `None` until the step's agent has finished. A step with `Some(_)` is
    /// never re-run after a restart — that is the whole point.
    output: Option<String>,
    /// Wall-clock second the step's output was checkpointed (for the report).
    completed_at: Option<u64>,
}

impl WorkflowStep {
    fn is_done(&self) -> bool {
        self.output.is_some()
    }
}

// ---------------------------------------------------------------------------
// Mock LLM — one canned reply per role, so the example runs with no API keys.
// The `calls` counter is the proof the researcher is NOT re-run after restart.
// In a real app this is an Ollama / OpenAI / Anthropic provider.
// ---------------------------------------------------------------------------

struct RoleLlm {
    model: &'static str,
    reply: String,
    calls: Arc<AtomicUsize>,
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
        self.calls.fetch_add(1, Ordering::SeqCst);
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
// One generic behavior — calls its provider with its system prompt. Same shape
// as the stigmergic-workflow example's `LatticeAgent`.
// ---------------------------------------------------------------------------

struct StepAgent {
    system_prompt: String,
    provider: Arc<dyn LlmProvider>,
}

#[async_trait::async_trait]
impl AgentBehavior for StepAgent {
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

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Spawn one step's agent, run it on `input`, and tear the actor down. Returns
/// the agent's output. `calls` is the provider's call counter so the caller can
/// assert how many times this step's model was actually invoked.
async fn run_step(
    agent_id: &str,
    system_prompt: &str,
    input_text: &str,
    reply: &'static str,
    calls: Arc<AtomicUsize>,
) -> Result<String, Box<dyn std::error::Error>> {
    let config = AgentConfig {
        id: AgentId::new(agent_id),
        name: agent_id.to_string(),
        provider: "mock".to_string(),
        model: agent_id.to_string(),
        system_prompt: Some(system_prompt.to_string()),
        ..AgentConfig::default()
    };
    let behavior = StepAgent {
        system_prompt: system_prompt.to_string(),
        provider: Arc::new(RoleLlm {
            model: "mock",
            reply: reply.to_string(),
            calls,
        }),
    };
    let (actor_ref, handle) = AgentActor::spawn(
        Some(agent_id.to_string()),
        AgentActor,
        (config, Box::new(behavior) as Box<dyn AgentBehavior>),
    )
    .await?;

    let output = execute_agent(&actor_ref, AgentInput::text(input_text))
        .await
        .map_err(|e| format!("{agent_id} failed: {e}"))?;

    // Drop the actor — part of the "crash" later, and good hygiene now.
    actor_ref.stop(None);
    let _ = handle.await;

    Ok(output.content)
}

// The two canned replies. `'static` so the call counter can carry them into the
// provider without lifetime juggling.
const RESEARCH_REPLY: &str = "FINDINGS:\n  \
    - SQLite WAL mode lets readers and one writer proceed concurrently.\n  \
    - Checkpointing folds the WAL back into the main db; PASSIVE never blocks writers.\n  \
    - `wal_autocheckpoint` defaults to 1000 pages (~4 MB) before an automatic checkpoint.";

const SUMMARY_REPLY: &str = "SUMMARY:\n  \
    WAL mode trades a small amount of disk for real read/write concurrency. Keep the\n  \
    default autocheckpoint unless write bursts grow the WAL faster than reads can\n  \
    tolerate; switch to manual PASSIVE checkpoints if you must avoid writer stalls.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Axocoatl: Crash Recovery (resume a workflow from a checkpoint) ===\n");

    // -----------------------------------------------------------------------
    // Checkpoint store — a real `CheckpointStore` in a unique temp dir, cleaned
    // up at the end. `EveryLlmCall` is the safest policy (the coordinator uses
    // it too); here we drive saves explicitly so the timing is visible.
    // -----------------------------------------------------------------------
    let checkpoint_dir =
        std::env::temp_dir().join(format!("axocoatl-crash-recovery-{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&checkpoint_dir).await; // fresh start
    println!("Checkpoint dir: {}\n", checkpoint_dir.display());

    let store = CheckpointStore::new(&checkpoint_dir, CheckpointPolicy::EveryLlmCall);

    // The coordinator agent id under which the workflow state is checkpointed.
    // `load_latest` keys on this id.
    let workflow_id = AgentId::new("research-pipeline");

    let goal = "Explain when to use SQLite WAL mode and how its checkpointing behaves.";

    // Call counters — proof of what actually ran in each phase.
    let researcher_calls = Arc::new(AtomicUsize::new(0));
    let summarizer_calls = Arc::new(AtomicUsize::new(0));

    // =======================================================================
    // PHASE 1 — first process run: execute the researcher, checkpoint, crash.
    // =======================================================================
    println!("{}", "=".repeat(70));
    println!("PHASE 1: first run — researcher executes, then we crash mid-workflow");
    println!("{}", "=".repeat(70));

    // The in-memory workflow as it exists during the first run.
    let mut state = WorkflowState {
        goal: goal.to_string(),
        steps: vec![
            WorkflowStep {
                agent_id: "researcher".to_string(),
                description: "Research the topic and produce raw findings".to_string(),
                depends_on: vec![],
                output: None,
                completed_at: None,
            },
            WorkflowStep {
                agent_id: "summarizer".to_string(),
                description: "Condense the researcher's findings into a recommendation".to_string(),
                depends_on: vec!["researcher".to_string()],
                output: None,
                completed_at: None,
            },
        ],
    };

    println!("\nGoal: {goal}");
    println!("Workflow: researcher ──▶ summarizer (summarizer depends on researcher)\n");

    // Run step 1 (the researcher). It has no upstream deps, so its input is the
    // goal. After it finishes we record the output IN the workflow state and
    // persist a checkpoint to disk.
    println!("⚙  Running step 1: researcher (entry step — input is the goal)");
    let research_output = run_step(
        "researcher",
        "You are a researcher. Produce concise raw findings on the topic.",
        goal,
        RESEARCH_REPLY,
        researcher_calls.clone(),
    )
    .await?;
    println!("{research_output}\n");

    {
        let step = &mut state.steps[0];
        step.output = Some(research_output.clone());
        step.completed_at = Some(now_ts());
    }

    // Persist: step 1 done, step 2 still pending. This is the checkpoint a crash
    // would leave behind. Serialize the workflow state to JSON and store it in
    // `behavior_state` — the same convention the coordinator uses.
    let checkpoint_version = 1;
    let ckpt = AgentCheckpoint {
        version: checkpoint_version,
        agent_id: workflow_id.0.clone(),
        checkpoint_time: now_ts(),
        session_messages: Vec::new(),
        cumulative_token_usage: TokenUsageStats::default(),
        behavior_state: Some(serde_json::to_string(&state)?),
    };
    store.save(&ckpt).await?;
    let ckpt_path = checkpoint_dir
        .join(&workflow_id.0)
        .join(format!("{:016}.ckpt", checkpoint_version));
    println!("✔  Checkpoint written: {}", ckpt_path.display());
    println!(
        "   (researcher output captured, completed_at={}, summarizer still pending)\n",
        state.steps[0].completed_at.unwrap()
    );

    // ---- THE CRASH -------------------------------------------------------
    // Drop everything held in memory. After this line the program knows nothing
    // about the first run except what is on disk. We rebind `state` so the
    // compiler can't let us accidentally reach the old value.
    println!("💥 *** Simulated crash: dropping all in-memory workflow state ***");
    drop(state);
    // (The step actors were already stopped+awaited inside run_step; the only
    // survivor of the crash is the .ckpt file on disk.)
    println!("   In-memory workflow gone. Only the checkpoint file survives.\n");

    println!(
        "   Researcher model invocations so far: {}",
        researcher_calls.load(Ordering::SeqCst)
    );

    // =======================================================================
    // PHASE 2 — restart: reconstruct the workflow purely from disk and resume.
    // =======================================================================
    println!("{}", "=".repeat(70));
    println!("PHASE 2: restart — rebuild the workflow from the checkpoint and resume");
    println!("{}", "=".repeat(70));

    // A fresh process would do exactly this: open the store and load the latest
    // checkpoint. We have NO in-memory state — this is the only input.
    println!("\n↻  Loading latest checkpoint for '{}'...", workflow_id.0);
    let restored_ckpt = store
        .load_latest(&workflow_id)
        .await?
        .ok_or("no checkpoint found — cannot resume")?;
    println!(
        "   Restored checkpoint v{}, written at unix={}",
        restored_ckpt.version, restored_ckpt.checkpoint_time
    );

    let restored_json = restored_ckpt
        .behavior_state
        .ok_or("checkpoint has no workflow state")?;
    let mut state: WorkflowState = serde_json::from_str(&restored_json)?;

    // Report what the persisted state says about each step. This — not a flag in
    // code — is what decides the skip.
    println!("\n   Resumed workflow state (from disk):");
    for step in &state.steps {
        if step.is_done() {
            println!(
                "     • {:<11} DONE      (checkpointed at unix={})",
                step.agent_id,
                step.completed_at.unwrap_or(0)
            );
        } else {
            println!("     • {:<11} PENDING", step.agent_id);
        }
    }
    println!();

    // Walk the steps in order. A step whose persisted output `is_done()` is
    // SKIPPED — its agent is never spawned. A pending step runs, with its
    // upstream steps' persisted outputs assembled into its input.
    let total_steps = state.steps.len();
    for idx in 0..total_steps {
        if state.steps[idx].is_done() {
            println!(
                "⏭  Skipping step {}: {} — already completed, restored from checkpoint \
                 (model NOT invoked)",
                idx + 1,
                state.steps[idx].agent_id
            );
            continue;
        }

        // Pending step — assemble upstream context from the persisted outputs of
        // its dependencies. This is how the summarizer "resumes with upstream
        // context": the researcher's output came back from disk, not memory.
        let agent_id = state.steps[idx].agent_id.clone();
        let depends_on = state.steps[idx].depends_on.clone();
        let mut input_text = format!("Goal: {}\n\nUpstream results:\n", state.goal);
        for dep in &depends_on {
            let upstream = state
                .steps
                .iter()
                .find(|s| &s.agent_id == dep)
                .and_then(|s| s.output.clone())
                .ok_or_else(|| format!("dependency '{dep}' has no checkpointed output"))?;
            input_text.push_str(&format!("\n[from {dep}]\n{upstream}\n"));
        }

        println!(
            "▶  Resuming step {}: {} — fed upstream context from {:?} (loaded from checkpoint)",
            idx + 1,
            agent_id,
            depends_on
        );

        let output = run_step(
            "summarizer",
            "You are a summarizer. Condense the upstream findings into a short recommendation.",
            &input_text,
            SUMMARY_REPLY,
            summarizer_calls.clone(),
        )
        .await?;
        println!("{output}\n");

        // Persist progress again — now both steps are done. A crash here would
        // resume with nothing left to run.
        state.steps[idx].output = Some(output);
        state.steps[idx].completed_at = Some(now_ts());
        let next_version = restored_ckpt.version + 1;
        let ckpt = AgentCheckpoint {
            version: next_version,
            agent_id: workflow_id.0.clone(),
            checkpoint_time: now_ts(),
            session_messages: Vec::new(),
            cumulative_token_usage: TokenUsageStats::default(),
            behavior_state: Some(serde_json::to_string(&state)?),
        };
        store.save(&ckpt).await?;
        let p = checkpoint_dir
            .join(&workflow_id.0)
            .join(format!("{:016}.ckpt", next_version));
        println!(
            "✔  Checkpoint updated: {} (workflow complete)\n",
            p.display()
        );
    }

    // =======================================================================
    // Report — the verification the README and the issue acceptance call for.
    // =======================================================================
    println!("{}", "=".repeat(70));
    println!("RESULT");
    println!("{}", "=".repeat(70));
    let r = researcher_calls.load(Ordering::SeqCst);
    let s = summarizer_calls.load(Ordering::SeqCst);
    println!("  researcher model invocations : {r}  (1 in phase 1, 0 in phase 2 — NOT re-run)");
    println!("  summarizer model invocations : {s}  (0 in phase 1, 1 in phase 2 — resumed)");
    println!();
    assert_eq!(r, 1, "researcher must run exactly once across the crash");
    assert_eq!(s, 1, "summarizer must run exactly once, after the restart");
    println!("  ✔ Completed upstream step (researcher) was restored from disk and skipped.");
    println!(
        "  ✔ Downstream step (summarizer) resumed using the researcher's checkpointed output."
    );
    println!("  ✔ The skip was driven by the persisted checkpoint, not a hardcoded flag.");

    // -----------------------------------------------------------------------
    // Cleanup — remove the temp checkpoint dir.
    // -----------------------------------------------------------------------
    tokio::fs::remove_dir_all(&checkpoint_dir).await.ok();
    println!("\n  (cleaned up {})", checkpoint_dir.display());

    println!("\n=== Done ===");
    Ok(())
}
