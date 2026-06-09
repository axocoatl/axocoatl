//! Default agent behavior that wires an LLM provider + token tracking + checkpointing.
//! This is the standard "call the LLM" behavior most agents will use.

use std::sync::Arc;

use axocoatl_core::{
    AgentConfig, AgentInput, AgentOutput, ChatMessage, MessageRole, OverflowPolicy, TokenUsageStats,
};
use axocoatl_llm::{ChatRequest, LlmProvider, ToolCall};
use axocoatl_memory::{AgentCheckpoint, CheckpointStore, SessionMemory};
use axocoatl_token::{BudgetError, TokenCounter, TokenTracker};
use axocoatl_tools::{ConcurrentToolDispatcher, HookRegistry, ToolExecutor};

use crate::behavior::AgentBehavior;
use crate::error::AgentError;

/// Default behavior: builds ChatRequest from input, calls LLM provider, tracks tokens,
/// maintains session memory, executes tool calls, and optionally checkpoints.
pub struct DefaultAgentBehavior {
    provider: Arc<dyn LlmProvider>,
    tracker: Option<TokenTracker>,
    counter: Arc<dyn TokenCounter>,
    system_prompt: Option<String>,
    /// The agent's configured model (from the YAML `model` field). Sent as the
    /// per-request model so a shared provider (OpenAI and OpenAI-compatible
    /// servers like MLX/oMLX, vLLM, etc.) uses this agent's model instead of
    /// the provider's hardcoded default. `None` falls back to that default.
    /// Ollama bakes the model into its per-agent provider, so this is
    /// redundant-but-harmless there.
    configured_model: Option<String>,
    session: SessionMemory,
    checkpoint_store: Option<Arc<CheckpointStore>>,
    checkpoint_version: u64,
    agent_id: String,
    tool_executor: Option<Arc<ToolExecutor>>,
    hook_registry: Option<Arc<HookRegistry>>,
    compression_pipeline: Option<axocoatl_token::CompressionPipeline>,
    /// Append-only daily log. Context compaction archives raw conversation
    /// segments here *before* summarizing, so nothing is lost. When unset, the
    /// archived segments are dropped (with a warning).
    daily_log: Option<Arc<axocoatl_memory::DailyLogMemory>>,
    /// Core memory (Tier 3) — agent-editable, curated blocks rendered into the
    /// system prompt and maintained via the core-memory tools. The curated top of
    /// the hierarchy; the lossless raw lives in Tier 2 (daily log) + Tier 4.
    core_memory: Option<Arc<tokio::sync::RwLock<axocoatl_memory::CoreMemoryStore>>>,
    /// Shared core-memory blocks this agent may read/edit (label → cross-agent handle).
    shared_blocks: std::collections::HashMap<String, axocoatl_memory::SharedBlock>,
    /// Agent-scoped core-memory edit tools (append / replace / set), built in `on_start`.
    core_memory_tools: Vec<(String, Arc<dyn axocoatl_tools::BuiltinTool>)>,
    /// Standing system-prompt line telling the agent its core-memory blocks exist
    /// and to keep them current. Set when a core-memory store is attached.
    core_capability_hint: Option<String>,
    /// Semantic memory (Tier 4) — vector recall of past exchanges. Internally
    /// synchronized, so a plain `Arc` is enough.
    semantic_memory: Option<Arc<axocoatl_memory::SemanticMemory>>,
    /// Semantically-retrieved context for the current turn (set in `execute`).
    semantic_context: String,
    /// Agent-scoped recall tools (`recall_search` / `recall_timeframe`), built
    /// from this agent's own memory stores. Held here — not on the shared
    /// `ToolExecutor` — because a recall tool must reach a *specific* agent's
    /// per-agent memory, which a shared executor can't provide.
    recall_tools: Vec<(String, Arc<dyn axocoatl_tools::BuiltinTool>)>,
    /// Standing system-prompt line telling the agent the recall tools exist and
    /// when to use them. Set when at least one recall tool is available.
    recall_capability_hint: Option<String>,
    /// Single (non-accumulating) "topics now searchable via recall" hint,
    /// overwritten on each context compaction.
    recall_toc_hint: Option<String>,
    /// Recall tuning (from `MemoryConfig::recall`), read in `on_start`.
    passive_inject: bool,
    recall_top_k: usize,
    recall_min_score: f32,
    /// Directory-session context — when the agent runs inside a session, this
    /// preamble tells it which working directory it operates in.
    session_context: Option<String>,
    /// Project-scoped instructions composed from `AXOCOATL.md` files found
    /// along the path from the filesystem root down to `working_dir`. Treated
    /// as authoritative team knowledge — shared/versioned in the repo, distinct
    /// from the personal `long_term_memory` and `semantic_memory` which are
    /// per-user.
    project_instructions: Option<String>,
    /// Set by the actor before a streaming execution — receives output chunks
    /// as the LLM generates them.
    stream_sink: Option<crate::behavior::StreamSink>,
}

impl DefaultAgentBehavior {
    pub fn new(provider: Arc<dyn LlmProvider>, counter: Arc<dyn TokenCounter>) -> Self {
        let model_context = provider.capabilities().max_context_tokens;
        let pipeline = if model_context > 0 {
            Some(axocoatl_token::CompressionPipeline::new(
                counter.clone(),
                model_context,
            ))
        } else {
            None
        };
        Self {
            provider,
            tracker: None,
            counter,
            system_prompt: None,
            configured_model: None,
            session: SessionMemory::new(),
            checkpoint_store: None,
            checkpoint_version: 0,
            agent_id: String::new(),
            tool_executor: None,
            hook_registry: None,
            compression_pipeline: pipeline,
            daily_log: None,
            core_memory: None,
            shared_blocks: std::collections::HashMap::new(),
            core_memory_tools: Vec::new(),
            core_capability_hint: None,
            semantic_memory: None,
            semantic_context: String::new(),
            recall_tools: Vec::new(),
            recall_capability_hint: None,
            recall_toc_hint: None,
            passive_inject: true,
            recall_top_k: 5,
            recall_min_score: 0.15,
            session_context: None,
            project_instructions: None,
            stream_sink: None,
        }
    }

    /// Consume the provider's token stream — forwarding each text/reasoning
    /// delta to the stream sink (if attached) — and assemble the equivalent
    /// `ChatResponse`. Used in place of the blocking `provider.chat()` so
    /// every agent call is live by default.
    async fn stream_chat(
        &self,
        request: ChatRequest,
    ) -> Result<axocoatl_llm::ChatResponse, AgentError> {
        use axocoatl_llm::{ChatResponse, FinishReason, StreamEvent, ToolCall};
        use tokio_stream::StreamExt;

        let mut stream = self
            .provider
            .chat_stream(request)
            .await
            .map_err(|e| AgentError::Provider(e.to_string()))?;

        let mut content = String::new();
        let mut usage = TokenUsageStats::default();
        let mut finish_reason = FinishReason::Stop;
        // Tool calls arrive as deltas. OpenAI-compatible providers send the id
        // only on the first chunk and key later argument fragments by `index`,
        // so we correlate by index when present and fall back to a non-empty id
        // (Anthropic repeats the id on every delta and omits an index).
        struct ToolAccum {
            index: Option<usize>,
            id: String,
            name: String,
            args: String,
        }
        let mut tool_accum: Vec<ToolAccum> = Vec::new();

        while let Some(ev) = stream.next().await {
            match ev.map_err(|e| AgentError::Provider(e.to_string()))? {
                StreamEvent::TextDelta { delta } => {
                    if let Some(sink) = &self.stream_sink {
                        let _ = sink.send(crate::behavior::AgentStreamChunk::Text(delta.clone()));
                    }
                    content.push_str(&delta);
                }
                StreamEvent::ReasoningDelta { delta } => {
                    if let Some(sink) = &self.stream_sink {
                        let _ = sink.send(crate::behavior::AgentStreamChunk::Reasoning(delta));
                    }
                }
                StreamEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    args_delta,
                } => {
                    let pos = tool_accum.iter().position(|t| match (t.index, index) {
                        (Some(a), Some(b)) => a == b,
                        _ => !id.is_empty() && t.id == id,
                    });
                    match pos {
                        Some(i) => {
                            let t = &mut tool_accum[i];
                            if t.id.is_empty() && !id.is_empty() {
                                t.id = id;
                            }
                            if let Some(n) = name {
                                if !n.is_empty() {
                                    t.name = n;
                                }
                            }
                            t.args.push_str(&args_delta);
                        }
                        None => tool_accum.push(ToolAccum {
                            index,
                            id,
                            name: name.unwrap_or_default(),
                            args: args_delta,
                        }),
                    }
                }
                StreamEvent::Usage(u) => usage = u,
                StreamEvent::Done { finish_reason: fr } => finish_reason = fr,
            }
        }

        let tool_calls = tool_accum
            .into_iter()
            .map(|t| ToolCall {
                id: t.id,
                name: t.name,
                arguments: serde_json::from_str(&t.args).unwrap_or_else(|_| serde_json::json!({})),
            })
            .collect();

