//! Memory recall — the full memory stack: Tier 3 core memory, Tier 4 semantic
//! recall, and sleep-time consolidation.
//!
//! The `customer-support` example covers the bottom two tiers (Tier 1 session
//! transcript, Tier 2 checkpoint/crash recovery). This one covers the top two,
//! plus the background pass that connects them:
//!
//! ```text
//!   Tier 1  SessionMemory     in-process conversation transcript   (customer-support)
//!   Tier 2  CheckpointStore   crash-recovery snapshots             (customer-support)
//!   Tier 3  CoreMemoryStore   agent-edited curated blocks ◀─┐      THIS EXAMPLE
//!   Tier 4  SemanticMemory    vector recall of past turns ──┘ consolidation
//! ```
//!
//! The whole point of Tiers 3 + 4 is **cross-session** memory: tell the agent
//! something once, in one session, and a *different* session later recalls it —
//! with no orchestrator threading the state through by hand. Both tiers persist
//! to disk; a new actor reloads them on spawn. That is the entire mechanism.
//!
//! ## What runs, in three phases
//!
//! 1. **Store (session A).** The user states a durable preference. The agent
//!    calls `core_memory_set` to write it into its `human` block (Tier 3). The
//!    exchange is also persisted to Tier 4 for later semantic recall. Session A
//!    then stops.
//! 2. **Recall (session B — a brand-new actor).** A fresh agent spawns against
//!    the *same* memory dir. Tier 3 blocks reload straight into its system
//!    prompt, and Tier 4 surfaces the past exchange two ways: **passively**
//!    (top-k hits injected into the prompt before the turn) and **actively**
//!    (the agent calls `recall_search`). No state was passed in by hand. The
//!    user then states a project-level convention, which the agent does NOT
//!    curate inline — it's left as raw Tier-4 activity for the next phase.
//! 3. **Consolidate (sleep-time).** We trigger `on_consolidate` on the idle
//!    session-B agent: a memory-manager LLM pass that reads recent Tier-4
//!    activity and *promotes* durable facts up into the curated Tier-3 blocks.
//!    It promotes the convention from step 2 into the `project` block (the
//!    `human` preference is already curated, so it's left alone). Promotion-only
//!    — it never evicts Tier 4. We print the `project` block before and after.
//!
//! ## No network, no model download
//!
//! Tier 4 has two embedding backends behind one seam (see
//! `crates/axocoatl-memory/src/semantic.rs`): a neural one (`all-MiniLM-L6-v2`
//! via Candle, ~90 MB download on first use) and a pure-Rust lexical fallback
//! (signed feature hashing — meaning ≈ word overlap). This example defaults to
//! the **hashed** backend via `SemanticMemory::new_hashed`, so it runs fully
//! offline. Pass `--with-embeddings` to use the neural backend instead (it will
//! download the model on first run); the demo content is chosen to recall under
//! either backend.
//!
//! The LLM is mocked — one struct that inspects the request and emits the tool
//! call a real model would, so the example needs no API keys. In a real app the
//! provider is Ollama / OpenAI / Anthropic and the agent decides for itself when
//! to call `core_memory_set` / `recall_search`.
//!
//! Run: `cargo run -p memory-recall`  (offline, hashed Tier-4)
//!      `cargo run -p memory-recall -- --with-embeddings`  (neural Tier-4)

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ractor::Actor;
use tokio::sync::RwLock;
use tokio_stream::Stream;

use axocoatl_actor::{
    consolidate_agent, execute_agent, AgentActor, AgentBehavior, DefaultAgentBehavior,
};
use axocoatl_core::{
    AgentConfig, AgentId, AgentInput, ChatMessage, CoreMemoryConfig, MemoryConfig, MessageRole,
    OverflowPolicy, RecallConfig, TokenBudget, TokenUsageStats,
};
use axocoatl_llm::{
    ChatRequest, ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError,
    StreamEvent,
};
use axocoatl_memory::{build_store, CoreMemoryStore, DailyLogMemory, MemoryBlock, SemanticMemory};
use axocoatl_token::TokenCounter;