        Ok(ChatResponse {
            content,
            tool_calls,
            finish_reason,
            usage,
            model: String::new(),
            provider: self.provider.provider_id().to_string(),
        })
    }

    /// Enable checkpointing with a shared checkpoint store.
    pub fn with_checkpoint_store(mut self, store: Arc<CheckpointStore>) -> Self {
        self.checkpoint_store = Some(store);
        self
    }

    /// Enable tool execution with a shared tool executor.
    pub fn with_tool_executor(mut self, executor: Arc<ToolExecutor>) -> Self {
        self.tool_executor = Some(executor);
        self
    }

    /// Enable hook-based tool execution hooks.
    pub fn with_hook_registry(mut self, registry: Arc<HookRegistry>) -> Self {
        self.hook_registry = Some(registry);
        self
    }

    /// Provide the append-only daily log used to archive raw conversation
    /// segments before context compaction summarizes them.
    pub fn with_daily_log(mut self, log: Arc<axocoatl_memory::DailyLogMemory>) -> Self {
        self.daily_log = Some(log);
        self
    }

    /// Attach this agent's core memory (Tier 3): its per-agent block store plus
    /// any shared blocks it may edit. Rendered into the prompt and maintained via
    /// the core-memory tools (built in `on_start`).
    pub fn with_core_memory(
        mut self,
        store: Arc<tokio::sync::RwLock<axocoatl_memory::CoreMemoryStore>>,
        shared: std::collections::HashMap<String, axocoatl_memory::SharedBlock>,
    ) -> Self {
        self.core_memory = Some(store);
        self.shared_blocks = shared;
        self
    }

    /// Enable semantic memory (Tier 4) — relevant past exchanges are retrieved
    /// by vector similarity and injected into the system prompt each turn, and
    /// each new exchange is stored for future cross-session recall.
    pub fn with_semantic_memory(mut self, memory: Arc<axocoatl_memory::SemanticMemory>) -> Self {
        self.semantic_memory = Some(memory);
        self
    }

    /// Bind this agent to a directory session — injects a working-directory
    /// preamble into the system prompt so the model knows its scope.
    pub fn with_session_context(mut self, working_dir: impl std::fmt::Display) -> Self {
        self.session_context = Some(format!(
            "You are working inside a directory session. Your working \
             directory is `{working_dir}`. All file and shell tools operate \
             inside a sandboxed container with that directory mounted — you \
             cannot reach anything outside it."
        ));
        self
    }

    /// Load project-level instructions from `AXOCOATL.md` files. Walks from
    /// the filesystem root down to `working_dir`, reading every `AXOCOATL.md`
    /// it finds (root-most first, working-dir-most last — so deeper, more
    /// specific files appear later and can override broader org-wide ones).
    ///
    /// This is the shared/versioned "team knowledge" layer — distinct from
    /// the per-user `long_term_memory` and `semantic_memory`. A file edit
    /// takes effect on the next actor spawn (session reopen).
    pub fn with_project_instructions(mut self, working_dir: &std::path::Path) -> Self {
        let mut chunks: Vec<(std::path::PathBuf, String)> = Vec::new();
        // Walk root → working_dir so the deepest file lands last.
        let mut ancestors: Vec<&std::path::Path> = working_dir.ancestors().collect();
        ancestors.reverse();
        for dir in ancestors {
            let candidate = dir.join("AXOCOATL.md");
            if let Ok(text) = std::fs::read_to_string(&candidate) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    chunks.push((candidate, trimmed.to_string()));
                }
            }
        }
        if chunks.is_empty() {
            self.project_instructions = None;
        } else {
            let mut composed = String::from(
                "Project-level instructions from `AXOCOATL.md` files in this \
                 repository (root → leaf). Treat these as authoritative team \
                 knowledge for working in this codebase:\n\n",
            );
            for (path, body) in &chunks {
                composed.push_str(&format!("--- from `{}` ---\n", path.display()));
                composed.push_str(body);
                composed.push_str("\n\n");
            }
            self.project_instructions = Some(composed);
        }
        self
    }

    /// Combined memory context for the current turn, ready to append to the
    /// system prompt. Composition order matters — earlier items frame the
    /// later ones:
    ///   1. session preamble (where the agent is)
    ///   2. project instructions from `AXOCOATL.md` (team-shared knowledge)
    ///   3. long-term facts (per-user Tier 3)
    ///   4. semantic recall (per-user Tier 4, retrieved for this turn)
    fn memory_context(&self) -> String {
        let mut parts = Vec::new();
        if let Some(sc) = &self.session_context {
            parts.push(sc.clone());
        }
        if let Some(pi) = &self.project_instructions {
            parts.push(pi.clone());
        }
        let core = self.core_memory_context();
        if !core.is_empty() {
            parts.push(core);
        }
        if !self.semantic_context.is_empty() {
            parts.push(self.semantic_context.clone());
        }
        // After semantic recall: post-compaction "what's recallable" topics, then
        // the standing capability hints last so they frame "if what you need isn't
        // above, search for it / curate it".
        if let Some(toc) = &self.recall_toc_hint {
            parts.push(toc.clone());
        }
        if let Some(hint) = &self.recall_capability_hint {
            parts.push(hint.clone());
        }
        if let Some(hint) = &self.core_capability_hint {
            parts.push(hint.clone());
        }
        parts.join("\n\n")
    }

    /// Forward a chunk to the streaming sink, if one is attached.
    fn emit_stream(&self, chunk: crate::behavior::AgentStreamChunk) {
        if let Some(sink) = &self.stream_sink {
            let _ = sink.send(chunk);
        }
    }

    /// Retrieve semantically-relevant past memories for `query`. Best-effort:
    /// a search failure logs and yields no context rather than failing the turn.
    fn retrieve_semantic_context(&self, query: &str) -> String {
        if !self.passive_inject {
            return String::new();
        }
        let Some(mem) = &self.semantic_memory else {
            return String::new();
        };
        match mem.search(query, self.recall_top_k) {
            Ok(hits) => {
                let relevant: Vec<String> = hits
                    .into_iter()
                    .filter(|h| h.score > self.recall_min_score)
                    .map(|h| format!("- {}", h.text.replace('\n', " ")))
                    .collect();
                if relevant.is_empty() {
                    String::new()
                } else {
                    format!(
                        "## Relevant memory from past sessions\n{}",
                        relevant.join("\n")
                    )
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "semantic search failed — skipping");
                String::new()
            }
        }
    }

    /// Get read access to session memory.
    pub fn session(&self) -> &SessionMemory {
        &self.session
    }

    /// Render core memory (Tier 3) for the system prompt — the agent's local
    /// blocks plus any shared blocks, under one `## Core Memory` header. Uses
    /// `try_read` so lock contention (e.g. a concurrent save) skips this turn
    /// rather than blocking the LLM call.
    fn core_memory_context(&self) -> String {
        let mut blocks: Vec<axocoatl_memory::MemoryBlock> = Vec::new();
        if let Some(store) = &self.core_memory {
            if let Ok(s) = store.try_read() {
                blocks.extend(s.blocks().iter().cloned());
            }
        }
        for shared in self.shared_blocks.values() {
            if let Ok(b) = shared.block.try_read() {
                blocks.push(b.clone());
            }
        }
        axocoatl_memory::render_blocks(blocks.iter())
    }

    /// Get tool definitions from the executor (if any) for sending to the LLM.
    fn tool_definitions(&self) -> Vec<axocoatl_llm::ToolDefinition> {
        let mut defs = self
            .tool_executor
            .as_ref()
            .map(|exec| exec.as_llm_tools())
            .unwrap_or_default();
        // Agent-scoped recall tools are advertised alongside the executor's.
        // The set is deterministic per agent, so the tool list is stable turn to
        // turn. They're read-only, hence `Safe`.
        for (name, tool) in &self.recall_tools {
            defs.push(axocoatl_llm::ToolDefinition {
                name: name.clone(),
                description: tool.description().to_string(),
                parameters: tool.parameters_schema(),
                concurrency: axocoatl_llm::ConcurrencyPolicy::Safe,
            });
        }
        // Core-memory edit tools — mutating, so advertised Exclusive.
        for (name, tool) in &self.core_memory_tools {
            defs.push(axocoatl_llm::ToolDefinition {
                name: name.clone(),
                description: tool.description().to_string(),
                parameters: tool.parameters_schema(),
                concurrency: axocoatl_llm::ConcurrencyPolicy::Exclusive,
            });
        }
        defs
    }

    /// True when the model emitted a call to one of this agent's recall tools.
    fn is_recall_tool(&self, name: &str) -> bool {
        self.recall_tools.iter().any(|(n, _)| n == name)
    }

    /// Execute an agent-scoped recall tool by name (the behavior owns these,
    /// since they reach this agent's per-agent memory). Unknown name → `NotFound`.
    async fn execute_recall_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, axocoatl_tools::ToolError> {
        match self.recall_tools.iter().find(|(n, _)| n == name) {
            Some((_, tool)) => tool.execute(arguments).await,
            None => Err(axocoatl_tools::ToolError::NotFound(name.to_string())),
        }
    }

    /// True when the model emitted a call to one of this agent's core-memory tools.
    fn is_core_memory_tool(&self, name: &str) -> bool {
        self.core_memory_tools.iter().any(|(n, _)| n == name)
    }

    async fn execute_core_memory_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, axocoatl_tools::ToolError> {
        match self.core_memory_tools.iter().find(|(n, _)| n == name) {
            Some((_, tool)) => tool.execute(arguments).await,
            None => Err(axocoatl_tools::ToolError::NotFound(name.to_string())),
        }
    }

    /// Any agent-scoped tool the behavior services itself (recall + core memory),
    /// rather than the shared executor.
    fn is_behavior_tool(&self, name: &str) -> bool {
        self.is_recall_tool(name) || self.is_core_memory_tool(name)
    }

    async fn execute_behavior_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, axocoatl_tools::ToolError> {
        if self.is_recall_tool(name) {
            self.execute_recall_tool(name, arguments).await
        } else {
            self.execute_core_memory_tool(name, arguments).await
        }
    }

    /// Build a ChatRequest from an AgentInput + optional system prompt.
    /// Used by tests and callers that manage their own history externally.
    #[cfg(test)]
    fn build_request(&self, input: &AgentInput) -> ChatRequest {
        let mut messages = Vec::new();

        // Add system prompt if configured
        if let Some(sys) = &self.system_prompt {
            messages.push(ChatMessage::system(sys));
        }

        // Add conversation history
        for msg in &input.history {
            messages.push(msg.clone());
        }

        // Add current user input
        messages.push(ChatMessage::user(&input.content));

        ChatRequest {
            messages,
            tools: self.tool_definitions(),
            max_tokens: None,
            temperature: None,
            stop_sequences: Vec::new(),
            provider_options: None,
            model_override: input.model_override.clone(),
        }
    }

    /// Build a ChatRequest from the current session memory.
    /// Includes system prompt + long-term memory context + full session history.
    /// `system_override` replaces the agent's configured system_prompt for
    /// this single call when `Some` — memory context still merges as usual.
    /// `model_override` swaps the model on the configured provider (same
    /// provider, same credentials — model name only).
    fn build_request_from_session(
        &self,
        system_override: Option<&str>,
        model_override: Option<String>,
    ) -> ChatRequest {
        let mut messages = Vec::new();

        // System prompt — overridden, agent-default, or none — then optionally
        // augmented with memory context (Tier 3 long-term + Tier 4 semantic).
        let mem_context = self.memory_context();
        let effective_system: Option<&str> = system_override.or(self.system_prompt.as_deref());
        match effective_system {
            Some(sys) if mem_context.is_empty() => {
                messages.push(ChatMessage::system(sys));
            }
            Some(sys) => {
                messages.push(ChatMessage::system(format!("{sys}\n\n{mem_context}")));
            }
            None if !mem_context.is_empty() => {
                messages.push(ChatMessage::system(&mem_context));
            }
            None => {}
        }

        messages.extend(self.session.as_chat_messages());

        // Check if compression is needed (stages 1-2 only, pure computation)
        if let Some(pipeline) = &self.compression_pipeline {
            if pipeline.needs_compression(&messages) {
                tracing::info!(
                    tokens = self.counter.count_messages(&messages),
                    "Context compression triggered (session follow-up)"
                );
                messages = pipeline.compress_sync(messages);
            }
        }

        ChatRequest {
            messages,
            tools: self.tool_definitions(),
            max_tokens: None,
            temperature: None,
            stop_sequences: Vec::new(),
            provider_options: None,
            // Per-request override (e.g. Chat tab) wins; otherwise use the
            // agent's configured model so shared providers don't fall back to
            // their hardcoded default.
            model_override: model_override.or_else(|| self.configured_model.clone()),
        }
    }

    /// Persistently compact the session toward `target_threshold` tokens: archive
    /// the raw transcript to the daily log (so nothing is lost), run the
    /// compression pipeline (summarizing old turns via the LLM), and replace the
    /// session with the compacted messages. No-op when already under the target
    /// or no pipeline is configured. Best-effort — never fails the turn.
    async fn compact_session(&mut self, target_threshold: usize) {
        if self.compression_pipeline.is_none() || self.session.total_tokens() <= target_threshold {
            return;
        }

        // Archive the raw transcript BEFORE compacting, so no history is lost
        // regardless of which pipeline stage drops or summarizes it.
        if let Some(daily_log) = &self.daily_log {
            let raw: Vec<_> = self
                .session
                .as_chat_messages()
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "role": format!("{:?}", m.role),
                        "content": m.text_content().unwrap_or(""),
                    })
                })
                .collect();
            let entry = axocoatl_memory::LogEntry {
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                entry_type: axocoatl_memory::LogEntryType::Conversation,
                content: serde_json::json!({
                    "reason": "context_compaction",
                    "target_threshold": target_threshold,
                    "messages": raw,
                }),
            };
            if let Err(e) = daily_log.append(entry).await {
                tracing::warn!(agent = %self.agent_id, error = %e, "failed to archive transcript before compaction");
            }
        } else {
            tracing::warn!(agent = %self.agent_id, "no daily log configured — compacted history will not be archived");
        }

        // Housekeeping budget for the LLM summarization stages: a slice of the
        // remaining token budget, or generous when there is no budget (pure
        // context-window compaction).
        let housekeeping = self
            .tracker
            .as_ref()
            .map(|t| {
                let remaining = t.budget().per_execution.saturating_sub(t.total_used());
                (remaining as f32 * axocoatl_token::HOUSEKEEPING_BUDGET_PCT) as usize
            })
            .unwrap_or(usize::MAX / 4);

        let messages = self.session.as_chat_messages();
        let summarizer =
            crate::summarizer::LlmSummarizer::new(self.provider.clone(), self.tracker.clone());
        let result = self
            .compression_pipeline
            .as_ref()
            .unwrap()
            .compress_to(messages, Some(&summarizer), housekeeping, target_threshold)
            .await;

        let counter = self.counter.clone();
        self.session
            .replace_with_chat_messages(&result.messages, |s| counter.count_text(s));

        // Single, overwritten-each-compaction hint pointing the agent at the
        // summary it can now see and telling it the detail behind it is
        // searchable. Only when recall is actually available.
        self.recall_toc_hint = if self.semantic_memory.is_some() {
            Some(
                "## Earlier context\nOlder turns in this conversation were summarized above to \
                 save space; their full detail lives in long-term memory. Use `recall_search` to \
                 retrieve specifics that aren't in the summary."
                    .to_string(),
            )
        } else {
            None
        };

        tracing::info!(
            agent = %self.agent_id,
            tokens_before = result.tokens_before,
            tokens_after = result.tokens_after,
            stages = ?result.stages_applied,
            "Compacted session context"
        );
    }
}