// ---------------------------------------------------------------------------
// Mock LLM — inspects the request and emits what a real model would.
//
// `DefaultAgentBehavior::execute` always streams (it calls `chat_stream`), so
// the mock must implement streaming for real: it emits the same `StreamEvent`s
// the daemon's accumulator expects (`ToolCallDelta` → `Usage` → `Done`). The
// non-streaming `chat` is only used by the consolidation pass, so we implement
// both paths.
//
// Decision logic (mirrors how a real model reacts to the conversation it sees):
//   - Last message is a Tool result → the tool ran; reply with a short confirmation.
//   - User states the dark-mode preference → call `core_memory_set` to record it.
//   - User asks about their editor preference → call `recall_search` to look it up.
//   - Otherwise → a plain text acknowledgement.
// ---------------------------------------------------------------------------

struct MockMemoryLlm {
    /// Distinct ids per emitted tool call, so the stream accumulator keys cleanly.
    next_call: AtomicUsize,
}

impl MockMemoryLlm {
    fn new() -> Self {
        Self {
            next_call: AtomicUsize::new(0),
        }
    }

    /// The single tool call this turn should make, if any — derived purely from
    /// the request the behavior built (system prompt + session history). Returns
    /// `(tool_name, arguments)`.
    fn planned_tool_call(
        &self,
        request: &ChatRequest,
    ) -> Option<(&'static str, serde_json::Value)> {
        let last = request.messages.last()?;

        // A tool result just came back — the agent's job for this turn is done.
        if matches!(last.role, MessageRole::Tool) {
            return None;
        }

        let user_text = last.text_content().unwrap_or("").to_lowercase();

        if user_text.contains("prefer") && user_text.contains("dark mode") {
            // A durable fact about the USER → the curated `human` block (Tier 3).
            return Some((
                "core_memory_set",
                serde_json::json!({
                    "block": "human",
                    "value": "Prefers dark mode editor themes; high contrast, easy on the eyes."
                }),
            ));
        }

        if user_text.contains("editor")
            && (user_text.contains("theme") || user_text.contains("prefer"))
        {
            // Look the editor preference up in long-term memory (Tier 4).
            return Some((
                "recall_search",
                serde_json::json!({ "query": "editor theme preference dark mode" }),
            ));
        }

        // A project-level convention stated in session B. We deliberately do NOT
        // call core_memory_set here — we let it flow into Tier 4 as a raw turn and
        // have the sleep-time consolidation pass promote it into `project` later
        // (Phase 3). That is the whole point of consolidation: the agent doesn't
        // have to curate every fact inline; the background pass tidies up.
        None
    }

    /// The assistant text for this turn (no tool call, or the follow-up after a
    /// tool result). Reads the last user/tool message to stay in character.
    fn planned_text(&self, request: &ChatRequest) -> String {
        if let Some(msg) = request.messages.last() {
            if matches!(msg.role, MessageRole::Tool) {
                let body = msg.text_content().unwrap_or("");
                // Confirmation after a core_memory_set (the result echoes the block).
                if body.contains("\"block\":\"human\"") || body.contains("\"block\": \"human\"") {
                    return "Got it — I've saved that you prefer dark, high-contrast editor \
                            themes. I'll remember it next time."
                        .to_string();
                }
                // Answer using whatever recall_search surfaced.
                if body.contains("dark mode") || body.contains("high contrast") {
                    return "Based on what you told me before, you prefer dark mode editor \
                            themes with high contrast — so I'll default the editor to a dark theme."
                        .to_string();
                }
                return "I checked my memory but didn't find anything specific on that yet."
                    .to_string();
            }

            // A plain user turn with no tool call — acknowledge a stated convention
            // so it reads naturally and lands in Tier 4 for later consolidation.
            let user_text = msg.text_content().unwrap_or("").to_lowercase();
            if user_text.contains("indent") || user_text.contains("spaces") {
                return "Noted — I'll keep that project convention in mind.".to_string();
            }
        }
        "Understood.".to_string()
    }

    fn usage() -> TokenUsageStats {
        TokenUsageStats::new(60, 30)
    }
}

#[async_trait::async_trait]
impl LlmProvider for MockMemoryLlm {
    fn provider_id(&self) -> &str {
        "mock-memory"
    }