/// Convert a chat-turn's attachments into multimodal `Parts` and graft them
/// onto the last (user) message of `request`.
///
/// Routing rules:
/// - **Image with no extracted text** → base64 `data:` URL as `ContentPart::Image`
/// - **Image WITH ocr text** → base64 image AND OCR inlined as `<attachment>` text
///   (gives both the vision model and non-vision providers something useful)
/// - **Text-bearing file with extracted text** (PDF, CSV, XLSX, plain) →
///   inline the extracted text as `<attachment name="..">…</attachment>`. The
///   raw bytes are NOT sent — extraction already produced what the LLM needs.
/// - **Anything else** → log + skip (we can't help an unrecognized binary).
fn attach_to_last_user_message(
    request: &mut ChatRequest,
    attachments: &[axocoatl_core::AgentAttachment],
) {
    use axocoatl_core::{ContentPart, ImageDetail, MessageContent};
    use base64::{engine::general_purpose::STANDARD as B64, Engine};

    let idx = request
        .messages
        .iter()
        .rposition(|m| matches!(m.role, axocoatl_core::MessageRole::User));
    let Some(idx) = idx else {
        return;
    };

    let original_text = match &request.messages[idx].content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    };

    let mut image_parts: Vec<ContentPart> = Vec::new();
    let mut text_with_files = original_text.clone();

    for a in attachments {
        let is_image = a.mime.starts_with("image/");

        if is_image {
            // Always base64-inline images for vision-capable models.
            match std::fs::read(&a.path) {
                Ok(bytes) => {
                    let data_uri = format!("data:{};base64,{}", a.mime, B64.encode(&bytes));
                    image_parts.push(ContentPart::Image {
                        url: data_uri,
                        detail: ImageDetail::Auto,
                    });
                }
                Err(e) => {
                    tracing::warn!(path = %a.path, error = %e, "image unreadable, skipping");
                    continue;
                }
            }
            // If the FileStore stashed OCR text, give non-vision providers
            // (and as redundancy for vision) a textual handle too.
            if let Some(ocr) = &a.extracted_text {
                text_with_files.push_str(&format!(
                    "\n\n<attachment name=\"{}\" type=\"image/ocr\">\n{ocr}\n</attachment>",
                    a.name
                ));
            }
        } else if let Some(extracted) = &a.extracted_text {
            // PDF/CSV/XLSX/plain text → use the pre-extracted text directly.
            // (We never re-parse here; extraction happened once at upload.)
            text_with_files.push_str(&format!(
                "\n\n<attachment name=\"{}\" type=\"{}\">\n{extracted}\n</attachment>",
                a.name, a.mime
            ));
        } else {
            // No image, no extracted text — last resort: if the bytes are UTF-8
            // (a markdown file uploaded as application/octet-stream, say),
            // inline directly. Otherwise log and skip.
            match std::fs::read(&a.path) {
                Ok(bytes) => match std::str::from_utf8(&bytes) {
                    Ok(s) => {
                        text_with_files.push_str(&format!(
                            "\n\n<attachment name=\"{}\">\n{s}\n</attachment>",
                            a.name
                        ));
                    }
                    Err(_) => {
                        tracing::warn!(name = %a.name, mime = %a.mime, "non-image binary with no extracted text, skipping");
                    }
                },
                Err(e) => {
                    tracing::warn!(path = %a.path, error = %e, "attachment unreadable, skipping");
                }
            }
        }
    }

    // Text first, then image refs — providers that walk parts in order see
    // the prompt context (and any extracted text) before the image bytes.
    let mut all_parts = vec![ContentPart::Text(text_with_files)];
    all_parts.extend(image_parts);
    request.messages[idx].content = MessageContent::Parts(all_parts);
}

#[async_trait::async_trait]
impl AgentBehavior for DefaultAgentBehavior {
    fn set_stream_sink(&mut self, sink: Option<crate::behavior::StreamSink>) {
        self.stream_sink = sink;
    }

    async fn on_start(&mut self, config: &AgentConfig) -> Result<(), AgentError> {
        self.system_prompt = config.system_prompt.clone();
        self.configured_model = if config.model.is_empty() {
            None
        } else {
            Some(config.model.clone())
        };
        self.agent_id = config.id.to_string();

        // Initialize token tracker if budget is configured
        if let Some(budget) = &config.token_budget {
            self.tracker = Some(TokenTracker::new(budget.clone(), self.counter.clone()));
        }

        // Restore from checkpoint if available
        if let Some(store) = &self.checkpoint_store {
            match store
                .load_latest(&config.id)
                .await
                .map_err(|e| AgentError::Internal(format!("Checkpoint restore: {e}")))?
            {
                Some(ckpt) => {
                    self.session.restore(ckpt.session_messages);
                    self.checkpoint_version = ckpt.version;
                    tracing::info!(
                        agent = %config.id,
                        version = ckpt.version,
                        messages = self.session.len(),
                        "Restored from checkpoint"
                    );
                }
                None => {
                    tracing::debug!(agent = %config.id, "No checkpoint found, starting fresh");
                }
            }
        }

        // Recall tuning (governs passive injection and the recall tools' defaults).
        let recall = &config.memory.recall;
        self.passive_inject = recall.passive_inject;
        self.recall_top_k = recall.top_k;
        self.recall_min_score = recall.min_score;

        // Assemble this agent's recall tools from whichever memory stores it has,
        // and a standing capability hint that names only the available ones.
        self.recall_tools.clear();
        let mut available: Vec<&str> = Vec::new();
        if let Some(sem) = &self.semantic_memory {
            self.recall_tools.push((
                crate::recall::RECALL_SEARCH.to_string(),
                Arc::new(crate::recall::RecallSearchTool::new(
                    sem.clone(),
                    recall.top_k,
                    recall.min_score,
                )) as Arc<dyn axocoatl_tools::BuiltinTool>,
            ));
            available.push("`recall_search` to look up past sessions and earlier context");
        }
        if let Some(log) = &self.daily_log {
            self.recall_tools.push((
                crate::recall::RECALL_TIMEFRAME.to_string(),
                Arc::new(crate::recall::RecallTimeframeTool::new(log.clone()))
                    as Arc<dyn axocoatl_tools::BuiltinTool>,
            ));
            available.push("`recall_timeframe` to read a specific day's activity log");
        }
        self.recall_capability_hint = if available.is_empty() {
            None
        } else {
            Some(format!(
                "## Recall\nYou have memory beyond this conversation. Before saying you don't \
                 know or don't remember something the user refers to, use {}.",
                available.join(", and "),
            ))
        };

        // Assemble this agent's core-memory edit tools + a standing hint, when a
        // core-memory store is attached. The tools hold clones of the SAME store
        // Arc the behavior renders from, so an edit shows on the next request.
        self.core_memory_tools.clear();
        if let Some(store) = &self.core_memory {
            let handles = crate::core_memory_tools::CoreMemoryHandles {
                store: store.clone(),
                shared: self.shared_blocks.clone(),
            };
            self.core_memory_tools = vec![
                (
                    crate::core_memory_tools::CORE_MEMORY_APPEND.to_string(),
                    Arc::new(crate::core_memory_tools::CoreMemoryAppendTool::new(
                        handles.clone(),
                    )) as Arc<dyn axocoatl_tools::BuiltinTool>,
                ),
                (
                    crate::core_memory_tools::CORE_MEMORY_REPLACE.to_string(),
                    Arc::new(crate::core_memory_tools::CoreMemoryReplaceTool::new(
                        handles.clone(),
                    )) as Arc<dyn axocoatl_tools::BuiltinTool>,
                ),
                (
                    crate::core_memory_tools::CORE_MEMORY_SET.to_string(),
                    Arc::new(crate::core_memory_tools::CoreMemorySetTool::new(handles))
                        as Arc<dyn axocoatl_tools::BuiltinTool>,
                ),
            ];
            let mut labels: Vec<String> = store
                .read()
                .await
                .blocks()
                .iter()
                .map(|b| b.label.clone())
                .collect();
            labels.extend(self.shared_blocks.keys().cloned());
            self.core_capability_hint = Some(format!(
                "## Core memory\nYou maintain editable memory blocks ({}). When you learn a \
                 durable fact about yourself, the user, or the project, update the relevant block \
                 with `core_memory_append` / `core_memory_replace`. Keep them accurate and \
                 concise; don't store ephemeral, task-scoped detail.",
                labels.join(", "),
            ));
        } else {
            self.core_capability_hint = None;
        }

        Ok(())
    }

    async fn execute(&mut self, input: AgentInput) -> Result<AgentOutput, AgentError> {
        // Append user input to session memory FIRST — the session is the
        // canonical conversation history. This enables multi-turn: the LLM
        // sees all prior user/assistant exchanges from this actor's lifetime.
        let input_tokens = self.counter.count_text(&input.content);
        self.session
            .append(MessageRole::User, &input.content, input_tokens);

        // Retrieve semantically-relevant memories for this turn (Tier 4).
        self.semantic_context = self.retrieve_semantic_context(&input.content);

        // Persistently summarize old context once the session has grown toward
        // the model's context window — old turns are LLM-summarized (and archived
        // to the daily log), not silently dropped. No-op under the threshold or
        // when no pipeline is configured.
        let ctx_target = (self.provider.capabilities().max_context_tokens as f32
            * axocoatl_token::COMPRESSION_TRIGGER_PCT) as usize;
        self.compact_session(ctx_target).await;

        // Build from session (not from input.history) so the LLM sees full
        // conversation history accumulated across all calls to this actor.
        // `input.system_override` (when Some, e.g. from a Chat tab call) takes
        // precedence over the agent's configured system_prompt for this turn.
        let mut request = self.build_request_from_session(
            input.system_override.as_deref(),
            input.model_override.clone(),
        );

        // If attachments came with this turn, upgrade the last (user) message
        // from a plain Text(content) into Parts(text + image parts) so the
        // provider can route them as vision content / inline blobs.
        if !input.attachments.is_empty() {
            attach_to_last_user_message(&mut request, &input.attachments);
        }

        // Pre-flight check of the spend budget (the cost cap). Context compaction
        // toward the model window already happened above and is independent of
        // this policy — summarization is NOT a budget mechanism (spent tokens are
        // sunk; you can't summarize them back).
        if let Some(tracker) = &self.tracker {
            let estimated = self.provider.count_tokens(&request);
            if let Err(BudgetError::WouldExceedBudget {
                current,
                requested,
                budget,
            }) = tracker.check_headroom(estimated)
            {
                match tracker.budget().overflow_policy {
                    OverflowPolicy::Abort => {
                        return Err(AgentError::TokenBudgetExceeded {
                            used: current + requested,
                            budget,
                        });
                    }
                    OverflowPolicy::Warn => {
                        tracing::warn!(
                            current,
                            requested,
                            budget,
                            "Token budget would be exceeded, continuing (warn policy)"
                        );
                    }
                }
            }
        }

        // Make the LLM call — always streamed, so output is live by default.
        let est_input = self.provider.count_tokens(&request);
        let mut response = self.stream_chat(request).await?;
        // Some providers' streams omit a final Usage event — fall back to a
        // local estimate so token accounting stays correct.
        if response.usage.total() == 0 {
            response.usage =
                TokenUsageStats::new(est_input, self.counter.count_text(&response.content));
        }

        // Fallback: some models (notably small Ollama-served ones doing
        // function-calling) intermittently emit tool calls as JSON in the
        // message text rather than via the structured tool_calls channel.
        // When `response.tool_calls` is empty we scan `response.content`
        // for top-level JSON objects of shape `{ "tool_name": { args } }`
        // where `tool_name` matches a registered tool, and adopt them.
        // No-op for any model that uses the structured channel
        // correctly — `tool_calls` is non-empty so the block is skipped.
        if response.tool_calls.is_empty() {
            if let Some(executor) = &self.tool_executor {
                let known: std::collections::HashSet<String> =
                    executor.tool_names().into_iter().collect();
                let mut fallback = Vec::new();
                for (idx, v) in extract_top_level_json(&response.content)
                    .into_iter()
                    .enumerate()
                {
                    let Some(obj) = v.as_object() else { continue };
                    if obj.len() != 1 {
                        continue;
                    }
                    let (key, value) = obj.iter().next().unwrap();
                    if !known.contains(key) {
                        continue;
                    }
                    if !value.is_object() {
                        continue;
                    }
                    fallback.push(ToolCall {
                        id: format!("text-fb-{idx}"),
                        name: key.clone(),
                        arguments: value.clone(),
                    });
                }
                if !fallback.is_empty() {
                    tracing::info!(
                        count = fallback.len(),
                        agent = %self.agent_id,
                        "Recovered tool calls from text body (model didn't use structured channel)"
                    );
                    response.tool_calls = fallback;
                }
            }
        }

        // Record token usage
        if let Some(tracker) = &self.tracker {
            let _ = tracker.record_usage(response.usage.input_tokens, response.usage.output_tokens);
        }

        // Tool execution loop: if LLM returns tool calls, execute them and continue
        let mut tool_records = Vec::new();
        let mut loop_count = 0;
        const MAX_TOOL_LOOPS: usize = 10;

        while !response.tool_calls.is_empty() && loop_count < MAX_TOOL_LOOPS {
            loop_count += 1;

            // Handle tool calls when anything can service them: the shared
            // executor and/or this agent's per-agent recall tools.
            let executor = self.tool_executor.clone();
            if executor.is_some()
                || !self.recall_tools.is_empty()
                || !self.core_memory_tools.is_empty()
            {
                // Record the assistant's tool-call turn in the session BEFORE its
                // results. The conversation must read
                // `[…, assistant(tool_calls), tool(result)…]`; without this turn
                // the follow-up request carries orphaned tool results and every
                // cloud provider rejects it (HTTP 400). `response.content` is
                // usually empty here (the model returned only tool calls).
                let assistant_tokens = self.counter.count_text(&response.content);
                self.session.append_assistant_tool_calls(
                    &response.content,
                    &response.tool_calls,
                    assistant_tokens,
                );

                // Phase 1: Run pre-hooks BEFORE dispatch — filter/transform tool calls
                let mut approved_calls = Vec::new();
                for tc in &response.tool_calls {
                    if let Some(hooks) = &self.hook_registry {
                        let (action, transformed_args) = hooks
                            .run_pre_hooks(&tc.name, &self.agent_id, tc.arguments.clone())
                            .await;
                        match action {
                            axocoatl_tools::HookAction::Deny { reason } => {
                                tracing::warn!(tool = %tc.name, reason = %reason, "Tool call denied by hook");
                                self.emit_stream(
                                    crate::behavior::AgentStreamChunk::ToolCallStarted {
                                        id: tc.id.clone(),
                                        name: tc.name.clone(),
                                        arguments: tc.arguments.clone(),
                                    },
                                );
                                self.emit_stream(
                                    crate::behavior::AgentStreamChunk::ToolCallResult {
                                        id: tc.id.clone(),
                                        name: tc.name.clone(),
                                        result: serde_json::json!({ "error": reason.clone() }),
                                        is_error: true,
                                    },
                                );
                                tool_records.push(axocoatl_core::ToolCallRecord {
                                    tool_name: tc.name.clone(),
                                    arguments: tc.arguments.clone(),
                                    result: Some(serde_json::json!({"error": reason})),
                                });
                                let result_str = serde_json::json!({"error": reason}).to_string();
                                let tool_tokens = self.counter.count_text(&result_str);
                                self.session.append_tool_result(
                                    &tc.name,
                                    &tc.id,
                                    &result_str,
                                    tool_tokens,
                                );
                                continue;
                            }
                            _ => {
                                // Allow or Transform — use (possibly transformed) arguments
                                approved_calls.push(axocoatl_llm::ToolCall {
                                    id: tc.id.clone(),
                                    name: tc.name.clone(),
                                    arguments: transformed_args,
                                });
                            }
                        }
                    } else {
                        approved_calls.push(tc.clone());
                    }
                }

                // Surface each approved call so the UI can render a live card.
                for call in &approved_calls {
                    self.emit_stream(crate::behavior::AgentStreamChunk::ToolCallStarted {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    });
                }

                // Phase 2: partition approved calls into agent-scoped recall
                // tools (serviced by the behavior, which owns this agent's
                // memory) and executor tools, preserving original order; dispatch
                // both and merge so Phase 3 is unchanged.
                let mut indexed: Vec<(usize, axocoatl_tools::ToolResult)> = Vec::new();
                let mut exec_calls: Vec<(usize, axocoatl_llm::ToolCall)> = Vec::new();
                for (i, call) in approved_calls.iter().enumerate() {
                    if self.is_behavior_tool(&call.name) {
                        let result = self
                            .execute_behavior_tool(&call.name, call.arguments.clone())
                            .await;
                        indexed.push((
                            i,
                            axocoatl_tools::ToolResult {
                                seq: i,
                                tool_call: call.clone(),
                                result,
                            },
                        ));
                    } else {
                        exec_calls.push((i, call.clone()));
                    }
                }
                if let Some(exec) = &executor {
                    let calls: Vec<axocoatl_llm::ToolCall> =
                        exec_calls.iter().map(|(_, c)| c.clone()).collect();
                    let exec_results = ConcurrentToolDispatcher::dispatch(exec, &calls, |name| {
                        exec.get_concurrency_policy(name)
                            .unwrap_or(axocoatl_llm::ConcurrencyPolicy::Safe)
                    })
                    .await;
                    for ((orig_i, _), r) in exec_calls.iter().zip(exec_results) {
                        indexed.push((*orig_i, r));
                    }
                } else {
                    // No executor: a non-recall call can't be serviced.
                    for (i, call) in &exec_calls {
                        indexed.push((
                            *i,
                            axocoatl_tools::ToolResult {
                                seq: *i,
                                tool_call: call.clone(),
                                result: Err(axocoatl_tools::ToolError::NotFound(call.name.clone())),
                            },
                        ));
                    }
                }
                indexed.sort_by_key(|(i, _)| *i);
                let results: Vec<axocoatl_tools::ToolResult> =
                    indexed.into_iter().map(|(_, r)| r).collect();

                // Phase 3: Run post-hooks and record results
                for tool_result in results {
                    let tc = &tool_result.tool_call;
                    let mut result = tool_result
                        .result
                        .unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}));

                    if let Some(hooks) = &self.hook_registry {
                        result = hooks.run_post_hooks(&tc.name, &self.agent_id, result).await;
                    }

                    tool_records.push(axocoatl_core::ToolCallRecord {
                        tool_name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                        result: Some(result.clone()),
                    });

                    self.emit_stream(crate::behavior::AgentStreamChunk::ToolCallResult {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        result: result.clone(),
                        is_error: result.get("error").is_some(),
                    });

                    let result_str = serde_json::to_string(&result).unwrap_or_default();
                    let tool_tokens = self.counter.count_text(&result_str);
                    self.session
                        .append_tool_result(&tc.name, &tc.id, &result_str, tool_tokens);
                }