    fn model_id(&self) -> &str {
        "mock-memory-v1"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            structured_output: false,
            vision: false,
            reasoning: false,
            embeddings: false,
            max_context_tokens: 32_000,
            max_output_tokens: 1_024,
        }
    }

    /// Non-streaming path. Used by the consolidation pass: it asks for a JSON
    /// edit array, so we return one that promotes the recalled preference into
    /// the `project` block. For any other (non-consolidation) call we fall back
    /// to the planned text.
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ProviderError> {
        let is_consolidation = request
            .messages
            .first()
            .and_then(|m| m.text_content())
            .map(|s| s.contains("memory manager"))
            .unwrap_or(false);

        let content = if is_consolidation {
            // The memory-manager prompt wants ONLY a JSON array of edits. A real
            // model reads the "Recent activity" the behavior passes in and decides
            // what durable facts to promote. We do the same: scan that activity and
            // promote the project-level indentation convention into `project` —
            // only when it's actually present. (The dark-mode preference is a USER
            // fact already curated in `human` during Phase 1, so consolidation
            // leaves it alone; that's why session A's graceful-stop pass is a no-op
            // for `project` and Phase 3 shows a real before→after.)
            let activity = request
                .messages
                .iter()
                .filter_map(|m| m.text_content())
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase();

            let edits = if activity.contains("indent") || activity.contains("spaces") {
                // `set` (idempotent) so re-running consolidation is stable.
                serde_json::json!([
                    {
                        "op": "set",
                        "block": "project",
                        "value": "Convention: use 2-space indentation throughout this project."
                    }
                ])
            } else {
                // Nothing project-durable to promote this pass.
                serde_json::json!([])
            };
            edits.to_string()
        } else {
            self.planned_text(&request)
        };

        Ok(ChatResponse {
            content,
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: Self::usage(),
            model: self.model_id().to_string(),
            provider: self.provider_id().to_string(),
        })
    }

    /// Streaming path — what `execute` actually calls. Emits a tool call (if the
    /// turn warrants one) or text, then a `Usage` event and `Done`, exactly like
    /// a real provider's normalized stream.
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        let mut events: Vec<Result<StreamEvent, ProviderError>> = Vec::new();

        if let Some((name, args)) = self.planned_tool_call(&request) {
            let seq = self.next_call.fetch_add(1, Ordering::Relaxed);
            // One delta carrying the whole call (a real stream may fragment the
            // args; the accumulator handles both — see `stream_chat`).
            events.push(Ok(StreamEvent::ToolCallDelta {
                index: Some(0),
                id: format!("call-{seq}"),
                name: Some(name.to_string()),
                args_delta: args.to_string(),
            }));
            events.push(Ok(StreamEvent::Usage(Self::usage())));
            events.push(Ok(StreamEvent::Done {
                finish_reason: FinishReason::ToolUse,
            }));
        } else {
            let text = self.planned_text(&request);
            events.push(Ok(StreamEvent::TextDelta { delta: text }));
            events.push(Ok(StreamEvent::Usage(Self::usage())));
            events.push(Ok(StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            }));
        }

        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

// ---------------------------------------------------------------------------
// Simple token counter (examples use this; real apps use TiktokenCounter).
// ---------------------------------------------------------------------------

struct SimpleCounter;

impl TokenCounter for SimpleCounter {
    fn count_text(&self, text: &str) -> usize {
        text.len() / 4 + 1
    }

    fn count_messages(&self, messages: &[ChatMessage]) -> usize {
        messages
            .iter()
            .map(|m| m.text_content().map_or(1, |t| self.count_text(t)))
            .sum()
    }

    fn count_tool_definition(&self, tool_json: &serde_json::Value) -> usize {
        self.count_text(&tool_json.to_string())
    }
}

// ---------------------------------------------------------------------------
// Memory stores. Both Tier 3 and Tier 4 are file-backed under one data dir, so
// a freshly-spawned actor pointed at the same dir reloads them — that's how
// "session B" inherits everything "session A" wrote.
// ---------------------------------------------------------------------------

const AGENT_ID: &str = "memory-agent";

/// The agent's config — three default core blocks (persona/human/project) and
/// default recall tuning, surfaced here so the example can print the knobs.
fn agent_config(recall: RecallConfig) -> AgentConfig {
    AgentConfig {
        id: AgentId::new(AGENT_ID),
        name: "Memory Agent".to_string(),
        provider: "mock-memory".to_string(),
        model: "mock-memory-v1".to_string(),
        system_prompt: Some(
            "You are a personal assistant with long-term memory. When the user tells you a \
             durable preference, record it with core_memory_set. When they ask about something \
             you don't see in this conversation, use recall_search before answering."
                .to_string(),
        ),
        token_budget: Some(TokenBudget {
            per_call: 2_048,
            per_execution: 20_000,
            overflow_policy: OverflowPolicy::Warn,
        }),
        tools: vec![],
        memory: MemoryConfig {
            recall,
            core: CoreMemoryConfig::default(),
            ..MemoryConfig::default()
        },
        ..AgentConfig::default()
    }
}

/// Build a `DefaultAgentBehavior` wired with all of Tier 3 (core memory) and
/// Tier 4 (semantic + daily log). Each call builds fresh stores pointed at the
/// shared `data_dir`, so a new actor reloads whatever a prior one persisted.
async fn build_behavior(
    data_dir: &std::path::Path,
    counter: Arc<dyn TokenCounter>,
    with_embeddings: bool,
) -> Result<DefaultAgentBehavior, Box<dyn std::error::Error>> {
    let provider: Arc<dyn LlmProvider> = Arc::new(MockMemoryLlm::new());

    // Tier 3 — core memory blocks (persona/human/project), seeded from config
    // defaults but kept across reloads (`build_store` won't clobber curated
    // values). No shared blocks in this single-agent example.
    let core_path = core_path(data_dir);
    let specs: Vec<MemoryBlock> = CoreMemoryConfig::default()
        .blocks
        .iter()
        .map(MemoryBlock::from)
        .collect();
    let core_store = build_store(AGENT_ID, &core_path, &specs).await;
    let core_store = Arc::new(RwLock::new(core_store));

    // Tier 4 — semantic store. `new_hashed` keeps it offline (no model download);
    // `new` uses the neural backend (downloads ~90 MB on first run) when asked.
    let semantic = open_semantic(data_dir, with_embeddings)?;

    // Tier 2 daily log — also powers the `recall_timeframe` tool.
    let daily_log = Arc::new(DailyLogMemory::new(AGENT_ID, data_dir.join("daily")));

    let behavior = DefaultAgentBehavior::new(provider, counter)
        .with_core_memory(core_store, std::collections::HashMap::new())
        .with_semantic_memory(Arc::new(semantic))
        .with_daily_log(daily_log);

    Ok(behavior)
}

fn core_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("core").join(format!("{AGENT_ID}.json"))
}

fn open_semantic(
    data_dir: &std::path::Path,
    with_embeddings: bool,
) -> Result<SemanticMemory, Box<dyn std::error::Error>> {
    let dir = data_dir.join("semantic");
    let mem = if with_embeddings {
        SemanticMemory::new(AGENT_ID, dir)?
    } else {
        SemanticMemory::new_hashed(AGENT_ID, dir)?
    };
    Ok(mem)
}

/// Read and render the agent's persisted Tier-3 blocks straight from disk — the
/// before/after snapshots the phases print. Reading from disk (not from a live
/// behavior) proves the state is genuinely persisted, not held in memory.
async fn snapshot_core_memory(data_dir: &std::path::Path) -> String {
    let mut store = CoreMemoryStore::new(AGENT_ID, core_path(data_dir));
    let _ = store.load().await;
    let rendered = store.as_context_string();
    if rendered.is_empty() {
        "(no core memory on disk yet)".to_string()
    } else {
        rendered
    }
}

/// Render just one block's value from disk (for tight before/after framing).
async fn snapshot_block(data_dir: &std::path::Path, label: &str) -> String {
    let mut store = CoreMemoryStore::new(AGENT_ID, core_path(data_dir));
    let _ = store.load().await;
    match store.block(label) {
        Some(b) if !b.value.trim().is_empty() => format!("### {label}\n{}", b.value),
        Some(_) => format!("### {label}\n(empty)"),
        None => format!("(no `{label}` block)"),
    }
}