                // Make follow-up LLM call with tool results — streamed too.
                // Same overrides apply as the original turn.
                let followup = self.build_request_from_session(
                    input.system_override.as_deref(),
                    input.model_override.clone(),
                );
                let est = self.provider.count_tokens(&followup);
                response = self.stream_chat(followup).await?;
                if response.usage.total() == 0 {
                    response.usage =
                        TokenUsageStats::new(est, self.counter.count_text(&response.content));
                }

                if let Some(tracker) = &self.tracker {
                    let _ = tracker
                        .record_usage(response.usage.input_tokens, response.usage.output_tokens);
                }
            } else {
                // No tool executor — record calls but don't execute
                for tc in &response.tool_calls {
                    tool_records.push(axocoatl_core::ToolCallRecord {
                        tool_name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                        result: None,
                    });
                }
                break;
            }
        }

        // Track assistant response in session
        let output_tokens = self.counter.count_text(&response.content);
        self.session
            .append(MessageRole::Assistant, &response.content, output_tokens);

        // Persist this exchange to semantic memory for future cross-session
        // recall. Best-effort — a store failure is logged, never fatal.
        if let Some(mem) = &self.semantic_memory {
            let exchange = format!("User: {}\nAssistant: {}", input.content, response.content);
            if let Err(e) = mem.store(&exchange, serde_json::json!({ "agent": self.agent_id })) {
                tracing::debug!(error = %e, "semantic memory store failed");
            }
        }

        // Checkpoint after execution
        if let Some(store) = &self.checkpoint_store {
            self.checkpoint_version += 1;
            let ckpt = AgentCheckpoint {
                version: self.checkpoint_version,
                agent_id: self.agent_id.clone(),
                checkpoint_time: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                session_messages: self.session.messages().to_vec(),
                cumulative_token_usage: self
                    .tracker
                    .as_ref()
                    .map(|t| TokenUsageStats::new(t.input_used(), t.output_used()))
                    .unwrap_or_default(),
                behavior_state: None,
            };
            if let Err(e) = store.save(&ckpt).await {
                tracing::warn!(agent = %self.agent_id, error = %e, "Checkpoint save failed");
            }
        }

        Ok(AgentOutput {
            content: response.content,
            tool_calls: tool_records,
            token_usage: response.usage,
        })
    }

    /// Background "sleep-time" consolidation: an LLM memory-manager pass that
    /// promotes durable facts from recent Tier-4 activity into the curated core
    /// blocks and tidies them. Promotion-only — it reads Tier 4, never evicts it.
    async fn on_consolidate(&mut self) -> Result<crate::behavior::ConsolidationReport, AgentError> {
        // Need a core store to write to and a semantic feed to promote from.
        let (Some(store), Some(sem)) = (self.core_memory.clone(), self.semantic_memory.clone())
        else {
            return Ok(crate::behavior::ConsolidationReport::skipped());
        };
        let recent = sem.recent(20).unwrap_or_default();
        let blocks_snapshot: Vec<(String, String, usize)> = {
            let s = store.read().await;
            s.blocks()
                .iter()
                .map(|b| (b.label.clone(), b.value.clone(), b.limit))
                .collect()
        };
        if recent.is_empty() || blocks_snapshot.is_empty() {
            return Ok(crate::behavior::ConsolidationReport::skipped());
        }

        let blocks_text = blocks_snapshot
            .iter()
            .map(|(l, v, lim)| {
                format!(
                    "### {l} (limit {lim} chars)\n{}",
                    if v.trim().is_empty() { "(empty)" } else { v }
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let activity = recent.join("\n---\n");
        let system = "You are a memory manager for an AI agent. Given its core-memory blocks and \
                      recent activity, promote DURABLE facts (about the user, the project, the \
                      agent's persona) into the right block, merge duplicates, and tighten wording \
                      — within each block's character limit. Do NOT store ephemeral or task-scoped \
                      detail. Output ONLY a JSON array of edits ([] if nothing should change). Each \
                      edit is one of: {\"op\":\"append\",\"block\":\"<label>\",\"text\":\"...\"}, \
                      {\"op\":\"replace\",\"block\":\"<label>\",\"old\":\"...\",\"new\":\"...\"}, \
                      {\"op\":\"set\",\"block\":\"<label>\",\"value\":\"...\"}.";
        let user = format!(
            "Core-memory blocks:\n{blocks_text}\n\nRecent activity:\n{activity}\n\nJSON edit array:"
        );
        let request = ChatRequest {
            messages: vec![ChatMessage::system(system), ChatMessage::user(&user)],
            tools: vec![],
            max_tokens: Some(800),
            temperature: Some(0.0),
            stop_sequences: Vec::new(),
            provider_options: None,
            model_override: None,
        };
        let response = self
            .provider
            .chat(request)
            .await
            .map_err(|e| AgentError::Provider(e.to_string()))?;
        let tokens_used = response.usage.input_tokens + response.usage.output_tokens;
        if let Some(tracker) = &self.tracker {
            let _ = tracker.record_usage(response.usage.input_tokens, response.usage.output_tokens);
        }

        let edits = parse_consolidation_edits(&response.content);
        let mut report = crate::behavior::ConsolidationReport {
            tokens_used,
            ..Default::default()
        };
        {
            let mut s = store.write().await;
            for e in &edits {
                match apply_consolidation_edit(&mut s, e) {
                    Ok(label) => {
                        if !report.blocks_touched.contains(&label) {
                            report.blocks_touched.push(label);
                        }
                        if e.op == "replace" {
                            report.rewritten += 1;
                        } else {
                            report.promoted += 1;
                        }
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, block = %e.block, "consolidation edit skipped")
                    }
                }
            }
            if !report.blocks_touched.is_empty() {
                if let Err(err) = s.save().await {
                    tracing::warn!(error = %err, "failed to save core memory after consolidation");
                }
            }
        }
        // `skipped` stays false: the LLM pass ran (even with zero edits), so the
        // daemon won't re-run it until the next interval.
        tracing::info!(
            agent = %self.agent_id,
            promoted = report.promoted,
            rewritten = report.rewritten,
            tokens = report.tokens_used,
            "Consolidated core memory"
        );
        Ok(report)
    }

    async fn on_stop(&mut self) -> Result<(), AgentError> {
        if let Some(tracker) = &self.tracker {
            tracing::info!(
                total_tokens = tracker.total_used(),
                input = tracker.input_used(),
                output = tracker.output_used(),
                "Agent stopping — final token usage"
            );
        }

        // A graceful stop runs one last consolidation pass (same as the idle
        // background loop) so this session's durable facts are promoted into the
        // curated blocks before shutdown.
        let _ = self.on_consolidate().await;
        Ok(())
    }
}

/// One edit emitted by the consolidation memory-manager LLM pass.
#[derive(Debug, serde::Deserialize)]
struct ConsolidationEdit {
    op: String,
    block: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    old: Option<String>,
    #[serde(default)]
    new: Option<String>,
    #[serde(default)]
    value: Option<String>,
}

/// Parse the model's JSON edit array, tolerating prose/code-fence wrapping by
/// falling back to the first `[ … ]` slice. Unparseable → no edits.
fn parse_consolidation_edits(content: &str) -> Vec<ConsolidationEdit> {
    let trimmed = content.trim();
    if let Ok(v) = serde_json::from_str::<Vec<ConsolidationEdit>>(trimmed) {
        return v;
    }
    if let (Some(start), Some(end)) = (trimmed.find('['), trimmed.rfind(']')) {
        if start < end {
            if let Ok(v) = serde_json::from_str::<Vec<ConsolidationEdit>>(&trimmed[start..=end]) {
                return v;
            }
        }
    }
    Vec::new()
}

/// Apply one consolidation edit to the store (limit-enforced by the block
/// methods). Returns the touched block label on success.
fn apply_consolidation_edit(
    store: &mut axocoatl_memory::CoreMemoryStore,
    e: &ConsolidationEdit,
) -> Result<String, axocoatl_memory::MemoryError> {
    let block = store
        .block_mut(&e.block)
        .ok_or_else(|| axocoatl_memory::MemoryError::NotFound(format!("block '{}'", e.block)))?;
    match e.op.as_str() {
        "append" => block.append(e.text.as_deref().unwrap_or("")),
        "replace" => block.replace(
            e.old.as_deref().unwrap_or(""),
            e.new.as_deref().unwrap_or(""),
        ),
        "set" => block.set(e.value.as_deref().unwrap_or("")),
        other => Err(axocoatl_memory::MemoryError::Invalid(format!(
            "unknown consolidation op '{other}'"
        ))),
    }?;
    Ok(e.block.clone())
}

/// Extract every top-level JSON object from a free-form text body.
///
/// Used by the text-format tool-call fallback in `DefaultAgentBehavior`
/// — some LLMs emit `{ "tool_name": { args } }` blocks in their message
/// content instead of going through the structured tool_calls channel.
/// We need to recover those, but the surrounding text is arbitrary prose,
/// so `serde_json::Deserializer::into_iter` won't get us all the way.
///
/// Strategy: walk the bytes, at every `{` count balanced braces (taking
/// string-escaping into account) until the matching `}` is found, then
/// attempt to parse that slice as a JSON value.  On parse failure or
/// unmatched braces we skip to the next byte.
fn extract_top_level_json(text: &str) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        // Walk forward from i, tracking balanced braces + JSON strings.
        let mut depth: i32 = 0;
        let mut in_string = false;
        let mut escape = false;
        let mut j = i;
        let mut found_end = false;
        while j < bytes.len() {
            let c = bytes[j];
            if escape {
                escape = false;
            } else if in_string {
                match c {
                    b'\\' => escape = true,
                    b'"' => in_string = false,
                    _ => {}
                }
            } else {
                match c {
                    b'"' => in_string = true,
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            found_end = true;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            j += 1;
        }
        if found_end {
            let slice = &text[i..=j];
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(slice) {
                out.push(v);
            }
            i = j + 1;
        } else {
            // Unbalanced — stop, there can't be another well-formed top-level
            // object after an unclosed one starting here.
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axocoatl_core::{AgentConfig, AgentId, OverflowPolicy, TokenBudget, TokenUsageStats};
    use axocoatl_llm::{
        ChatResponse, FinishReason, LlmProvider, ProviderCapabilities, ProviderError, StreamEvent,
    };
    use std::pin::Pin;
    use tokio_stream::Stream;

    /// Mock provider that returns a fixed response.
    struct MockLlm {
        response_content: String,
        usage: TokenUsageStats,
    }

    impl MockLlm {
        fn new(content: &str, input_tokens: usize, output_tokens: usize) -> Self {
            Self {
                response_content: content.to_string(),
                usage: TokenUsageStats::new(input_tokens, output_tokens),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for MockLlm {
        fn provider_id(&self) -> &str {
            "mock"
        }
        fn model_id(&self) -> &str {
            "mock-model"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, ProviderError> {
            Ok(ChatResponse {
                content: self.response_content.clone(),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: self.usage.clone(),
                model: "mock-model".to_string(),
                provider: "mock".to_string(),
            })
        }
        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let events = vec![
                Ok(StreamEvent::TextDelta {
                    delta: self.response_content.clone(),
                }),
                Ok(StreamEvent::Usage(self.usage.clone())),
                Ok(StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                }),
            ];
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    /// Mock provider that always fails.
    struct FailingLlm;

    #[async_trait::async_trait]
    impl LlmProvider for FailingLlm {
        fn provider_id(&self) -> &str {
            "failing"
        }
        fn model_id(&self) -> &str {
            "fail"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            Err(ProviderError::ApiError {
                provider: "failing".to_string(),
                status: 500,
                message: "mock LLM failure".to_string(),
            })
        }
        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            Err(ProviderError::ApiError {
                provider: "failing".to_string(),
                status: 500,
                message: "mock LLM failure".to_string(),
            })
        }
    }

    /// Stateful mock: first stream returns a tool call, every later stream
    /// returns a final text answer. Captures each request it receives so a test
    /// can assert the follow-up replays the assistant tool-call turn + result.
    struct ToolThenTextLlm {
        calls: std::sync::atomic::AtomicUsize,
        captured: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ToolThenTextLlm {
        fn provider_id(&self) -> &str {
            "tooltext"
        }
        fn model_id(&self) -> &str {
            "tooltext-model"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                ..Default::default()
            }
        }
        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unimplemented!("round-trip test uses chat_stream")
        }
        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            self.captured.lock().unwrap().push(request);
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events = if n == 0 {
                vec![
                    Ok(StreamEvent::ToolCallDelta {
                        index: Some(0),
                        id: "call_1".to_string(),
                        name: Some("echo".to_string()),
                        args_delta: "{\"text\":\"hi\"}".to_string(),
                    }),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::ToolUse,
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta {
                        delta: "final answer".to_string(),
                    }),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::Stop,
                    }),
                ]
            };
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    fn simple_counter() -> Arc<dyn TokenCounter> {
        struct SimpleCounter;
        impl TokenCounter for SimpleCounter {
            fn count_text(&self, text: &str) -> usize {
                text.len() / 4 + 1
            }
            fn count_messages(&self, msgs: &[ChatMessage]) -> usize {
                msgs.iter()
                    .map(|m| m.text_content().map_or(1, |t| self.count_text(t)))
                    .sum()
            }
            fn count_tool_definition(&self, j: &serde_json::Value) -> usize {
                self.count_text(&j.to_string())
            }
        }
        Arc::new(SimpleCounter)
    }

    fn test_config_with_budget(per_execution: usize) -> AgentConfig {
        AgentConfig {
            id: AgentId::new("test"),
            name: "Test".to_string(),
            system_prompt: Some("You are helpful.".to_string()),
            token_budget: Some(TokenBudget {
                per_call: per_execution,
                per_execution,
                overflow_policy: OverflowPolicy::Abort,
            }),
            ..AgentConfig::default()
        }
    }

    #[tokio::test]
    async fn tool_round_trip_records_assistant_call_and_result() {
        use axocoatl_tools::{EchoTool, ToolExecutor};

        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(ToolThenTextLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: captured.clone(),
        });

        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(EchoTool));

        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
            .with_tool_executor(Arc::new(executor));
        behavior.on_start(&AgentConfig::default()).await.unwrap();

        let output = behavior
            .execute(AgentInput::text("please echo hi"))
            .await
            .unwrap();

        // The model's final turn (after seeing the tool result) is the output.
        assert_eq!(output.content, "final answer");
        assert_eq!(output.tool_calls.len(), 1);
        assert_eq!(output.tool_calls[0].tool_name, "echo");

        // The crux of the round-trip: the follow-up request must replay the
        // assistant's tool-call turn followed by the correlated tool result.
        // Without that sequence, real provider APIs reject the request.
        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 2, "expected an initial call and one follow-up");
        let followup = &reqs[1];

        let assistant = followup
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Assistant && !m.tool_calls.is_empty())
            .expect("assistant tool-call turn must be replayed in the follow-up");
        assert_eq!(assistant.tool_calls[0].name, "echo");
        assert_eq!(assistant.tool_calls[0].id, "call_1");

        let tool_msg = followup
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("tool result must be present in the follow-up");
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(tool_msg.name.as_deref(), Some("echo"));
    }

    /// Emits a configurable set of tool calls on the first response, then text.
    struct ToolCallsThenTextLlm {
        calls: std::sync::atomic::AtomicUsize,
        captured: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
        /// (id, name, args_json) for the first response.
        first: Vec<(String, String, String)>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for ToolCallsThenTextLlm {
        fn provider_id(&self) -> &str {
            "tct"
        }
        fn model_id(&self) -> &str {
            "tct-model"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                ..Default::default()
            }
        }
        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            unimplemented!("uses chat_stream")
        }
        async fn chat_stream(
            &self,
            request: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            self.captured.lock().unwrap().push(request);
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events = if n == 0 {
                let mut ev: Vec<Result<StreamEvent, ProviderError>> = self
                    .first
                    .iter()
                    .enumerate()
                    .map(|(i, (id, name, args))| {
                        Ok(StreamEvent::ToolCallDelta {
                            index: Some(i),
                            id: id.clone(),
                            name: Some(name.clone()),
                            args_delta: args.clone(),
                        })
                    })
                    .collect();
                ev.push(Ok(StreamEvent::Done {
                    finish_reason: FinishReason::ToolUse,
                }));
                ev
            } else {
                vec![
                    Ok(StreamEvent::TextDelta {
                        delta: "final answer".to_string(),
                    }),
                    Ok(StreamEvent::Done {
                        finish_reason: FinishReason::Stop,
                    }),
                ]
            };
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    fn hashed_semantic(dir: &std::path::Path, text: &str) -> Arc<axocoatl_memory::SemanticMemory> {
        let mem = axocoatl_memory::SemanticMemory::new_hashed("test", dir).unwrap();
        mem.store(text, serde_json::json!({})).unwrap();
        Arc::new(mem)
    }

    #[tokio::test]
    async fn recall_tool_advertised_only_when_memory_present() {
        let dir = tempfile::tempdir().unwrap();
        // Semantic memory present, no daily log → only recall_search advertised.
        let mut b = DefaultAgentBehavior::new(Arc::new(MockLlm::new("x", 1, 1)), simple_counter())
            .with_semantic_memory(hashed_semantic(dir.path(), "alpha"));
        b.on_start(&AgentConfig::default()).await.unwrap();
        let names: Vec<String> = b.tool_definitions().into_iter().map(|d| d.name).collect();
        assert!(names.iter().any(|n| n == "recall_search"), "{names:?}");
        assert!(!names.iter().any(|n| n == "recall_timeframe"), "{names:?}");

        // No memory at all → no recall tools advertised.
        let mut b2 = DefaultAgentBehavior::new(Arc::new(MockLlm::new("x", 1, 1)), simple_counter());
        b2.on_start(&AgentConfig::default()).await.unwrap();
        assert!(b2.tool_definitions().is_empty());
    }

    #[tokio::test]
    async fn recall_search_round_trip_without_executor() {
        let dir = tempfile::tempdir().unwrap();
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(ToolCallsThenTextLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: captured.clone(),
            first: vec![(
                "call_r".to_string(),
                "recall_search".to_string(),
                "{\"query\":\"the deploy key is stored in vault\"}".to_string(),
            )],
        });
        let mut b = DefaultAgentBehavior::new(provider, simple_counter()).with_semantic_memory(
            hashed_semantic(dir.path(), "the deploy key is stored in vault"),
        );
        b.on_start(&AgentConfig::default()).await.unwrap();

        let out = b
            .execute(AgentInput::text("where is the deploy key?"))
            .await
            .unwrap();
        assert_eq!(out.content, "final answer");
        assert!(out
            .tool_calls
            .iter()
            .any(|t| t.tool_name == "recall_search"));

        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 2, "initial + follow-up");
        let tool_msg = reqs[1]
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("recall tool result replayed in follow-up");
        assert_eq!(tool_msg.name.as_deref(), Some("recall_search"));
        assert!(tool_msg.text_content().unwrap().contains("deploy key"));
    }

    #[tokio::test]
    async fn mixed_executor_and_recall_calls_recorded_in_order() {
        use axocoatl_tools::{EchoTool, ToolExecutor};
        let dir = tempfile::tempdir().unwrap();
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(ToolCallsThenTextLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: captured.clone(),
            first: vec![
                ("call_e".into(), "echo".into(), "{\"text\":\"hi\"}".into()),
                (
                    "call_r".into(),
                    "recall_search".into(),
                    "{\"query\":\"alpha beta gamma\"}".into(),
                ),
            ],
        });
        let mut executor = ToolExecutor::new();
        executor.register_builtin("echo", Arc::new(EchoTool));
        let mut b = DefaultAgentBehavior::new(provider, simple_counter())
            .with_tool_executor(Arc::new(executor))
            .with_semantic_memory(hashed_semantic(dir.path(), "alpha beta gamma"));
        b.on_start(&AgentConfig::default()).await.unwrap();

        let out = b.execute(AgentInput::text("do both")).await.unwrap();
        assert_eq!(out.content, "final answer");

        let reqs = captured.lock().unwrap();
        let tool_msgs: Vec<_> = reqs[1]
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .collect();
        assert_eq!(tool_msgs.len(), 2, "both tool results recorded");
        // Original call order preserved: echo (idx 0) before recall (idx 1).
        assert_eq!(tool_msgs[0].name.as_deref(), Some("echo"));
        assert_eq!(tool_msgs[1].name.as_deref(), Some("recall_search"));
    }

    #[tokio::test]
    async fn passive_inject_can_be_disabled() {
        use axocoatl_core::{MemoryConfig, RecallConfig};
        let dir = tempfile::tempdir().unwrap();
        let mem = hashed_semantic(dir.path(), "the sky is blue");

        // Default (passive on) → a matching query yields injected context.
        let mut on = DefaultAgentBehavior::new(Arc::new(MockLlm::new("x", 1, 1)), simple_counter())
            .with_semantic_memory(mem.clone());
        on.on_start(&AgentConfig::default()).await.unwrap();
        assert!(!on.retrieve_semantic_context("the sky is blue").is_empty());

        // passive_inject=false → nothing injected regardless of matches.
        let cfg = AgentConfig {
            memory: MemoryConfig {
                recall: RecallConfig {
                    passive_inject: false,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..AgentConfig::default()
        };
        let mut off =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("x", 1, 1)), simple_counter())
                .with_semantic_memory(mem);
        off.on_start(&cfg).await.unwrap();
        assert!(off.retrieve_semantic_context("the sky is blue").is_empty());
    }

    #[tokio::test]
    async fn capability_hint_present_only_with_recall() {
        let dir = tempfile::tempdir().unwrap();
        let mut with =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("x", 1, 1)), simple_counter())
                .with_semantic_memory(hashed_semantic(dir.path(), "x"));
        with.on_start(&AgentConfig::default()).await.unwrap();
        assert!(with.memory_context().contains("recall_search"));

        let mut without =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("x", 1, 1)), simple_counter());
        without.on_start(&AgentConfig::default()).await.unwrap();
        assert!(!without.memory_context().contains("## Recall"));
    }

    fn behavior_with_core(
        provider: Arc<dyn LlmProvider>,
        dir: &std::path::Path,
    ) -> DefaultAgentBehavior {
        let mut store = axocoatl_memory::CoreMemoryStore::new("a", dir.join("a.json"));
        store.ensure_block(axocoatl_memory::MemoryBlock::new("human", 0));
        DefaultAgentBehavior::new(provider, simple_counter()).with_core_memory(
            Arc::new(tokio::sync::RwLock::new(store)),
            std::collections::HashMap::new(),
        )
    }

    #[tokio::test]
    async fn core_memory_tools_advertised_only_with_store() {
        let dir = tempfile::tempdir().unwrap();
        let mut with = behavior_with_core(Arc::new(MockLlm::new("x", 1, 1)), dir.path());
        with.on_start(&AgentConfig::default()).await.unwrap();
        let names: Vec<String> = with
            .tool_definitions()
            .into_iter()
            .map(|d| d.name)
            .collect();
        for t in [
            "core_memory_append",
            "core_memory_replace",
            "core_memory_set",
        ] {
            assert!(names.iter().any(|n| n == t), "missing {t} in {names:?}");
        }

        let mut without =
            DefaultAgentBehavior::new(Arc::new(MockLlm::new("x", 1, 1)), simple_counter());
        without.on_start(&AgentConfig::default()).await.unwrap();
        assert!(!without
            .tool_definitions()
            .iter()
            .any(|d| d.name.starts_with("core_memory")));
    }

    #[tokio::test]
    async fn core_memory_renders_and_hints() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = axocoatl_memory::CoreMemoryStore::new("a", dir.path().join("a.json"));
        let mut human = axocoatl_memory::MemoryBlock::new("human", 0);
        human.set("name: Alice").unwrap();
        store.ensure_block(human);
        let mut b = DefaultAgentBehavior::new(Arc::new(MockLlm::new("x", 1, 1)), simple_counter())
            .with_core_memory(
                Arc::new(tokio::sync::RwLock::new(store)),
                std::collections::HashMap::new(),
            );
        b.on_start(&AgentConfig::default()).await.unwrap();
        let ctx = b.memory_context();
        assert!(ctx.contains("## Core Memory"));
        assert!(ctx.contains("name: Alice"));
        assert!(
            ctx.contains("core_memory_append"),
            "capability hint present"
        );
    }

    #[tokio::test]
    async fn core_memory_edit_persists_and_renders_same_turn() {
        let dir = tempfile::tempdir().unwrap();
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(ToolCallsThenTextLlm {
            calls: std::sync::atomic::AtomicUsize::new(0),
            captured: captured.clone(),
            first: vec![(
                "c1".to_string(),
                "core_memory_append".to_string(),
                "{\"block\":\"human\",\"text\":\"name: Alice\"}".to_string(),
            )],
        });
        let mut b = behavior_with_core(provider, dir.path());
        b.on_start(&AgentConfig::default()).await.unwrap();

        let out = b
            .execute(AgentInput::text("my name is Alice"))
            .await
            .unwrap();
        assert_eq!(out.content, "final answer");

        // The store reflects the edit...
        let stored = b
            .core_memory
            .as_ref()
            .unwrap()
            .read()
            .await
            .block("human")
            .unwrap()
            .value
            .clone();
        assert!(stored.contains("name: Alice"));

        // ...and the follow-up request's system prompt re-rendered it same-turn.
        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 2, "initial + follow-up");
        let sys = reqs[1]
            .messages
            .iter()
            .find(|m| m.role == MessageRole::System)
            .expect("system message with core memory");
        assert!(sys.text_content().unwrap().contains("name: Alice"));
    }

    #[tokio::test]
    async fn on_consolidate_promotes_into_block() {
        let dir = tempfile::tempdir().unwrap();
        let sem = hashed_semantic(dir.path(), "the user said their name is Alice");
        let mut store = axocoatl_memory::CoreMemoryStore::new("a", dir.path().join("core.json"));
        store.ensure_block(axocoatl_memory::MemoryBlock::new("human", 0));
        let store = Arc::new(tokio::sync::RwLock::new(store));
        // The memory-manager LLM returns a fixed edit list.
        let provider = Arc::new(MockLlm::new(
            "[{\"op\":\"append\",\"block\":\"human\",\"text\":\"name: Alice\"}]",
            10,
            10,
        ));
        let mut b = DefaultAgentBehavior::new(provider, simple_counter())
            .with_semantic_memory(sem)
            .with_core_memory(store.clone(), std::collections::HashMap::new());
        b.on_start(&AgentConfig::default()).await.unwrap();

        let report = b.on_consolidate().await.unwrap();
        assert!(!report.skipped);
        assert_eq!(report.promoted, 1);
        assert_eq!(report.blocks_touched, vec!["human".to_string()]);
        assert!(store
            .read()
            .await
            .block("human")
            .unwrap()
            .value
            .contains("Alice"));
    }

    #[tokio::test]
    async fn on_consolidate_skips_without_memory() {
        // No core/semantic memory → a cheap skip, no LLM-applied edits.
        let mut b = DefaultAgentBehavior::new(Arc::new(MockLlm::new("[]", 1, 1)), simple_counter());
        b.on_start(&AgentConfig::default()).await.unwrap();
        let report = b.on_consolidate().await.unwrap();
        assert!(report.skipped);
        assert_eq!(report.promoted, 0);
    }

    #[tokio::test]
    async fn default_behavior_calls_llm() {
        let provider = Arc::new(MockLlm::new("Hello from LLM", 50, 20));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter());

        behavior
            .on_start(&AgentConfig {
                system_prompt: Some("You are helpful.".to_string()),
                ..AgentConfig::default()
            })
            .await
            .unwrap();

        let output = behavior.execute(AgentInput::text("Hi")).await.unwrap();
        assert_eq!(output.content, "Hello from LLM");
        assert_eq!(output.token_usage.input_tokens, 50);
        assert_eq!(output.token_usage.output_tokens, 20);
    }

    #[tokio::test]
    async fn default_behavior_includes_system_prompt() {
        let provider = Arc::new(MockLlm::new("response", 10, 5));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter());

        behavior
            .on_start(&AgentConfig {
                system_prompt: Some("You are a researcher.".to_string()),
                ..AgentConfig::default()
            })
            .await
            .unwrap();

        // The request should include the system prompt + user message
        let input = AgentInput::text("Find papers on AI");
        let request = behavior.build_request(&input);
        assert_eq!(request.messages.len(), 2);
        assert_eq!(
            request.messages[0].text_content(),
            Some("You are a researcher.")
        );
        assert_eq!(
            request.messages[1].text_content(),
            Some("Find papers on AI")
        );
    }

    #[tokio::test]
    async fn default_behavior_tracks_tokens() {
        let provider = Arc::new(MockLlm::new("resp", 100, 50));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter());

        behavior
            .on_start(&test_config_with_budget(10000))
            .await
            .unwrap();

        // Execute twice
        behavior.execute(AgentInput::text("first")).await.unwrap();
        behavior.execute(AgentInput::text("second")).await.unwrap();

        // Tracker should show accumulated usage
        let tracker = behavior.tracker.as_ref().unwrap();
        assert_eq!(tracker.total_used(), 300); // (100+50) * 2
    }

    #[tokio::test]
    async fn default_behavior_budget_abort() {
        // Mock returns 100 input + 50 output = 150 tokens per call
        let provider = Arc::new(MockLlm::new("resp", 100, 50));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter());

        // Budget of 160 — first call uses 150, leaving only 10
        // The headroom check estimates request size (small), but after the first
        // call's 150 tokens are recorded, the second call's response (150 more)
        // will exceed the budget when recorded. The post-record check catches it.
        behavior
            .on_start(&test_config_with_budget(160))
            .await
            .unwrap();

        let first = behavior.execute(AgentInput::text("first")).await;
        assert!(first.is_ok());

        // After first call: 150 used, 10 remaining
        // Second call: headroom check estimates ~5 tokens for "second" message,
        // which fits in 10. But the actual LLM response adds 150 more → 300 total > 160.
        // The record_usage call will return BudgetExceeded. We don't abort from that
        // (it's logged), but the response still returns Ok. The budget enforcement
        // is a pre-flight check + post-recording signal.
        //
        // For strict abort before the LLM call, the headroom estimate must include
        // expected response size. Let's test that the tracker correctly shows overuse:
        let second = behavior.execute(AgentInput::text("second")).await;
        // The call succeeds (LLM was called), but tracker shows we're over budget
        assert!(second.is_ok());
        let tracker = behavior.tracker.as_ref().unwrap();
        assert!(
            tracker.total_used() > 160,
            "Should exceed budget after 2 calls"
        );
    }

    /// Provider with a real context window (so the compression pipeline is built)
    /// and a fixed per-call usage to drive the budget.
    struct CtxLlm {
        per_call: usize,
    }
    #[async_trait::async_trait]
    impl LlmProvider for CtxLlm {
        fn provider_id(&self) -> &str {
            "ctx"
        }
        fn model_id(&self) -> &str {
            "ctx-model"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                max_context_tokens: 200,
                ..ProviderCapabilities::default()
            }
        }
        async fn chat(&self, _: ChatRequest) -> Result<ChatResponse, ProviderError> {
            Ok(ChatResponse {
                content: "summary".to_string(),
                tool_calls: vec![],
                finish_reason: FinishReason::Stop,
                usage: TokenUsageStats::new(self.per_call, 0),
                model: "ctx-model".to_string(),
                provider: "ctx".to_string(),
            })
        }
        async fn chat_stream(
            &self,
            _: ChatRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let events = vec![
                Ok(StreamEvent::TextDelta {
                    delta: "resp".to_string(),
                }),
                Ok(StreamEvent::Usage(TokenUsageStats::new(self.per_call, 0))),
                Ok(StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                }),
            ];
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    #[test]
    fn abort_is_default_policy() {
        // A configured spend budget is enforced by default — overflow aborts.
        assert_eq!(OverflowPolicy::default(), OverflowPolicy::Abort);
    }

    #[tokio::test]
    async fn warn_policy_continues_where_abort_fails() {
        // Each turn records 100 tokens; budget 120. By the 3rd turn the pre-flight
        // check trips (200 used + request > 120).
        // Abort policy (the default) → the 3rd turn returns a budget error.
        let mut abort =
            DefaultAgentBehavior::new(Arc::new(CtxLlm { per_call: 100 }), simple_counter());
        abort.on_start(&test_config_with_budget(120)).await.unwrap();
        abort.execute(AgentInput::text("t1")).await.unwrap();
        abort.execute(AgentInput::text("t2")).await.unwrap();
        let third_abort = abort.execute(AgentInput::text("t3")).await;
        assert!(
            matches!(third_abort, Err(AgentError::TokenBudgetExceeded { .. })),
            "abort should error, got {third_abort:?}"
        );

        // Warn policy → the same overflow logs and continues past the budget
        // (the spend cap is advisory; context compaction is independent of it).
        let mut cfg = test_config_with_budget(120);
        cfg.token_budget.as_mut().unwrap().overflow_policy = OverflowPolicy::Warn;
        let mut warn =
            DefaultAgentBehavior::new(Arc::new(CtxLlm { per_call: 100 }), simple_counter());
        warn.on_start(&cfg).await.unwrap();
        warn.execute(AgentInput::text("t1")).await.unwrap();
        warn.execute(AgentInput::text("t2")).await.unwrap();
        let third_warn = warn.execute(AgentInput::text("t3")).await;
        assert!(
            third_warn.is_ok(),
            "warn should continue, got {third_warn:?}"
        );
    }

    #[tokio::test]
    async fn default_behavior_llm_failure_propagates() {
        let provider = Arc::new(FailingLlm);
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter());

        behavior.on_start(&AgentConfig::default()).await.unwrap();

        let result = behavior.execute(AgentInput::text("trigger")).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("mock LLM failure"));
    }

    #[tokio::test]
    async fn system_override_replaces_configured_prompt() {
        // Regression for the Chat tab's per-chat system prompt feature.
        // When AgentInput.system_override is Some, build_request_from_session
        // must use that string instead of self.system_prompt (memory context
        // still merges normally).
        let provider = Arc::new(MockLlm::new("ok", 1, 1));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter());
        behavior
            .on_start(&AgentConfig {
                system_prompt: Some("Default prompt.".to_string()),
                ..AgentConfig::default()
            })
            .await
            .unwrap();

        // First populate the session with a user turn so build_request_from_session
        // has something to render against.
        behavior.session.append(MessageRole::User, "hi", 1);

        let with_override = behavior.build_request_from_session(Some("Respond in haiku."), None);
        let with_default = behavior.build_request_from_session(None, None);

        let sys_override = with_override.messages[0].text_content().unwrap();
        let sys_default = with_default.messages[0].text_content().unwrap();

        assert!(sys_override.contains("Respond in haiku."));
        assert!(!sys_override.contains("Default prompt."));
        assert!(sys_default.contains("Default prompt."));
    }

    #[tokio::test]
    async fn default_behavior_with_history() {
        let provider = Arc::new(MockLlm::new("response", 10, 5));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter());
        behavior.on_start(&AgentConfig::default()).await.unwrap();

        let input = AgentInput::text("follow up").with_history(vec![
            ChatMessage::user("original question"),
            ChatMessage::assistant("first answer"),
        ]);

        let request = behavior.build_request(&input);
        // No system prompt (default config) + 2 history + 1 user = 3
        assert_eq!(request.messages.len(), 3);
    }

    // Integration: spawn as actor and execute
    #[tokio::test]
    async fn actor_with_default_behavior() {
        use crate::actor_impl::{execute_agent, AgentActor};
        use ractor::Actor;

        let provider = Arc::new(MockLlm::new("actor response", 30, 15));
        let behavior = DefaultAgentBehavior::new(provider, simple_counter());

        let config = AgentConfig {
            id: AgentId::new("llm-agent"),
            name: "LLM Agent".to_string(),
            system_prompt: Some("You help with code.".to_string()),
            token_budget: Some(TokenBudget {
                per_call: 5000,
                per_execution: 10000,
                overflow_policy: OverflowPolicy::Abort,
            }),
            ..AgentConfig::default()
        };

        let (actor_ref, handle) = AgentActor::spawn(
            Some("llm-test".to_string()),
            AgentActor,
            (config, Box::new(behavior) as Box<dyn AgentBehavior>),
        )
        .await
        .unwrap();

        let output = execute_agent(&actor_ref, AgentInput::text("Write a function"))
            .await
            .unwrap();

        assert_eq!(output.content, "actor response");
        assert_eq!(output.token_usage.total(), 45);

        actor_ref.stop(None);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn session_memory_tracks_messages() {
        let provider = Arc::new(MockLlm::new("response", 10, 5));
        let mut behavior = DefaultAgentBehavior::new(provider, simple_counter());
        behavior.on_start(&AgentConfig::default()).await.unwrap();

        behavior.execute(AgentInput::text("hello")).await.unwrap();
        behavior.execute(AgentInput::text("world")).await.unwrap();

        // Session should have 4 messages: user, assistant, user, assistant
        assert_eq!(behavior.session().len(), 4);
    }

    #[tokio::test]
    async fn checkpoint_save_and_restore() {
        use axocoatl_memory::CheckpointPolicy;
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(CheckpointStore::new(
            tmp.path(),
            CheckpointPolicy::EveryLlmCall,
        ));

        let agent_config = AgentConfig {
            id: AgentId::new("ckpt-agent"),
            name: "Checkpoint Agent".to_string(),
            system_prompt: Some("Be helpful.".to_string()),
            ..AgentConfig::default()
        };

        // Phase 1: Execute with checkpointing
        {
            let provider = Arc::new(MockLlm::new("first response", 10, 5));
            let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
                .with_checkpoint_store(store.clone());

            behavior.on_start(&agent_config).await.unwrap();
            behavior.execute(AgentInput::text("hello")).await.unwrap();
            behavior
                .execute(AgentInput::text("how are you"))
                .await
                .unwrap();

            // Should have 4 messages and 2 checkpoints saved
            assert_eq!(behavior.session().len(), 4);
            assert_eq!(behavior.checkpoint_version, 2);
        }

        // Phase 2: Restore from checkpoint (simulating restart)
        {
            let provider = Arc::new(MockLlm::new("restored response", 10, 5));
            let mut behavior = DefaultAgentBehavior::new(provider, simple_counter())
                .with_checkpoint_store(store.clone());

            behavior.on_start(&agent_config).await.unwrap();

            // Session should be restored from checkpoint
            assert_eq!(behavior.session().len(), 4);
            assert_eq!(behavior.checkpoint_version, 2);

            // Execute one more — should continue from restored state
            behavior
                .execute(AgentInput::text("continue"))
                .await
                .unwrap();
            assert_eq!(behavior.session().len(), 6); // 4 restored + 2 new
            assert_eq!(behavior.checkpoint_version, 3);
        }
    }
}