/// Read the Tier-4 store's size from disk (a new handle, so this reflects what's
/// persisted, not an in-process cache).
fn semantic_count(data_dir: &std::path::Path, with_embeddings: bool) -> usize {
    open_semantic(data_dir, with_embeddings)
        .map(|m| m.len())
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("warn")
        .with_target(false)
        .init();

    let with_embeddings = std::env::args().any(|a| a == "--with-embeddings");

    println!(
        "=== Axocoatl: Memory Recall (Tier 3 core memory + Tier 4 semantic + consolidation) ===\n"
    );
    println!(
        "Tier-4 backend: {}",
        if with_embeddings {
            "neural (all-MiniLM-L6-v2 via Candle — downloads ~90 MB on first run)"
        } else {
            "hashed lexical fallback (offline, no model download)"
        }
    );

    let recall = RecallConfig::default();
    println!(
        "memory.recall knobs: passive_inject={}, top_k={}, min_score={}\n",
        recall.passive_inject, recall.top_k, recall.min_score
    );

    let counter: Arc<dyn TokenCounter> = Arc::new(SimpleCounter);

    // A throwaway data dir for all four tiers' files. Cleaned up at the end.
    let data_dir =
        std::env::temp_dir().join(format!("axocoatl-memory-recall-{}", std::process::id()));
    tokio::fs::create_dir_all(&data_dir).await?;
    println!("Memory data dir: {}\n", data_dir.display());

    // =======================================================================
    // PHASE 1 — STORE (session A): the user states a durable preference.
    // =======================================================================
    println!("{}", "=".repeat(72));
    println!("PHASE 1 — STORE (session A)");
    println!("{}", "=".repeat(72));

    println!("\nBEFORE — core memory on disk:");
    println!("{}", snapshot_core_memory(&data_dir).await);
    println!(
        "BEFORE — Tier-4 semantic store: {} memories\n",
        semantic_count(&data_dir, with_embeddings)
    );

    let behavior_a = build_behavior(&data_dir, counter.clone(), with_embeddings).await?;
    let (agent_a, handle_a) = AgentActor::spawn(
        Some("memory-agent-A".to_string()),
        AgentActor,
        (
            agent_config(recall.clone()),
            Box::new(behavior_a) as Box<dyn AgentBehavior>,
        ),
    )
    .await?;

    let turn1 = "I prefer dark mode editor themes — they're easier on my eyes.";
    println!("[User → session A]: {turn1}");
    let out1 = execute_agent(&agent_a, AgentInput::text(turn1))
        .await
        .map_err(|e| format!("session A turn failed: {e}"))?;
    println!("[Agent]: {}", out1.content);
    println!(
        "  tool calls this turn: {:?}",
        out1.tool_calls
            .iter()
            .map(|tc| tc.tool_name.as_str())
            .collect::<Vec<_>>()
    );

    // Graceful stop runs post_stop → on_stop → one consolidation pass. Session
    // A's only durable fact (the dark-mode preference) is a USER fact already
    // curated in `human`, so this pass finds nothing project-level to promote and
    // leaves `project` empty — which is what lets Phase 3 show a real before→after.
    println!("\n*** session A ending (graceful stop also runs a consolidation pass) ***");
    agent_a.stop(None);
    handle_a.await?;

    println!("\nAFTER — `human` block on disk (Tier 3, written by core_memory_set):");
    println!("{}", snapshot_block(&data_dir, "human").await);
    println!(
        "\nAFTER — Tier-4 semantic store: {} memories (the exchange was persisted)\n",
        semantic_count(&data_dir, with_embeddings)
    );

    // =======================================================================
    // PHASE 2 — RECALL (session B): a brand-new actor inherits the memory.
    // =======================================================================
    println!("{}", "=".repeat(72));
    println!("PHASE 2 — RECALL (session B, a fresh actor — nothing passed in by hand)");
    println!("{}", "=".repeat(72));

    let behavior_b = build_behavior(&data_dir, counter.clone(), with_embeddings).await?;
    let (agent_b, handle_b) = AgentActor::spawn(
        Some("memory-agent-B".to_string()),
        AgentActor,
        (
            agent_config(recall.clone()),
            Box::new(behavior_b) as Box<dyn AgentBehavior>,
        ),
    )
    .await?;

    println!("\nThis fresh actor loaded Tier 3 straight into its prompt and can recall Tier 4:");
    println!("  • Tier 3 (always in prompt): the `human` block from session A.");
    println!(
        "  • Tier 4 passive injection: top-{} hits with score > {} injected before the turn.",
        recall.top_k, recall.min_score
    );
    println!("  • Tier 4 active: the agent may call `recall_search` itself.\n");

    let turn2 = "What editor theme should you use for me by default?";
    println!("[User → session B]: {turn2}");
    let out2 = execute_agent(&agent_b, AgentInput::text(turn2))
        .await
        .map_err(|e| format!("session B turn failed: {e}"))?;
    println!("[Agent]: {}", out2.content);
    let recall_calls: Vec<&str> = out2
        .tool_calls
        .iter()
        .map(|tc| tc.tool_name.as_str())
        .collect();
    println!("  tool calls this turn: {recall_calls:?}");
    if let Some(rc) = out2
        .tool_calls
        .iter()
        .find(|tc| tc.tool_name == "recall_search")
    {
        if let Some(result) = &rc.result {
            let count = result.get("count").and_then(|c| c.as_u64()).unwrap_or(0);
            println!("  recall_search returned {count} hit(s) from past sessions:");
            if let Some(hits) = result.get("results").and_then(|r| r.as_array()) {
                for h in hits {
                    let text = h.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    let score = h.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0);
                    println!("    - (score {score:.3}) {}", text.replace('\n', " "));
                }
            }
        }
    }

    // A second turn in session B: the user states a PROJECT-level convention.
    // The agent just acknowledges it — it does NOT curate it inline. The turn
    // lands in Tier 4 as raw activity, and Phase 3's consolidation will be what
    // promotes it up into the `project` block.
    let turn3 = "For this project, always use 2-space indentation, never tabs.";
    println!("\n[User → session B]: {turn3}");
    let out3 = execute_agent(&agent_b, AgentInput::text(turn3))
        .await
        .map_err(|e| format!("session B turn failed: {e}"))?;
    println!("[Agent]: {}", out3.content);
    println!(
        "  tool calls this turn: {:?} (none — left for consolidation to promote)",
        out3.tool_calls
            .iter()
            .map(|tc| tc.tool_name.as_str())
            .collect::<Vec<_>>()
    );

    // =======================================================================
    // PHASE 3 — CONSOLIDATE (sleep-time): promote durable facts into Tier 3.
    // =======================================================================
    println!("\n{}", "=".repeat(72));
    println!("PHASE 3 — CONSOLIDATE (sleep-time memory-manager pass)");
    println!("{}", "=".repeat(72));

    println!("\nBEFORE — `project` block on disk:");
    println!("{}\n", snapshot_block(&data_dir, "project").await);

    // `consolidate_agent` only runs the LLM pass if the actor has been idle for
    // at least `idle_threshold_secs`. The daemon uses a real interval; here we
    // pass 0 to force the pass now (the actor still gates it — this is the exact
    // same path the background loop drives).
    println!("Triggering on_consolidate (idle threshold 0s = run now)...");
    let report = consolidate_agent(&agent_b, 0)
        .await
        .map_err(|e| format!("consolidation failed: {e}"))?;
    println!(
        "Consolidation report: skipped={}, promoted={}, rewritten={}, blocks_touched={:?}, tokens_used={}",
        report.skipped, report.promoted, report.rewritten, report.blocks_touched, report.tokens_used
    );

    println!("\nAFTER — `project` block on disk (durable fact promoted up from Tier 4):");
    println!("{}", snapshot_block(&data_dir, "project").await);

    println!("\n*** session B ending ***");
    agent_b.stop(None);
    handle_b.await?;

    // =======================================================================
    // Report.
    // =======================================================================
    println!("\n{}", "─".repeat(72));
    println!("\nWhat just happened, across the four tiers:");
    println!(
        "  Tier 1  session transcript  — each actor's in-process turn history (drained on stop)"
    );
    println!("  Tier 2  daily log           — raw activity, queryable via recall_timeframe");
    println!("  Tier 3  core memory         — `human` set in A, read in B; `project` grown by consolidation");
    println!("  Tier 4  semantic store      — exchange stored in A; recalled in B (passive + recall_search)");
    println!(
        "\nFinal Tier-4 size: {} memories. Final Tier-3 state:\n{}",
        semantic_count(&data_dir, with_embeddings),
        snapshot_core_memory(&data_dir).await
    );

    // Clean up the throwaway data dir.
    tokio::fs::remove_dir_all(&data_dir).await.ok();

    println!("\n=== Done ===");
    Ok(())
}
